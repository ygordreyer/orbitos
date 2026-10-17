//! Geometry before image effects, shared by the loader and Ink renderer.
use super::pack::{Arrangement, Binding, Component, Fit, Kind, Pack, ScreenId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
    pub logo_box: bool,
}
impl Target {
    pub fn logo(self, width: u32, height: u32) -> Self {
        let scale = (self.width as f32 / width as f32).min(self.height as f32 / height as f32);
        Self {
            width: (width as f32 * scale).round().max(1.0) as u32,
            height: (height as f32 * scale).round().max(1.0) as u32,
            fit: Fit::Contain,
            logo_box: false,
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
            let target = |fit| Target { width: c.rect.2, height: c.rect.3, fit, logo_box: false };
            if c.logo || (c.kind == Kind::Image && c.binding == Some(Binding::SelectedLogo)) {
                candidates[2].push(Some(Target { logo_box: true, ..target(Fit::Contain) }));
            } else if c.kind == Kind::Image {
                match c.binding {
                    Some(Binding::SelectedHero) => candidates[0].push(Some(target(c.fit.unwrap_or(Fit::Contain)))),
                    Some(Binding::SelectedCover | Binding::NextCover) => candidates[1].push(Some(target(cover_fit))),
                    _ => (),
                }
            }
            if c.kind == Kind::Entries {
                if c.layout == Some(Arrangement::Shelf) {
                    let (width, height) = c.paint.as_ref().and_then(|p| p.art.as_ref())
                        .map(|a| (a.width, a.height))
                        .unwrap_or((c.art_size.unwrap_or(0), c.art_size.unwrap_or(0)));
                    candidates[1].push(Some(Target { width, height, fit: cover_fit, logo_box: false }));
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
