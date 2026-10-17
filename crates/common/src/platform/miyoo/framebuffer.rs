use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
#[cfg(feature = "miyoo")]
use framebuffer::{FixScreeninfo, Framebuffer, VarScreeninfo};
#[cfg(all(any(test, feature = "perf-bench"), not(feature = "miyoo")))]
#[path = "framebuffer_test_iface.rs"]
mod test_iface;
use log::{debug, trace, warn};
#[cfg(all(any(test, feature = "perf-bench"), not(feature = "miyoo")))]
use test_iface::{FixScreeninfo, Framebuffer, VarScreeninfo};
use tiny_skia::{Pixmap, PixmapMut, PixmapRef};

use crate::display::color::Color;
use crate::display::overlay::{
    CapturedOverlay, OverlayContent, OverlayFormat, OverlayLayout, OverlayLayoutSpec,
    PreparedOverlay,
};
use crate::display::{Display, RectHold};
use crate::display::packed::{PackDiagnostic, PackedRow, PackedSpans};
use crate::geom::Rect;

/// Pause between stamping passes, matching Onion; probing or yielding instead costs whole
/// frames on this dual-core SoC
const STAMP_PAUSE: Duration = Duration::from_micros(100);
static NEXT_DISPLAY_ID: AtomicU64 = AtomicU64::new(1);

#[path = "framebuffer_damage.rs"]
mod framebuffer_damage;
use framebuffer_damage::PageHistory;

#[inline]
fn copy_packed_row(target: &mut [u8], source: &[u8]) {
    #[cfg(all(target_arch = "arm", target_feature = "neon"))]
    {
        use core::arch::arm::{vld1q_u8, vst1q_u8};
        assert_eq!(target.len(), source.len());
        let mut copied = 0;
        // The cfg guarantees NEON. Disjoint, checked slices bound every unaligned
        // load/store to a complete 64-byte block; the tail stays slice-checked.
        unsafe {
            while source.len() - copied >= 64 {
                let src = source.as_ptr().add(copied);
                let dst = target.as_mut_ptr().add(copied);
                let a = vld1q_u8(src);
                let b = vld1q_u8(src.add(16));
                let c = vld1q_u8(src.add(32));
                let d = vld1q_u8(src.add(48));
                vst1q_u8(dst, a);
                vst1q_u8(dst.add(16), b);
                vst1q_u8(dst.add(32), c);
                vst1q_u8(dst.add(48), d);
                copied += 64;
            }
        }
        target[copied..].copy_from_slice(&source[copied..]);
    }
    #[cfg(not(all(target_arch = "arm", target_feature = "neon")))]
    target.copy_from_slice(source);
}

pub struct FramebufferDisplay {
    page_history: Option<PageHistory>,
    present_damage: Vec<Rect>,
    composition_revision: u64,
    pixmap: crate::display::SharedPixmap,
    iface: Framebuffer,
    scratch: Vec<u8>,
    capture_pread: bool,
    saved: Vec<Arc<Pixmap>>,
    reusable_snapshots: Vec<Pixmap>,
    overlay_instance: u64,
    overlay_mapping: Option<OverlayLayoutSpec>,
}

impl FramebufferDisplay {
    pub fn new() -> Result<FramebufferDisplay> {
        let mut display = Self::blank()?;
        let frame = display.bounding_box();
        display.read_frame_rect(frame)?;
        Ok(display)
    }

    /// A display whose pixmap starts empty; the caller reads what it needs with `read_rect`
    pub fn blank() -> Result<FramebufferDisplay> {
        let iface = Framebuffer::new("/dev/fb0")?;
        trace!(
            "init fb: var_screen_info: {:?}, fix_screen_info: {:?}",
            iface.var_screen_info, iface.fix_screen_info,
        );

        validate_layout(&iface, iface.var_screen_info.yoffset as usize)?;
        let width = iface.var_screen_info.xres;
        let height = iface.var_screen_info.yres;
        let pixmap = Pixmap::new(width, height)
            .ok_or_else(|| anyhow!("Failed to create pixmap {}x{}", width, height))?;

        let overlay_instance = NEXT_DISPLAY_ID.fetch_add(1, Ordering::Relaxed);
        let overlay_mapping = overlay_spec(
            &iface,
            overlay_instance,
            &iface.var_screen_info,
            &iface.fix_screen_info,
        )
        .ok();
        Ok(FramebufferDisplay {
            page_history: None,
            present_damage: Vec::new(),
            composition_revision: 0,
            scratch: Vec::new(),
            capture_pread: std::env::var("ALLIUM_FB_CAPTURE").is_ok_and(|v| v == "pread"),
            pixmap: pixmap.into(),
            iface,
            saved: Vec::new(),
            reusable_snapshots: Vec::with_capacity(2),
            overlay_instance,
            overlay_mapping,
        })
    }

