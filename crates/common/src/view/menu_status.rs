use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use crate::battery::Battery;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatterySample {
    pub percentage: i32,
    pub charging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiSample {
    pub enabled: bool,
    pub connected: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MenuStatusSnapshot {
    pub battery: Option<BatterySample>,
    pub wifi: Option<WifiSample>,
}

#[derive(Clone, Debug)]
pub struct MenuStatusCache {
    snapshot: Arc<Mutex<MenuStatusSnapshot>>,
    requests: mpsc::SyncSender<()>,
    changed: Arc<tokio::sync::Notify>,
    pending: Arc<AtomicBool>,
}

impl MenuStatusCache {
    pub fn new(mut battery: impl Battery + 'static) -> Self {
        Self::with_samplers(
            move || {
                battery
                    .sample()
                    .ok()?
                    .map(|(percentage, charging)| BatterySample {
                        percentage,
                        charging,
                    })
            },
            || {
                sample_wifi(
                    crate::wifi::WiFiSettings::load().ok()?.wifi,
                    menu_ip_address,
                )
            },
        )
    }

    fn with_samplers(
        mut battery: impl FnMut() -> Option<BatterySample> + Send + 'static,
        mut wifi: impl FnMut() -> Option<WifiSample> + Send + 'static,
    ) -> Self {
        let snapshot = Arc::new(Mutex::new(MenuStatusSnapshot::default()));
        let published = snapshot.clone();
        let changed = Arc::new(tokio::sync::Notify::new());
        let notification = changed.clone();
        let (requests, receive) = mpsc::sync_channel(1);
        let pending = Arc::new(AtomicBool::new(false));
        let outstanding = pending.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("menu-status".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                while receive.recv().is_ok() {
                    let battery = battery();
                    {
                        let mut state = published.lock().unwrap_or_else(|e| e.into_inner());
                        if battery.is_some() {
                            state.battery = battery;
                        }
                    }
                    notification.notify_one();
                    let wifi = wifi();
                    {
                        let mut state = published.lock().unwrap_or_else(|e| e.into_inner());
                        if wifi.is_some() {
                            state.wifi = wifi;
                        }
                    }
                    outstanding.store(false, Ordering::Release);
                    notification.notify_one();
                }
            })
        {
            log::error!("Failed to start menu status worker: {error}");
        }
        Self {
            snapshot,
            requests,
            changed,
            pending,
        }
    }

    pub fn snapshot(&self) -> MenuStatusSnapshot {
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn request_refresh(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) && self.requests.try_send(()).is_err() {
            self.pending.store(false, Ordering::Release);
        }
    }

    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}

pub(super) fn sample_wifi(
    enabled: bool,
    probe: impl FnOnce() -> anyhow::Result<Option<std::net::Ipv4Addr>>,
) -> Option<WifiSample> {
    let connected = enabled && probe().ok()?.is_some();
    Some(WifiSample { enabled, connected })
}

fn menu_ip_address() -> anyhow::Result<Option<std::net::Ipv4Addr>> {
    crate::wifi::sample_ip_address()
}

#[cfg(test)]
#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
mod tests {
    use super::*;
    use crate::wifi::{bounded_output, decode_route};
    use std::time::{Duration, Instant};

    fn wait_idle(cache: &MenuStatusCache) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.pending.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !cache.pending.load(Ordering::Acquire),
            "menu status worker stalled"
        );
    }

    #[test]
    fn status_menu_refreshes_are_deduplicated_while_sampler_is_in_flight() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let samples = calls.clone();
        let (entered, waiting) = mpsc::sync_channel(1);
        let (resume, paused) = mpsc::sync_channel(1);
        let cache = MenuStatusCache::with_samplers(
            || None,
            move || {
                samples.fetch_add(1, Ordering::AcqRel);
                entered.send(()).unwrap();
                paused.recv_timeout(Duration::from_secs(2)).unwrap();
                Some(WifiSample {
                    enabled: true,
                    connected: true,
                })
            },
        );
        cache.request_refresh();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..100 {
            cache.request_refresh();
        }
        resume.send(()).unwrap();
        wait_idle(&cache);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        cache.request_refresh();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        resume.send(()).unwrap();
        wait_idle(&cache);
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    fn wait_for_wifi_sample(cache: &MenuStatusCache, expected: Option<WifiSample>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.snapshot().wifi != expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(cache.snapshot().wifi, expected);
    }

    #[test]
    fn perf5b_failed_wifi_probe_keeps_last_real_sample() {
        let (done, finished) = mpsc::channel();
        let mut calls = 0;
        let cache = MenuStatusCache::with_samplers(
            || None,
            move || {
                calls += 1;
                let sample = sample_wifi(true, || {
                    if calls == 1 {
                        Ok(Some("192.168.1.2".parse().unwrap()))
                    } else {
                        Err(anyhow::anyhow!("probe execution failed"))
                    }
                });
                done.send(()).unwrap();
                sample
            },
        );
        let real = Some(WifiSample {
            enabled: true,
            connected: true,
        });
        cache.request_refresh();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        wait_idle(&cache);
        wait_for_wifi_sample(&cache, real);
        cache.request_refresh();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        wait_idle(&cache);
        cache.request_refresh();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        wait_idle(&cache);
        assert_eq!(cache.snapshot().wifi, real);
    }

    #[test]
    fn perf5b_initial_failed_wifi_probe_stays_hidden() {
        let (done, finished) = mpsc::channel();
        let cache = MenuStatusCache::with_samplers(
            || None,
            move || {
                let sample = sample_wifi(true, || decode_route(vec![0xff]));
                done.send(()).unwrap();
                sample
            },
        );
        for _ in 0..2 {
            cache.request_refresh();
            finished.recv_timeout(Duration::from_secs(2)).unwrap();
            wait_idle(&cache);
        }
        assert_eq!(cache.snapshot().wifi, None);
    }

    #[test]
    fn perf5b_hung_probe_is_reaped_and_worker_serves_next_request() {
        let (spawned, pid) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let mut calls = 0;
        let cache = MenuStatusCache::with_samplers(
            || None,
            move || {
                calls += 1;
                let sample = sample_wifi(true, || {
                    if calls == 1 {
                        let child = std::process::Command::new("sleep")
                            .arg("1")
                            .stdout(std::process::Stdio::piped())
                            .spawn()?;
                        spawned.send(child.id()).unwrap();
                        decode_route(bounded_output(child, Duration::from_millis(30))?)
                    } else {
                        Ok(Some("192.168.1.2".parse().unwrap()))
                    }
                });
                done.send(()).unwrap();
                sample
            },
        );
        cache.request_refresh();
        let pid = pid.recv_timeout(Duration::from_secs(2)).unwrap() as libc::pid_t;
        finished
            .recv_timeout(Duration::from_millis(400))
            .expect("hung probe stalled status worker");
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "probe process still exists"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(cache.snapshot().wifi, None);
        cache.request_refresh();
        finished.recv_timeout(Duration::from_millis(400)).unwrap();
        wait_idle(&cache);
        wait_for_wifi_sample(
            &cache,
            Some(WifiSample {
                enabled: true,
                connected: true,
            }),
        );
    }

    #[test]
    fn perf5b_child_execution_and_output_errors_are_failed_probes() {
        for script in [
            "exit 1",
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        ] {
            let child = std::process::Command::new("sh")
                .args(["-c", script])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id() as libc::pid_t;
            assert!(bounded_output(child, Duration::from_millis(300)).is_err());
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        let child = std::process::Command::new("printf")
            .arg("broadcast dev wlan0 src 192.168.1.2 uid 0\n")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(
            decode_route(bounded_output(child, Duration::from_millis(300)).unwrap()).unwrap(),
            Some("192.168.1.2".parse().unwrap())
        );
    }

    #[test]
    fn perf5b_successful_probe_without_address_is_known_offline() {
        assert_eq!(
            sample_wifi(true, || decode_route(b"broadcast dev wlan0\n".to_vec())),
            Some(WifiSample {
                enabled: true,
                connected: false
            })
        );
        assert_eq!(
            sample_wifi(false, || panic!("disabled Wi-Fi probed")),
            Some(WifiSample {
                enabled: false,
                connected: false
            })
        );
        assert!(decode_route(b"broadcast src invalid\n".to_vec()).is_err());
        assert_eq!(
            decode_route(b"broadcast src 192.168.1.2 uid 0\n".to_vec()).unwrap(),
            Some("192.168.1.2".parse().unwrap())
        );
    }

    #[test]
    fn perf5_inflight_status_worker_drop_never_blocks_reopen() {
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let cache = MenuStatusCache::with_samplers(
            move || {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                None
            },
            || None,
        );
        cache.request_refresh();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(cache);
        assert!(start.elapsed() < Duration::from_millis(40));
        release.send(()).unwrap();
    }

    #[tokio::test]
    async fn perf5_failed_samples_preserve_real_cache_without_default_percentage() {
        struct UnknownBattery;
        impl Battery for UnknownBattery {
            fn update(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn sample(&mut self) -> anyhow::Result<Option<(i32, bool)>> {
                Ok(None)
            }
            fn percentage(&self) -> i32 {
                100
            }
            fn charging(&self) -> bool {
                false
            }
        }
        let cache = MenuStatusCache::new(UnknownBattery);
        cache.request_refresh();
        tokio::time::timeout(Duration::from_secs(2), cache.changed())
            .await
            .unwrap();
        assert_eq!(cache.snapshot().battery, None);

        let mut calls = 0;
        let cache = MenuStatusCache::with_samplers(
            move || {
                calls += 1;
                (calls == 1).then_some(BatterySample {
                    percentage: 37,
                    charging: true,
                })
            },
            || None,
        );
        cache.request_refresh();
        tokio::time::timeout(Duration::from_secs(2), cache.changed())
            .await
            .unwrap();
        let real = cache.snapshot().battery;
        assert_eq!(real.unwrap().percentage, 37);
        cache.request_refresh();
        tokio::time::timeout(Duration::from_secs(2), cache.changed())
            .await
            .unwrap();
        assert_eq!(cache.snapshot().battery, real);
    }

    #[test]
    fn perf5_slow_status_samplers_do_not_delay_first_frame() {
        let start = Instant::now();
        let cache = MenuStatusCache::with_samplers(
            || {
                std::thread::sleep(Duration::from_millis(120));
                Some(BatterySample {
                    percentage: 37,
                    charging: false,
                })
            },
            || {
                std::thread::sleep(Duration::from_millis(120));
                Some(WifiSample {
                    enabled: true,
                    connected: true,
                })
            },
        );
        let mut map = type_map::TypeMap::new();
        let styles = crate::stylesheet::Stylesheet::load().unwrap();
        map.insert(styles.clone());
        let mut bar = crate::view::StatusBar::<
            <crate::platform::DefaultPlatform as crate::platform::Platform>::Battery,
        >::new_cached(
            crate::resources::Resources::new(map),
            crate::geom::Point::new(620, 8),
            cache.clone(),
        );
        use crate::platform::Platform;
        use crate::view::View;
        let mut platform = crate::platform::DefaultPlatform::new().unwrap();
        let mut display = platform.display().unwrap();
        bar.draw(&mut display, &styles).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(80),
            "first frame waited {:?} for sampling",
            start.elapsed()
        );
        assert_eq!(bar.theme_inputs().battery, None);
        assert_eq!(bar.theme_inputs().network, None);
        cache.request_refresh();
        let drop_start = Instant::now();
        drop(bar);
        assert!(drop_start.elapsed() < Duration::from_millis(40));
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.snapshot().wifi.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(cache.snapshot().battery.unwrap().percentage, 37);
        assert!(cache.snapshot().wifi.unwrap().connected);
    }
}
