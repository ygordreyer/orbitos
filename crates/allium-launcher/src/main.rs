#![deny(clippy::all)]
#![warn(rust_2018_idioms)]

mod allium_launcher;
mod art_cache;
mod consoles;
mod discs;
mod entry;
mod media_refresh;
mod network_cache;
mod network_fs;
mod network_library;
mod network_runtime;
mod network_transfer;
mod ota;
mod theme_adapter;
mod view;

use anyhow::Result;

use allium_launcher::AlliumLauncher;
use common::platform::{DefaultPlatform, Platform};
use simple_logger::SimpleLogger;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    SimpleLogger::new().env().init().unwrap();

    // Reverse drop order quiesces boost work before the final trace drain,
    // including startup errors and unwinding out of the event loop.
    let _trace_flush = common::perf_trace::FlushPendingOnDrop;
    #[cfg(feature = "miyoo")]
    let _boost_guard = common::platform::cpu_tuning::input_boost_guard();

    #[cfg(feature = "miyoo")]
    {
        common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Frontend);
        common::platform::miyoo::try_fix_resolution().await?;
    }

    let platform = DefaultPlatform::new()?;
    let mut app = AlliumLauncher::new(platform)?;
    let result = app.run_event_loop().await;
    #[cfg(feature = "miyoo")]
    common::platform::cpu_tuning::restore_input_boost();
    result
}
