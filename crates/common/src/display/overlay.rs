use std::{mem::size_of, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use tiny_skia::Pixmap;

use crate::display::{Display, RectHold};
use crate::theme::{ScreenId, ThemeHandle, pack::Rect};

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const MAX_SPANS: usize = 8192;
const MAX_PAGES: usize = 3;
const MAX_OWNED_BYTES: usize = 16 * 1024 * 1024;
const MAX_COPY_BYTES_PER_SECOND: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlaySpan {
    pub x: u32,
    pub y: u32,
    pub len: u32,
    pub byte_start: usize,
}

struct OverlayPlane {
    theme: ThemeHandle,
    screen: ScreenId,
    bounds: Rect,
    source: Box<[u8]>,
    spans: Box<[OverlaySpan]>,
    covered_bytes: usize,
}

#[derive(Clone)]
pub struct PreparedOverlay(Arc<OverlayPlane>);

impl PreparedOverlay {
    pub(crate) fn from_frame(theme: ThemeHandle, screen: ScreenId, frame: Pixmap) -> Result<Self> {
        ensure!(screen.is_osd(), "Masked overlay requires an OSD ScreenId");
        ensure!(
            frame.width() as usize == WIDTH && frame.height() as usize == HEIGHT,
            "Unexpected overlay canvas"
        );
        let rectangles: Vec<Rect> = theme.pack().screens[&screen]
            .components
            .iter()
            .map(|c| c.rect)
            .collect();
        Self::from_rects(theme, screen, frame, &rectangles)
    }

    fn from_rects(
        theme: ThemeHandle,
        screen: ScreenId,
        frame: Pixmap,
        rectangles: &[Rect],
    ) -> Result<Self> {
        ensure!(!rectangles.is_empty(), "Empty overlay descriptor");
        let mut left = WIDTH;
        let mut top = HEIGHT;
        let mut right = 0usize;
        let mut bottom = 0usize;
        for rect in rectangles {
            let Rect(x, y, w, h) = *rect;
            rect.validate()?;
            left = left.min(x as usize);
            top = top.min(y as usize);
            right = right.max(
                x.checked_add(w)
                    .ok_or_else(|| anyhow::anyhow!("Overlay x overflow"))? as usize,
            );
            bottom = bottom.max(
                y.checked_add(h)
                    .ok_or_else(|| anyhow::anyhow!("Overlay y overflow"))? as usize,
            );
        }
        ensure!(
            right <= WIDTH && bottom <= HEIGHT && left < right && top < bottom,
            "Invalid overlay union"
        );
        let bounds = Rect(
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        );
        let raw = frame.data();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if (x < left || x >= right || y < top || y >= bottom)
                    && raw[(y * WIDTH + x) * 4 + 3] != 0
                {
                    anyhow::bail!("Prepared pixels outside descriptor union");
                }
            }
        }
        let crop_width = right - left;
        let crop_height = bottom - top;
        let source_len = crop_width
            .checked_mul(crop_height)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("Overlay crop overflow"))?;
        let mut source = Vec::with_capacity(source_len);
        for y in top..bottom {
            let start = (y * WIDTH + left) * 4;
            source.extend_from_slice(&raw[start..start + crop_width * 4]);
        }
        let mut spans = Vec::new();
        let mut covered_bytes = 0usize;
        for row in 0..crop_height {
            let mut x = 0;
            while x < crop_width {
                while x < crop_width && source[(row * crop_width + x) * 4 + 3] == 0 {
                    x += 1;
                }
                if x == crop_width {
                    break;
                }
                let start = x;
                while x < crop_width && source[(row * crop_width + x) * 4 + 3] != 0 {
                    x += 1;
                }
                ensure!(spans.len() < MAX_SPANS, "Overlay span cap exceeded");
                let len = x - start;
                spans.push(OverlaySpan {
                    x: (left + start) as u32,
                    y: (top + row) as u32,
                    len: len as u32,
                    byte_start: (row * crop_width + start) * 4,
                });
                covered_bytes = covered_bytes
                    .checked_add(len * 4)
                    .ok_or_else(|| anyhow::anyhow!("Overlay coverage overflow"))?;
            }
        }
        ensure!(
            source_len + spans.len() * size_of::<OverlaySpan>() + WIDTH * HEIGHT * 4
                <= MAX_OWNED_BYTES,
            "Overlay preparation budget exceeded"
        );
        Ok(Self(Arc::new(OverlayPlane {
            theme,
            screen,
            bounds,
            source: source.into_boxed_slice(),
            spans: spans.into_boxed_slice(),
            covered_bytes,
        })))
    }

    pub fn bounds(&self) -> Rect {
        self.0.bounds
    }
    pub fn spans(&self) -> &[OverlaySpan] {
        &self.0.spans
    }
    pub fn snapshot(&self) -> &ThemeHandle {
        &self.0.theme
    }
    pub fn screen(&self) -> ScreenId {
        self.0.screen
    }
    pub fn accounted_bytes(&self) -> usize {
        self.0.source.len() + self.0.spans.len() * size_of::<OverlaySpan>()
    }
    pub fn covered_bytes(&self) -> usize {
        self.0.covered_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayFormat {
    pub bits_per_pixel: u32,
    pub red: (u32, u32),
    pub green: (u32, u32),
    pub blue: (u32, u32),
    pub alpha: (u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub struct OverlayLayoutSpec {
    pub instance: u64,
    pub device: (u64, u64, u64),
    pub backing_start: usize,
    pub organization: [u32; 4],
    pub channel_msb_right: [u32; 4],
    pub orientation: [u32; 2],
    pub width: u32,
    pub height: u32,
    pub virtual_width: u32,
    pub virtual_height: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub row_stride: u32,
    pub mapped_bytes: usize,
    pub physical_bytes: usize,
    pub format: OverlayFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayLayout {
    instance: u64,
    device: (u64, u64, u64),
    backing_start: usize,
    organization: [u32; 4],
    orientation: [u32; 2],
    width: usize,
    height: usize,
    virtual_width: usize,
    virtual_height: usize,
    xoffset: usize,
    row_stride: usize,
    page_stride: usize,
    pages: usize,
    mapped_bytes: usize,
    physical_bytes: usize,
    format: OverlayFormat,
}

impl OverlayLayout {
    pub fn checked(spec: OverlayLayoutSpec) -> Result<Self> {
        let OverlayLayoutSpec {
            instance,
            device,
            backing_start,
            organization,
            channel_msb_right,
            orientation,
            width,
            height,
            virtual_width,
            virtual_height,
            xoffset,
            yoffset,
            row_stride,
            mapped_bytes,
            physical_bytes,
            format,
        } = spec;
        ensure!(
            width as usize == WIDTH && height as usize == HEIGHT,
            "Unexpected overlay display dimensions"
        );
        ensure!(
            format
                == OverlayFormat {
                    bits_per_pixel: 32,
                    red: (16, 8),
                    green: (8, 8),
                    blue: (0, 8),
                    alpha: (24, 8)
                },
            "Unsupported overlay pixel format"
        );
        ensure!(
            organization == [0, 0, 0, 2] && channel_msb_right == [0; 4],
            "Unsupported overlay framebuffer organization"
        );
        let (w, h, vw, vh, xo, yo, stride) = (
            width as usize,
            height as usize,
            virtual_width as usize,
            virtual_height as usize,
            xoffset as usize,
            yoffset as usize,
            row_stride as usize,
        );
        ensure!(
            vh % h == 0 && (1..=MAX_PAGES).contains(&(vh / h)),
            "Unsupported overlay page count"
        );
        ensure!(
            xo.checked_add(w).is_some_and(|v| v <= vw)
                && vw.checked_mul(4).is_some_and(|v| v <= stride),
            "Invalid overlay row stride or x offset"
        );
        ensure!(yo % h == 0 && yo < vh, "Unsupported overlay page offset");
        let page_stride = stride
            .checked_mul(h)
            .ok_or_else(|| anyhow::anyhow!("Overlay page stride overflow"))?;
        ensure!(
            stride.checked_mul(vh).is_some_and(|n| n <= mapped_bytes)
                && mapped_bytes <= physical_bytes,
            "Overlay mapping exceeds framebuffer capacity"
        );
        Ok(Self {
            instance,
            device,
            backing_start,
            organization,
            orientation,
            width: w,
            height: h,
            virtual_width: vw,
            virtual_height: vh,
            xoffset: xo,
            row_stride: stride,
            page_stride,
            pages: vh / h,
            mapped_bytes,
            physical_bytes,
            format,
        })
    }

    pub fn checked_binding(open: OverlayLayoutSpec, observed: OverlayLayoutSpec) -> Result<Self> {
        let bound = Self::checked(open)?;
        let current = Self::checked(observed)?;
        ensure!(
            bound == current,
            "Overlay framebuffer mapping changed since open"
        );
        Ok(bound)
    }

    fn addresses(&self, overlay: &PreparedOverlay) -> Result<Vec<RunAddress>> {
        let mut runs = Vec::with_capacity(overlay.spans().len());
        let mut packed_start = 0usize;
        for span in overlay.spans() {
            let (x, y, len) = (span.x as usize, span.y as usize, span.len as usize);
            ensure!(
                len > 0 && x.checked_add(len).is_some_and(|v| v <= self.width) && y < self.height,
                "Overlay span outside display"
            );
            let row = (self.height - 1 - y)
                .checked_mul(self.row_stride)
                .ok_or_else(|| anyhow::anyhow!("Overlay row overflow"))?;
            let column = self
                .xoffset
                .checked_add(self.width - x - len)
                .and_then(|v| v.checked_mul(4))
                .ok_or_else(|| anyhow::anyhow!("Overlay column overflow"))?;
            let offset = row
                .checked_add(column)
                .ok_or_else(|| anyhow::anyhow!("Overlay offset overflow"))?;
            let byte_len = len
                .checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("Overlay span byte overflow"))?;
            ensure!(
                offset
                    .checked_add(byte_len)
                    .is_some_and(|v| v <= self.page_stride),
                "Overlay span exceeds page"
            );
            for page in 0..self.pages {
                ensure!(
                    page.checked_mul(self.page_stride)
                        .and_then(|base| base.checked_add(offset))
                        .and_then(|start| start.checked_add(byte_len))
                        .is_some_and(|end| end <= self.mapped_bytes),
                    "Overlay span exceeds mapping"
                );
            }
            runs.push(RunAddress {
                within_page: offset,
                packed_start,
                byte_len,
                source_start: span.byte_start,
            });
            packed_start = packed_start
                .checked_add(byte_len)
                .ok_or_else(|| anyhow::anyhow!("Overlay packed offset overflow"))?;
        }
        ensure!(
            packed_start == overlay.covered_bytes(),
            "Overlay coverage length mismatch"
        );
        Ok(runs)
    }

    fn check_budget(&self, overlay: &PreparedOverlay, run_count: usize) -> Result<()> {
        let per_page = overlay.covered_bytes();
        let projected = overlay
            .accounted_bytes()
            .checked_add(WIDTH * HEIGHT * 4)
            .and_then(|v| v.checked_add(self.pages * per_page * 2))
            .and_then(|v| v.checked_add(run_count * size_of::<RunAddress>()))
            .ok_or_else(|| anyhow::anyhow!("Overlay allocation overflow"))?;
        ensure!(
            projected <= MAX_OWNED_BYTES,
            "Overlay capture budget exceeded"
        );
        Ok(())
    }

    pub fn check_overlay(&self, overlay: &PreparedOverlay) -> Result<()> {
        let runs = self.addresses(overlay)?;
        self.check_budget(overlay, runs.len())
    }

    pub fn pages(&self) -> usize {
        self.pages
    }
}

struct RunAddress {
    within_page: usize,
    packed_start: usize,
    byte_len: usize,
    source_start: usize,
}
struct PagePixels {
    original: Box<[u8]>,
    composited: Box<[u8]>,
}

pub enum OverlayContent {
    Composited,
    Original,
}

pub struct CapturedOverlay {
    overlay: PreparedOverlay,
    layout: OverlayLayout,
    runs: Box<[RunAddress]>,
    pages: Box<[PagePixels]>,
}

impl CapturedOverlay {
    pub fn capture(overlay: &PreparedOverlay, layout: OverlayLayout, frame: &[u8]) -> Result<Self> {
        ensure!(
            frame.len() == layout.mapped_bytes,
            "Overlay mapping capacity changed"
        );
        let runs = layout.addresses(overlay)?;
        layout.check_budget(overlay, runs.len())?;
        let per_page = overlay.covered_bytes();
        let mut pages = Vec::with_capacity(layout.pages);
        for page in 0..layout.pages {
            let mut original = Vec::with_capacity(per_page);
            let mut composited = Vec::with_capacity(per_page);
            let base = page * layout.page_stride;
            for run in &runs {
                let raw = &frame[base + run.within_page..base + run.within_page + run.byte_len];
                original.extend_from_slice(raw);
                for (i, pixel) in raw.chunks_exact(4).enumerate() {
                    let source = run.source_start + run.byte_len - (i + 1) * 4;
                    let src = &overlay.0.source[source..source + 4];
                    let inverse = 255u16 - src[3] as u16;
                    let mix = |channel: usize| -> u8 {
                        (src[channel] as u16 + ((pixel[2 - channel] as u16 * inverse + 127) / 255))
                            .min(255) as u8
                    };
                    composited.extend_from_slice(&[
                        mix(2),
                        mix(1),
                        mix(0),
                        (src[3] as u16 + (pixel[3] as u16 * inverse + 127) / 255).min(255) as u8,
                    ]);
                }
            }
            pages.push(PagePixels {
                original: original.into_boxed_slice(),
                composited: composited.into_boxed_slice(),
            });
        }
        Ok(Self {
            overlay: overlay.clone(),
            layout,
            runs: runs.into_boxed_slice(),
            pages: pages.into_boxed_slice(),
        })
    }

    pub fn present(
        &self,
        layout: &OverlayLayout,
        frame: &mut [u8],
        content: OverlayContent,
    ) -> Result<()> {
        ensure!(
            layout == &self.layout && frame.len() == self.layout.mapped_bytes,
            "Overlay backend layout changed"
        );
        for (page_index, page) in self.pages.iter().enumerate() {
            let base = page_index * self.layout.page_stride;
            let source = match content {
                OverlayContent::Composited => &page.composited,
                OverlayContent::Original => &page.original,
            };
            for run in self.runs.iter() {
                let start = base + run.within_page;
                frame[start..start + run.byte_len]
                    .copy_from_slice(&source[run.packed_start..run.packed_start + run.byte_len]);
            }
        }
        Ok(())
    }

    pub fn check_layout(&self, layout: &OverlayLayout) -> Result<()> {
        ensure!(layout == &self.layout, "Overlay backend layout changed");
        Ok(())
    }
    pub fn has_coverage(&self) -> bool {
        !self.runs.is_empty()
    }
    pub fn copy_bytes_per_pass(&self) -> usize {
        self.overlay.covered_bytes() * self.layout.pages
    }
    pub fn stamp_pause(&self) -> Duration {
        let bytes = self.copy_bytes_per_pass() as u64;
        let micros = bytes
            .saturating_mul(1_000_000)
            .div_ceil(MAX_COPY_BYTES_PER_SECOND)
            .max(100);
        Duration::from_micros(micros)
    }
    pub fn accounted_bytes(&self) -> usize {
        self.overlay.accounted_bytes()
            + self
                .pages
                .iter()
                .map(|p| p.original.len() + p.composited.len())
                .sum::<usize>()
            + self.runs.len() * size_of::<RunAddress>()
    }
    pub fn snapshot(&self) -> &ThemeHandle {
        self.overlay.snapshot()
    }
    pub fn screen(&self) -> ScreenId {
        self.overlay.screen()
    }
}

pub struct OverlaySurface {
    captured: Arc<CapturedOverlay>,
    hold: Option<RectHold>,
    faulted: bool,
}

impl OverlaySurface {
    pub fn captured(&self) -> &CapturedOverlay {
        &self.captured
    }
    pub fn faulted(&self) -> bool {
        self.faulted
    }

    fn install(
        display: &mut impl Display,
        prepared: &PreparedOverlay,
        slot: &mut Option<Self>,
    ) -> Result<()> {
        let captured = Arc::new(display.capture_overlay(prepared)?);
        if !captured.has_coverage() {
            *slot = Some(Self {
                captured,
                hold: None,
                faulted: false,
            });
            return Ok(());
        }
        if let Err(error) = display.present_overlay(&captured, OverlayContent::Composited) {
            if let Err(restore) = display.present_overlay(&captured, OverlayContent::Original) {
                *slot = Some(Self {
                    captured,
                    hold: None,
                    faulted: true,
                });
                anyhow::bail!("Overlay present failed: {error}; masked restore failed: {restore}");
            }
            return Err(error);
        }
        match display.hold_overlay(Arc::clone(&captured)) {
            Ok(Some(hold)) => {
                *slot = Some(Self {
                    captured,
                    hold: Some(hold),
                    faulted: false,
                });
                Ok(())
            }
            Ok(None) => {
                if let Err(restore) = display.present_overlay(&captured, OverlayContent::Original) {
                    *slot = Some(Self {
                        captured,
                        hold: None,
                        faulted: true,
                    });
                    anyhow::bail!(
                        "Covered overlay had no continuous hold; masked restore failed: {restore}"
                    );
                }
                anyhow::bail!("Covered overlay requires a continuous hold")
            }
            Err(error) => {
                if let Err(restore) = display.present_overlay(&captured, OverlayContent::Original) {
                    *slot = Some(Self {
                        captured,
                        hold: None,
                        faulted: true,
                    });
                    anyhow::bail!("Overlay hold failed: {error}; masked restore failed: {restore}");
                }
                Err(error)
            }
        }
    }

    fn stop_and_restore(&mut self, display: &mut impl Display) -> Result<()> {
        ensure!(!self.faulted, "Overlay faulted; explicit recovery required");
        if let Some(hold) = self.hold.take() {
            if let Err(error) = hold.stop_and_join() {
                self.faulted = true;
                return Err(error);
            }
        }
        if let Err(error) = display.present_overlay(&self.captured, OverlayContent::Original) {
            self.faulted = true;
            return Err(error);
        }
        Ok(())
    }

    pub fn replace(
        display: &mut impl Display,
        current: &mut Option<Self>,
        candidate: Option<&PreparedOverlay>,
    ) -> Result<()> {
        ensure!(
            !current.as_ref().is_some_and(|surface| surface.faulted),
            "Overlay faulted; explicit recovery required"
        );
        if let Some(prepared) = candidate {
            display.check_overlay(prepared)?;
        }
        if let Some(old) = current.as_mut() {
            old.stop_and_restore(display)?;
            current.take();
        }
        if let Some(prepared) = candidate {
            Self::install(display, prepared, current)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{
        FontBank, frame::FrameInputs, render::PreparedFrame, runtime::ThemeSnapshot,
    };
    use std::path::Path;

    fn handle() -> ThemeHandle {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../static/.allium/fonts")
            .canonicalize()
            .unwrap();
        let bank = FontBank::bind(&root).unwrap();
        ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap()
    }

    fn spec(pages: u32, xoffset: u32, padding: u32) -> OverlayLayoutSpec {
        let virtual_width = 640 + xoffset;
        let row_stride = virtual_width * 4 + padding;
        OverlayLayoutSpec {
            instance: 7,
            device: (1, 2, 3),
            backing_start: 0x1000,
            organization: [0, 0, 0, 2],
            channel_msb_right: [0; 4],
            orientation: [0, 0],
            width: 640,
            height: 480,
            virtual_width,
            virtual_height: 480 * pages,
            xoffset,
            yoffset: 480 * pages.saturating_sub(1),
            row_stride,
            mapped_bytes: (row_stride * 480 * pages) as usize,
            physical_bytes: (row_stride * 480 * pages) as usize,
            format: OverlayFormat {
                bits_per_pixel: 32,
                red: (16, 8),
                green: (8, 8),
                blue: (0, 8),
                alpha: (24, 8),
            },
        }
    }

    fn pixel_at(layout: &OverlayLayout, page: usize, x: usize, y: usize) -> usize {
        page * layout.page_stride + (479 - y) * layout.row_stride + (layout.xoffset + 639 - x) * 4
    }

    fn painted() -> PreparedOverlay {
        let mut image = Pixmap::new(640, 480).unwrap();
        for (x, y, rgba) in [
            (1, 1, [128, 0, 0, 128]),
            (9, 9, [0, 255, 0, 255]),
            (30, 3, [0, 0, 255, 255]),
            (20, 20, [64, 64, 0, 128]),
        ] {
            let at = (y * 640 + x) * 4;
            image.data_mut()[at..at + 4].copy_from_slice(&rgba);
        }
        PreparedOverlay::from_rects(
            handle(),
            ScreenId::OsdVolume,
            image,
            &[Rect(0, 0, 10, 10), Rect(30, 0, 10, 10), Rect(20, 20, 5, 5)],
        )
        .unwrap()
    }

    #[test]
    fn overlay_mask_restamps_without_alpha_accumulation_and_restores_each_page() {
        let overlay = painted();
        assert_eq!(overlay.bounds(), Rect(0, 0, 40, 25));
        assert_eq!(overlay.spans().len(), 4);
        let layout = OverlayLayout::checked(spec(3, 2, 8)).unwrap();
        let mut flipped = spec(3, 2, 8);
        flipped.yoffset = 0;
        assert_eq!(OverlayLayout::checked(flipped).unwrap(), layout);
        let mut frame = vec![0u8; layout.mapped_bytes];
        for page in 0..3 {
            let base = page * layout.page_stride;
            frame[base..base + layout.page_stride].fill((page as u8 + 1) * 10);
        }
        let original = frame.clone();
        let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
        assert_eq!(captured.copy_bytes_per_pass(), 4 * 4 * 3);
        let hole = pixel_at(&layout, 2, 15, 15);
        let outside = pixel_at(&layout, 0, 500, 100);
        frame[hole..hole + 4].copy_from_slice(&[91, 92, 93, 94]);
        frame[outside..outside + 4].copy_from_slice(&[81, 82, 83, 84]);
        captured
            .present(&layout, &mut frame, OverlayContent::Composited)
            .unwrap();
        let first = frame.clone();
        for _ in 0..100 {
            captured
                .present(&layout, &mut frame, OverlayContent::Composited)
                .unwrap();
        }
        assert_eq!(
            frame, first,
            "fixed replacement bytes do not accumulate alpha"
        );
        assert_eq!(&frame[hole..hole + 4], &[91, 92, 93, 94]);
        assert_eq!(&frame[outside..outside + 4], &[81, 82, 83, 84]);
        for page in 0..3 {
            let at = pixel_at(&layout, page, 1, 1);
            let bg = (page as u16 + 1) * 10;
            let expected_blue = ((bg * 127 + 127) / 255) as u8;
            let expected_red = (128 + (bg * 127 + 127) / 255) as u8;
            assert_eq!(
                &frame[at..at + 4],
                &[
                    expected_blue,
                    expected_blue,
                    expected_red,
                    (128 + (bg * 127 + 127) / 255) as u8
                ]
            );
        }
        captured
            .present(&layout, &mut frame, OverlayContent::Original)
            .unwrap();
        for page in 0..3 {
            for &(x, y) in &[(1, 1), (9, 9), (30, 3), (20, 20)] {
                let at = pixel_at(&layout, page, x, y);
                assert_eq!(&frame[at..at + 4], &original[at..at + 4]);
            }
        }
        assert_eq!(&frame[hole..hole + 4], &[91, 92, 93, 94]);
        assert_eq!(&frame[outside..outside + 4], &[81, 82, 83, 84]);
        assert!(captured.accounted_bytes() < MAX_OWNED_BYTES);
    }

    #[test]
    fn overlay_bounds_format_and_capacity_reject_before_mutation() {
        let overlay = painted();
        for pages in 1..=3 {
            let layout = OverlayLayout::checked(spec(pages, 2, 8)).unwrap();
            let mut frame = vec![57u8; layout.mapped_bytes];
            let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
            let before = frame.clone();
            let mut changed = layout.clone();
            changed.instance += 1;
            assert!(
                captured
                    .present(&changed, &mut frame, OverlayContent::Composited)
                    .is_err()
            );
            assert_eq!(frame, before);
            captured
                .present(&layout, &mut frame, OverlayContent::Composited)
                .unwrap();
        }
        for bad in [
            OverlayLayoutSpec {
                format: OverlayFormat {
                    bits_per_pixel: 16,
                    ..spec(1, 0, 0).format
                },
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                virtual_height: 1920,
                mapped_bytes: (640 * 4 * 1920) as usize,
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                row_stride: 3,
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                mapped_bytes: 100,
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                physical_bytes: 100,
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                yoffset: 1,
                ..spec(1, 0, 0)
            },
            OverlayLayoutSpec {
                xoffset: 3,
                ..spec(1, 2, 8)
            },
        ] {
            assert!(OverlayLayout::checked(bad).is_err());
        }
        let mut outside = Pixmap::new(640, 480).unwrap();
        outside.data_mut()[(479 * 640 + 639) * 4 + 3] = 255;
        assert!(
            PreparedOverlay::from_rects(
                handle(),
                ScreenId::OsdVolume,
                outside,
                &[Rect(0, 0, 2, 2)]
            )
            .is_err()
        );
        assert!(OverlayLayout::checked(spec(0, 0, 0)).is_err());
    }

    #[test]
    fn mmap_open_binding_rejects_stale_layout_and_reopened_backing() {
        let prepared = painted();
        let open = spec(2, 1, 8);
        let mut flipped = open;
        flipped.yoffset = 0;
        let layout = OverlayLayout::checked_binding(open, flipped).unwrap();
        let mut frame = vec![77u8; layout.mapped_bytes];
        let original = frame.clone();

        for stale in [
            OverlayLayoutSpec {
                backing_start: open.backing_start + 0x1000,
                ..flipped
            },
            OverlayLayoutSpec {
                row_stride: open.row_stride + 8,
                ..flipped
            },
            OverlayLayoutSpec {
                virtual_width: open.virtual_width + 1,
                ..flipped
            },
            OverlayLayoutSpec {
                physical_bytes: open.physical_bytes + 4096,
                ..flipped
            },
            OverlayLayoutSpec {
                orientation: [1, 0],
                ..flipped
            },
        ] {
            assert!(OverlayLayout::checked_binding(open, stale).is_err());
            assert_eq!(frame, original);
        }
        let captured = CapturedOverlay::capture(&prepared, layout, &frame).unwrap();
        let reopened = OverlayLayoutSpec {
            backing_start: open.backing_start + 0x1000,
            ..open
        };
        let worker_binding = OverlayLayout::checked_binding(reopened, reopened).unwrap();
        let mut writer_started = false;
        if captured.check_layout(&worker_binding).is_ok() {
            writer_started = true;
            captured
                .present(&worker_binding, &mut frame, OverlayContent::Composited)
                .unwrap();
        }
        assert!(!writer_started);
        assert_eq!(frame, original);
    }

    #[test]
    fn full_background_transparency_and_span_cap_are_bounded() {
        let mut full = Pixmap::new(640, 480).unwrap();
        for pixel in full.data_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&[10, 20, 30, 255]);
        }
        let overlay = PreparedOverlay::from_rects(
            handle(),
            ScreenId::OsdBrightness,
            full,
            &[Rect(0, 0, 640, 480)],
        )
        .unwrap();
        assert_eq!(overlay.spans().len(), 480);
        let layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
        let frame = vec![71u8; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&overlay, layout, &frame).unwrap();
        assert!(captured.accounted_bytes() < MAX_OWNED_BYTES);
        assert!(captured.stamp_pause() >= Duration::from_micros(100_000));
        let transparent = PreparedOverlay::from_rects(
            handle(),
            ScreenId::OsdVolume,
            Pixmap::new(640, 480).unwrap(),
            &[Rect(0, 0, 640, 480)],
        )
        .unwrap();
        assert!(transparent.spans().is_empty());
        let mut fragments = Pixmap::new(640, 480).unwrap();
        for y in 0..30 {
            for x in (0..640).step_by(2) {
                fragments.data_mut()[(y * 640 + x) * 4 + 3] = 255;
            }
        }
        assert!(
            PreparedOverlay::from_rects(
                handle(),
                ScreenId::OsdVolume,
                fragments,
                &[Rect(0, 0, 640, 480)]
            )
            .is_err()
        );
    }

    struct FakeDisplay {
        pixmap: Pixmap,
        frame: Vec<u8>,
        layout: OverlayLayout,
        events: Arc<std::sync::Mutex<Vec<&'static str>>>,
        gate: Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
        fail_restore: bool,
        fail_new_restore: bool,
        fail_check: bool,
        fail_hold: bool,
        panic_old: bool,
    }

    impl FakeDisplay {
        fn new() -> Self {
            let layout = OverlayLayout::checked(spec(2, 0, 0)).unwrap();
            Self {
                pixmap: Pixmap::new(640, 480).unwrap(),
                frame: vec![33; layout.mapped_bytes],
                layout,
                events: Arc::new(std::sync::Mutex::new(Vec::new())),
                gate: Arc::new((
                    std::sync::Mutex::new((false, false)),
                    std::sync::Condvar::new(),
                )),
                fail_restore: false,
                fail_new_restore: false,
                fail_check: false,
                fail_hold: false,
                panic_old: false,
            }
        }
        fn record(&self, name: &'static str) {
            self.events.lock().unwrap().push(name);
            self.gate.1.notify_all();
        }
        fn release(&self) {
            let (lock, cv) = &*self.gate;
            lock.lock().unwrap().1 = true;
            cv.notify_all();
        }
        fn wait_old_started(&self) {
            let (lock, cv) = &*self.gate;
            let mut state = lock.lock().unwrap();
            while !state.0 {
                state = cv.wait(state).unwrap();
            }
        }
    }

    impl Display for FakeDisplay {
        fn width(&self) -> u32 {
            640
        }
        fn height(&self) -> u32 {
            480
        }
        fn pixmap(&self) -> tiny_skia::PixmapRef<'_> {
            self.pixmap.as_ref()
        }
        fn pixmap_mut(&mut self) -> tiny_skia::PixmapMut<'_> {
            self.pixmap.as_mut()
        }
        fn map_pixels<F>(&mut self, _f: F) -> Result<()>
        where
            F: FnMut(crate::display::color::Color) -> crate::display::color::Color,
        {
            Ok(())
        }
        fn save(&mut self) -> Result<()> {
            Ok(())
        }
        fn load(&mut self, _area: crate::geom::Rect) -> Result<()> {
            Ok(())
        }
        fn pop(&mut self) -> bool {
            false
        }
        fn check_overlay(&mut self, overlay: &PreparedOverlay) -> Result<()> {
            self.record("check");
            ensure!(!self.fail_check, "injected check failure");
            self.layout.check_overlay(overlay)
        }
        fn capture_overlay(&mut self, overlay: &PreparedOverlay) -> Result<CapturedOverlay> {
            self.record(if overlay.screen() == ScreenId::OsdVolume {
                "capture_old"
            } else {
                "capture_new"
            });
            CapturedOverlay::capture(overlay, self.layout.clone(), &self.frame)
        }
        fn present_overlay(
            &mut self,
            overlay: &CapturedOverlay,
            content: OverlayContent,
        ) -> Result<()> {
            let event = match (overlay.screen(), &content) {
                (ScreenId::OsdVolume, OverlayContent::Original) => "restore_old",
                (ScreenId::OsdBrightness, OverlayContent::Original) => "restore_new",
                (ScreenId::OsdVolume, OverlayContent::Composited) => "present_old",
                _ => "present_new",
            };
            self.record(event);
            ensure!(
                !(self.fail_restore && event == "restore_old"),
                "injected restore failure"
            );
            ensure!(
                !(self.fail_new_restore && event == "restore_new"),
                "injected new restore failure"
            );
            overlay.present(&self.layout, &mut self.frame, content)
        }
        fn hold_overlay(&mut self, overlay: Arc<CapturedOverlay>) -> Result<Option<RectHold>> {
            self.record(if overlay.screen() == ScreenId::OsdVolume {
                "hold_old"
            } else {
                "hold_new"
            });
            ensure!(!self.fail_hold, "injected hold failure");
            if overlay.screen() != ScreenId::OsdVolume {
                return Ok(Some(RectHold::try_spawn(|_| Ok(()))?));
            }
            let events = Arc::clone(&self.events);
            let gate = Arc::clone(&self.gate);
            let panic_old = self.panic_old;
            Ok(Some(RectHold::try_spawn(move |_| {
                let (lock, cv) = &*gate;
                let mut state = lock.lock().unwrap();
                state.0 = true;
                cv.notify_all();
                while !state.1 {
                    state = cv.wait(state).unwrap();
                }
                drop(state);
                if panic_old {
                    panic!("injected old writer panic");
                }
                events.lock().unwrap().push("join_ack");
                Ok(())
            })?))
        }
    }

    fn brightness_candidate() -> PreparedOverlay {
        let mut image = Pixmap::new(640, 480).unwrap();
        image.data_mut()[(12 * 640 + 12) * 4..(12 * 640 + 12) * 4 + 4]
            .copy_from_slice(&[60, 0, 0, 255]);
        PreparedOverlay::from_rects(
            handle(),
            ScreenId::OsdBrightness,
            image,
            &[Rect(10, 10, 10, 10)],
        )
        .unwrap()
    }

    #[test]
    fn overlay_replace_acks_old_writer_before_restore_and_new_capture() {
        let mut display = FakeDisplay::new();
        let mut current = None;
        OverlaySurface::replace(&mut display, &mut current, Some(&painted())).unwrap();
        display.wait_old_started();
        let events = Arc::clone(&display.events);
        let gate = Arc::clone(&display.gate);
        let releaser = std::thread::spawn(move || {
            let (lock, cv) = &*gate;
            let mut state = lock.lock().unwrap();
            while events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "check")
                .count()
                < 2
            {
                state = cv.wait(state).unwrap();
            }
            let before = events.lock().unwrap().clone();
            assert!(!before.contains(&"restore_old") && !before.contains(&"capture_new"));
            state.1 = true;
            cv.notify_all();
        });
        OverlaySurface::replace(&mut display, &mut current, Some(&brightness_candidate())).unwrap();
        releaser.join().unwrap();
        let events = display.events.lock().unwrap();
        let index = |value| events.iter().position(|e| *e == value).unwrap();
        assert!(index("join_ack") < index("restore_old"));
        assert!(index("restore_old") < index("capture_new"));
        assert!(index("capture_new") < index("present_new"));
        assert!(index("present_new") < index("hold_new"));
        drop(events);
        OverlaySurface::replace(&mut display, &mut current, None).unwrap();
        assert!(current.is_none());
    }

    #[test]
    fn overlay_replace_failures_never_start_second_writer() {
        for failure in ["check", "join", "restore"] {
            let mut display = FakeDisplay::new();
            display.panic_old = failure == "join";
            let mut current = None;
            OverlaySurface::replace(&mut display, &mut current, Some(&painted())).unwrap();
            display.wait_old_started();
            display.fail_check = failure == "check";
            display.fail_restore = failure == "restore";
            display.release();
            assert!(
                OverlaySurface::replace(&mut display, &mut current, Some(&brightness_candidate()))
                    .is_err()
            );
            assert!(current.is_some());
            assert_eq!(current.as_ref().unwrap().faulted(), failure != "check");
            let events = display.events.lock().unwrap();
            assert!(!events.contains(&"capture_new") && !events.contains(&"hold_new"));
            if failure == "join" {
                assert!(!events.contains(&"restore_old"));
            }
            drop(events);
            if failure == "check" {
                display.fail_check = false;
                OverlaySurface::replace(&mut display, &mut current, None).unwrap();
                assert!(current.is_none());
            } else {
                assert!(OverlaySurface::replace(&mut display, &mut current, None).is_err());
            }
        }
        let mut display = FakeDisplay::new();
        display.fail_hold = true;
        let mut current = None;
        assert!(
            OverlaySurface::replace(&mut display, &mut current, Some(&brightness_candidate()))
                .is_err()
        );
        assert!(current.is_none());
        let events = display.events.lock().unwrap();
        assert!(events.contains(&"present_new") && events.contains(&"restore_new"));
        drop(events);
        let mut display = FakeDisplay::new();
        display.fail_hold = true;
        display.fail_new_restore = true;
        let mut current = None;
        assert!(
            OverlaySurface::replace(&mut display, &mut current, Some(&brightness_candidate()))
                .is_err()
        );
        assert!(current.as_ref().unwrap().faulted());
        assert!(OverlaySurface::replace(&mut display, &mut current, None).is_err());
    }

    #[test]
    fn prepared_frame_consumes_exact_osd_screen_and_union() {
        let mut input = FrameInputs::new(ScreenId::OsdVolume);
        input
            .text
            .insert(crate::theme::pack::Binding::Title, "Volume".into());
        input.progress = Some(0.4);
        let frame = PreparedFrame::prepare(handle(), &input).unwrap();
        let overlay = frame.into_overlay().unwrap();
        assert_eq!(overlay.screen(), ScreenId::OsdVolume);
        assert_eq!(overlay.bounds(), Rect(128, 360, 384, 88));
        assert!(!overlay.spans().is_empty());
        let not_osd =
            PreparedFrame::prepare(handle(), &FrameInputs::new(ScreenId::Library)).unwrap();
        assert!(not_osd.into_overlay().is_err());
    }
}
