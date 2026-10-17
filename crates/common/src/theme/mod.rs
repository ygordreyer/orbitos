//! Finite presentation descriptors. Domain controllers retain all action authority.
pub mod fonts;
pub mod frame;
pub mod pack;
pub mod render;
mod render_navigation;
pub mod runtime;
mod runtime_neon;
pub mod selection;
pub mod settings;
pub mod session;
pub use session::{Activation, Boundary, ThemeRuntime, ThemeStatus};
#[cfg(test)]
mod proof_tests;
#[cfg(test)]
mod parity_b_tests;
pub use fonts::{FontBank, ThemeHandle};
pub use pack::{Component, Kind, Pack, Rect, ScreenId};

pub fn logo_suppressed_by_cover(component: &Component, input: &frame::FrameInputs) -> bool {
    component.logo_when_no_cover
        && (input.art.selected_cover.is_some()
            || input.selected.get(&pack::Binding::Entries)
                .is_some_and(|id| input.art.entries.contains_key(id)))
}

pub mod choices;
pub use choices::{ChoiceKind, ThemeChoice, ThemeChoices};

mod image_cache;

mod blit;
mod ink_blit_neon;
mod text_cache;

pub mod ink;
