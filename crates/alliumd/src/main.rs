#![deny(clippy::all)]
#![warn(rust_2018_idioms)]

mod alliumd;
mod osd;
#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod memory_profile_tests;

use anyhow::Result;
use simple_logger::SimpleLogger;

use crate::alliumd::AlliumD;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    common::memory::configure_daemon_allocator();
    common::timezone::load_saved_timezone();
    let _trace_drain = allium_menu::open_trace::ShutdownDrain;
    SimpleLogger::new().env().init().unwrap();

    #[cfg(feature = "console")]
    {
        log::info!("Starting tokio console at :6669");
        console_subscriber::init();
    }

    let mut app = AlliumD::new().await?;
    app.run_event_loop().await?;
    Ok(())
}
