use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

use crate::battery::Battery;
use crate::display::Display;
use crate::geom::{Alignment, Point, Rect};
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::Stylesheet;
use crate::view::{BatteryIndicator, Clock, Command, Row, View, WifiIndicator};

#[derive(Debug)]
pub struct StatusBar<B>
where
    B: Battery + 'static,
{
    row: Row<Box<dyn View>>,
    themed_wifi: Option<WifiIndicator>,
    wifi_capable: bool,
    _phantom: std::marker::PhantomData<B>,
}

impl<B> StatusBar<B>
where
    B: Battery + 'static,
{
    pub fn new(res: Resources, point: Point, battery: B) -> Self {
        Self::new_with_wifi_capability(res, point, battery, DefaultPlatform::has_wifi())
    }

    fn new_with_wifi_capability(
        res: Resources,
        point: Point,
        battery: B,
        wifi_capable: bool,
    ) -> Self {
        Self::build(res, point, Some(battery), wifi_capable, None)
    }

    pub fn new_cached(
        res: Resources,
        point: Point,
        cache: super::menu_status::MenuStatusCache,
    ) -> Self {
        Self::build(res, point, None, DefaultPlatform::has_wifi(), Some(cache))
    }

    fn build(
        res: Resources,
        point: Point,
        battery: Option<B>,
        wifi_capable: bool,
        cache: Option<super::menu_status::MenuStatusCache>,
    ) -> Self {
        let styles = res.get::<Stylesheet>();

        let battery_indicator = if let Some(cache) = &cache {
            BatteryIndicator::<B>::new_cached(
                res.clone(),
                Point::zero(),
                cache.clone(),
                styles.status_bar.show_battery_level,
            )
        } else {
            BatteryIndicator::new(
                res.clone(),
                Point::zero(),
                battery.unwrap(),
                styles.status_bar.show_battery_level,
            )
        };
        let make_wifi = || {
            if let Some(cache) = &cache {
                WifiIndicator::new_cached(Point::zero(), cache.clone())
            } else {
                WifiIndicator::new(res.clone(), Point::zero())
            }
        };

        let mut children: Vec<Box<dyn View>> = vec![Box::new(battery_indicator)];

        if styles.status_bar.show_wifi {
            let wifi_indicator = make_wifi();
            children.push(Box::new(wifi_indicator));
        }

        if styles.status_bar.show_clock {
            let clock = Clock::new(res.clone(), Point::new(0, 0), Alignment::Right);
            children.push(Box::new(clock));
        }

        let row = Row::new(point, children, Alignment::Right, styles.ui.margin_x);
        // Keep the legacy row governed by show_wifi. A themed status slot still
        // needs the cached network state on Wi-Fi capable devices.
        let themed_wifi = (wifi_capable && !styles.status_bar.show_wifi).then(make_wifi);

        drop(styles);

        Self {
            row,
            themed_wifi,
            wifi_capable,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn theme_inputs(&self) -> crate::theme::frame::Status {
        let mut status = crate::theme::frame::Status::default();
        for child in self.row.children() {
            if let Some(value) = child.theme_status() {
                if value.clock.is_some() {
                    status.clock = value.clock;
                }
                if value.network.is_some() {
                    status.network = value.network;
                }
                if value.battery.is_some() {
                    status.battery = value.battery;
                    status.charging = value.charging;
                }
            }
        }
        if !self.wifi_capable {
            status.network = None;
        } else if let Some(wifi) = &self.themed_wifi {
            status.network = wifi.theme_status().and_then(|value| value.network);
        }
        status
    }

    pub fn row(&self) -> &Row<Box<dyn View>> {
        &self.row
    }

    pub fn row_mut(&mut self) -> &mut Row<Box<dyn View>> {
        &mut self.row
    }
}

#[async_trait(?Send)]
impl<B> View for StatusBar<B>
where
    B: Battery,
{
    fn update(&mut self, dt: std::time::Duration) {
        self.row.update(dt);
        if let Some(wifi) = self.themed_wifi.as_mut() {
            wifi.update(dt);
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        if self.should_draw() && styles.status_bar.status_backdrop_color.a() > 0 {
            let mut bbox = self.row.bounding_box(styles);
            if bbox.w > 0 && bbox.h > 0 {
                let padding_x = styles.ui.padding_x;
                let padding_y = styles.ui.padding_y;
                bbox.x -= padding_x;
                bbox.y -= padding_y;
                bbox.w += (padding_x * 2) as u32;
                bbox.h += (padding_y * 2) as u32;

                // Make sure it doesn't go off the top of the screen
                if bbox.y < 0 {
                    bbox.h = (bbox.h as i32 + bbox.y) as u32;
                    bbox.y = 0;
                }

                crate::display::fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    bbox,
                    styles.ui.margin_x as u32,
                    styles.status_bar.status_backdrop_color,
                );
            }
        }

        if self.row.draw(display, styles)? {
            drawn = true;
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.row.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.row.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        self.row.handle_key_event(event, commands, bubble).await
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.row as &dyn View]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.row as &mut dyn View]
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        self.row.bounding_box(styles)
    }

    fn set_position(&mut self, point: Point) {
        self.row.set_position(point);
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod semantic_status_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use crate::constants::{ALLIUM_SD_ROOT, ALLIUM_WIFI_SETTINGS};

    #[derive(Clone)]
    struct ChangingBattery(Arc<Mutex<(i32, bool)>>);
    impl Battery for ChangingBattery {
        fn update(&mut self) -> Result<()> { Ok(()) }
        fn percentage(&self) -> i32 { self.0.lock().unwrap().0 }
        fn charging(&self) -> bool { self.0.lock().unwrap().1 }
    }

    #[test]
    fn real_boxed_status_bar_polls_battery_clock_and_network() {
        assert!(ALLIUM_SD_ROOT.starts_with(std::env::temp_dir().canonicalize().unwrap()), "owned TEMP fixture required");
        let path = ALLIUM_WIFI_SETTINGS.as_path();
        let saved = std::fs::read(path).ok();
        struct Restore(std::path::PathBuf, Option<Vec<u8>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(bytes) = &self.1 { std::fs::write(&self.0, bytes).unwrap(); }
                else { std::fs::remove_file(&self.0).unwrap(); }
            }
        }
        let _restore = Restore(path.to_owned(), saved);
        let write_wifi = |enabled| std::fs::write(path, serde_json::to_vec(&serde_json::json!({"wifi": enabled})).unwrap()).unwrap();
        write_wifi(false);
        let mut styles = Stylesheet::load().unwrap();
        styles.status_bar.show_wifi = true;
        styles.status_bar.show_clock = true;
        let mut map = type_map::TypeMap::new();
        map.insert(styles.clone());
        let res = Resources::new(map);
        let source = Arc::new(Mutex::new((82, false)));
        let battery = ChangingBattery(source.clone());
        let mut bar = StatusBar::new_with_wifi_capability(
            res.clone(), Point::new(620, 8), battery.clone(), true,
        );
        let initial = bar.theme_inputs(); // Actual constructor Row<Box<dyn View>>.
        assert_eq!(initial.battery, Some(82));
        assert!(!initial.charging);
        assert_eq!(initial.network, None);
        assert!(chrono::NaiveTime::parse_from_str(initial.clock.as_ref().unwrap(), "%H:%M").is_ok());
        // Expire real widget polling clocks deterministically, without sleeping or OS changes.
        bar.row = Row::new(Point::new(620, 8), vec![
            Box::new(BatteryIndicator::new(res.clone(), Point::zero(), battery.clone(), true).fixture_expired()) as Box<dyn View>,
            Box::new(WifiIndicator::new(res.clone(), Point::zero()).fixture_expired()),
            Box::new(Clock::new(res, Point::zero(), Alignment::Right).fixture_expired()),
        ], Alignment::Right, styles.ui.margin_x);
        let before = bar.theme_inputs();
        let mut platform = DefaultPlatform::new().unwrap();
        let mut display = platform.display().unwrap();
        bar.draw(&mut display, &styles).unwrap();
        assert!(!bar.should_draw());
        *source.lock().unwrap() = (41, true);
        write_wifi(true); // MockPlatform has no IP, so enabled network reports offline.
        bar.update(Duration::ZERO);
        let after = bar.theme_inputs();
        assert_eq!(after.battery, Some(41));
        assert!(after.charging);
        assert_eq!(after.network.as_deref(), Some("offline"));
        assert_ne!(before.clock, after.clock);
        assert!(chrono::NaiveTime::parse_from_str(after.clock.as_ref().unwrap(), "%H:%M").is_ok());
        assert!(bar.should_draw(), "changed real leaf widgets must invalidate boxed row");
        bar.draw(&mut display, &styles).unwrap();
        assert!(!bar.should_draw());
        bar.update(Duration::ZERO);
        assert_eq!(bar.theme_inputs().battery, Some(41));
        assert!(!bar.should_draw(), "unchanged polling snapshot must settle");

        styles.status_bar.show_wifi = false;
        let mut map = type_map::TypeMap::new();
        map.insert(styles.clone());
        let res = Resources::new(map);
        let hidden = StatusBar::new_with_wifi_capability(
            res.clone(), Point::new(620, 8), battery.clone(), true,
        );
        assert_eq!(hidden.row.children().len(), 2, "legacy row has no Wi-Fi icon or gap");
        assert_eq!(hidden.theme_inputs().network.as_deref(), Some("offline"));

        let unsupported = StatusBar::new_with_wifi_capability(
            res, Point::new(620, 8), battery.clone(), false,
        );
        assert_eq!(unsupported.theme_inputs().network, None);

        styles.status_bar.show_wifi = true;
        let mut map = type_map::TypeMap::new();
        map.insert(styles);
        let visible = StatusBar::new_with_wifi_capability(
            Resources::new(map), Point::new(620, 8), battery, true,
        );
        assert_eq!(visible.row.children().len(), 3);
        assert!(visible.themed_wifi.is_none(), "poll one Wi-Fi indicator");
        assert_eq!(visible.theme_inputs().network.as_deref(), Some("offline"));
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod shutdown_forwarding_tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };
    struct ChildBattery {
        polls: Arc<AtomicUsize>,
        entered: mpsc::SyncSender<u32>,
        reaped: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for ChildBattery {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    impl Battery for ChildBattery {
        fn update(&mut self) -> Result<()> {
            if self.polls.fetch_add(1, Ordering::AcqRel) > 0 {
                let mut child = std::process::Command::new("/bin/sleep").arg("2").spawn()?;
                self.entered.send(child.id())?;
                use wait_timeout::ChildExt;
                if child.wait_timeout(Duration::from_millis(100))?.is_none() {
                    child.kill()?;
                    child.wait()?;
                }
                self.reaped.store(true, Ordering::Release);
            }
            Ok(())
        }
        fn percentage(&self) -> i32 {
            82
        }
        fn charging(&self) -> bool {
            false
        }
    }
    #[test]
    fn retained_outer_row_stops_real_boxed_status_bar_inflight_battery_and_never_repolls() {
        let mut styles = Stylesheet::load().unwrap();
        styles.status_bar.show_wifi = false;
        styles.status_bar.show_clock = false;
        let mut map = type_map::TypeMap::new();
        map.insert(styles);
        let (entered, waiting) = mpsc::sync_channel(1);
        let polls = Arc::new(AtomicUsize::new(0));
        let reaped = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let bar = StatusBar::new(
            Resources::new(map),
            Point::zero(),
            ChildBattery {
                polls: polls.clone(),
                entered,
                reaped: reaped.clone(),
                dropped: dropped.clone(),
            },
        );
        assert_eq!(bar.row().len(), 1);
        let mut outer = Row::new(
            Point::zero(),
            vec![Box::new(bar) as Box<dyn View>],
            Alignment::Right,
            0,
        );
        assert_eq!(polls.load(Ordering::Acquire), 1);
        std::thread::sleep(crate::constants::BATTERY_UPDATE_INTERVAL + Duration::from_millis(10));
        outer.update(Duration::ZERO);
        let pid = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !reaped.load(Ordering::Acquire),
            "poll must still be in flight"
        );
        outer.stop_workers();
        assert!(
            reaped.load(Ordering::Acquire),
            "child reaped before shutdown returned"
        );
        assert!(
            dropped.load(Ordering::Acquire),
            "battery owner dropped while entire view tree is retained"
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        outer.stop_workers();
        let stopped_polls = polls.load(Ordering::Acquire);
        std::thread::sleep(crate::constants::BATTERY_UPDATE_INTERVAL + Duration::from_millis(10));
        for _ in 0..3 {
            outer.update(Duration::ZERO);
            outer.stop_workers();
        }
        assert_eq!(polls.load(Ordering::Acquire), stopped_polls);
        assert_eq!(
            outer.len(),
            1,
            "tree remains alive until all assertions finish"
        );
    }
}