    /// Copies `rect` of the visible frame into the pixmap, unrotating it and BGRA to RGBA
    fn read_frame_rect(&mut self, rect: Rect) -> Result<()> {
        self.composition_revision = self.composition_revision.wrapping_add(1);
        validate_layout(&self.iface, self.iface.var_screen_info.yoffset as usize)?;
        let width = self.pixmap.width() as usize;
        let height = self.pixmap.height() as usize;
        let bytes_per_pixel = (self.iface.var_screen_info.bits_per_pixel / 8) as usize;
        let xoffset = self.iface.var_screen_info.xoffset as usize;
        let yoffset = self.iface.var_screen_info.yoffset as usize;
        let location = (yoffset * width + xoffset) * bytes_per_pixel;
        crate::platform::framebuffer_pixels::capture_rotated_bgra(
            &mut self.scratch,
            &mut self.pixmap,
            bytes_per_pixel,
            (
                rect.x.max(0) as usize,
                rect.y.max(0) as usize,
                (rect.right().max(0) as usize).min(width),
                (rect.bottom().max(0) as usize).min(height),
            ),
            |cached, offset| {
                let at = location + offset;
                if !self.capture_pread
                    || self.iface.device.read_exact_at(cached, at as u64).is_err()
                {
                    cached.copy_from_slice(&self.iface.read_frame()[at..at + cached.len()]);
                }
            },
        );
        Ok(())
    }

    /// Packs `area` into fb-ordered rows, so the stamping thread only does memcpy
    fn stamp(&self, area: Rect, corner_radius: u32) -> Option<Stamp> {
        validate_layout(&self.iface, 0).ok()?;
        let width = self.width() as usize;
        let height = self.height() as usize;
        let bytes_per_pixel = (self.iface.var_screen_info.bits_per_pixel / 8) as usize;

        let x0 = area.x.max(0) as usize;
        let y0 = area.y.max(0) as usize;
        let x1 = (area.right().max(0) as usize).min(width);
        let y1 = (area.bottom().max(0) as usize).min(height);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        // Trim to the rounding so the corners keep the app's pixels, not a frozen frame
        let radius = (corner_radius as usize)
            .min((x1 - x0) / 2)
            .min((y1 - y0) / 2) as f32;
        let row_h = (y1 - y0) as f32;
        let mut rows = Vec::with_capacity(y1 - y0);
        let mut bytes = Vec::with_capacity((y1 - y0) * (x1 - x0) * bytes_per_pixel);
        for y in y0..y1 {
            let dy = (y - y0) as f32 + 0.5;
            let dist = (radius - dy).max(dy - (row_h - radius)).max(0.0);
            let inset = (radius - (radius * radius - dist * dist).sqrt()).ceil() as usize;
            let (rx0, rx1) = (x0 + inset, x1 - inset);
            if rx0 >= rx1 {
                continue;
            }
            let start = bytes.len();
            // Reversed x, and fb_y below, are the two halves of the 180 degree rotation
            for x in (rx0..rx1).rev() {
                let pixel = self.pixmap.pixels()[y * width + x];
                bytes.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
            }
            let fb_y = height - 1 - y;
            let fb_x0 = width - rx1;
            rows.push(StampRow {
                offset: (fb_y * width + fb_x0) * bytes_per_pixel,
                start,
                len: bytes.len() - start,
            });
        }
        if rows.is_empty() {
            return None;
        }

        let pages = (self.iface.var_screen_info.yres_virtual as usize / height.max(1)).clamp(1, 3);
        debug!(
            "holding {}x{} rect over fb {}x{} (virtual {}, stride {}, {} bpp, {} pages)",
            x1 - x0,
            y1 - y0,
            width,
            height,
            self.iface.var_screen_info.yres_virtual,
            self.iface.fix_screen_info.line_length,
            bytes_per_pixel * 8,
            pages,
        );
        Some(Stamp {
            bytes: bytes.into_boxed_slice(),
            rows: rows.into_boxed_slice(),
            pages,
            page_stride: width * height * bytes_per_pixel,
        })
    }

