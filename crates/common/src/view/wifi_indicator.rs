use super::menu_status::{WifiSample, sample_wifi};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct WifiPolling(Arc<WifiWorker>);

#[derive(Debug)]
struct WifiWorker {
    snapshot: Arc<Mutex<Option<WifiSample>>>,
    requests: mpsc::SyncSender<()>,
    pending: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<mpsc::Receiver<()>>,
}

impl WifiPolling {
    fn new(mut sample: impl FnMut() -> Option<WifiSample> + Send + 'static) -> Self {
        let snapshot = Arc::new(Mutex::new(None));
        let published = snapshot.clone();
        let pending = Arc::new(AtomicBool::new(false));
        let outstanding = pending.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let (requests, receive) = mpsc::sync_channel(1);
        let (complete, finished) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("wifi-poll".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                while receive.recv().is_ok() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(state) = sample() {
                        *published.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
                    }
                    outstanding.store(false, Ordering::Release);
                }
                let _ = complete.send(());
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                log::error!("Failed to start Wi-Fi worker: {error}");
                None
            }
        };
        let polling = Self(Arc::new(WifiWorker {
            snapshot,
            requests,
            pending,
            stopping,
            worker: Mutex::new(worker),
            finished: Mutex::new(finished),
        }));
        polling.request();
        polling
    }

    fn snapshot(&self) -> Option<WifiSample> {
        *self.0.snapshot.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn request(&self) {
        if !self.0.stopping.load(Ordering::Acquire)
            && !self.0.pending.swap(true, Ordering::AcqRel)
            && self.0.requests.try_send(()).is_err()
        {
            self.0.pending.store(false, Ordering::Release);
        }
    }

    fn stop(&self) {
        self.0.stop();
    }

    #[cfg(test)]
    fn wait_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.0.pending.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !self.0.pending.load(Ordering::Acquire),
            "Wi-Fi worker stalled"
        );
    }
}

impl WifiWorker {
    fn stop(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.requests.try_send(());
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if worker.is_none() {
            return;
        }
        // The fallback probe kills and reaps its child within 300 ms.
        if self
            .finished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_millis(400))
            .is_ok()
            || worker.as_ref().is_some_and(|worker| worker.is_finished())
        {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        } else {
            log::error!("Wi-Fi worker did not stop within 400 ms");
        }
    }
}

impl Drop for WifiWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

use crate::constants::WIFI_UPDATE_INTERVAL;
use crate::geom::{Point, Rect};
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::Stylesheet;
use crate::view::{Command, View, WifiIcon};
use crate::wifi;

#[derive(Debug, Clone)]
pub struct WifiIndicator {
    point: Point,
    connected: bool,
    enabled: bool,
    last_updated: Instant,
    icon: WifiIcon,
    cached: Option<super::menu_status::MenuStatusCache>,
    polling: Option<WifiPolling>,
    #[cfg(test)]
    wait_for_poll: bool,
}

impl WifiIndicator {
    pub fn new(_res: Resources, point: Point) -> Self {
        Self::with_polling(
            point,
            WifiPolling::new(|| {
                sample_wifi(
                    wifi::WiFiSettings::load().ok()?.wifi,
                    wifi::sample_ip_address,
                )
            }),
        )
    }

