//! Geometry before image effects, shared by the loader and Ink renderer.
use super::pack::{Arrangement, Binding, Component, Fit, Kind, Pack, ScreenId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
    pub logo_box: bool,
    /// Keep contain covers unpadded so their frame can follow the image bounds.
    pub cover_box: bool,
}
impl Target {
    pub fn logo(self, width: u32, height: u32) -> Self {
        let scale = (self.width as f32 / width as f32).min(self.height as f32 / height as f32);
        Self {
            width: (width as f32 * scale).round().max(1.0) as u32,
            height: (height as f32 * scale).round().max(1.0) as u32,
            fit: Fit::Contain,
            logo_box: false,
            cover_box: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Targets {
    pub hero: Option<Target>,
    pub cover: Option<Target>,
    pub logo: Option<Target>,
}
impl Targets {
    pub fn for_screens(pack: &Pack, screens: &[ScreenId]) -> Self {
        Self::from_components(pack, screens.iter().flat_map(|screen| &pack.screens[screen].components))
    }
    pub fn from_components<'a>(pack: &Pack, components: impl IntoIterator<Item = &'a Component>) -> Self {
        if !pack.ink() { return Self::default(); }
        let cover_fit = if pack.alignment.as_ref().is_some_and(|a| a.cover_shape == "square") {
            Fit::Cover
        } else { Fit::Contain };
        let mut candidates = [Vec::new(), Vec::new(), Vec::new()];
        for c in components {
            if c.layout == Some(Arrangement::Hidden) { continue; }
            let target = |fit| Target { width: c.rect.2, height: c.rect.3, fit, logo_box: false, cover_box: false };
            if c.logo || (c.kind == Kind::Image && c.binding == Some(Binding::SelectedLogo)) {
                let rect = c.logo_rect.unwrap_or(c.rect);
                candidates[2].push(Some(Target { width: rect.2, height: rect.3, fit: Fit::Contain, logo_box: true, cover_box: false }));
            } else if c.kind == Kind::Image {
                match c.binding {
                    Some(Binding::SelectedHero) => candidates[0].push(Some(target(c.fit.unwrap_or(Fit::Contain)))),
                    Some(Binding::SelectedCover | Binding::NextCover) => {
                        let fit = c.fit.unwrap_or(cover_fit);
                        candidates[1].push(Some(Target { cover_box: fit == Fit::Contain, ..target(fit) }));
                    }
                    _ => (),
                }
            }
            if c.kind == Kind::Entries {
                if c.layout == Some(Arrangement::Shelf) {
                    let (width, height) = c.paint.as_ref().and_then(|p| p.art.as_ref())
                        .map(|a| (a.width, a.height))
                        .unwrap_or((c.art_size.unwrap_or(0), c.art_size.unwrap_or(0)));
                    candidates[1].push(Some(Target { width, height, fit: c.fit.unwrap_or(cover_fit), logo_box: false, cover_box: false }));
                } else if c.art_size.unwrap_or(0) > 0 {
                    candidates[1].push(None);
                }
            }
        }
        // A shared FrameInputs source can have several consumers. Preserve the
        // source when their geometries differ, rather than fitting it twice.
        let unique = |targets: &Vec<Option<Target>>| {
            targets.first().copied().flatten().filter(|target| {
                target.width > 0 && target.height > 0 && targets.iter().all(|other| *other == Some(*target))
            })
        };
        Self { hero: unique(&candidates[0]), cover: unique(&candidates[1]), logo: unique(&candidates[2]) }
    }
}

/// Area sampling keeps thin logo strokes when a destination pixel covers many source pixels.
/// Only the displayed raster is allocated; premultiplied channels are averaged together.
pub fn fit_logo(source: &tiny_skia::Pixmap, w: u32, h: u32) -> anyhow::Result<tiny_skia::Pixmap> {
    let (sw, sh) = (source.width(), source.height());
    if w > sw || h > sh || (w == sw && h == sh) {
        return super::runtime::fit_image(source, w, h, Fit::Contain);
    }
    let mut out = tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| anyhow::anyhow!("Invalid logo size"))?;
    crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
    crate::perf_trace::count(crate::perf_trace::Count::ResizePixels, w as u64 * h as u64);
    let divisor = u64::from(sw) * u64::from(sh);
    for y in 0..h {
        let (top, bottom) = (y * sh, (y + 1) * sh);
        for x in 0..w {
            let (left, right) = (x * sw, (x + 1) * sw);
            let mut sums = [0u64; 4];
            for sy in top / h..bottom.div_ceil(h) {
                let wy = bottom.min((sy + 1) * h) - top.max(sy * h);
                for sx in left / w..right.div_ceil(w) {
                    let wx = right.min((sx + 1) * w) - left.max(sx * w);
                    let at = ((sy * sw + sx) * 4) as usize;
                    for (sum, value) in sums.iter_mut().zip(&source.data()[at..at + 4]) {
                        *sum += u64::from(*value) * u64::from(wx) * u64::from(wy);
                    }
                }
            }
            let at = ((y * w + x) * 4) as usize;
            for (pixel, sum) in out.data_mut()[at..at + 4].iter_mut().zip(sums) {
                *pixel = ((sum + divisor / 2) / divisor) as u8;
            }
        }
    }
    Ok(out)
}

/// Only logos narrower than 4:5 (width:height) lose to the title text. Measured on 1,756
/// scraped logos, a slot-width threshold hid most of them in wide slots; this hides 5.
pub fn logo_readable(width: u32, height: u32, slot_width: u32, slot_height: u32) -> bool {
    width > 0 && height > 0 && slot_width > 0 && slot_height > 0
        && u64::from(width) * 5 >= u64::from(height) * 4
}