    fn refresh_present_layout(&mut self) -> Result<()> {
        let var = Framebuffer::get_var_screeninfo(&self.iface.device)?;
        let fix = Framebuffer::get_fix_screeninfo(&self.iface.device)?;
        anyhow::ensure!(
            var.xres == self.pixmap.width() && var.yres == self.pixmap.height(),
            "Framebuffer dimensions changed"
        );
        let old = &self.iface.var_screen_info;
        let old_fix = &self.iface.fix_screen_info;
        // A pan changes the page to repair, not the logical pixmap or its damage base.
        if var.xres_virtual != old.xres_virtual || var.yres_virtual != old.yres_virtual
            || var.xoffset != old.xoffset || var.bits_per_pixel != old.bits_per_pixel
            || fix.line_length != old_fix.line_length || fix.smem_start != old_fix.smem_start
            || fix.smem_len != old_fix.smem_len
        {
            self.composition_revision = self.composition_revision.wrapping_add(1);
        }
        // Screen-info refreshes cannot retarget the existing mmap to a new backing.
        let mapped_length = |fix: &FixScreeninfo, var: &VarScreeninfo| {
            u64::from(fix.line_length) * u64::from(var.yres_virtual)
        };
        if fix.smem_start != self.iface.fix_screen_info.smem_start
            || fix.smem_len != self.iface.fix_screen_info.smem_len
            || mapped_length(&fix, &var)
                != mapped_length(&self.iface.fix_screen_info, &self.iface.var_screen_info)
        {
            self.page_history = None;
            let iface = Framebuffer::new("/dev/fb0").map_err(|error| {
                anyhow!("Failed to remap framebuffer; reinitialize display: {error}")
            })?;
            anyhow::ensure!(
                iface.var_screen_info.xres == self.pixmap.width()
                    && iface.var_screen_info.yres == self.pixmap.height(),
                "Framebuffer dimensions changed during remap"
            );
            validate_layout(&iface, iface.var_screen_info.yoffset as usize)?;
            self.iface = iface;
        } else {
            self.iface.var_screen_info = var;
            self.iface.fix_screen_info = fix;
        }
        validate_layout(&self.iface, self.iface.var_screen_info.yoffset as usize)
    }

    fn packed_layout(&self) -> [u64; 32] {
        let v = &self.iface.var_screen_info;
        let f = &self.iface.fix_screen_info;
        [self.overlay_instance, v.xres as u64, v.yres as u64, v.xres_virtual as u64,
            v.yres_virtual as u64, v.xoffset as u64, v.yoffset as u64, v.bits_per_pixel as u64,
            v.grayscale as u64, v.nonstd as u64, v.rotate as u64, v.vmode as u64,
            v.red.offset as u64, v.red.length as u64, v.red.msb_right as u64,
            v.green.offset as u64, v.green.length as u64, v.green.msb_right as u64,
            v.blue.offset as u64, v.blue.length as u64, v.blue.msb_right as u64,
            v.transp.offset as u64, v.transp.length as u64, v.transp.msb_right as u64,
            f.line_length as u64, f.smem_start as u64, f.smem_len as u64,
            f.fb_type as u64, f.visual as u64, self.iface.frame.len() as u64, self.iface.frame.as_ptr() as u64, 0]
    }

