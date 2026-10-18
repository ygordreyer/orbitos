use super::{fonts::WeakThemeHandle, pack::ScreenId, ThemeHandle};
use crate::display::SharedPixmap;
use std::{cell::RefCell, collections::VecDeque, sync::Arc};
use tiny_skia::Pixmap;

// Warm backgrounds are mostly constant rows. Keep exact RGBA bytes without four
// permanently resident framebuffers. No image decoding is needed on a key press.
enum Pixels {
    Runs(Box<[(u32, u32)]>),
    Raw(Arc<Pixmap>),
}
impl Pixels {
    fn new(pixels: Arc<Pixmap>) -> Self {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for pixel in pixels.data().chunks_exact(4) {
            let word = u32::from_ne_bytes(pixel.try_into().unwrap());
            if let Some((last, count)) = runs.last_mut() && *last == word {
                *count += 1;
            } else {
                if (runs.len() + 1) * 8 >= pixels.data().len() {
                    return Self::Raw(pixels);
                }
                runs.push((word, 1));
            }
        }
        Self::Runs(runs.into_boxed_slice())
    }
    fn restore(&self, target: &mut SharedPixmap) {
        match self {
            Self::Raw(pixels) => *target = SharedPixmap(pixels.clone()),
            Self::Runs(runs) => {
                let mut remaining = target.data_mut();
                for &(word, count) in runs.iter() {
                    let (run, rest) = remaining.split_at_mut(count as usize * 4);
                    if let Ok(words) = bytemuck::try_cast_slice_mut::<u8, u32>(run) {
                        words.fill(word);
                    } else {
                        let pixel = word.to_ne_bytes();
                        for out in run.chunks_exact_mut(4) { out.copy_from_slice(&pixel); }
                    }
                    remaining = rest;
                }
                debug_assert!(remaining.is_empty());
            }
        }
    }
    #[cfg(test)]
    fn bytes(&self) -> usize {
        match self { Self::Runs(runs) => runs.len() * 8, Self::Raw(p) => p.data().len() }
    }
}
type Base = (WeakThemeHandle, ScreenId, bool, Pixels);
thread_local! { static BASES: RefCell<VecDeque<Base>> = const { RefCell::new(VecDeque::new()) }; }
pub(super) fn restore(theme: &ThemeHandle, screen: ScreenId, repainting: bool, target: &mut SharedPixmap) -> bool {
    BASES.with(|bases| {
        let bases = bases.borrow();
        if let Some((_, _, _, pixels)) = bases.iter()
            .find(|(stamp, id, game, _)| stamp.matches(theme) && *id == screen && *game == repainting) {
            pixels.restore(target);
            true
        } else { false }
    })
}
pub(super) fn insert(theme: &ThemeHandle, screen: ScreenId, repainting: bool, pixels: Arc<Pixmap>) {
    let pixels = Pixels::new(pixels);
    BASES.with(|bases| {
        let mut bases = bases.borrow_mut();
        bases.retain(|(stamp, _, _, _)| stamp.matches(theme));
        if bases.len() == 4 { bases.pop_front(); }
        bases.push_back((theme.downgrade(), screen, repainting, pixels));
    });
}
pub(super) fn clear() { BASES.with(|bases| *bases.borrow_mut() = VecDeque::new()); }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lossless_runs_and_raw_fallback_are_bounded() {
        for noise in [false, true] {
            let mut source = Pixmap::new(640, 480).unwrap();
            for (i, pixel) in source.data_mut().chunks_exact_mut(4).enumerate() {
                let word = if noise { i as u32 } else { (i / 640) as u32 };
                pixel.copy_from_slice(&word.to_ne_bytes());
            }
            let source = Arc::new(source);
            let packed = Pixels::new(source.clone());
            assert!(packed.bytes() <= source.data().len());
            if !noise { assert_eq!(packed.bytes(), 480 * 8); }
            let mut target: SharedPixmap = Pixmap::new(640, 480).unwrap().into();
            packed.restore(&mut target);
            assert_eq!(target.data(), source.data());
        }
    }
    #[test]
    fn warmed_theme_backgrounds_stay_below_one_frame() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = super::super::FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        for id in ["compact-plus", "orbit-hero", "orbit-row"] {
            clear();
            let theme = ThemeHandle::prepare(super::super::runtime::ThemeSnapshot::load(&repo.join("themes").join(id)).unwrap(), &bank, 1).unwrap();
            for screen in [ScreenId::OsdVolume, ScreenId::OsdBrightness] {
                let mut input = super::super::frame::FrameInputs::new(screen);
                input.text.insert(super::super::pack::Binding::Title, "Level".into());
                input.progress = Some(0.5);
                for repainting in [false, true] {
                    let first = super::super::render::PreparedFrame::prepare_osd_overlay(theme.clone(), &input, repainting).unwrap();
                    super::super::render::release_transient_caches();
                    let second = super::super::render::PreparedFrame::prepare_osd_overlay(theme.clone(), &input, repainting).unwrap();
                    let mut a = Pixmap::new(640, 480).unwrap();
                    let mut b = a.clone();
                    first.compose(&mut a.as_mut());
                    second.compose(&mut b.as_mut());
                    assert_eq!(a.data(), b.data());
                }
            }
            let bytes = BASES.with(|bases| bases.borrow().iter().map(|(_, _, _, p)| p.bytes()).sum::<usize>());
            eprintln!("OSD {id} four backgrounds: {bytes} bytes (previously {})", 4 * 640 * 480 * 4);
            assert!(bytes < 640 * 480 * 4, "{id}: {bytes}");
        }
        clear();
    }
}
