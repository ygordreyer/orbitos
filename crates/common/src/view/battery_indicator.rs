use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use log::error;
use tokio::sync::mpsc::Sender;

use crate::battery::Battery;
use crate::constants::BATTERY_UPDATE_INTERVAL;
use crate::geom::{Point, Rect};
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::Stylesheet;
use crate::view::{BatteryIcon, Command, Label, View};

#[derive(Debug, Clone, PartialEq, Eq)]
struct BatteryState {
    charging: bool,
    percentage: i32,
}

#[derive(Debug)]
enum PollRequest {
    Poll(Option<SyncSender<()>>),
    Stop,
}
#[derive(Debug)]
struct BatteryPolling {
    snapshot: Arc<Mutex<BatteryState>>,
    requests: SyncSender<PollRequest>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    finished: Receiver<()>,
}
impl BatteryPolling {
    fn new<B: Battery + 'static>(battery: B) -> (Self, BatteryState) {
        let fallback = battery_state(&battery);
        let snapshot = Arc::new(Mutex::new(fallback.clone()));
        let published = snapshot.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let (requests, receive) = mpsc::sync_channel::<PollRequest>(1);
        let (initial, first) = mpsc::sync_channel(1);
        let (complete, finished) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("battery-poll".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                run_battery_worker(battery, published, stop, receive, initial);
                let _ = complete.send(());
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                error!("Failed to start battery worker: {}", error);
                None
            }
        };
        let state = first.recv().unwrap_or(fallback);
        (
            Self {
                snapshot,
                requests,
                stopping,
                worker,
                finished,
            },
            state,
        )
    }
    fn request(&self) {
        if !self.stopping.load(Ordering::Acquire) {
            let _ = self.requests.try_send(PollRequest::Poll(None));
        }
    }
    fn snapshot(&self) -> BatteryState {
        self.snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.requests.try_send(PollRequest::Stop);
        if self.worker.is_none() {
            return;
        }
        // Miyoo's in-flight read owns the child's 100 ms timeout and reap.
        if self
            .finished
            .recv_timeout(Duration::from_millis(250))
            .is_ok()
            || self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
        {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        } else {
            error!("Battery worker did not stop within 250 ms");
        }
    }
    #[cfg(test)]
    fn request_and_wait(&self) {
        let (send, receive) = mpsc::sync_channel(1);
        self.requests.send(PollRequest::Poll(Some(send))).unwrap();
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
impl Drop for BatteryPolling {
    fn drop(&mut self) {
        self.stop();
    }
}
fn run_battery_worker(
    mut battery: impl Battery,
    published: Arc<Mutex<BatteryState>>,
    stopping: Arc<AtomicBool>,
    requests: Receiver<PollRequest>,
    initial: SyncSender<BatteryState>,
) {
    if let Err(error) = battery.update() {
        error!("Failed to update battery: {}", error);
    }
    let state = battery_state(&battery);
    *published.lock().unwrap_or_else(|error| error.into_inner()) = state.clone();
    if initial.send(state).is_err() {
        return;
    }
    while let Ok(PollRequest::Poll(acknowledge)) = requests.recv() {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match poll_battery(&mut battery) {
            Ok(state) => {
                *published.lock().unwrap_or_else(|error| error.into_inner()) = state;
            }
            Err(error) => error!("Failed to update battery: {}", error),
        }
        if let Some(acknowledge) = acknowledge {
            let _ = acknowledge.send(());
        }
    }
}
fn battery_state(battery: &impl Battery) -> BatteryState {
    BatteryState {
        charging: battery.charging(),
        percentage: battery.percentage(),
    }
}
fn poll_battery(battery: &mut impl Battery) -> std::result::Result<BatteryState, String> {
    battery.update().map_err(|error| error.to_string())?;
    Ok(battery_state(battery))
}

#[derive(Debug)]
pub struct BatteryIndicator<B>
where
    B: Battery + 'static,
{
    res: Resources,
    point: Point,
    last_state: BatteryState,
    last_updated: Instant,
    label: Option<Label<String>>,
    polling: Option<BatteryPolling>,
    cached: Option<super::menu_status::MenuStatusCache>,
    known: bool,
    _battery: std::marker::PhantomData<fn() -> B>,
    #[cfg(test)]
    wait_for_poll: bool,
    icon: BatteryIcon,
}

impl<B> BatteryIndicator<B>
where
    B: Battery + 'static,
{
    pub fn new(res: Resources, point: Point, battery: B, show_percentage: bool) -> Self {
        let (polling, last_state) = BatteryPolling::new(battery);

        Self::with_state(
            res,
            point,
            show_percentage,
            Some(polling),
            None,
            Some(last_state),
        )
    }

    pub fn new_cached(
        res: Resources,
        point: Point,
        cache: super::menu_status::MenuStatusCache,
        show_percentage: bool,
    ) -> Self {
        let state = cache.snapshot().battery.map(|s| BatteryState {
            charging: s.charging,
            percentage: s.percentage,
        });
        Self::with_state(res, point, show_percentage, None, Some(cache), state)
    }

    fn with_state(
        res: Resources,
        point: Point,
        show_percentage: bool,
        polling: Option<BatteryPolling>,
        cached: Option<super::menu_status::MenuStatusCache>,
        state: Option<BatteryState>,
    ) -> Self {
        let known = state.is_some();
        let last_state = state.unwrap_or(BatteryState {
            charging: false,
            percentage: -1,
        });
        let label = if show_percentage {
            let styles = res.get::<Stylesheet>();
            let mut label = Label::new(
                point,
                format_battery_percentage(last_state.charging, last_state.percentage),
                crate::geom::Alignment::Right,
                None,
            );
            label.font_size(styles.status_bar.font_size);
            label.color(crate::stylesheet::StylesheetColor::StatusBar);
            label.stroke_color(crate::stylesheet::StylesheetColor::StatusBarStroke);
            Some(label)
        } else {
            None
        };

        let icon = BatteryIcon::new(point);

        Self {
            res,
            point,
            last_state,
            last_updated: Instant::now(),
            label,
            polling,
            cached,
            known,
            _battery: std::marker::PhantomData,
            #[cfg(test)]
            wait_for_poll: false,
            icon,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture_expired(mut self) -> Self {
        self.last_updated = Instant::now() - BATTERY_UPDATE_INTERVAL;
        self.wait_for_poll = true;
        self
    }

    fn layout(&mut self) {
        let styles = self.res.get::<Stylesheet>();
        let label_w = if let Some(ref mut label) = self.label {
            label.bounding_box(&styles).w as i32
        } else {
            0
        };
        let label_w = if label_w > 0 {
            label_w + styles.ui.margin_x
        } else {
            0
        };

        self.icon
            .set_state(self.last_state.charging, self.last_state.percentage);
        self.icon
            .set_position(Point::new(self.point.x - label_w, self.point.y));
    }
}

#[async_trait(?Send)]
impl<B> View for BatteryIndicator<B>
where
    B: Battery,
{
    fn stop_workers(&mut self) {
        if let Some(polling) = &mut self.polling {
            polling.stop();
        }
    }

    fn theme_status(&self) -> Option<crate::theme::frame::Status> {
        Some(crate::theme::frame::Status {
            battery: u8::try_from(self.last_state.percentage)
                .ok()
                .filter(|n| *n <= 100),
            charging: self.last_state.charging,
            ..Default::default()
        })
    }
    fn update(&mut self, _dt: Duration) {
        if self.last_updated.elapsed() >= BATTERY_UPDATE_INTERVAL {
            self.last_updated = Instant::now();
            #[cfg(test)]
            if self.wait_for_poll {
                if let Some(polling) = &self.polling {
                    polling.request_and_wait();
                }
            } else {
                if let Some(polling) = &self.polling {
                    polling.request();
                }
            }
            #[cfg(not(test))]
            if let Some(polling) = &self.polling {
                polling.request();
            }
        }
        let state = if let Some(cache) = &self.cached {
            let Some(sample) = cache.snapshot().battery else {
                return;
            };
            BatteryState {
                percentage: sample.percentage,
                charging: sample.charging,
            }
        } else {
            self.polling.as_ref().unwrap().snapshot()
        };
        if state == self.last_state {
            return;
        }
        self.known = true;
        self.last_state = state;
        if let Some(ref mut label) = self.label {
            label.set_text(format_battery_percentage(
                self.last_state.charging,
                self.last_state.percentage,
            ));
        }

        self.layout();
        self.set_should_draw();
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if !self.known {
            return Ok(false);
        }
        let mut drawn = false;

        drawn |= self.icon.should_draw() && self.icon.draw(display, styles)?;
        if let Some(ref mut label) = self.label {
            drawn |= label.should_draw() && label.draw(display, styles)?;
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.known
            && (self.icon.should_draw() || self.label.as_ref().is_some_and(|l| l.should_draw()))
    }

    fn set_should_draw(&mut self) {
        self.icon.set_should_draw();
        if let Some(ref mut label) = self.label {
            label.set_should_draw()
        }
    }

    async fn handle_key_event(
        &mut self,
        _event: KeyEvent,
        _commands: Sender<Command>,
        _bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        Ok(false)
    }

    fn children(&self) -> Vec<&dyn View> {
        if let Some(ref label) = self.label {
            vec![label, &self.icon]
        } else {
            vec![&self.icon]
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        if let Some(ref mut label) = self.label {
            vec![label, &mut self.icon]
        } else {
            vec![&mut self.icon]
        }
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        if !self.known {
            return Rect::zero();
        }
        let label_w = if let Some(ref mut label) = self.label {
            label.bounding_box(styles).w as i32
        } else {
            0
        };
        let label_w = if label_w > 0 {
            label_w + styles.ui.margin_x
        } else {
            0
        };

        let icon_bbox = self.icon.bounding_box(styles);
        // Extend bounding box to include label width
        Rect::new(
            icon_bbox.x,
            icon_bbox.y,
            icon_bbox.w + label_w as u32,
            icon_bbox.h,
        )
    }

    fn set_position(&mut self, point: Point) {
        self.point = point;
        if let Some(ref mut label) = self.label {
            label.set_position(point);
        }
        self.layout();
    }
}

fn format_battery_percentage(charging: bool, percentage: i32) -> String {
    if charging {
        String::new()
    } else {
        format!("{}%", percentage)
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod semantic_battery_tests {
    use super::*;
    use crate::display::Display;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct PollBattery(Arc<Mutex<(i32, bool, bool)>>);
    impl Battery for PollBattery {
        fn update(&mut self) -> Result<()> {
            anyhow::ensure!(!self.0.lock().unwrap().2, "fixture poll failed");
            Ok(())
        }
        fn percentage(&self) -> i32 {
            self.0.lock().unwrap().0
        }
        fn charging(&self) -> bool {
            self.0.lock().unwrap().1
        }
    }
    #[test]
    fn battery_charging_only_and_failed_poll_keep_last_successful_snapshot() {
        let mut map = type_map::TypeMap::new();
        let styles = Stylesheet::load().unwrap();
        map.insert(styles.clone());
        let res = Resources::new(map);
        let state = Arc::new(Mutex::new((82, false, false)));
        let mut indicator =
            BatteryIndicator::new(res, Point::new(100, 10), PollBattery(state.clone()), false);
        let mut platform = DefaultPlatform::new().unwrap();
        let mut display = platform.display().unwrap();
        indicator.draw(&mut display, &styles).unwrap();
        assert!(!indicator.should_draw());
        *state.lock().unwrap() = (82, true, false);
        indicator = indicator.fixture_expired();
        indicator.update(Duration::ZERO);
        assert!(indicator.theme_status().unwrap().charging);
        assert_eq!(indicator.theme_status().unwrap().battery, Some(82));
        assert!(
            indicator.should_draw(),
            "charging-only observation invalidates icon"
        );
        indicator.draw(&mut display, &styles).unwrap();
        *state.lock().unwrap() = (20, false, true);
        indicator = indicator.fixture_expired();
        indicator.update(Duration::ZERO);
        assert_eq!(indicator.theme_status().unwrap().battery, Some(82));
        assert!(indicator.theme_status().unwrap().charging);
        assert!(
            !indicator.should_draw(),
            "failed poll preserves drawn successful snapshot"
        );
        state.lock().unwrap().2 = false;
        indicator = indicator.fixture_expired();
        indicator.update(Duration::ZERO);
        assert_eq!(indicator.theme_status().unwrap().battery, Some(20));
        assert!(!indicator.theme_status().unwrap().charging);
        assert!(indicator.should_draw());
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    struct BlockingBattery {
        polls: usize,
        entered: std::sync::mpsc::SyncSender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    }
    impl Battery for BlockingBattery {
        fn update(&mut self) -> Result<()> {
            self.polls += 1;
            if self.polls > 1 {
                self.entered.send(())?;
                self.resume.recv_timeout(Duration::from_secs(2))?;
            }
            Ok(())
        }
        fn percentage(&self) -> i32 {
            if self.polls == 1 { 82 } else { 41 }
        }
        fn charging(&self) -> bool {
            self.polls > 1
        }
    }
    #[test]
    fn pending_poll_never_holds_the_snapshot_lock_and_publishes_on_completion() {
        let (entered, waiting) = std::sync::mpsc::sync_channel(1);
        let (resume, paused) = std::sync::mpsc::sync_channel(1);
        let (polling, initial) = BatteryPolling::new(BlockingBattery {
            polls: 0,
            entered,
            resume: paused,
        });
        assert_eq!(initial.percentage, 82);
        let (acknowledge, finished) = std::sync::mpsc::sync_channel(1);
        polling
            .requests
            .try_send(PollRequest::Poll(Some(acknowledge)))
            .unwrap();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(polling.snapshot(), initial);
        resume.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            polling.snapshot(),
            BatteryState {
                percentage: 41,
                charging: true
            }
        );
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    struct TimedBattery {
        polls: usize,
        entered: mpsc::SyncSender<()>,
        reaped: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for TimedBattery {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    impl Battery for TimedBattery {
        fn update(&mut self) -> Result<()> {
            self.polls += 1;
            if self.polls > 1 {
                let mut child = std::process::Command::new("/bin/sleep").arg("2").spawn()?;
                self.entered.send(())?;
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
    fn polling() -> (
        BatteryPolling,
        mpsc::Receiver<()>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let (entered, waiting) = mpsc::sync_channel(1);
        let reaped = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let (polling, _) = BatteryPolling::new(TimedBattery {
            polls: 0,
            entered,
            reaped: reaped.clone(),
            dropped: dropped.clone(),
        });
        (polling, waiting, reaped, dropped)
    }
    #[test]
    fn battery_worker_stop_joins_inflight_child_after_timeout_and_reap() {
        let (mut polling, waiting, reaped, dropped) = polling();
        polling.request();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        polling.request();
        let start = Instant::now();
        polling.stop();
        assert!(start.elapsed() < Duration::from_millis(250));
        assert!(polling.worker.is_none(), "worker was joined");
        assert!(reaped.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        polling.stop();
    }
    #[test]
    fn dropping_battery_owner_joins_inflight_child() {
        let (polling, waiting, reaped, dropped) = polling();
        polling.request();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(polling);
        assert!(reaped.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
    }
    struct FailedFirstRead(usize);
    impl Battery for FailedFirstRead {
        fn update(&mut self) -> Result<()> {
            self.0 += 1;
            anyhow::ensure!(self.0 > 1, "first poll failed");
            Ok(())
        }
        fn percentage(&self) -> i32 {
            if self.0 < 2 { 100 } else { 41 }
        }
        fn charging(&self) -> bool {
            self.0 > 1
        }
    }
    #[test]
    fn failed_first_battery_read_keeps_device_state_and_retries() {
        let (polling, initial) = BatteryPolling::new(FailedFirstRead(0));
        assert_eq!(
            initial,
            BatteryState {
                charging: false,
                percentage: 100
            }
        );
        assert_eq!(polling.snapshot(), initial);
        polling.request_and_wait();
        assert_eq!(
            polling.snapshot(),
            BatteryState {
                charging: true,
                percentage: 41
            }
        );
    }
}