    fn write_packed_rect(&mut self, area: Rect, packed: &PackedSpans) -> Result<bool> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        let location = self.iface.var_screen_info.yoffset as usize * width * 4;
        let (x0, x1) = (area.x as usize, area.right() as usize);
        let mut copied_pixels = 0;
        for y in area.y as usize..area.bottom() as usize {
            let first = packed.rows.partition_point(|row| row.y < y);
            let mut x = x0;
            for row in packed.rows[first..].iter().take_while(|row| row.y == y) {
                let left = row.x0.max(x);
                let right = row.x1.min(x1);
                if left >= right { continue; }
                if x < left { self.write_rect(Rect::new(x as i32, y as i32, (left - x) as u32, 1))?; }
                let at = location + ((height - 1 - y) * width + width - right) * 4;
                let start = row.start + (row.x1 - right) * 4;
                let len = (right - left) * 4;
                self.iface.frame[at..at + len].copy_from_slice(&packed.bytes[start..start + len]);
                copied_pixels += right - left;
                x = right;
            }
            if x < x1 { self.write_rect(Rect::new(x as i32, y as i32, (x1 - x) as u32, 1))?; }
        }
        crate::perf_trace::count(crate::perf_trace::Count::FlushedPixels, copied_pixels as u64);
        Ok(copied_pixels != 0)
    }

    fn present_rects(&mut self, areas: &[Rect], packed: Option<&PackedSpans>) -> Result<()> {
        let layout_stage = crate::perf_trace::recording()
            .then(|| crate::perf_trace::stage(crate::perf_trace::Stage::PresentLayout));
        self.refresh_present_layout()?;
        let packed = packed.filter(|packed| {
            let reason = if packed.layout != self.packed_layout() { "layout_or_format" }
                else if packed.revision != self.composition_revision { "composition_revision" }
                else if packed.candidate != Arc::as_ptr(&self.pixmap.0) as usize { "candidate_identity" }
                else if packed.damage != areas { "damage_mismatch" }
                else { "hit" };
            if crate::perf_trace::recording() { crate::perf_trace::packed_span_reason(reason); }
            reason == "hit"
        });
        drop(layout_stage);
        let history_stage = crate::perf_trace::stage(crate::perf_trace::Stage::PageHistory);
        let layout_changed = self
            .page_history
            .as_ref()
            .is_some_and(|h| !h.matches(&self.iface));
        if self.page_history.is_none() || layout_changed {
            self.page_history = PageHistory::new(&self.iface);
        }
        let Some(history) = &mut self.page_history else {
            if packed.is_some() && crate::perf_trace::recording() { crate::perf_trace::packed_span_reason("untracked_layout"); }
            crate::perf_trace::present_reason("untracked_layout");
            drop(history_stage);
            return self.flush();
        };
        let Some(page) = history.page(self.iface.var_screen_info.yoffset) else {
            if packed.is_some() && crate::perf_trace::recording() { crate::perf_trace::packed_span_reason("unaligned_page"); }
            crate::perf_trace::present_reason("unaligned_page");
            drop(history_stage);
            return self.flush();
        };
        let valid = history.is_valid(page);
        let direct = packed.is_some_and(|packed| packed.coverage_rects <= 4096)
            && !history.needs_repair(page);
        history.add(areas);
        // A clean page needs exactly the packet's clipped, aligned union. Other pages
        // may need older damage or a full repair, including pixels outside the packet.
        if direct {
            drop(history_stage);
            let packed = packed.unwrap();
            crate::perf_trace::present_reason(if packed.bytes.len() == self.pixmap.data().len() {
                "damage_full"
            } else { "partial_page_history" });
            if crate::perf_trace::recording() {
                crate::perf_trace::damage_counts(packed.coverage_rects as u64, (packed.bytes.len() / 4) as u64);
            }
            let _copy_stage = crate::perf_trace::recording()
                .then(|| crate::perf_trace::stage(crate::perf_trace::Stage::PresentCopy));
            let width = self.width() as usize;
            let height = self.height() as usize;
            let location = self.iface.var_screen_info.yoffset as usize * width * 4;
            for row in packed.rows.iter().rev() {
                let at = location + ((height - 1 - row.y) * width + width - row.x1) * 4;
                let len = (row.x1 - row.x0) * 4;
                copy_packed_row(&mut self.iface.frame[at..at + len], &packed.bytes[row.start..row.start + len]);
            }
            if crate::perf_trace::recording() {
                crate::perf_trace::count(crate::perf_trace::Count::FlushedPixels, (packed.bytes.len() / 4) as u64);
                crate::perf_trace::count(crate::perf_trace::Count::PackedSpanHit, 1);
            }
            self.page_history.as_mut().unwrap().presented(page);
            return Ok(());
        }
        let mut damage = std::mem::take(&mut self.present_damage);
        history.damage_into(page, &mut damage);
        drop(history_stage);
        let full_damage = damage.len() == 1 && damage[0] == self.bounding_box();
        crate::perf_trace::present_reason(if layout_changed {
            "framebuffer_layout_changed"
        } else if !valid {
            "page_uninitialized"
        } else if full_damage {
            "damage_full"
        } else {
            "partial_page_history"
        });
        crate::perf_trace::damage(&damage);
        let _copy_stage = crate::perf_trace::recording()
            .then(|| crate::perf_trace::stage(crate::perf_trace::Stage::PresentCopy));
        let mut copied = false;
        let result = damage.iter().copied().try_for_each(|area| {
            if let Some(packed) = packed { copied |= self.write_packed_rect(area, packed)?; }
            else { self.write_rect(area)?; }
            Ok::<_, anyhow::Error>(())
        });
        self.present_damage = damage;
        result?;
        if copied && crate::perf_trace::recording() {
            crate::perf_trace::count(crate::perf_trace::Count::PackedSpanHit, 1);
        }
        self.page_history.as_mut().unwrap().presented(page);
        Ok(())
    }

    fn write_rect(&mut self, rect: Rect) -> Result<()> {
        let yoffset = self.iface.var_screen_info.yoffset as usize;
        self.write_rect_at(rect, yoffset)
    }

    fn write_rect_at(&mut self, rect: Rect, yoffset: usize) -> Result<()> {
        validate_layout(&self.iface, yoffset)?;
        let xoffset = self.iface.var_screen_info.xoffset as usize;
        let width = self.width() as usize;
        let height = self.height() as usize;
        let bytes_per_pixel = (self.iface.var_screen_info.bits_per_pixel / 8) as usize;
        let location = (yoffset * width + xoffset) * bytes_per_pixel;

        if location + height * width * bytes_per_pixel > self.iface.frame.len() {
            return Ok(());
        }

        let x0 = rect.x.max(0) as usize;
        let y0 = rect.y.max(0) as usize;
        let x1 = (rect.right().max(0) as usize).min(width);
        let y1 = (rect.bottom().max(0) as usize).min(height);

        if x0 >= x1 || y0 >= y1 {
            return Ok(());
        }
        crate::perf_trace::count(
            crate::perf_trace::Count::FlushedPixels,
            ((x1 - x0) * (y1 - y0)) as u64,
        );
        if bytes_per_pixel != 4 {
            ensure_scratch(&mut self.scratch, width * height * bytes_per_pixel);
        }
        crate::platform::framebuffer_pixels::write_rotated_bgra(
            &mut self.iface.frame,
            &mut self.scratch,
            self.pixmap.as_ref(),
            location,
            bytes_per_pixel,
            (x0, y0, x1, y1),
        );
        Ok(())
    }
}

