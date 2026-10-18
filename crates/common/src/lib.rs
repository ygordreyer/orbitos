#![deny(clippy::all, unsafe_op_in_unsafe_fn)]
#![warn(rust_2018_idioms)]

pub mod battery;
pub mod command;
pub mod constants;
pub mod database;
pub mod display;
pub mod game_info;
pub mod geom;
pub mod locale;
pub mod media;
pub mod osd;
pub mod platform;
pub mod perf_trace;
pub mod power;
pub mod product;
pub mod resources;
pub mod retroarch;
pub mod stylesheet;
pub mod theme;
pub mod view;
pub mod wifi;
pub mod timezone;

#[cfg(feature = "perf-bench")]
pub mod perf;
