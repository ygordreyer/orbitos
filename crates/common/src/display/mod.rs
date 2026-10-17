pub mod color;
pub mod font;
pub mod image;
pub mod overlay;
pub mod packed;
pub mod settings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tiny_skia::{
    BlendMode, FillRule, Paint, Path, PathBuilder, PixmapMut, PixmapRef, Stroke, Transform,
};

use crate::display::color::Color;
use crate::display::overlay::{CapturedOverlay, OverlayContent, PreparedOverlay};
use crate::geom::{Point, Rect, Size};

/// Shared immutable frame storage; writes detach so cached frames stay unchanged.
pub(crate) struct SharedPixmap(pub(crate) Arc<tiny_skia::Pixmap>);

impl From<tiny_skia::Pixmap> for SharedPixmap {
    fn from(pixels: tiny_skia::Pixmap) -> Self {
        Self(Arc::new(pixels))
    }
}

impl SharedPixmap {
    pub(crate) fn clear(&mut self) {
        if let Some(pixels) = Arc::get_mut(&mut self.0) {
            pixels.data_mut().fill(0);
        } else {
            *self = tiny_skia::Pixmap::new(self.width(), self.height()).unwrap().into();
        }
    }

    pub(crate) fn into_pixmap(self) -> tiny_skia::Pixmap {
        Arc::unwrap_or_clone(self.0)
    }
}

impl std::ops::Deref for SharedPixmap {
    type Target = tiny_skia::Pixmap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SharedPixmap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if Arc::strong_count(&self.0) > 1 {
            if self.0.width() == 640 && self.0.height() == 480 {
                crate::perf_trace::count(crate::perf_trace::Count::FullFrameDetach, 1);
            }
            crate::perf_trace::count(
                crate::perf_trace::Count::CopiedPixels,
                self.0.width() as u64 * self.0.height() as u64,
            );
        }
        Arc::make_mut(&mut self.0)
    }
}

/// A thread stamping a region over a repainting foreground app; it runs until this is dropped
pub struct RectHold {
    stop: Arc<AtomicBool>,
    writer: Option<std::thread::JoinHandle<Result<()>>>,
}