fn validate_layout(iface: &Framebuffer, yoffset: usize) -> Result<()> {
    let var = &iface.var_screen_info;
    crate::platform::framebuffer_pixels::validate_packed_layout(
        var.xres as usize,
        var.yres as usize,
        (var.bits_per_pixel / 8) as usize,
        iface.fix_screen_info.line_length as usize,
        var.xoffset as usize,
        yoffset,
        iface.frame.len(),
    )
}

fn ensure_scratch(scratch: &mut Vec<u8>, size: usize) {
    if scratch.len() != size {
        scratch.resize(size, 0);
    }
}

fn overlay_spec(
    iface: &Framebuffer,
    instance: u64,
    var: &VarScreeninfo,
    fix: &FixScreeninfo,
) -> Result<OverlayLayoutSpec> {
    let meta = iface.device.metadata()?;
    Ok(OverlayLayoutSpec {
        instance,
        device: (meta.dev(), meta.ino(), meta.rdev()),
        backing_start: fix.smem_start,
        organization: [var.grayscale, var.nonstd, fix.fb_type, fix.visual],
        channel_msb_right: [
            var.red.msb_right,
            var.green.msb_right,
            var.blue.msb_right,
            var.transp.msb_right,
        ],
        orientation: [var.rotate, var.vmode],
        width: var.xres,
        height: var.yres,
        virtual_width: var.xres_virtual,
        virtual_height: var.yres_virtual,
        xoffset: var.xoffset,
        yoffset: var.yoffset,
        row_stride: fix.line_length,
        mapped_bytes: iface.frame.len(),
        physical_bytes: fix.smem_len as usize,
        format: OverlayFormat {
            bits_per_pixel: var.bits_per_pixel,
            red: (var.red.offset, var.red.length),
            green: (var.green.offset, var.green.length),
            blue: (var.blue.offset, var.blue.length),
            alpha: (var.transp.offset, var.transp.length),
        },
    })
}

fn overlay_layout(iface: &Framebuffer, open: OverlayLayoutSpec) -> Result<OverlayLayout> {
    let var = Framebuffer::get_var_screeninfo(&iface.device)?;
    let fix = Framebuffer::get_fix_screeninfo(&iface.device)?;
    let observed = overlay_spec(iface, open.instance, &var, &fix)?;
    OverlayLayout::checked_binding(open, observed)
}

/// One row of the stamp: where it goes within a page, and its slice of `Stamp::bytes`
struct StampRow {
    offset: usize,
    start: usize,
    len: usize,
}

/// Rows of a rect in fb byte order, to be repeated across every page the app may flip to
struct Stamp {
    /// Every row back to back, so a pass reads the source linearly
    bytes: Box<[u8]>,
    rows: Box<[StampRow]>,
    pages: usize,
    page_stride: usize,
}