    fn with_polling(point: Point, polling: WifiPolling) -> Self {
        Self {
            point,
            connected: false,
            enabled: false,
            last_updated: Instant::now(),
            icon: WifiIcon::new(point),
            cached: None,
            polling: Some(polling),
            #[cfg(test)]
            wait_for_poll: false,
        }
    }
    pub fn new_cached(point: Point, cache: super::menu_status::MenuStatusCache) -> Self {
        let sample = cache.snapshot().wifi;
        let connected = sample.is_some_and(|s| s.connected);
        let mut icon = WifiIcon::new(point);
        icon.set_connected(connected);
        Self {
            point,
            connected,
            enabled: sample.is_some_and(|s| s.enabled),
            last_updated: Instant::now(),
            icon,
            cached: Some(cache),
            polling: None,
            #[cfg(test)]
            wait_for_poll: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture_expired(mut self) -> Self {

        if let Some(polling) = &self.polling {
            polling.wait_idle();
        }
        self.last_updated = Instant::now() - WIFI_UPDATE_INTERVAL;
        self.wait_for_poll = true;
        self
    }

}

#[async_trait(?Send)]
impl View for WifiIndicator {
    fn stop_workers(&mut self) {
        if let Some(polling) = &self.polling {
            polling.stop();
        }
    }

    fn theme_status(&self) -> Option<crate::theme::frame::Status> {
        Some(crate::theme::frame::Status {
            network: if self.enabled {
                Some(if self.connected { "online" } else { "offline" }.into())
            } else {
                None
            },
            ..Default::default()
        })
    }
    fn update(&mut self, _dt: Duration) {
        if let Some(polling) = &self.polling {
            if self.last_updated.elapsed() >= WIFI_UPDATE_INTERVAL {
                self.last_updated = Instant::now();
                polling.request();
                #[cfg(test)]
                if self.wait_for_poll {
                    polling.wait_idle();
                }
            }
        }
        // Adopt completions every update, including the first sample, rather
        // than waiting another full polling interval before invalidating.
        let sample = if let Some(cache) = &self.cached {
            cache.snapshot().wifi
        } else {
            self.polling.as_ref().and_then(WifiPolling::snapshot)
        };
        let Some(WifiSample { enabled, connected }) = sample else {
            return;
        };

        if enabled != self.enabled || connected != self.connected {
            self.enabled = enabled;
            self.connected = connected;
            self.icon.set_connected(connected);
            self.set_should_draw();
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        self.icon.draw(display, styles)
    }

    fn should_draw(&self) -> bool {
        self.enabled && self.icon.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.icon.set_should_draw();
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
        vec![&self.icon]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.icon]
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        if !self.enabled {
            return Rect::zero();
        }
        self.icon.bounding_box(styles)
    }

    fn set_position(&mut self, point: Point) {
        self.point = point;
        self.icon.set_position(point);
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod asynchronous_wifi_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn status_slow_wifi_sample_never_delays_construction_update_or_input_and_is_deduplicated()
    {
        let calls = Arc::new(AtomicUsize::new(0));
        let samples = calls.clone();
        let (done, duration) = mpsc::sync_channel(1);
        let start = Instant::now();
        let mut indicator = WifiIndicator::with_polling(
            Point::zero(),
            WifiPolling::new(move || {
                samples.fetch_add(1, Ordering::AcqRel);
                let start = Instant::now();
                std::thread::sleep(Duration::from_millis(150));
                done.send(start.elapsed()).unwrap();
                Some(WifiSample {
                    enabled: true,
                    connected: true,
                })
            }),
        );
        let construction = start.elapsed();
        assert!(
            construction < Duration::from_millis(80),
            "construction: {construction:?}"
        );
        assert_eq!(indicator.theme_status().unwrap().network, None);
        assert!(!indicator.should_draw());
        let (commands, _receive) = tokio::sync::mpsc::channel(1);
        let input = Instant::now();
        for _ in 0..100 {
            indicator.last_updated = Instant::now() - WIFI_UPDATE_INTERVAL;
            indicator.update(Duration::ZERO);
        }
        assert!(
            !indicator
                .handle_key_event(
                    KeyEvent::Pressed(crate::platform::Key::Right),
                    commands,
                    &mut VecDeque::new()
                )
                .await
                .unwrap()
        );
        let input = input.elapsed();
        assert!(input < Duration::from_millis(40), "input/update: {input:?}");
        let sampling = duration.recv_timeout(Duration::from_secs(2)).unwrap();
        indicator.polling.as_ref().unwrap().wait_idle();
        indicator.update(Duration::ZERO);
        assert_eq!(
            indicator.theme_status().unwrap().network.as_deref(),
            Some("online")
        );
        assert!(
            indicator.should_draw(),
            "first completion must invalidate without waiting for the interval"
        );
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "no queued duplicate probe"
        );
        eprintln!(
            "STATUS wifi synchronous_sample={sampling:?} async_construction={construction:?} hundred_updates_and_input={input:?}"
        );
    }

    #[test]
    fn status_wifi_failure_stays_unknown_then_retains_last_valid_result() {
        let mut calls = 0;
        let polling = WifiPolling::new(move || {
            calls += 1;
            match calls {
                1 | 3 => None,
                2 => Some(WifiSample {
                    enabled: true,
                    connected: true,
                }),
                _ => Some(WifiSample {
                    enabled: false,
                    connected: false,
                }),
            }
        });
        polling.wait_idle();
        assert_eq!(polling.snapshot(), None);
        polling.request();
        polling.wait_idle();
        let real = Some(WifiSample {
            enabled: true,
            connected: true,
        });
        assert_eq!(polling.snapshot(), real);
        polling.request();
        polling.wait_idle();
        assert_eq!(polling.snapshot(), real);
        polling.request();
        polling.wait_idle();
        assert_eq!(
            polling.snapshot(),
            Some(WifiSample {
                enabled: false,
                connected: false
            })
        );
    }

    #[test]
    fn status_wifi_stop_during_initial_probe_kills_reaps_and_joins_even_with_retained_clone() {
        let calls = Arc::new(AtomicUsize::new(0));
        let samples = calls.clone();
        let (spawned, waiting) = mpsc::sync_channel(1);
        let polling = WifiPolling::new(move || {
            samples.fetch_add(1, Ordering::AcqRel);
            let child = std::process::Command::new("/bin/sleep")
                .arg("2")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            spawned.send(child.id()).unwrap();
            assert!(wifi::bounded_output(child, Duration::from_millis(100)).is_err());
            None
        });
        let pid = waiting.recv_timeout(Duration::from_secs(2)).unwrap() as libc::pid_t;
        let clone = polling.clone();
        for _ in 0..100 {
            clone.request();
        }
        let start = Instant::now();
        polling.stop();
        assert!(start.elapsed() < Duration::from_millis(400));
        assert!(
            polling.0.worker.lock().unwrap().is_none(),
            "stop joined the worker"
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
        clone.request();
        clone.stop();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn status_wifi_last_owner_drop_joins_but_dropping_a_clone_keeps_worker_live() {
        let (entered, waiting) = mpsc::sync_channel(1);
        let (resume, paused) = mpsc::sync_channel(1);
        let (complete, finished) = mpsc::sync_channel(1);
        let polling = WifiPolling::new(move || {
            entered.send(()).unwrap();
            paused.recv_timeout(Duration::from_millis(200)).unwrap();
            complete.send(()).unwrap();
            Some(WifiSample {
                enabled: true,
                connected: false,
            })
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(polling.clone());
        assert!(!polling.0.stopping.load(Ordering::Acquire));
        resume.send(()).unwrap();
        drop(polling);
        finished
            .try_recv()
            .expect("last owner joined its in-flight sample");
    }
}