impl RectHold {
    /// Runs `stamp` on a thread until the returned hold is dropped; it polls the flag it is handed
    pub fn spawn(stamp: impl FnOnce(&AtomicBool) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let writer = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                stamp(&stop);
                Ok(())
            }
        });
        Self {
            stop,
            writer: Some(writer),
        }
    }
    pub fn try_spawn(
        stamp: impl FnOnce(&AtomicBool) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let writer = std::thread::Builder::new()
            .name("overlay-hold".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || stamp(&stop)
            })?;
        Ok(Self {
            stop,
            writer: Some(writer),
        })
    }
    /// Stop and acknowledge completion before restoring pixels/capturing new geometry.
    /// Call without holding any Display/framebuffer lock the writer may need.
    /// Existing Drop remains stop-only; OS I/O can block, so no deadline is promised.
    pub fn stop_and_join(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| anyhow::anyhow!("Framebuffer hold writer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for RectHold {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub trait Display: Sized {
    /// Get the width of the display in pixels
    fn width(&self) -> u32;

    /// Get the height of the display in pixels
    fn height(&self) -> u32;

    /// Get the size of the display
    fn size(&self) -> Size {
        Size::new(self.width(), self.height())
    }

    /// Get the bounding box of the display (entire screen area)
    fn bounding_box(&self) -> Rect {
        Rect::new(0, 0, self.width(), self.height())
    }

    /// Get a reference to the underlying pixmap
    fn pixmap(&self) -> PixmapRef<'_>;

    /// Get a mutable reference to the underlying pixmap
    fn pixmap_mut(&mut self) -> PixmapMut<'_>;

    /// Adopt a complete frame without copying. Return false without changing the display
    /// if unsupported. Subsequent writes must preserve other owners' pixels.
    fn set_shared_pixmap(&mut self, _pixels: &Arc<tiny_skia::Pixmap>) -> bool {
        false
    }

    /// Set the diagnostic on every successful return, including declined admission.
    fn pack_spans(&mut self, _pixels: &Arc<tiny_skia::Pixmap>, _areas: &[Rect], _available: usize, diagnostic: &mut packed::PackDiagnostic) -> Result<Option<packed::PackedSpans>> {
        *diagnostic = packed::PackDiagnostic::new("pack_unsupported_display");
        diagnostic.record();
        Ok(None)
    }

    fn packed_base_matches(&self, _packed: &packed::PackedSpans) -> bool { false }

    fn flush_packed_rects(&mut self, areas: &[Rect], _packed: &packed::PackedSpans) -> Result<()> {
        if crate::perf_trace::recording() { crate::perf_trace::packed_span_reason("unsupported_display"); }
        self.flush_rects(areas)
    }

    /// Apply a function to all pixels
    fn map_pixels<F>(&mut self, f: F) -> Result<()>
    where
        F: FnMut(Color) -> Color;

    /// Clear the display with a solid color
    fn clear(&mut self, color: Color) -> Result<()> {
        let rect = self.bounding_box();
        fill_rect(&mut self.pixmap_mut(), rect, color);
        Ok(())
    }

    /// Changes when external reads or framebuffer layout changes invalidate retained pixels.
    /// Page repair and ordinary drawing do not change this revision.
    fn composition_revision(&mut self) -> Result<u64> {
        Ok(0)
    }

    /// Whether the visible backing needs repair even when composition has no damage.
    fn needs_present(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Flush any pending changes to the display
    fn flush(&mut self) -> Result<()> {
        crate::perf_trace::count(
            crate::perf_trace::Count::FlushedPixels,
            self.width() as u64 * self.height() as u64,
        );
        Ok(())
    }

    /// Flush a region of the display; defaults to a full flush
    fn flush_rect(&mut self, _area: Rect) -> Result<()> {
        self.flush()
    }

    /// Present a damage batch. Platforms without partial presentation flush once.
    fn flush_rects(&mut self, _areas: &[Rect]) -> Result<()> {
        self.flush()
    }

    /// Read `area` of the visible frame into the pixmap; a no-op where it already holds it
    fn read_rect(&mut self, _area: Rect) -> Result<()> {
        Ok(())
    }

    /// Keep `area`, already drawn by the caller, restamped over an app that repaints the
    /// screen, until the returned hold is dropped. `None` when the platform cannot stamp
    fn hold_rect(&mut self, _area: Rect, _corner_radius: u32) -> Result<Option<RectHold>> {
        Ok(None)
    }

    fn capture_overlay(&mut self, _overlay: &PreparedOverlay) -> Result<CapturedOverlay> {
        anyhow::bail!("Masked overlay unsupported by this Display")
    }

    fn check_overlay(&mut self, _overlay: &PreparedOverlay) -> Result<()> {
        anyhow::bail!("Masked overlay unsupported by this Display")
    }

    fn present_overlay(
        &mut self,
        _overlay: &CapturedOverlay,
        _content: OverlayContent,
    ) -> Result<()> {
        anyhow::bail!("Masked overlay unsupported by this Display")
    }

    fn hold_overlay(&mut self, _overlay: Arc<CapturedOverlay>) -> Result<Option<RectHold>> {
        anyhow::bail!("Masked overlay unsupported by this Display")
    }

    /// Sync with the display hardware
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    /// Save the current display state
    fn save(&mut self) -> Result<()>;

    fn save_unchanged(&mut self) -> Result<()> {
        self.save()
    }

    /// Load a previously saved display state
    fn load(&mut self, area: Rect) -> Result<()>;

    /// Pop the most recent saved state
    fn pop(&mut self) -> bool;
}

// Primitive drawing helpers

/// Fill a rectangle on the pixmap
pub fn fill_rect(pixmap: &mut PixmapMut<'_>, rect: Rect, color: Color) {
    let w = rect
        .right()
        .min(pixmap.width() as i32)
        .saturating_sub(rect.x.max(0))
        .max(0) as u64;
    let h = rect
        .bottom()
        .min(pixmap.height() as i32)
        .saturating_sub(rect.y.max(0))
        .max(0) as u64;
    crate::perf_trace::count(crate::perf_trace::Count::ComposedPixels, w * h);
    if color.a() == 255 {
        let width = pixmap.width() as usize;
        let x0 = rect.x.max(0).min(pixmap.width() as i32) as usize;
        let x1 = rect.right().max(0).min(pixmap.width() as i32) as usize;
        if x0 >= x1 {
            return;
        }
        let y0 = rect.y.max(0).min(pixmap.height() as i32) as usize;
        let y1 = rect.bottom().max(0).min(pixmap.height() as i32) as usize;
        let pixel = color.into();
        for y in y0..y1 {
            pixmap.pixels_mut()[y * width + x0..y * width + x1].fill(pixel);
        }
        return;
    }
    let paint = Paint {
        shader: tiny_skia::Shader::SolidColor(color.into()),
        blend_mode: BlendMode::SourceOver,
        anti_alias: false,
        ..Default::default()
    };

    if let Some(ts_rect) =
        tiny_skia::Rect::from_xywh(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32)
    {
        let path = PathBuilder::from_rect(ts_rect);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Fill a rounded rectangle on the pixmap
pub fn fill_rounded_rect(pixmap: &mut PixmapMut<'_>, rect: Rect, radius: u32, color: Color) {
    let w = rect
        .right()
        .min(pixmap.width() as i32)
        .saturating_sub(rect.x.max(0))
        .max(0) as u64;
    let h = rect
        .bottom()
        .min(pixmap.height() as i32)
        .saturating_sub(rect.y.max(0))
        .max(0) as u64;
    crate::perf_trace::count(crate::perf_trace::Count::ComposedPixels, w * h);
    let paint = Paint {
        shader: tiny_skia::Shader::SolidColor(color.into()),
        blend_mode: BlendMode::SourceOver,
        anti_alias: true, // Enable AA for rounded corners
        ..Default::default()
    };

    if let Some(path) = build_rounded_rect_path(rect, radius) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Fill a circle on the pixmap
pub fn fill_circle(pixmap: &mut PixmapMut<'_>, center: Point, radius: u32, color: Color) {
    let paint = Paint {
        shader: tiny_skia::Shader::SolidColor(color.into()),
        blend_mode: BlendMode::SourceOver,
        anti_alias: true, // Enable AA for circles
        ..Default::default()
    };

    if let Some(path) = PathBuilder::from_circle(center.x as f32, center.y as f32, radius as f32) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Stroke a rectangle on the pixmap
pub fn stroke_rect(pixmap: &mut PixmapMut<'_>, rect: Rect, stroke_width: f32, color: Color) {
    let paint = Paint {
        shader: tiny_skia::Shader::SolidColor(color.into()),
        blend_mode: BlendMode::SourceOver,
        anti_alias: false,
        ..Default::default()
    };

    let stroke = Stroke {
        width: stroke_width,
        ..Default::default()
    };

    if let Some(ts_rect) =
        tiny_skia::Rect::from_xywh(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32)
    {
        let path = PathBuilder::from_rect(ts_rect);
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Draw a speaker-with-waves volume icon inside `rect`
pub fn draw_speaker_icon(pixmap: &mut PixmapMut<'_>, rect: Rect, color: Color) {
    let s = rect.w.min(rect.h) as f32;
    let (x, y) = (rect.x as f32, rect.y as f32);
    let paint = icon_paint(color);

    // Speaker body and cone as one polygon
    let mut pb = PathBuilder::new();
    pb.move_to(x + 0.08 * s, y + 0.36 * s);
    pb.line_to(x + 0.26 * s, y + 0.36 * s);
    pb.line_to(x + 0.48 * s, y + 0.16 * s);
    pb.line_to(x + 0.48 * s, y + 0.84 * s);
    pb.line_to(x + 0.26 * s, y + 0.64 * s);
    pb.line_to(x + 0.08 * s, y + 0.64 * s);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // Two sound waves to the right of the cone
    let stroke = icon_stroke(s);
    let (cx, cy) = (x + 0.52 * s, y + 0.5 * s);
    let sweep = std::f32::consts::PI * 0.6;
    const SEGMENTS: u32 = 12;
    for radius in [0.22 * s, 0.36 * s] {
        let mut pb = PathBuilder::new();
        for i in 0..=SEGMENTS {
            let angle = -sweep / 2.0 + sweep * i as f32 / SEGMENTS as f32;
            let px = cx + radius * angle.cos();
            let py = cy + radius * angle.sin();
            if i == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

/// Draw a sun brightness icon inside `rect`
pub fn draw_sun_icon(pixmap: &mut PixmapMut<'_>, rect: Rect, color: Color) {
    let s = rect.w.min(rect.h) as f32;
    let (cx, cy) = (rect.x as f32 + 0.5 * s, rect.y as f32 + 0.5 * s);
    let paint = icon_paint(color);

    if let Some(path) = PathBuilder::from_circle(cx, cy, 0.18 * s) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // Eight rays around the core
    let stroke = icon_stroke(s);
    let mut pb = PathBuilder::new();
    for i in 0..8 {
        let (sin, cos) = (std::f32::consts::FRAC_PI_4 * i as f32).sin_cos();
        pb.move_to(cx + 0.30 * s * cos, cy + 0.30 * s * sin);
        pb.line_to(cx + 0.44 * s * cos, cy + 0.44 * s * sin);
    }
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn icon_paint(color: Color) -> Paint<'static> {
    Paint {
        shader: tiny_skia::Shader::SolidColor(color.into()),
        blend_mode: BlendMode::SourceOver,
        anti_alias: true,
        ..Default::default()
    }
}

fn icon_stroke(icon_side: f32) -> Stroke {
    Stroke {
        width: (icon_side / 12.0).max(1.0),
        line_cap: tiny_skia::LineCap::Round,
        ..Default::default()
    }
}

/// Build a path for a rounded rectangle
pub fn build_rounded_rect_path(rect: Rect, radius: u32) -> Option<Path> {
    let x = rect.x as f32;
    let y = rect.y as f32;
    let w = rect.w as f32;
    let h = rect.h as f32;
    let r = radius.min(rect.w / 2).min(rect.h / 2) as f32;

    // Bezier control point offset for 90° arc: 4/3 * tan(π/8)
    const K: f32 = 0.552_284_8;
    let k = r * K;

    let mut pb = PathBuilder::new();

    // Start at top-left corner (after the radius)
    pb.move_to(x + r, y);

    // Top edge
    pb.line_to(x + w - r, y);

    // Top-right corner
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);

    // Right edge
    pb.line_to(x + w, y + h - r);

    // Bottom-right corner
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);

    // Bottom edge
    pb.line_to(x + r, y + h);

    // Bottom-left corner
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);

    // Left edge
    pb.line_to(x, y + r);

    // Top-left corner
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);

    pb.close();
    pb.finish()
}

#[cfg(any(test, feature = "miyoo", feature = "perf-bench"))]
pub(crate) fn save_shared_snapshot(
    saved: &mut Vec<std::sync::Arc<tiny_skia::Pixmap>>,
    pixels: &tiny_skia::Pixmap,
) {
    let next = saved
        .last()
        .filter(|last| last.as_ref() == pixels)
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(pixels.clone()));
    saved.push(next);
}

#[cfg(any(test, feature = "miyoo", feature = "perf-bench"))]
pub(crate) fn save_unchanged_snapshot(saved: &mut Vec<Arc<tiny_skia::Pixmap>>) -> Result<()> {
    let last = saved
        .last()
        .ok_or_else(|| anyhow::anyhow!("No saved image"))?
        .clone();
    saved.push(last);
    Ok(())
}

#[cfg(any(test, feature = "miyoo", feature = "perf-bench"))]
pub(crate) fn save_reusing_snapshot(
    saved: &mut Vec<Arc<tiny_skia::Pixmap>>,
    pixels: &tiny_skia::Pixmap,
    reusable: &mut Vec<tiny_skia::Pixmap>,
) {
    if let Some(last) = saved.last().filter(|last| last.as_ref() == pixels) {
        saved.push(last.clone());
        return;
    }
    let mut next = reusable
        .pop()
        .filter(|p| p.width() == pixels.width() && p.height() == pixels.height())
        .unwrap_or_else(|| pixels.clone());
    next.data_mut().copy_from_slice(pixels.data());
    saved.push(Arc::new(next));
}

#[cfg(any(test, feature = "miyoo", feature = "perf-bench"))]
pub(crate) fn pop_reusing_snapshot(
    saved: &mut Vec<Arc<tiny_skia::Pixmap>>,
    reusable: &mut Vec<tiny_skia::Pixmap>,
) -> bool {
    if let Some(image) = saved.pop().and_then(|image| Arc::try_unwrap(image).ok()) {
        if reusable.len() < 2 {
            reusable.push(image);
        }
    }
    !saved.is_empty()
}

#[cfg(test)]
mod snapshot_tests {
    #[test]
    fn shared_pixmap_clear_reuses_unique_storage_and_preserves_shared_pixels() {
        use super::SharedPixmap;
        use std::sync::Arc;
        let mut pixels: SharedPixmap = tiny_skia::Pixmap::new(8, 6).unwrap().into();
        pixels.fill(tiny_skia::Color::WHITE);
        let address = pixels.data().as_ptr();
        pixels.clear();
        assert_eq!(pixels.data().as_ptr(), address);
        assert!(pixels.data().iter().all(|byte| *byte == 0));
        pixels.fill(tiny_skia::Color::WHITE);
        let presented = Arc::clone(&pixels.0);
        pixels.clear();
        assert!(!Arc::ptr_eq(&pixels.0, &presented));
        assert_eq!((pixels.width(), pixels.height()), (8, 6));
        assert!(pixels.data().iter().all(|byte| *byte == 0));
        assert!(presented.data().iter().all(|byte| *byte == 255));
    }

    #[test]
    fn ux4_identical_saves_share_pixels_and_changed_saves_stay_independent() {
        let mut pixels = tiny_skia::Pixmap::new(640, 480).unwrap();
        let mut saved = vec![];
        super::save_shared_snapshot(&mut saved, &pixels);
        super::save_shared_snapshot(&mut saved, &pixels);
        assert!(std::sync::Arc::ptr_eq(&saved[0], &saved[1]));
        pixels.data_mut()[0] = 255;
        super::save_shared_snapshot(&mut saved, &pixels);
        assert!(!std::sync::Arc::ptr_eq(&saved[1], &saved[2]));
        assert_eq!(saved[0].data()[0], 0);
        assert_eq!(saved[2].data()[0], 255);
    }
    #[test]
    fn unchanged_save_preserves_two_restore_levels_and_original_screenshot() {
        let mut pixels = tiny_skia::Pixmap::new(640, 480).unwrap();
        pixels.data_mut()[17] = 83;
        let mut saved = vec![];
        super::save_shared_snapshot(&mut saved, &pixels);
        super::save_unchanged_snapshot(&mut saved).unwrap();
        assert!(std::sync::Arc::ptr_eq(&saved[0], &saved[1]));
        pixels.data_mut()[17] = 129;
        assert_eq!(saved[0].data()[17], 83);
        pixels
            .data_mut()
            .copy_from_slice(saved.last().unwrap().data());
        saved.pop();
        assert_eq!(pixels.data()[17], 83);
        pixels.data_mut()[17] = 255;
        pixels
            .data_mut()
            .copy_from_slice(saved.last().unwrap().data());
        saved.pop();
        assert_eq!(pixels.data()[17], 83);
        assert!(saved.is_empty());
        assert!(super::save_unchanged_snapshot(&mut saved).is_err());
    }

    #[test]
    fn snapshots_reuse_both_legacy_buffers_across_opens() {
        let mut pixels = tiny_skia::Pixmap::new(640, 480).unwrap();
        let mut saved = vec![];
        let mut reusable = vec![];
        let mut storage = vec![];
        for session in 0..4 {
            pixels.data_mut()[17] = session;
            super::save_reusing_snapshot(&mut saved, &pixels, &mut reusable);
            pixels.data_mut()[17] = session + 64;
            super::save_reusing_snapshot(&mut saved, &pixels, &mut reusable);
            assert_eq!(saved[0].data()[17], session);
            assert_eq!(saved[1].data()[17], session + 64);
            let mut pointers: Vec<_> = saved.iter().map(|p| p.data().as_ptr()).collect();
            pointers.sort();
            if session == 0 {
                storage = pointers;
            } else {
                assert_eq!(pointers, storage);
            }
            assert!(super::pop_reusing_snapshot(&mut saved, &mut reusable));
            assert!(!super::pop_reusing_snapshot(&mut saved, &mut reusable));
            assert_eq!(reusable.len(), 2);
        }
    }
}

#[cfg(test)]
mod dl17_stroke_tests {
    #[test]
    fn opaque_stroke_preserves_interior_and_matches_translucent_shape() {
        let mut opaque = tiny_skia::Pixmap::new(20, 20).unwrap();
        let mut alpha = opaque.clone();
        let rect = crate::geom::Rect::new(3, 3, 14, 14);
        super::stroke_rect(
            &mut opaque.as_mut(),
            rect,
            2.0,
            super::color::Color::rgba(80, 90, 100, 255),
        );
        super::stroke_rect(
            &mut alpha.as_mut(),
            rect,
            2.0,
            super::color::Color::rgba(80, 90, 100, 254),
        );
        assert_eq!(opaque.pixel(10, 10).unwrap().alpha(), 0);
        assert!(
            opaque
                .pixels()
                .iter()
                .zip(alpha.pixels())
                .all(|(a, b)| (a.alpha() == 0) == (b.alpha() == 0))
        );
    }
}