impl Stamp {
    fn blit(&self, frame: &mut [u8]) {
        for page in 0..self.pages {
            let base = page * self.page_stride;
            for row in &self.rows {
                let at = base + row.offset;
                if let Some(dst) = frame.get_mut(at..at + row.len) {
                    dst.copy_from_slice(&self.bytes[row.start..row.start + row.len]);
                }
            }
        }
    }
}

impl Display for FramebufferDisplay {
    fn width(&self) -> u32 {
        self.pixmap.width()
    }

    fn height(&self) -> u32 {
        self.pixmap.height()
    }

    fn pixmap(&self) -> PixmapRef<'_> {
        self.pixmap.as_ref()
    }

    fn pixmap_mut(&mut self) -> PixmapMut<'_> {
        self.pixmap.as_mut()
    }

    fn set_shared_pixmap(&mut self, pixels: &Arc<Pixmap>) -> bool {
        if pixels.width() != self.width() || pixels.height() != self.height() {
            return false;
        }
        self.pixmap = crate::display::SharedPixmap(Arc::clone(pixels));
        true
    }

    fn swap_shared_pixmap(&mut self, pixels: &mut Arc<Pixmap>) -> bool {
        if pixels.width() != self.width() || pixels.height() != self.height() {
            return false;
        }
        std::mem::swap(&mut self.pixmap.0, pixels);
        true
    }

    fn pack_spans(&mut self, pixels: &Arc<Pixmap>, areas: &[Rect], available: usize, diagnostic: &mut PackDiagnostic) -> Result<Option<PackedSpans>> {
        self.refresh_present_layout()?;
        let v = &self.iface.var_screen_info;
        macro_rules! decline {
            ($reason:expr) => {{
                *diagnostic = PackDiagnostic::new($reason);
                diagnostic.record();
                return Ok(None);
            }};
        }
        if v.bits_per_pixel != 32 { decline!("pack_format:bits_per_pixel"); }
        if v.grayscale != 0 { decline!("pack_format:grayscale"); }
        if v.nonstd != 0 { decline!("pack_format:nonstd"); }
        if (v.red.offset, v.red.length, v.red.msb_right) != (16, 8, 0) { decline!("pack_format:red"); }
        if (v.green.offset, v.green.length, v.green.msb_right) != (8, 8, 0) { decline!("pack_format:green"); }
        if (v.blue.offset, v.blue.length, v.blue.msb_right) != (0, 8, 0) { decline!("pack_format:blue"); }
        if v.transp.msb_right != 0 || !(v.transp.length == 0 || (v.transp.offset, v.transp.length) == (24, 8)) {
            decline!("pack_format:transp");
        }
        if pixels.width() != self.width() || pixels.height() != self.height() { decline!("pack_size"); }
        let width = self.width() as usize;
        let height = self.height() as usize;
        let mut rows = Vec::new();
        for y in 0..height {
            let mut intervals: Vec<_> = areas.iter().filter(|r| r.y <= y as i32 && r.bottom() > y as i32)
                .filter_map(|r| {
                    let x0 = r.x.max(0).min(width as i32) as usize;
                    let x1 = r.right().max(0).min(width as i32) as usize;
                    (x0 < x1).then(|| (x0 / 8 * 8, x1.div_ceil(8).saturating_mul(8).min(width)))
                }).collect();
            intervals.sort_unstable();
            for (x0, x1) in intervals {
                if let Some(row) = rows.last_mut().filter(|row: &&mut PackedRow| row.y == y && row.x1 >= x0) {
                    row.x1 = row.x1.max(x1);
                } else { rows.push(PackedRow { y, x0, x1, start: 0 }); }
            }
        }
        let length: usize = rows.iter().map(|row| (row.x1 - row.x0) * 4).sum();
        let coverage_rects = rows.iter().filter(|row| {
            let Some(previous_y) = row.y.checked_sub(1) else { return true; };
            let first = rows.partition_point(|r| r.y < previous_y);
            !rows[first..].iter().take_while(|r| r.y == previous_y)
                .any(|r| r.x0 == row.x0 && r.x1 == row.x1)
        }).count();
        let mut packed = PackedSpans { layout: self.packed_layout(), base: Arc::as_ptr(&self.pixmap.0) as usize,
            candidate: Arc::as_ptr(pixels) as usize, revision: self.composition_revision,
            damage: areas.to_vec(), rows, coverage_rects, bytes: Vec::new() };
        if length == 0 { decline!("pack_empty"); }
        let needed = packed.charge().saturating_add(length);
        if needed > available {
            *diagnostic = PackDiagnostic { reason: "pack_budget", needed_bytes: needed, available_bytes: available };
            diagnostic.record();
            return Ok(None);
        }
        packed.bytes = vec![0; length];
        if packed.charge() > available {
            *diagnostic = PackDiagnostic { reason: "pack_budget_capacity", needed_bytes: packed.charge(), available_bytes: available };
            diagnostic.record();
            return Ok(None);
        }
        let mut start = 0;
        // Keep rows searchable by (y, x0), but store bytes in direct-present order.
        for row in packed.rows.iter_mut().rev() {
            row.start = start;
            let len = (row.x1 - row.x0) * 4;
            crate::platform::framebuffer_pixels::write_reversed_bgra(&mut packed.bytes[start..start + len],
                &pixels.data()[(row.y * width + row.x0) * 4..(row.y * width + row.x1) * 4]);
            start += len;
        }
        *diagnostic = PackDiagnostic::new("hit");
        Ok(Some(packed))
    }

    fn packed_base_matches(&self, packed: &PackedSpans) -> bool {
        packed.base == Arc::as_ptr(&self.pixmap.0) as usize && packed.revision == self.composition_revision
    }

    fn sync(&mut self) -> Result<()> {
        self.page_history = None;
        self.iface.var_screen_info = Framebuffer::get_var_screeninfo(&self.iface.device)
            .map_err(|e| anyhow!("failed to get var_screen_info: {}", e))?;

        let yoffset = self.iface.var_screen_info.yoffset as usize;
        let width = self.width() as usize;
        let height = self.height() as usize;
        let bytes_per_pixel = (self.iface.var_screen_info.bits_per_pixel / 8) as usize;

        let frame = self.bounding_box();
        self.read_frame_rect(frame)?;

        if yoffset != 0 {
            let frame_size = width * height * bytes_per_pixel;
            let _phase = crate::perf_trace::open_phase("page_normalize");
            crate::platform::framebuffer_pixels::normalize_page(
                &mut self.iface.frame,
                &self.scratch[..frame_size],
            );
            self.iface.var_screen_info.yoffset = 0;
            Framebuffer::put_var_screeninfo(&self.iface.device, &self.iface.var_screen_info)
                .map_err(|e| anyhow!("failed to set var_screen_info: {}", e))?;
        }

        Ok(())
    }

    fn map_pixels<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(Color) -> Color,
    {
        for pixel in self.pixmap.pixels_mut() {
            let color: Color = (*pixel).into();
            *pixel = f(color).into();
        }
        Ok(())
    }

    fn composition_revision(&mut self) -> Result<u64> {
        self.refresh_present_layout()?;
        Ok(self.composition_revision)
    }

    fn needs_present(&mut self) -> Result<bool> {
        self.refresh_present_layout()?;
        Ok(self.page_history.as_ref().is_none_or(|history| {
            !history.matches(&self.iface)
                || history
                    .page(self.iface.var_screen_info.yoffset)
                    .is_none_or(|page| history.needs_repair(page))
        }))
    }

    fn flush(&mut self) -> Result<()> {
        self.refresh_present_layout()?;
        self.write_rect(self.bounding_box())?;
        self.page_history = PageHistory::new(&self.iface);
        if let Some(history) = &mut self.page_history {
            if let Some(page) = history.page(self.iface.var_screen_info.yoffset) {
                history.presented(page);
            }
        }
        Ok(())
    }

    fn flush_rect(&mut self, area: Rect) -> Result<()> {
        self.page_history = None;
        // The foreground app may have moved yoffset since creation.
        self.iface.var_screen_info = Framebuffer::get_var_screeninfo(&self.iface.device)
            .map_err(|e| anyhow!("failed to get var_screen_info: {}", e))?;
        self.write_rect(area)?;
        Ok(())
    }

    fn flush_rects(&mut self, areas: &[Rect]) -> Result<()> {
        self.present_rects(areas, None)
    }

    fn flush_packed_rects(&mut self, areas: &[Rect], packed: &PackedSpans) -> Result<()> {
        self.present_rects(areas, Some(packed))
    }

    fn read_rect(&mut self, area: Rect) -> Result<()> {
        self.page_history = None;
        // Re-read offsets: the foreground app may have moved yoffset since creation
        self.iface.var_screen_info = Framebuffer::get_var_screeninfo(&self.iface.device)
            .map_err(|e| anyhow!("failed to get var_screen_info: {}", e))?;
        self.read_frame_rect(area)?;
        Ok(())
    }

    fn hold_rect(&mut self, area: Rect, corner_radius: u32) -> Result<Option<RectHold>> {
        self.flush_rect(area)?;
        let Some(stamp) = self.stamp(area, corner_radius) else {
            return Ok(None);
        };

        Ok(Some(RectHold::spawn(move |stop| {
            let Ok(mut iface) = Framebuffer::new("/dev/fb0") else {
                return;
            };
            while !stop.load(Ordering::Relaxed) {
                stamp.blit(&mut iface.frame);
                std::thread::sleep(STAMP_PAUSE);
            }
        })))
    }

    fn capture_overlay(&mut self, overlay: &PreparedOverlay) -> Result<CapturedOverlay> {
        let open = self
            .overlay_mapping
            .ok_or_else(|| anyhow!("Overlay framebuffer open binding unavailable"))?;
        let layout = overlay_layout(&self.iface, open)?;
        CapturedOverlay::capture(overlay, layout, &self.iface.frame)
    }

    fn check_overlay(&mut self, overlay: &PreparedOverlay) -> Result<()> {
        let open = self
            .overlay_mapping
            .ok_or_else(|| anyhow!("Overlay framebuffer open binding unavailable"))?;
        let layout = overlay_layout(&self.iface, open)?;
        layout.check_overlay(overlay)
    }

    fn present_overlay(
        &mut self,
        overlay: &CapturedOverlay,
        content: OverlayContent,
    ) -> Result<()> {
        self.page_history = None;
        let open = self
            .overlay_mapping
            .ok_or_else(|| anyhow!("Overlay framebuffer open binding unavailable"))?;
        let layout = overlay_layout(&self.iface, open)?;
        overlay.present(&layout, &mut self.iface.frame, content)
    }

    fn hold_overlay(&mut self, overlay: Arc<CapturedOverlay>) -> Result<Option<RectHold>> {
        if !overlay.has_coverage() {
            return Ok(None);
        }
        let instance = self.overlay_instance;
        let mut iface = Framebuffer::new("/dev/fb0")?;
        let open = overlay_spec(
            &iface,
            instance,
            &iface.var_screen_info,
            &iface.fix_screen_info,
        )?;
        let layout = overlay_layout(&iface, open)?;
        overlay.check_layout(&layout)?;
        let pause = overlay.stamp_pause();
        let hold = RectHold::try_spawn(move |stop| {
            while !stop.load(Ordering::Acquire) {
                let current = overlay_layout(&iface, open)?;
                overlay.present(&current, &mut iface.frame, OverlayContent::Composited)?;
                std::thread::sleep(pause);
            }
            Ok(())
        })?;
        Ok(Some(hold))
    }

    fn save(&mut self) -> Result<()> {
        crate::display::save_reusing_snapshot(
            &mut self.saved,
            &self.pixmap,
            &mut self.reusable_snapshots,
        );
        Ok(())
    }

    fn save_unchanged(&mut self) -> Result<()> {
        crate::display::save_unchanged_snapshot(&mut self.saved)
    }

    fn load(&mut self, mut rect: Rect) -> Result<()> {
        let Some(saved) = self.saved.last() else {
            bail!("No saved image");
        };

        let size = self.size();
        if rect.x < 0
            || rect.y < 0
            || rect.x as u32 + rect.w > size.w
            || rect.y as u32 + rect.h > size.h
        {
            warn!(
                "Area exceeds display bounds: x: {}, y: {}, w: {}, h: {}",
                rect.x, rect.y, rect.w, rect.h,
            );
            rect.x = rect.x.max(0);
            rect.y = rect.y.max(0);
            rect.w = rect.w.min(size.w - rect.x as u32);
            rect.h = rect.h.min(size.h - rect.y as u32);
        }

        if rect.w == 0 || rect.h == 0 {
            return Ok(());
        }
        // Copy saved region to current pixmap
        let width = self.width() as usize;
        let pixels = self.pixmap.pixels_mut();
        let saved = saved.pixels();
        for y in rect.y as usize..rect.y as usize + rect.h as usize {
            let start = y * width + rect.x as usize;
            let end = start + rect.w as usize;
            pixels[start..end].copy_from_slice(&saved[start..end]);
        }

        Ok(())
    }

    fn pop(&mut self) -> bool {
        crate::display::pop_reusing_snapshot(&mut self.saved, &mut self.reusable_snapshots)
    }
}

#[cfg(all(feature = "perf-bench", not(feature = "miyoo")))]
pub use test_iface::PresenterProbe;
