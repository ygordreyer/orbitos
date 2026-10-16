#![deny(clippy::all)]
#![warn(rust_2018_idioms)]

use std::time::Duration;

use anyhow::Result;

use allium_menu::{AlliumMenu, RetroArchInfo};
use common::{
    constants::{ALLIUM_BASE_DIR, ALLIUM_FONTS_DIR, ALLIUM_THEMES_DIR},
    platform::{DefaultPlatform, Platform},
    retroarch::RetroArchCommand,
    stylesheet::Stylesheet,
    theme::{Boundary, ThemeRuntime},
};
use simple_logger::SimpleLogger;

#[tokio::main]
async fn main() -> Result<()> {
    let _trace_drain = allium_menu::open_trace::ShutdownDrain;
    SimpleLogger::new().env().init().unwrap();
    #[cfg(feature = "miyoo")]
    let _foreground = common::platform::cpu_tuning::foreground();

    #[cfg(not(feature = "simulator"))]
    let info = RetroArchCommand::GetInfo.send_recv().await?.map(|ret| {
        let mut rets = ret.split_ascii_whitespace().skip(1);

        let max_disk_slots = rets.next().map_or(0, |s| s.parse().unwrap_or(0));
        let disk_slot = rets.next().map_or(0, |s| s.parse().unwrap_or(0));
        let state_slot = rets.next().map(|s| s.parse().unwrap_or(0));

        RetroArchInfo {
            max_disk_slots,
            disk_slot,
            state_slot,
        }
    });

    #[cfg(feature = "simulator")]
    let info = Some(RetroArchInfo {
        max_disk_slots: 3,
        disk_slot: 0,
        state_slot: Some(0),
    });

    let capture_deadline = if info.is_some() {
        RetroArchCommand::Pause.send().await?;
        Some(std::time::Instant::now() + Duration::from_millis(50))
    } else {
        None
    };

    let platform = DefaultPlatform::new()?;
    let mut app =
        AlliumMenu::new_after_settle(platform, Stylesheet::load()?, capture_deadline).await?;
    let theme = ThemeRuntime::bind(
        &ALLIUM_BASE_DIR.join("state"),
        &ALLIUM_THEMES_DIR,
        &ALLIUM_FONTS_DIR,
    )
    .map(|mut runtime| runtime.activate(Boundary::MenuOpening).snapshot)
    .unwrap_or_else(|error| {
        log::warn!("Theme runtime unavailable, using Legacy menu: {error:#}");
        None
    });
    app.prepare(
        info,
        theme,
        common::game_info::GameInfo::read()?.unwrap_or_default(),
    )
    .await?;
    app.run_event_loop_after_settle(capture_deadline).await?;
    app.save()?;
    Ok(())
}
