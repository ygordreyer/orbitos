//! Finite presentation descriptors. Domain controllers retain all action authority.
pub mod fonts;
pub mod frame;
pub mod pack;
pub mod render;
pub mod runtime;
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

pub mod choices;
pub use choices::{ChoiceKind, ThemeChoice, ThemeChoices};

mod image_cache;

mod blit;
mod text_cache;

pub mod ink;
