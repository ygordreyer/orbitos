use std::collections::VecDeque;
use std::process;
use std::time::{Duration, Instant};

use anyhow::Result;
use common::command::Command;
use common::geom;
use common::locale::{Locale, LocaleSettings};
use common::resources::Resources;
use common::view::View;
use common::view::menu_status::MenuStatusCache;
use log::{trace, warn};

use common::database::Database;
use common::display::Display;
use common::platform::{DefaultPlatform, Platform};
use common::stylesheet::Stylesheet;
use type_map::TypeMap;

use crate::view::App;

#[derive(Debug)]
pub struct ActivityTracker<P: Platform> {
    platform: P,
    display: P::Display,
    res: Resources,
    view: App<P::Battery>,
    status: MenuStatusCache,
}

impl ActivityTracker<DefaultPlatform> {
    pub fn new(mut platform: DefaultPlatform) -> Result<Self> {
        let display = platform.display()?;
        let battery = platform.battery()?;

        let mut res = TypeMap::new();
        res.insert(Database::new()?);
        res.insert(Stylesheet::load()?);
        res.insert(Locale::new(&LocaleSettings::load()?.lang));
        res.insert(Into::<geom::Size>::into(display.size()));
        let res = Resources::new(res);

        let status = MenuStatusCache::new(battery);
        let view = App::new(display.bounding_box(), res.clone(), status.clone())?;

        Ok(ActivityTracker {
            platform,
            display,
            res,
            view,
            status,
        })
    }

    pub async fn run_event_loop(&mut self) -> Result<()> {
        self.display
            .clear(self.res.get::<Stylesheet>().ui.background_color)?;
        self.display.save()?;

        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);

        let refresh_period = common::constants::BATTERY_UPDATE_INTERVAL
            .min(common::constants::WIFI_UPDATE_INTERVAL)
            .min(common::constants::CLOCK_UPDATE_INTERVAL);
        let mut refresh =
            tokio::time::interval_at(tokio::time::Instant::now() + refresh_period, refresh_period);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_update = Instant::now();
        self.status.request_refresh();

        loop {
            let now = Instant::now();
            self.update_and_draw(now.duration_since(last_update))?;
            last_update = now;

            #[cfg(unix)]
            tokio::select! {
                _ = self.status.changed() => {}
                _ = refresh.tick() => self.status.request_refresh(),
                _ = sigterm.recv() => {
                    self.handle_command(Command::Exit).await?;
                }
                event = self.platform.poll() => {
                    let mut bubble = VecDeque::new();
                    self.view.handle_key_event(event, tx.clone(), &mut bubble).await?;
                }
                else => {}
            }

            #[cfg(not(unix))]
            tokio::select! {
                _ = self.status.changed() => {}
                _ = refresh.tick() => self.status.request_refresh(),
                event = self.platform.poll() => {
                    let mut bubble = VecDeque::new();
                    self.view.handle_key_event(event, tx.clone(), &mut bubble).await?;
                }
                else => {}
            }

            while let Ok(cmd) = rx.try_recv() {
                self.handle_command(cmd).await?;
            }
        }
    }

    fn update_and_draw(&mut self, elapsed: Duration) -> Result<()> {
        self.view.update(elapsed);
        if self.view.should_draw()
            && self
                .view
                .draw(&mut self.display, &self.res.get::<Stylesheet>())?
        {
            self.display.flush()?;
        }
        Ok(())
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Exit => {
                process::exit(0);
            }
            Command::Redraw => {
                trace!("redrawing");
                self.display.load(self.display.bounding_box())?;
                self.view.set_should_draw();
            }
            command => {
                warn!("unhandled command: {:?}", command);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod tests {
    use super::*;
    use common::constants::{ALLIUM_SD_ROOT, ALLIUM_WIFI_SETTINGS};

    fn samples(app: &ActivityTracker<DefaultPlatform>) -> (Option<u8>, Option<String>) {
        let mut battery = None;
        let mut network = None;
        for child in app.view.children()[0].children()[0].children() {
            if let Some(status) = child.theme_status() {
                battery = battery.or(status.battery);
                network = network.or(status.network);
            }
        }
        (battery, network)
    }

    #[tokio::test]
    async fn activity_tracker_loop_adopts_and_draws_first_status_samples_without_input() {
        assert!(ALLIUM_SD_ROOT.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        let path = ALLIUM_WIFI_SETTINGS.as_path();
        struct Restore(std::path::PathBuf, Option<Vec<u8>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(bytes) = &self.1 {
                    std::fs::write(&self.0, bytes).unwrap();
                } else {
                    std::fs::remove_file(&self.0).unwrap();
                }
            }
        }
        let _restore = Restore(path.to_owned(), std::fs::read(path).ok());
        std::fs::write(path, br#"{"wifi":true}"#).unwrap();
        let mut app = ActivityTracker::new(DefaultPlatform::new().unwrap()).unwrap();
        let mut styles = app.res.get::<Stylesheet>().clone();
        styles.status_bar.show_wifi = true;
        app.res.insert(styles);
        app.view = App::new(
            app.display.bounding_box(),
            app.res.clone(),
            app.status.clone(),
        )
        .unwrap();
        assert_eq!(samples(&app), (None, None));

        // MockPlatform::poll never returns. The real loop must wake on cache
        // completion, before its ten-second periodic refresh, and draw it.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), app.run_event_loop())
                .await
                .is_err()
        );
        assert!(app.status.snapshot().battery.is_some());
        assert!(app.status.snapshot().wifi.is_some());
        assert_eq!(samples(&app), (Some(50), Some("offline".into())));
        assert!(!app.view.should_draw(), "adopted samples must be drawn");
    }
}
