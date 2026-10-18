use std::{
    mem::size_of,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use tiny_skia::Pixmap;
use log::info;

use crate::display::{Display, RectHold};
use crate::theme::{ScreenId, ThemeHandle, pack::Rect};

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const MAX_SPANS: usize = 8192;
const MAX_PAGES: usize = 3;
const MAX_OWNED_BYTES: usize = 16 * 1024 * 1024;
const MAX_COPY_BYTES_PER_SECOND: u64 = 32 * 1024 * 1024;
pub const OVERLAY_POLL_INTERVAL: Duration = Duration::from_millis(1);
// Bound unsampled partial repaint detection to one mask per page every eight ms.
const OVERLAY_VERIFY_INTERVAL: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlaySpan {
    pub x: u32,
    pub y: u32,
    pub len: u32,
    pub byte_start: usize,
}

#[derive(Clone)]
struct OverlayPlane {
    theme: Option<ThemeHandle>,
    screen: ScreenId,
    bounds: Rect,
    source: Box<[u8]>,
    spans: Box<[OverlaySpan]>,
    covered_bytes: usize,
    repainting: bool,
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
        Self::from_pixels(Some(theme), screen, frame, rectangles)
    }

    pub(crate) fn from_plate(theme: ThemeHandle, screen: ScreenId, frame: Pixmap, rect: Rect) -> Result<Self> {
        Ok(Self::from_pixels(Some(theme), screen, frame, &[rect])?.with_repainting(true))
    }

    pub fn from_legacy_frame(screen: ScreenId, frame: Pixmap, rect: Rect) -> Result<Self> {
        ensure!(screen.is_osd(), "Masked overlay requires an OSD ScreenId");
        Self::from_pixels(None, screen, frame, &[rect])
    }

    fn from_pixels(
        theme: Option<ThemeHandle>,
        screen: ScreenId,
        frame: Pixmap,
        rectangles: &[Rect],
    ) -> Result<Self> {
        ensure!(
            frame.width() as usize == WIDTH && frame.height() as usize == HEIGHT,
            "Unexpected overlay canvas"
        );
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
            repainting: false,
        })))
    }

    pub fn with_repainting(mut self, repainting: bool) -> Self {
        Arc::make_mut(&mut self.0).repainting = repainting;
        self
    }

    pub fn bounds(&self) -> Rect {
        self.0.bounds
    }
    /// Blend into a frame owned by the caller, without acquiring a framebuffer writer.
    pub fn compose(&self, pixels: &mut tiny_skia::PixmapMut<'_>) {
        let Rect(x, y, width, height) = self.0.bounds;
        let source = tiny_skia::PixmapRef::from_bytes(&self.0.source, width, height).unwrap();
        pixels.draw_pixmap(x as i32, y as i32, source, &Default::default(),
            tiny_skia::Transform::identity(), None);
    }
    pub fn spans(&self) -> &[OverlaySpan] {
        &self.0.spans
    }
    pub fn snapshot(&self) -> Option<&ThemeHandle> {
        self.0.theme.as_ref()
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
            .and_then(|v| v.checked_add((self.pages * 2 + 1) * per_page))
            .and_then(|v| {
                v.checked_add(run_count * size_of::<RunAddress>() + 6 * size_of::<(usize, usize)>())
            })
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
    stamped: bool,
    verify_at: Instant,
    needs_stamp: bool,
}

struct OverlayState {
    source: Box<[u8]>,
    pages: Box<[PagePixels]>,
    credit: usize,
    refill_at: Instant,
    next_page: usize,
    polls: u64,
    restamps: usize,
    pages_touched: u8,
    hold_until: Instant,
}

pub enum OverlayContent {
    Composited,
    Original,
}

pub struct CapturedOverlay {
    overlay: PreparedOverlay,
    layout: OverlayLayout,
    runs: Box<[RunAddress]>,
    partial: Box<[PartialRun]>,
    #[cfg(test)]
    stamp_reads: std::sync::atomic::AtomicUsize,
    sentinels: Box<[(usize, usize)]>,
    state: Mutex<OverlayState>,
    hold_fault: std::sync::atomic::AtomicBool,
}

fn sentinel_pixels(runs: &[RunAddress], row_stride: usize) -> Vec<(usize, usize)> {
    // Increasing framebuffer addresses are written last, despite the logical 180 degree rotation.
    let mut last_rows: Vec<_> = runs
        .iter()
        .map(|run| run.within_page / row_stride)
        .collect();
    last_rows.sort_unstable();
    last_rows.dedup();
    let mut sentinels = Vec::with_capacity(6);
    for row in last_rows.into_iter().rev().take(2) {
        let run = runs
            .iter()
            .filter(|run| run.within_page / row_stride == row)
            .max_by_key(|run| run.within_page)
            .unwrap();
        for offset in [0, (run.byte_len / 8) * 4, run.byte_len - 4] {
            let sentinel = (run.within_page + offset, run.packed_start + offset);
            if !sentinels.contains(&sentinel) {
                sentinels.push(sentinel);
            }
        }
    }
    sentinels
}

struct PartialRun {
    within_page: usize,
    packed: std::ops::Range<usize>,
}

fn plate_sentinels(runs: &[RunAddress]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    // At most 8 rows by 3 columns, including both ends of the memory write order.
    for sample in 0..runs.len().min(8) {
        let run = &runs[sample * (runs.len() - 1) / (runs.len().min(8) - 1).max(1)];
        for offset in [0, (run.byte_len / 8) * 4, run.byte_len - 4] {
            let pixel = (run.within_page + offset, run.packed_start + offset);
            if !result.contains(&pixel) { result.push(pixel); }
        }
    }
    result
}

fn packed_source(overlay: &PreparedOverlay, runs: &[RunAddress]) -> Box<[u8]> {
    let mut source = Vec::with_capacity(overlay.covered_bytes());
    for run in runs {
        for pixel in overlay.0.source[run.source_start..run.source_start + run.byte_len]
            .chunks_exact(4).rev() {
            source.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    source.into_boxed_slice()
}

fn blend_translucent(source: &[u8], original: &[u8], target: &mut [u8]) {
    for ((src, background), pixel) in source
        .chunks_exact(4)
        .zip(original.chunks_exact(4))
        .zip(target.chunks_exact_mut(4))
    {
        if src[3] == 255 {
            continue;
        }
        let inverse = 255u16 - src[3] as u16;
        for channel in 0..4 {
            pixel[channel] = (src[channel] as u16
                + (background[channel] as u16 * inverse + 127) / 255)
                .min(255) as u8;
        }
    }
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
        let source = packed_source(overlay, &runs);
        let mut partial = Vec::new();
        if overlay.0.repainting {
            for run in &runs {
                let mut at = run.packed_start;
                while at < run.packed_start + run.byte_len {
                    if source[at + 3] == 255 { at += 4; continue; }
                    let start = at;
                    while at < run.packed_start + run.byte_len && source[at + 3] != 255 { at += 4; }
                    ensure!(partial.len() < MAX_SPANS, "Overlay translucent run cap exceeded");
                    partial.push(PartialRun { within_page: run.within_page + start - run.packed_start, packed: start..at });
                }
            }
        }
        ensure!(overlay.accounted_bytes() + per_page * (layout.pages * 2 + 1)
            + runs.len() * size_of::<RunAddress>() + partial.len() * size_of::<PartialRun>()
            + 24 * size_of::<(usize, usize)>() + WIDTH * HEIGHT * 4 <= MAX_OWNED_BYTES,
            "Overlay capture budget exceeded");
        let sentinels = if overlay.0.repainting { plate_sentinels(&runs) }
            else { sentinel_pixels(&runs, layout.row_stride) };
        let now = Instant::now();
        let mut pages = Vec::with_capacity(layout.pages);
        for page in 0..layout.pages {
            let mut original = Vec::with_capacity(per_page);
            let base = page * layout.page_stride;
            for run in &runs {
                original.extend_from_slice(
                    &frame[base + run.within_page..base + run.within_page + run.byte_len],
                );
            }
            let composited = source.clone();
            pages.push(PagePixels {
                original: original.into_boxed_slice(),
                composited,
                stamped: false,
                verify_at: now + OVERLAY_VERIFY_INTERVAL,
                needs_stamp: false,
            });
        }
        let credit = per_page * layout.pages;
        Ok(Self {
            overlay: overlay.clone(),
            hold_fault: std::sync::atomic::AtomicBool::new(false),
            layout,
            runs: runs.into_boxed_slice(),
            partial: partial.into_boxed_slice(),
            #[cfg(test)]
            stamp_reads: std::sync::atomic::AtomicUsize::new(0),
            sentinels: sentinels.into_boxed_slice(),
            state: Mutex::new(OverlayState {
                source,
                pages: pages.into_boxed_slice(),
                credit,
                refill_at: Instant::now(),
                next_page: 0,
                polls: 0,
                restamps: 0,
                pages_touched: 0,
                hold_until: Instant::now() + Duration::from_secs(1),
            }),
        })
    }

    fn check_frame(&self, layout: &OverlayLayout, frame: &[u8]) -> Result<()> {
        ensure!(
            layout == &self.layout && frame.len() == self.layout.mapped_bytes,
            "Overlay backend layout changed"
        );
        Ok(())
    }

    fn page_dirty(&self, page: &PagePixels, base: usize, frame: &[u8]) -> bool {
        self.sentinels.iter().any(|&(offset, packed)| {
            let pixel = &frame[base + offset..base + offset + 4];
            // The checked slice bounds this unaligned load; mmap pixels can change outside Rust.
            let live = unsafe { std::ptr::read_volatile(pixel.as_ptr().cast::<[u8; 4]>()) };
            live != page.composited[packed..packed + 4]
        })
    }

    fn stamp_page(&self, source: &[u8], page: &mut PagePixels, base: usize, frame: &mut [u8]) {
        if self.overlay.0.repainting {
            // Only anti-aliased/translucent runs read live framebuffer pixels during the hold.
            for part in &self.partial {
                let range = part.packed.clone();
                let at = base + part.within_page;
                #[cfg(test)]
                self.stamp_reads.fetch_add(range.len(), std::sync::atomic::Ordering::Relaxed);
                for ((live, old), last) in frame[at..at + range.len()].chunks_exact(4)
                    .zip(page.original[range.clone()].chunks_exact_mut(4))
                    .zip(page.composited[range].chunks_exact(4)) {
                    if !page.stamped || live != last { old.copy_from_slice(live); }
                }
            }
            if page.needs_stamp { page.composited.copy_from_slice(source); }
            for part in &self.partial {
                let range = part.packed.clone();
                blend_translucent(&source[range.clone()], &page.original[range.clone()], &mut page.composited[range]);
            }
            for run in &self.runs {
                let at = base + run.within_page;
                frame[at..at + run.byte_len].copy_from_slice(&page.composited[run.packed_start..run.packed_start + run.byte_len]);
            }
            page.stamped = true;
            page.needs_stamp = false;
            return;
        }
        for run in self.runs.iter() {
            let raw = &mut frame[base + run.within_page..base + run.within_page + run.byte_len];
            let range = run.packed_start..run.packed_start + run.byte_len;
            let original = &mut page.original[range.clone()];
            let composited = &mut page.composited[range.clone()];
            for ((live, old), last) in raw
                .chunks_exact(4)
                .zip(original.chunks_exact_mut(4))
                .zip(composited.chunks_exact(4))
            {
                // Unchanged owned pixels retain their background, avoiding repeated alpha blending.
                // A fresh pixel equal to the previous composite keeps its older background until the app repaints it.
                if !page.stamped || live != last {
                    old.copy_from_slice(live);
                }
            }
            blend_translucent(&source[range], original, composited);
            raw.copy_from_slice(composited);
        }
        page.stamped = true;
    }

    pub fn poll(&self, layout: &OverlayLayout, frame: &mut [u8]) -> Result<usize> {
        self.poll_at(layout, frame, Instant::now())
    }

    fn mask_dirty(&self, page: &PagePixels, base: usize, frame: &[u8]) -> bool {
        self.runs.iter().any(|run| {
            (0..run.byte_len).step_by(4).any(|offset| {
                let at = base + run.within_page + offset;
                let packed = run.packed_start + offset;
                // As with sentinels, the checked slice bounds a volatile framebuffer read.
                let pixel = &frame[at..at + 4];
                let live = unsafe { std::ptr::read_volatile(pixel.as_ptr().cast::<[u8; 4]>()) };
                live != page.composited[packed..packed + 4]
            })
        })
    }

    fn poll_at(&self, layout: &OverlayLayout, frame: &mut [u8], now: Instant) -> Result<usize> {
        self.check_frame(layout, frame)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Overlay state poisoned"))?;
        let refill = (now.duration_since(state.refill_at).as_nanos()
            * MAX_COPY_BYTES_PER_SECOND as u128
            / 1_000_000_000)
            .min(usize::MAX as u128) as usize;
        if refill > 0 {
            state.credit = state
                .credit
                .saturating_add(refill)
                .min(self.copy_bytes_per_pass());
            state.refill_at = now;
        }
        state.polls += 1;
        let mut stamped = 0;
        let first_page = state.next_page;
        for offset in 0..state.pages.len() {
            let index = (first_page + offset) % state.pages.len();
            let base = index * self.layout.page_stride;
            let verify = now >= state.pages[index].verify_at;
            if verify {
                state.pages[index].verify_at = now + OVERLAY_VERIFY_INTERVAL;
            }
            if (state.pages[index].needs_stamp || self.page_dirty(&state.pages[index], base, frame)
                || (verify && self.mask_dirty(&state.pages[index], base, frame)))
                && state.credit >= state.source.len()
            {
                let OverlayState { source, pages, .. } = &mut *state;
                self.stamp_page(source, &mut pages[index], base, frame);
                state.credit -= state.source.len();
                state.next_page = (index + 1) % state.pages.len();
                stamped += 1;
                state.restamps += 1;
                state.pages_touched |= 1 << index;
            }
        }
        Ok(stamped)
    }

    fn restore_page(&self, page: &mut PagePixels, base: usize, frame: &mut [u8]) {
        if !page.stamped { return; }
        // Concurrent writers provide no ownership fence: even equality can be a fresh game
        // pixel. Never restore a saved game image; its next frame retires the plate.
        if self.overlay.0.repainting { page.stamped = false; return; }
        for run in self.runs.iter() {
            let raw = &mut frame[base + run.within_page..base + run.within_page + run.byte_len];
            let range = run.packed_start..run.packed_start + run.byte_len;
            for ((live, last), original) in raw
                .chunks_exact_mut(4)
                .zip(page.composited[range.clone()].chunks_exact(4))
                .zip(page.original[range].chunks_exact(4))
            {
                if live == last {
                    live.copy_from_slice(original);
                }
            }
        }
        page.stamped = false;
    }

    pub fn present(
        &self,
        layout: &OverlayLayout,
        frame: &mut [u8],
        content: OverlayContent,
    ) -> Result<()> {
        self.check_frame(layout, frame)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Overlay state poisoned"))?;
        let start = Instant::now();
        let mut restored = 0;
        let OverlayState { source, pages, .. } = &mut *state;
        for (index, page) in pages.iter_mut().enumerate() {
            let base = index * self.layout.page_stride;
            match content {
                OverlayContent::Composited => {
                    self.stamp_page(source, page, base, frame);
                    info!("overlay stamp page={index} byte_offset={base} xoffset={} elapsed_ms={} readback_ok={}", self.layout.xoffset, start.elapsed().as_millis(), !self.page_dirty(page, base, frame));
                }
                OverlayContent::Original => {
                    if page.stamped && !self.overlay.0.repainting { restored += 1; }
                    self.restore_page(page, base, frame);
                }
            }
        }
        if matches!(content, OverlayContent::Original) {
            info!("overlay restore pages={restored} repainting={}", self.overlay.0.repainting);
        } else {
            state.pages_touched |= (1 << self.layout.pages) - 1;
            state.hold_until = Instant::now() + Duration::from_secs(1);
        }
        Ok(())
    }

    pub fn hold_expired(&self) -> bool {
        self.state.lock().map_or(true, |state| Instant::now() >= state.hold_until)
    }

    pub fn log_hold_summary(&self, reason: &str) {
        let reason = if self.hold_fault.load(std::sync::atomic::Ordering::Relaxed) { "fault" } else { reason };
        if let Ok(state) = self.state.lock() {
            info!("overlay hold exit polls={} restamps={} pages_touched={:#05b} reason={reason}", state.polls, state.restamps, state.pages_touched);
        } else { info!("overlay hold exit reason=fault state=poisoned"); }
    }

    fn update(&self, candidate: &PreparedOverlay) -> Result<bool> {
        if !self.overlay.0.repainting || !candidate.0.repainting
            || candidate.screen() != self.screen() || candidate.bounds() != self.overlay.bounds()
            || candidate.spans() != self.overlay.spans()
            || !self.snapshot().zip(candidate.snapshot()).is_some_and(|(old, new)| old.downgrade().matches(new)) {
            return Ok(false);
        }
        let source = packed_source(candidate, &self.runs);
        let mut state = self.state.lock().map_err(|_| anyhow::anyhow!("Overlay state poisoned"))?;
        if source.iter().skip(3).step_by(4).ne(state.source.iter().skip(3).step_by(4)) { return Ok(false); }
        state.source = source;
        for page in &mut state.pages { page.needs_stamp = true; }
        Ok(true)
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
    pub fn accounted_bytes(&self) -> usize {
        self.overlay.accounted_bytes()
            + self.overlay.covered_bytes()
            + self.overlay.covered_bytes() * self.layout.pages * 2
            + self.runs.len() * size_of::<RunAddress>()
            + self.sentinels.len() * size_of::<(usize, usize)>()
            + self.partial.len() * size_of::<PartialRun>()
    }
    pub fn snapshot(&self) -> Option<&ThemeHandle> {
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
        let start = Instant::now();
        let captured = Arc::new(display.capture_overlay(prepared).inspect_err(|error| info!("overlay capture error={error:#}"))?);
        ensure!(captured.has_coverage(), "OSD overlay has no non-transparent pixels");
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
        info!("overlay install first_stamp_ms={} pixels={}", start.elapsed().as_millis(), prepared.covered_bytes() / 4);
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

    /// Refresh compatible game plates without stopping the writer or restoring the background.
    pub fn update(&mut self, display: &mut impl Display, candidate: &PreparedOverlay) -> Result<bool> {
        ensure!(!self.faulted, "Overlay faulted; explicit recovery required");
        let result = (|| {
            display.check_overlay(candidate)?;
            if !self.captured.update(candidate)? { return Ok(false); }
            display.present_overlay(&self.captured, OverlayContent::Composited)?;
            Ok(true)
        })();
        if let Err(error) = result {
            info!("overlay update error={error:#}");
            self.captured.hold_fault.store(true, std::sync::atomic::Ordering::Relaxed);
            self.faulted = true;
            if let Some(hold) = self.hold.take() {
                if let Err(join) = hold.stop_and_join() { info!("overlay update stop error={join:#}"); }
            }
            return Err(error);
        }
        result
    }

    pub fn replace(
        display: &mut impl Display,
        current: &mut Option<Self>,
        candidate: Option<&PreparedOverlay>,
    ) -> Result<()> {
        ensure!(
            candidate.is_none() || !current.as_ref().is_some_and(|surface| surface.faulted),
            "Overlay faulted; explicit recovery required"
        );
        if let Some(prepared) = candidate {
            display.check_overlay(prepared)?;
        }
        if let Some(old) = current.as_mut() {
            old.stop_and_restore(display).inspect_err(|error| info!("overlay stop/restore error={error:#}"))?;
            current.take();
        }
        if let Some(prepared) = candidate {
            Self::install(display, prepared, current).inspect_err(|error| info!("overlay install error={error:#}"))?;
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
    fn overlay_poll_uses_last_memory_rows_and_preserves_owned_translucent_pixels() {
        let overlay = painted();
        let layout = OverlayLayout::checked(spec(3, 2, 8)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
        let mut last_rows: Vec<_> = captured.runs.iter()
            .map(|run| run.within_page / layout.row_stride).collect();
        last_rows.sort_unstable();
        last_rows.dedup();
        last_rows.reverse();
        last_rows.truncate(2);
        assert!(captured.sentinels.iter().all(|&(offset, _)|
            last_rows.contains(&(offset / layout.row_stride))));
        captured
            .present(&layout, &mut frame, OverlayContent::Composited)
            .unwrap();
        let unchanged_edge = pixel_at(&layout, 0, 20, 20);
        let original_edge = frame[unchanged_edge..unchanged_edge + 4].to_vec();
        let changed_edge = pixel_at(&layout, 0, 1, 1);
        frame[changed_edge..changed_edge + 4].copy_from_slice(&[50, 60, 70, 255]);
        assert_eq!(captured.poll(&layout, &mut frame).unwrap(), 1);
        assert_eq!(&frame[changed_edge..changed_edge + 4], &[25, 30, 163, 255]);
        assert_eq!(&frame[unchanged_edge..unchanged_edge + 4], original_edge);
        captured
            .present(&layout, &mut frame, OverlayContent::Original)
            .unwrap();
        assert_eq!(&frame[changed_edge..changed_edge + 4], &[50, 60, 70, 255]);
        assert_eq!(&frame[unchanged_edge..unchanged_edge + 4], &[30; 4]);
    }

    fn verification_plate() -> PreparedOverlay {
        let mut image = Pixmap::new(640, 480).unwrap();
        for y in 10..16 {
            for x in 10..18 {
                let rgba = if x == 10 { [128, 0, 0, 128] } else { [20, 60, 90, 255] };
                image.data_mut()[(y * 640 + x) * 4..(y * 640 + x + 1) * 4]
                    .copy_from_slice(&rgba);
            }
        }
        PreparedOverlay::from_legacy_frame(ScreenId::OsdVolume, image, Rect(10, 10, 8, 6))
            .unwrap()
    }

    #[test]
    fn overlay_hide_restores_all_owned_pixels_after_partial_repaint() {
        let overlay = verification_plate();
        let layout = OverlayLayout::checked(spec(3, 2, 8)).unwrap();
        for overwrite_sentinel in [true, false] {
            let mut frame = vec![30; layout.mapped_bytes];
            let mut expected = frame.clone();
            let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
            captured.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
            let offset = if overwrite_sentinel {
                captured.sentinels[0].0
            } else {
                pixel_at(&layout, 0, 11, 15)
            };
            assert_eq!(captured.sentinels.iter().any(|&(at, _)| at == offset), overwrite_sentinel);
            for page in 0..3 {
                let at = page * layout.page_stride + offset;
                let fresh = [71 + page as u8, 81, 91, 255];
                frame[at..at + 4].copy_from_slice(&fresh);
                expected[at..at + 4].copy_from_slice(&fresh);
            }
            captured.present(&layout, &mut frame, OverlayContent::Original).unwrap();
            assert_eq!(frame, expected, "all owned pixels restored, fresh pixels untouched: sentinel={overwrite_sentinel}");
            let state = captured.state.lock().unwrap();
            for (page, pixels) in state.pages.iter().enumerate() {
                for run in captured.runs.iter() {
                    for offset in (0..run.byte_len).step_by(4) {
                        let at = page * layout.page_stride + run.within_page + offset;
                        let packed = run.packed_start + offset;
                        assert_ne!(&frame[at..at + 4], &pixels.composited[packed..packed + 4],
                            "no plate remnants after hide");
                    }
                }
            }
        }
    }

    #[test]
    fn overlay_verification_repairs_repeated_lower_row_repaints_within_eight_ms() {
        let overlay = verification_plate();
        let layout = OverlayLayout::checked(spec(3, 2, 8)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
        captured.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
        let start = captured.state.lock().unwrap().pages[0].verify_at - OVERLAY_VERIFY_INTERVAL;
        let edge = pixel_at(&layout, 0, 10, 15);
        let opaque = pixel_at(&layout, 0, 11, 15);
        assert!(captured.sentinels.iter().all(|&(at, _)| at != edge && at != opaque));
        for millisecond in 1..=64 {
            let background = [40 + millisecond as u8, 51, 61, 255];
            for page in 0..3 {
                for offset in [edge, opaque] {
                    let at = page * layout.page_stride + offset;
                    frame[at..at + 4].copy_from_slice(&background);
                }
            }
            let now = start + Duration::from_millis(millisecond);
            let repaired = captured.poll_at(&layout, &mut frame, now).unwrap();
            if millisecond % 8 == 0 {
                assert_eq!(repaired, 3, "each page repaired within the verification period");
                let blend = |value: u8| ((value as u16 * 127 + 127) / 255) as u8;
                for page in 0..3 {
                    let base = page * layout.page_stride;
                    assert_eq!(&frame[base + edge..base + edge + 4],
                        &[blend(background[0]), blend(51), 128 + blend(61), 255]);
                    assert_eq!(&frame[base + opaque..base + opaque + 4], &[90, 60, 20, 255]);
                }
            } else {
                assert_eq!(repaired, 0, "sentinels unchanged between full-mask checks");
            }
        }
        let stamped = frame.clone();
        for millisecond in 65..=96 {
            assert_eq!(captured.poll_at(&layout, &mut frame,
                start + Duration::from_millis(millisecond)).unwrap(), 0,
                "unchanged pages cause no writes, including full-mask checks");
            assert_eq!(frame, stamped);
        }
        captured.present(&layout, &mut frame, OverlayContent::Original).unwrap();
        for page in 0..3 {
            let base = page * layout.page_stride;
            for offset in [edge, opaque] {
                assert_eq!(&frame[base + offset..base + offset + 4], &[104, 51, 61, 255]);
            }
        }
    }

    #[test]
    fn overlay_poll_keeps_copy_budget_for_repeated_rewrites() {
        let overlay = painted();
        let layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&overlay, layout.clone(), &frame).unwrap();
        captured
            .present(&layout, &mut frame, OverlayContent::Composited)
            .unwrap();
        frame.fill(50);
        assert_eq!(captured.poll(&layout, &mut frame).unwrap(), 3);
        frame.fill(70);
        {
            let mut state = captured.state.lock().unwrap();
            state.credit = 0;
            state.refill_at = Instant::now() + Duration::from_secs(1);
        }
        let before = frame.clone();
        assert_eq!(captured.poll(&layout, &mut frame).unwrap(), 0);
        assert_eq!(frame, before);
        captured.state.lock().unwrap().refill_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(captured.poll(&layout, &mut frame).unwrap(), 3);
        assert!(captured.state.lock().unwrap().credit < captured.overlay.covered_bytes());
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
        assert_eq!(captured.state.lock().unwrap().credit, 640 * 480 * 4 * 3);
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
        frame: memmap2::MmapMut,
        layout: OverlayLayout,
        events: Arc<std::sync::Mutex<Vec<&'static str>>>,
        gate: Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
        fail_restore: bool,
        fail_new_restore: bool,
        fail_check: bool,
        fail_hold: bool,
        fail_present: bool,
        panic_old: bool,
    }

    impl FakeDisplay {
        fn new() -> Self {
            let layout = OverlayLayout::checked(spec(2, 0, 0)).unwrap();
            Self {
                pixmap: Pixmap::new(640, 480).unwrap(),
                frame: {
                    let mut mapping = memmap2::MmapMut::map_anon(layout.mapped_bytes).unwrap();
                    mapping.fill(33);
                    mapping
                },
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
                fail_present: false,
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
            ensure!(!(self.fail_present && event == "present_old"), "injected present failure");
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

    fn plate_candidate(theme: ThemeHandle, color: u8) -> PreparedOverlay {
        let mut image = Pixmap::new(640, 480).unwrap();
        for y in 10..16 {
            for x in 10..18 {
                let at = (y * 640 + x) * 4;
                let rgba = if x == 10 { [color / 2, 0, 0, 128] } else { [color, 60, 90, 255] };
                image.data_mut()[at..at + 4].copy_from_slice(&rgba);
            }
        }
        PreparedOverlay::from_plate(theme, ScreenId::OsdVolume, image, Rect(10, 10, 8, 6)).unwrap()
    }

    #[test]
    fn cached_osd_prefix_matches_cold_pixels_after_title_progress_and_theme_changes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for name in ["compact-plus", "big-picture"] {
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes").join(name)).unwrap(), &bank, 1).unwrap();
            for repainting in [false, true] {
                let mut input = FrameInputs::new(ScreenId::OsdVolume);
                input.text.insert(crate::theme::pack::Binding::Title, "Volume +".into());
                input.progress = Some(0.1);
                PreparedFrame::prepare_osd_overlay(theme.clone(), &input, repainting).unwrap();
                input.text.insert(crate::theme::pack::Binding::Title, "Volume -".into());
                input.progress = Some(0.85);
                let start = Instant::now();
                let warm = PreparedFrame::prepare_osd_overlay(theme.clone(), &input, repainting).unwrap();
                eprintln!("HOST warm OSD theme={name} repainting={repainting} prepare={:?}", start.elapsed());
                let fresh = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes").join(name)).unwrap(), &bank, 2).unwrap();
                let cold = PreparedFrame::prepare_osd_overlay(fresh, &input, repainting).unwrap();
                assert!(warm.0.source == cold.0.source, "theme={name} repainting={repainting}");
                assert_eq!(warm.spans(), cold.spans());
            }
        }
    }

    #[test]
    fn compact_plus_first_show_stamps_real_shared_file_all_pans_and_update_paths() {
        use std::os::unix::fs::FileExt;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, 42).unwrap();
        for repainting in [false, true] {
            let path = std::env::temp_dir().join(format!("allium-osd-fb-{}-{repainting}", std::process::id()));
            let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
            let mut display = FakeDisplay::new();
            display.layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
            file.set_len(display.layout.mapped_bytes as u64).unwrap();
            // The held file fixes the mapping's length and lifetime; no other process truncates it.
            display.frame = unsafe { memmap2::MmapOptions::new().map_mut(&file).unwrap() };
            display.frame.fill(33);
            std::fs::remove_file(&path).unwrap();
            let mut input = FrameInputs::new(ScreenId::OsdVolume);
            input.text.insert(crate::theme::pack::Binding::Title, "Volume +".into());
            input.progress = Some(0.6);
            let mut current = None;
            for step in 0..2 {
                input.progress = Some(0.6 + step as f32 * 0.05);
                let candidate = PreparedFrame::prepare_osd_overlay(theme.clone(), &input, repainting).unwrap();
                assert!(candidate.covered_bytes() > 0);
                if step == 0 {
                    OverlaySurface::replace(&mut display, &mut current, Some(&candidate)).unwrap();
                    display.wait_old_started();
                    display.release();
                } else if !current.as_mut().unwrap().update(&mut display, &candidate).unwrap() {
                    OverlaySurface::replace(&mut display, &mut current, Some(&candidate)).unwrap();
                }
                for pan in [0, 480, 960] {
                    let page = pan / 480;
                    let mut observed = vec![0; display.layout.page_stride];
                    file.read_exact_at(&mut observed, (page * display.layout.page_stride) as u64).unwrap();
                    let at = pixel_at(&display.layout, 0, 360, 50);
                    assert_ne!(&observed[at..at + 4], &[33; 4], "mode={repainting} step={step} pan={pan}");
                    assert_eq!(&observed[at..at + 4], &display.frame[page * display.layout.page_stride + at..][..4]);
                }
            }
            OverlaySurface::replace(&mut display, &mut current, None).unwrap();
        }
    }

    #[test]
    fn game_plate_hide_never_writes_uncertain_background_or_equal_color_collision() {
        let prepared = plate_candidate(handle(), 80);
        let layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&prepared, layout.clone(), &frame).unwrap();
        captured.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
        frame.fill(100);
        captured.poll(&layout, &mut frame).unwrap();
        // A game can write exactly our last composite. Byte equality cannot prove ownership.
        let collision = pixel_at(&layout, 0, 11, 15);
        frame[collision..collision + 4].copy_from_slice(&[90, 60, 80, 255]);
        let before_hide = frame.clone();
        captured.present(&layout, &mut frame, OverlayContent::Original).unwrap();
        assert!(frame == before_hide, "repainting foreground owns retirement of ambiguous pixels");
    }

    #[test]
    fn game_plate_update_fault_stops_writer_and_hide_can_recover() {
        let theme = handle();
        let mut display = FakeDisplay::new();
        let mut current = None;
        OverlaySurface::replace(&mut display, &mut current, Some(&plate_candidate(theme.clone(), 80))).unwrap();
        display.wait_old_started();
        display.release();
        display.fail_present = true;
        assert!(current.as_mut().unwrap().update(&mut display, &plate_candidate(theme, 100)).is_err());
        assert!(current.as_ref().unwrap().hold.is_none(), "fault must join the writer");
        display.fail_present = false;
        OverlaySurface::replace(&mut display, &mut current, None).unwrap();
        assert!(current.is_none());
        assert!(display.events.lock().unwrap().contains(&"join_ack"));
    }

    #[test]
    fn game_plate_restamps_every_page_in_one_poll_and_reads_only_translucent_runs() {
        let prepared = plate_candidate(handle(), 80);
        let layout = OverlayLayout::checked(spec(3, 2, 8)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&prepared, layout.clone(), &frame).unwrap();
        assert!(captured.sentinels.len() <= 24);
        captured.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
        captured.stamp_reads.store(0, std::sync::atomic::Ordering::Relaxed);
        let start = captured.state.lock().unwrap().refill_at;
        for tick in 1..=60 {
            frame.fill(40 + tick as u8);
            assert_eq!(captured.poll_at(&layout, &mut frame, start + Duration::from_millis(tick * 17)).unwrap(), 3);
            for page in 0..3 {
                let opaque = pixel_at(&layout, page, 11, 15);
                assert_eq!(&frame[opaque..opaque + 4], &[90, 60, 80, 255]);
            }
        }
        assert_eq!(captured.stamp_reads.load(std::sync::atomic::Ordering::Relaxed), 6 * 4 * 3 * 60);
        // Game hide leaves retirement to the foreground, including equal-color writes.
        let fresh = pixel_at(&layout, 0, 11, 15);
        frame[fresh..fresh + 4].copy_from_slice(&[1, 2, 3, 255]);
        captured.present(&layout, &mut frame, OverlayContent::Original).unwrap();
        assert_eq!(&frame[fresh..fresh + 4], &[1, 2, 3, 255]);
        assert_eq!(&frame[pixel_at(&layout, 1, 11, 15)..][..4], &[90, 60, 80, 255]);
    }

    #[test]
    fn game_plate_verification_repairs_unsampled_holes_within_eight_ms() {
        let prepared = plate_candidate(handle(), 80);
        let layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
        let mut frame = vec![30; layout.mapped_bytes];
        let captured = CapturedOverlay::capture(&prepared, layout.clone(), &frame).unwrap();
        captured.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
        let layout_ref = &layout;
        let hole = (10..16).flat_map(|y| (10..18).map(move |x| pixel_at(layout_ref, 0, x, y)))
            .find(|at| captured.sentinels.iter().all(|(sentinel, _)| sentinel != at)).unwrap();
        frame[hole..hole + 4].fill(111);
        assert_eq!(captured.poll_at(&layout, &mut frame, Instant::now() + OVERLAY_VERIFY_INTERVAL).unwrap(), 1);
        assert_ne!(&frame[hole..hole + 4], &[111; 4]);
    }

    #[test]
    fn repeated_plate_updates_keep_running_writer_and_original_capture() {
        let theme = handle();
        let mut display = FakeDisplay::new();
        let mut current = None;
        OverlaySurface::replace(&mut display, &mut current, Some(&plate_candidate(theme.clone(), 80))).unwrap();
        display.wait_old_started();
        let capture = Arc::clone(&current.as_ref().unwrap().captured);
        for color in [90, 100, 110] {
            assert!(current.as_mut().unwrap().update(&mut display, &plate_candidate(theme.clone(), color)).unwrap());
            assert!(Arc::ptr_eq(&capture, &current.as_ref().unwrap().captured));
            let at = pixel_at(&display.layout, 0, 11, 15);
            assert_eq!(&display.frame[at..at + 4], &[90, 60, color, 255]);
            let edge = pixel_at(&display.layout, 0, 10, 15);
            assert_eq!(&display.frame[edge..edge + 4], &[16, 16, color / 2 + 16, 144]);
        }
        let events = display.events.lock().unwrap().clone();
        assert_eq!(events.iter().filter(|event| **event == "capture_old").count(), 1);
        assert_eq!(events.iter().filter(|event| **event == "hold_old").count(), 1);
        assert!(!events.contains(&"restore_old"));
        display.release();
        OverlaySurface::replace(&mut display, &mut current, None).unwrap();
        let events = display.events.lock().unwrap();
        assert!(events.iter().position(|e| *e == "join_ack").unwrap() < events.iter().position(|e| *e == "restore_old").unwrap());
        let at = pixel_at(&display.layout, 0, 11, 15);
        assert_eq!(&display.frame[at..at + 4], &[90, 60, 110, 255]);
    }

    #[test]
    fn plate_descriptors_preserve_launcher_pixels_and_bound_game_coverage_with_host_timings() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for pack in ["compact-plus", "big-picture"] {
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes").join(pack)).unwrap(), &bank, 42).unwrap();
            for screen in [ScreenId::OsdVolume, ScreenId::OsdBrightness] {
                let mut input = FrameInputs::new(screen);
                input.text.insert(crate::theme::pack::Binding::Title, "Volume".into());
                input.progress = Some(0.6);
                input.hints.push(crate::theme::frame::Hint { button: "B".into(), label: "Back".into() });
                let start = Instant::now();
                let baseline = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                let raster_time = start.elapsed();
                let start = Instant::now();
                let expected = baseline.into_overlay().unwrap();
                let mask_time = start.elapsed();
                let launcher = PreparedFrame::prepare_osd_overlay(theme.clone(), &input, false).unwrap();
                assert_eq!(launcher.0.source, expected.0.source);
                assert_eq!(launcher.spans(), expected.spans());
                assert_eq!(launcher.bounds(), Rect(0, 0, 640, 480));
                let start = Instant::now();
                let plate = PreparedFrame::prepare_osd_overlay(theme.clone(), &input, true).unwrap();
                let plate_time = start.elapsed();
                let rect = theme.pack().component(screen, "plate").unwrap().rect;
                assert_eq!(plate.bounds(), rect);
                if pack == "compact-plus" { assert_eq!(rect, Rect(352, 24, 272, 112)); }
                assert!(plate.covered_bytes() <= (rect.2 * rect.3 * 4) as usize);
                let layout = OverlayLayout::checked(spec(3, 0, 0)).unwrap();
                let mut frame = vec![30; layout.mapped_bytes];
                let mut timings = Vec::new();
                for overlay in [&expected, &plate] {
                    let start = Instant::now();
                    layout.check_overlay(overlay).unwrap();
                    let check = start.elapsed();
                    let start = Instant::now();
                    let capture = CapturedOverlay::capture(overlay, layout.clone(), &frame).unwrap();
                    let capture_time = start.elapsed();
                    let start = Instant::now();
                    capture.present(&layout, &mut frame, OverlayContent::Composited).unwrap();
                    let present = start.elapsed();
                    let partial = capture.state.lock().unwrap().source.iter().skip(3).step_by(4).filter(|alpha| **alpha != 255).count() * 4;
                    timings.push((check, capture_time, present));
                    eprintln!("HOST {pack} {screen:?} game={} covered={} partial={} grid={} check={check:?} capture={capture_time:?} present={present:?}", overlay.0.repainting, overlay.covered_bytes(), partial, capture.sentinels.len());
                    if overlay.0.repainting {
                        capture.stamp_reads.store(0, std::sync::atomic::Ordering::Relaxed);
                        let begin = Instant::now();
                        let poll_start = capture.state.lock().unwrap().refill_at;
                        for tick in 1..=60u64 {
                            let page = tick as usize % 3;
                            frame[page * layout.page_stride..(page + 1) * layout.page_stride].fill(41 + tick as u8);
                            assert_eq!(capture.poll_at(&layout, &mut frame, poll_start + Duration::from_millis(tick * 17)).unwrap(), 1);
                        }
                        eprintln!("HOST {pack} {screen:?} 60 rotating rewrites hold={:?} stamp_read_bytes={} estimated_reads_per_second<={} writes_per_second={}", begin.elapsed(), capture.stamp_reads.load(std::sync::atomic::Ordering::Relaxed), partial * 60 + capture.sentinels.len() * 4 * 3 * 1000, plate.covered_bytes() * 60);
                        assert_eq!(capture.stamp_reads.load(std::sync::atomic::Ordering::Relaxed), partial * 60);
                    }
                    capture.present(&layout, &mut frame, OverlayContent::Original).unwrap();
                }
                eprintln!("HOST {pack} {screen:?} full_raster={raster_time:?} full_mask={mask_time:?} plate_prepare={plate_time:?} full_show={:?} plate_show={:?}", raster_time + mask_time + timings[0].0 + timings[0].1 + timings[0].2, plate_time + timings[1].0 + timings[1].1 + timings[1].2);
            }
        }
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
                if failure == "restore" {
                    assert!(OverlaySurface::replace(&mut display, &mut current, None).is_err());
                    display.fail_restore = false;
                }
                OverlaySurface::replace(&mut display, &mut current, None).unwrap();
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
