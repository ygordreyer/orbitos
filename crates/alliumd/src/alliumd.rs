use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

use allium_menu::{AlliumMenu, RetroArchInfo};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use common::battery::Battery;
use common::constants::{
    ALLIUM_BASE_DIR, ALLIUM_FONTS_DIR, ALLIUM_GAME_INFO, ALLIUM_LAUNCHER, ALLIUM_SD_ROOT,
    ALLIUM_THEMES_DIR, ALLIUM_VERSION, ALLIUMD_STATE, BATTERY_SHUTDOWN_THRESHOLD,
    BATTERY_UPDATE_INTERVAL, BATTERY_WARNING_THRESHOLD, IDLE_TIMEOUT, MAX_BRIGHTNESS, MAX_VOLUME,
};
use common::display::settings::DisplaySettings;
use common::locale::{Locale, LocaleSettings};
use common::osd::OsdEvent;
use common::power::{PowerButtonAction, PowerSettings, VolumeOnStartup};
use common::retroarch::RetroArchCommand;
use common::stylesheet::Stylesheet;
use common::theme::{Boundary, ThemeHandle, ThemeRuntime};
use common::wifi::WiFiSettings;
use enum_map::EnumMap;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use common::database::Database;
use common::game_info::GameInfo;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};

use crate::osd::{Osd, OsdKind};

#[cfg(unix)]
use {
    nix::sys::signal::Signal, nix::sys::signal::kill, nix::unistd::Pid,
    tokio::signal::unix::SignalKind,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlliumDState {
    #[serde(default = "Utc::now")]
    time: DateTime<Utc>,
    volume: i32,
    brightness: u8,
}

struct MenuSession {
    info: Option<RetroArchInfo>,
    theme: Option<ThemeHandle>,
    game_info: GameInfo,
    capture_deadline: Option<Instant>,
    trace: allium_menu::open_trace::OpenTrace,
}

enum MenuRequest {
    Open(MenuSession),
    Prewarm,
}

/// Handle to the persistent menu thread
struct MenuHandle {
    tx: mpsc::Sender<MenuRequest>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    _handle: JoinHandle<Result<()>>,
}

impl MenuHandle {
    fn new(styles: Stylesheet) -> Self {
        let (tx, rx) = mpsc::channel::<MenuRequest>();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = tokio::runtime::Handle::current();

        let handle = std::thread::spawn(move || -> Result<()> {
            let result = rt.block_on(async {
                let platform = common::platform::DefaultPlatform::new()
                    .context("initializing menu platform")?;
                let mut app = AlliumMenu::new(platform, styles)
                    .await
                    .context("initializing menu app")?;

                let mut prewarmer = MenuPrewarmer::new();
                loop {
                    match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(MenuRequest::Open(session)) => {
                            #[cfg(feature = "miyoo")]
                            let foreground = common::platform::cpu_tuning::foreground();
                            prewarmer.info = session.info.clone();
                            app.set_open_trace(session.trace);
                            let prepared = {
                                let _phase = session.trace.phase("menu_prepare");
                                app.prepare(session.info, session.theme, session.game_info)
                                    .await
                            };
                            if let Err(e) = prepared {
                                log::error!("menu prepare failed: {:?}", e);
                            } else if let Err(e) = app
                                .run_event_loop_after_settle(session.capture_deadline)
                                .await
                            {
                                log::error!("menu run failed: {:?}", e);
                            }
                            if let Err(e) = app.save() {
                                log::error!("menu save failed: {:?}", e);
                            }
                            #[cfg(feature = "miyoo")]
                            drop(foreground);
                            let _ = done_tx.send(());
                        }
                        Ok(MenuRequest::Prewarm) => {
                            let _phase = common::perf_trace::menu_phase("press_prewarm");
                            prewarmer.prepare(&mut app, true).await;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            prewarmer.prepare(&mut app, false).await
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Ok(())
            });
            if let Err(error) = &result {
                error!("menu thread failed: {error:#}");
            }
            result
        });

        MenuHandle {
            tx,
            done_rx,
            _handle: handle,
        }
    }
}

struct MenuPrewarmer {
    game: Option<(std::path::PathBuf, i64)>,
    info: Option<RetroArchInfo>,
    runtime: Option<ThemeRuntime>,
    modified: Option<std::time::SystemTime>,
}

impl MenuPrewarmer {
    fn new() -> Self {
        Self {
            game: None,
            info: None,
            modified: None,
            runtime: ThemeRuntime::bind(
                &ALLIUM_BASE_DIR.join("state"),
                &ALLIUM_THEMES_DIR,
                &ALLIUM_FONTS_DIR,
            )
            .ok(),
        }
    }

    async fn prepare(&mut self, app: &mut AlliumMenu<DefaultPlatform>, on_press: bool) {
        let modified = fs::metadata(ALLIUM_GAME_INFO.as_path())
            .ok()
            .and_then(|m| m.modified().ok());
        if !on_press && modified == self.modified && self.game.is_some() {
            return;
        }
        self.modified = modified;
        let Some(game) = fs::read(ALLIUM_GAME_INFO.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<GameInfo>(&bytes).ok())
            .filter(|game| game.has_menu)
        else {
            self.game = None;
            self.info = None;
            return;
        };
        let _phase = common::perf_trace::menu_phase("game_prewarm");
        let identity = (game.path.clone(), game.start_time.timestamp_millis());
        let changed = self.game.as_ref() != Some(&identity);
        if changed {
            // Only offscreen pixels use this seed; release validates every value with GET_INFO.
            self.info = Some(RetroArchInfo {
                max_disk_slots: 1,
                disk_slot: 0,
                state_slot: Some(0),
            });
        }
        self.game = Some(identity);
        let theme = self
            .runtime
            .as_mut()
            .and_then(|runtime| runtime.activate(Boundary::MenuOpening).snapshot);
        if let Err(error) = app.prewarm(self.info.clone(), theme, game).await {
            warn!("menu prewarm failed: {error:#}");
        }
    }
}

fn menu_press_is_unmodified(keys: &EnumMap<Key, bool>) -> bool {
    keys.iter()
        .all(|(key, pressed)| key == Key::Menu || !pressed)
}

fn parse_retroarch_info(reply: String) -> RetroArchInfo {
    let mut fields = reply.split_ascii_whitespace().skip(1);
    RetroArchInfo {
        max_disk_slots: fields.next().map_or(0, |s| s.parse().unwrap_or(0)),
        disk_slot: fields.next().map_or(0, |s| s.parse().unwrap_or(0)),
        state_slot: fields.next().map(|s| s.parse().unwrap_or(0)),
    }
}

#[derive(Clone, Copy)]
struct BatteryReading {
    percentage: i32,
    charging: bool,
}

impl BatteryReading {
    fn from_battery(battery: &impl Battery) -> Self {
        Self {
            percentage: battery.percentage(),
            charging: battery.charging(),
        }
    }
    fn percentage(self) -> i32 {
        self.percentage
    }
    fn charging(self) -> bool {
        self.charging
    }
}

struct BatteryMonitor {
    requests: mpsc::SyncSender<()>,
    readings: tokio::sync::mpsc::UnboundedReceiver<BatteryReading>,
    _thread: JoinHandle<()>,
}

impl BatteryMonitor {
    fn new(mut battery: impl Battery + 'static) -> Result<Self> {
        let (requests, receive) = mpsc::sync_channel(1);
        let (send, readings) = tokio::sync::mpsc::unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("daemon-battery".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                while receive.recv().is_ok() {
                    if let Err(error) = battery.update() {
                        error!("failed to update battery: {error}");
                    }
                    if send.send(BatteryReading::from_battery(&battery)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            readings,
            _thread: thread,
        })
    }

    fn request(&self) {
        let _ = self.requests.try_send(());
    }
}

pub struct AlliumD<P: Platform> {
    platform: P,
    main: Child,
    menu: MenuHandle,
    menu_open: bool,
    menu_available: bool,
    theme_runtime: Option<ThemeRuntime>,
    keys: EnumMap<Key, bool>,
    is_menu_pressed_alone: bool,
    is_terminating: bool,
    state: AlliumDState,
    locale: Locale,
    power_settings: PowerSettings,
    osd: Osd<P>,
}

impl AlliumDState {
    pub fn new() -> Self {
        Self {
            time: Utc::now(),
            volume: 20,
            brightness: 50,
        }
    }

    pub fn load() -> Result<AlliumDState> {
        if ALLIUMD_STATE.exists() {
            debug!("found state, loading from file");
            if let Ok(json) = fs::read_to_string(ALLIUMD_STATE.as_path())
                && let Ok(this) = serde_json::from_str::<AlliumDState>(&json)
            {
                if Utc::now() < this.time {
                    info!(
                        "RTC is not working, advancing time to {}",
                        this.time.format("%F %T")
                    );
                    let mut date = std::process::Command::new("date")
                        .arg("--utc")
                        .arg("--set")
                        .arg(this.time.format("%F %T").to_string())
                        .spawn()?;
                    date.wait()?;
                    let mut hwclock = std::process::Command::new("/sbin/hwclock")
                        .arg("--systohc")
                        .arg("--utc")
                        .arg(this.time.format("%F %T").to_string())
                        .spawn()?;
                    hwclock.wait()?;
                }
                return Ok(this);
            }
            warn!("failed to read state file, removing");
            fs::remove_file(ALLIUMD_STATE.as_path())?;
        }
        Ok(Self::new())
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_string(self).unwrap();
        File::create(ALLIUMD_STATE.as_path())?.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Sleeps until `deadline`, or forever when there is none — the select! arm needs no precondition
async fn sleep_until_wake(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn spawn_main() -> Result<Child> {
    common::timezone::load_saved_timezone();
    #[cfg(feature = "miyoo")]
    return Ok(match GameInfo::load()? {
        Some(mut game_info) => {
            debug!("found game info, resuming game");
            game_info.start_time = Utc::now();
            game_info.save()?;
            common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Game);
            game_info.command().into()
        }
        None => {
            debug!("no game info found, launching launcher");
            common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Frontend);
            use common::constants::ALLIUM_LAUNCHER;
            common::perf_trace::lifecycle(common::perf_trace::LAUNCHER_SPAWN);
            Command::new(ALLIUM_LAUNCHER.as_path())
        }
    }
    .spawn()?);

    #[cfg(not(feature = "miyoo"))]
    {
        common::perf_trace::lifecycle(common::perf_trace::LAUNCHER_SPAWN);
        return Ok(Command::new("/bin/sh")
            .arg("-c")
            .arg("make simulator-launcher")
            .spawn()?);
    }
}

impl AlliumD<DefaultPlatform> {
    pub async fn new() -> Result<AlliumD<DefaultPlatform>> {
        #[cfg(feature = "miyoo")]
        common::platform::miyoo::try_fix_resolution().await?;

        #[cfg(feature = "miyoo")]
        common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Frontend);

        common::perf_trace::initialize();
        let mut platform = DefaultPlatform::new()?;
        let mut state = AlliumDState::load()?;

        let mut keys = EnumMap::default();

        // Poll all key events for 100ms to sync initial key state
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
        loop {
            tokio::select! {
                key_event = platform.poll() => {
                    match key_event {
                        KeyEvent::Pressed(key) => keys[key] = true,
                        KeyEvent::Released(key) => keys[key] = false,
                        KeyEvent::Autorepeat(key) => keys[key] = true,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break;
                }
            }
        }

        if keys[Key::Menu] || keys[Key::B] {
            info!("menu or B key held at startup, bypass game resume");
            GameInfo::delete()?;
        }

        let power_settings = PowerSettings::load()?;

        state.volume = if keys[Key::VolDown] {
            info!("volume down key held at startup, muting");
            0
        } else {
            match power_settings.volume_on_startup {
                VolumeOnStartup::Muted => {
                    info!("volume on startup is muted");
                    0
                }
                VolumeOnStartup::Restore => state.volume,
            }
        };

        info!("setting volume: {}", state.volume);
        platform.set_volume(state.volume)?;

        info!("setting brightness: {}", state.brightness);
        platform.set_brightness(state.brightness)?;

        info!("loading display settings");
        platform.set_display_settings(&mut DisplaySettings::load()?)?;

        let main = spawn_main().await?;
        let locale = Locale::new(&LocaleSettings::load()?.lang);

        // One load per process: font data is Arc'd, so clones share it
        let styles = Stylesheet::load()?;

        let mut theme_runtime = ThemeRuntime::bind(
            &ALLIUM_BASE_DIR.join("state"),
            &ALLIUM_THEMES_DIR,
            &ALLIUM_FONTS_DIR,
        )
        .map_err(|error| warn!("Theme runtime unavailable, using Legacy menu/OSD: {error:#}"))
        .ok();

        if let Some(runtime) = theme_runtime.as_mut() {
            let activation = runtime.activate_osd();
            if let Some(message) = activation.diagnostic_once { warn!("{message}"); }
            if let Some(theme) = activation.snapshot {
                if let Err(error) = Osd::<DefaultPlatform>::warm_theme(theme) {
                    warn!("OSD warmup failed: {error:#}");
                }
            }
        }

        // Spawn the persistent menu thread at startup
        let menu = MenuHandle::new(styles.clone());

        platform.daemon();

        Ok(AlliumD {
            platform,
            main,
            menu,
            menu_open: false,
            menu_available: true,
            theme_runtime,
            keys,
            is_menu_pressed_alone: false,
            is_terminating: false,
            state,
            locale,
            power_settings,
            osd: Osd::new(styles),
        })
    }

    pub async fn run_event_loop(&mut self) -> Result<()> {
        info!("hello from Allium {}", *ALLIUM_VERSION);

        if DefaultPlatform::has_wifi() {
            info!("wifi detected, loading wifi settings");
            if let Err(error) = WiFiSettings::load()?.init_sequenced().await {
                error!("WiFi/SSH startup failed; keeping daemon supervision active: {error:#}");
            }
        }

        info!("starting event loop");
        #[cfg(unix)]
        {
            let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())?;
            let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

            // If battery is charging, suspend.
            let mut battery = self.platform.battery()?;
            battery.update()?;
            charging_startup(battery.charging(), Path::new("/tmp/stay_awake"), || {
                self.handle_charging()
            })
            .await?;

            let mut latest_battery = BatteryReading::from_battery(&battery);
            let mut battery_monitor = BatteryMonitor::new(battery)?;
            let mut battery_interval = tokio::time::interval_at(
                tokio::time::Instant::now() + BATTERY_UPDATE_INTERVAL,
                BATTERY_UPDATE_INTERVAL,
            );
            battery_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut battery_led_task = None;
            let mut idle_since = Instant::now();

            loop {
                let auto_sleep_duration = match self.power_settings.auto_sleep_duration_minutes {
                    0 => std::time::Duration::MAX, // disabled
                    t => std::time::Duration::new(t as u64 * 60, 0),
                };
                tokio::select! {
                    _ = battery_interval.tick() => battery_monitor.request(),
                    Some(reading) = battery_monitor.readings.recv() => {
                        latest_battery = reading;
                        self.handle_battery_reading(reading, &mut battery_led_task).await?;
                    }
                    key_event = self.platform.poll() => {
                        self.handle_key_event(key_event).await?;
                        idle_since = Instant::now();
                    }
                    _ = sleep_until_wake(self.osd.next_wake()) => {
                        if let Err(e) = self.osd.tick() {
                            error!("failed to update OSD: {}", e);
                        }
                        idle_since = Instant::now();
                    }
                    completion = self.menu.done_rx.recv(), if self.menu_available => {
                        idle_since = Instant::now();
                        self.is_menu_pressed_alone = false;
                        match completion {
                            Some(()) => {
                                info!("menu finished, resuming game");
                                self.hide_osd();
                                self.menu_open = false;
                                RetroArchCommand::Unpause.send().await?;
                            }
                            None => {
                                error!("menu thread unavailable; disabling menu completion channel");
                                if disable_menu(&mut self.menu_available, &mut self.menu_open) {
                                    self.hide_osd();
                                    RetroArchCommand::Unpause.send().await?;
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(auto_sleep_duration.saturating_sub(idle_since.elapsed())) => {
                        idle_since = Instant::now();
                        if !self.power_settings.auto_sleep_when_charging && latest_battery.charging() {
                            info!("battery charging, don't auto sleep");
                        } else if Path::new("/tmp/stay_awake").exists() {
                            info!("/tmp/stay_awake exists, don't auto sleep");
                        } else {
                            info!("idle timeout, shutting down");
                            self.handle_quit().await?;
                        }
                    }
                    _ = self.main.wait() => {
                        common::perf_trace::lifecycle(common::perf_trace::CHILD_EXIT);
                        #[cfg(feature = "miyoo")]
                        restore_launcher_boost();
                        idle_since = Instant::now();
                        if !self.is_terminating {
                            info!("main process terminated, recording play time");
                            self.update_play_time()?;
                            GameInfo::delete()?;
                            self.main = spawn_main().await?;
                        }
                    }
                    _ = sigint.recv() => self.handle_quit().await?,
                    _ = sigterm.recv() => self.handle_quit().await?,
                }
            }
        }

        #[cfg(not(unix))]
        loop {
            tokio::select! {
                key_event = self.platform.poll() => {
                    self.handle_key_event(key_event).await?;
                }
                _ = sleep_until_wake(self.osd.next_wake()) => {
                    if let Err(e) = self.osd.tick() {
                        error!("failed to update OSD: {}", e);
                    }
                }
            }
        }
    }

    async fn handle_battery_reading(
        &mut self,
        reading: BatteryReading,
        battery_led_task: &mut Option<tokio::task::JoinHandle<()>>,
    ) -> Result<()> {
        if reading.percentage() <= BATTERY_WARNING_THRESHOLD && !reading.charging() {
            if battery_led_task.is_none() {
                warn!(
                    "battery is low ({}%), consider charging soon",
                    reading.percentage()
                );

                *battery_led_task = Some(tokio::spawn(async {
                    loop {
                        <DefaultPlatform as Platform>::Battery::update_led(true);
                        tokio::time::sleep(std::time::Duration::from_millis(1750)).await;
                        <DefaultPlatform as Platform>::Battery::update_led(false);
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                }));
            }
        } else if let Some(task) = battery_led_task.take() {
            info!("aborting battery LED blink task");
            task.abort();
            <DefaultPlatform as Platform>::Battery::update_led(false);
        }

        if reading.percentage() <= BATTERY_SHUTDOWN_THRESHOLD && !reading.charging() {
            warn!("battery is low, shutting down");
            self.handle_quit().await?;
        }
        Ok(())
    }

    async fn handle_key_event(&mut self, key_event: KeyEvent) -> Result<()> {
        let release_trace = matches!(key_event, KeyEvent::Released(Key::Menu))
            .then(allium_menu::open_trace::OpenTrace::start);
        debug!(
            "main: {:?}, ingame: {}, key_event: {:?}",
            self.main.id(),
            self.is_ingame(),
            key_event
        );

        // Handle menu key
        match key_event {
            KeyEvent::Pressed(Key::Menu) => {
                self.is_menu_pressed_alone = menu_press_is_unmodified(&self.keys);
                if self.is_menu_pressed_alone && self.menu_available && !self.menu_open {
                    let _ = self.menu.tx.send(MenuRequest::Prewarm);
                }
            }
            KeyEvent::Pressed(_) => {
                self.is_menu_pressed_alone = false;
            }
            KeyEvent::Released(_) | KeyEvent::Autorepeat(_) => {}
        }

        // Update self.keys
        match key_event {
            KeyEvent::Pressed(key) => {
                self.keys[key] = true;
            }
            KeyEvent::Released(key) => {
                self.keys[key] = false;
            }
            KeyEvent::Autorepeat(_) => {}
        }

        if self.keys[Key::Menu] {
            // Global hotkeys
            match key_event {
                KeyEvent::Pressed(Key::Up | Key::VolUp)
                | KeyEvent::Autorepeat(Key::Up | Key::VolUp) => {
                    self.add_brightness(5).await?;
                }
                KeyEvent::Pressed(Key::Down | Key::VolDown)
                | KeyEvent::Autorepeat(Key::Down | Key::VolDown) => {
                    self.add_brightness(-5).await?;
                }
                KeyEvent::Pressed(Key::Left) | KeyEvent::Autorepeat(Key::Left) => {
                    self.add_volume(-1).await?;
                }
                KeyEvent::Pressed(Key::Right) | KeyEvent::Autorepeat(Key::Right) => {
                    self.add_volume(1).await?;
                }
                KeyEvent::Released(Key::Power) => {
                    let game_info = GameInfo::load()?;
                    let name = match game_info.as_ref() {
                        Some(game_info) => game_info.name.as_str(),
                        None => common::product::NAME,
                    };
                    let file_name = format!(
                        "{}-{}.png",
                        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"),
                        name,
                    );
                    Command::new("screenshot")
                        .arg(ALLIUM_SD_ROOT.join("Screenshots").join(file_name))
                        .arg("--rumble")
                        .spawn()?
                        .wait()
                        .await?;
                }
                _ => {}
            }
        } else {
            match key_event {
                KeyEvent::Pressed(Key::VolDown) | KeyEvent::Autorepeat(Key::VolDown) => {
                    self.add_volume(-1).await?
                }
                KeyEvent::Pressed(Key::VolUp) | KeyEvent::Autorepeat(Key::VolUp) => {
                    self.add_volume(1).await?
                }
                KeyEvent::Autorepeat(Key::Power) if !self.keys[Key::Menu] => {
                    #[cfg(unix)]
                    self.handle_quit().await?;
                }
                KeyEvent::Released(Key::Power) if !self.keys[Key::Menu] => {
                    #[cfg(unix)]
                    match self.power_settings.power_button_action {
                        PowerButtonAction::Suspend => self.handle_suspend().await?,
                        PowerButtonAction::Shutdown => self.handle_quit().await?,
                        PowerButtonAction::Nothing => {}
                    }
                }
                KeyEvent::Pressed(Key::LidClose) =>
                {
                    #[cfg(unix)]
                    match self.power_settings.lid_close_action {
                        PowerButtonAction::Suspend => self.handle_suspend().await?,
                        PowerButtonAction::Shutdown => self.handle_quit().await?,
                        PowerButtonAction::Nothing => {}
                    }
                }
                KeyEvent::Released(Key::Menu) if self.is_menu_pressed_alone => {
                    let open_trace = release_trace.expect("MENU release trace");
                    let _release = open_trace.phase("release");
                    if self.menu_available
                        && !self.menu_open
                        && self.is_ingame()
                        && self
                            .keys
                            .iter()
                            .all(|(k, pressed)| k == Key::Menu || !pressed)
                        && let Some(game_info) = {
                            let _phase = common::perf_trace::menu_phase("game_info");
                            GameInfo::read()?
                        }
                        && game_info.has_menu
                    {
                        let get_info_phase = open_trace.phase("get_info");
                        let info = RetroArchCommand::GetInfo
                            .send_recv()
                            .await?
                            .map(parse_retroarch_info);

                        drop(get_info_phase);
                        // Stop the overlay writer before starting the pause settle window.
                        {
                            let _phase = open_trace.phase("osd_stop_join");
                            self.hide_osd();
                        }
                        let paused = info.is_some();
                        let capture_deadline = if paused {
                            let _phase = open_trace.phase("pause");
                            RetroArchCommand::Pause.send().await?;
                            Some(Instant::now() + std::time::Duration::from_millis(50))
                        } else {
                            None
                        };

                        let theme = self.theme_runtime.as_mut().and_then(|runtime| {
                            let _phase = open_trace.phase("theme_activation");
                            let locale =
                                fs::read(common::constants::ALLIUM_LOCALE_SETTINGS.as_path())
                                    .ok()
                                    .and_then(|bytes| {
                                        serde_json::from_slice::<LocaleSettings>(&bytes).ok()
                                    })
                                    .unwrap_or_default();
                            let activation = runtime.activate_menu(&locale.lang);
                            if let Some(message) = activation.diagnostic_once {
                                warn!("{message}");
                            }
                            activation.snapshot
                        });
                        self.menu_open = true;
                        open_trace.event("open_dispatch", None);
                        if self
                            .menu
                            .tx
                            .send(MenuRequest::Open(MenuSession {
                                info,
                                theme,
                                game_info,
                                capture_deadline,
                                trace: open_trace,
                            }))
                            .is_err()
                        {
                            error!("failed to send to menu thread");
                            if failed_menu_send(
                                &mut self.menu_available,
                                &mut self.menu_open,
                                paused,
                            ) {
                                RetroArchCommand::Unpause.send().await?;
                            }
                        }
                    }
                    self.is_menu_pressed_alone = false;
                }
                _ => {}
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    async fn handle_charging(&mut self) -> Result<()> {
        info!("charging...");

        self.hide_osd();

        #[cfg(feature = "miyoo")]
        restore_launcher_boost();
        #[cfg(feature = "miyoo")]
        let _boost_pause = pause_launcher_boost();
        signal(&self.main, Signal::SIGSTOP)?;

        Command::new("say")
            .arg(self.locale.t("charging"))
            .spawn()?
            .wait()
            .await?;

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        Command::new("show").arg("-c").spawn()?.wait().await?;

        #[allow(clippy::let_unit_value)]
        let ctx = self.platform.suspend()?;

        let mut battery = self.platform.battery()?;

        loop {
            tokio::select! {
                key_event = self.platform.poll() => {
                    if matches!(key_event, KeyEvent::Released(Key::Power)) {
                        break;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    battery.update()?;
                    if !battery.charging() {
                        allium_menu::open_trace::drain_on_shutdown();
                        self.platform.shutdown()?;
                    }
                }
            }
        }

        signal(&self.main, Signal::SIGCONT)?;
        self.platform.unsuspend(ctx)
    }

    #[cfg(unix)]
    async fn handle_suspend(&mut self) -> Result<()> {
        info!("suspending...");
        self.hide_osd();
        #[cfg(feature = "miyoo")]
        restore_launcher_boost();
        #[cfg(feature = "miyoo")]
        let _boost_pause = pause_launcher_boost();
        #[allow(clippy::let_unit_value)]
        let ctx = self.platform.suspend()?;
        signal(&self.main, Signal::SIGSTOP)?;

        loop {
            tokio::select! {
                key_event = self.platform.poll()=> {
                    if matches!(key_event, KeyEvent::Released(Key::Power)) || matches!(key_event, KeyEvent::Released(Key::LidClose)) {
                        self.keys[Key::Power] = false;
                        self.keys[Key::LidClose] = false;
                        break;
                    }
                }
                _ = tokio::time::sleep(IDLE_TIMEOUT) => {
                    info!("idle timeout, shutting down");
                    signal(&self.main, Signal::SIGCONT)?;
                    self.platform.unsuspend(ctx)?;
                    self.handle_quit().await?;
                    return Ok(());
                }
            }
        }

        info!("waking up from suspend...");
        signal(&self.main, Signal::SIGCONT)?;
        self.platform.unsuspend(ctx)
    }

    #[cfg(unix)]
    async fn handle_quit(&mut self) -> Result<()> {
        if self.is_terminating {
            return Ok(());
        }

        debug!("terminating, saving state");

        self.hide_osd();
        self.state.time = Utc::now();
        self.state.save()?;

        if self.is_ingame() {
            self.update_play_time()?;
            // Menu thread will exit on its own when the process shuts down
        }

        terminate(&mut self.main).await?;

        self.is_terminating = true;

        Command::new("show").arg("--darken").spawn()?.wait().await?;
        Command::new("say")
            .arg(self.locale.t("powering-off"))
            .spawn()?
            .wait()
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        allium_menu::open_trace::drain_on_shutdown();
        self.platform.shutdown()?;

        Ok(())
    }

    #[allow(unused)]
    fn update_play_time(&self) -> Result<()> {
        if !self.is_ingame() {
            return Ok(());
        }

        let file = File::open(ALLIUM_GAME_INFO.as_path())?;
        let mut game_info: GameInfo = serde_json::from_reader(file)?;

        let duration = game_info.play_time();

        // As a sanity check, don't add play time if the game was played for more than 24 hours
        if duration > Duration::hours(24) {
            warn!("play time is too long, not adding to database");
            return Ok(());
        }

        let database = Database::new()?;
        let start_time = game_info.start_time.timestamp();
        let end_time = Utc::now().timestamp();
        database.record_game_session(
            game_info.path.as_path(),
            start_time,
            end_time,
            duration.num_seconds(),
        )?;
        common::perf_trace::lifecycle(common::perf_trace::SESSION_RECORD);

        Ok(())
    }

    fn is_ingame(&self) -> bool {
        Path::new(&*ALLIUM_GAME_INFO).exists()
    }

    async fn add_volume(&mut self, add: i32) -> Result<()> {
        info!("adding volume: {}", add);
        self.state.volume = (self.state.volume + add).clamp(0, MAX_VOLUME);
        // Draw first: set_volume spawns myctl, which costs tens of ms on this SoC
        self.show_osd(
            OsdKind::Volume,
            self.state.volume as f32 / MAX_VOLUME as f32,
            add.signum(),
        )
        .await;
        self.platform.set_volume(self.state.volume)?;
        Ok(())
    }

    async fn add_brightness(&mut self, add: i8) -> Result<()> {
        info!("adding brightness: {}", add);
        self.state.brightness =
            (self.state.brightness as i8 + add).clamp(0, MAX_BRIGHTNESS as i8) as u8;
        // Draw first, matching add_volume, so the two paths behave the same
        self.show_osd(
            OsdKind::Brightness,
            self.state.brightness as f32 / MAX_BRIGHTNESS as f32,
            i32::from(add.signum()),
        )
        .await;
        self.platform.set_brightness(self.state.brightness)?;
        Ok(())
    }

    async fn show_osd(&mut self, kind: OsdKind, fraction: f32, direction: i32) {
        let game_info = fs::read(ALLIUM_GAME_INFO.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<GameInfo>(&bytes).ok());
        let launcher = self.main.id().and_then(|pid| fs::canonicalize(format!("/proc/{pid}/exe")).ok())
            .zip(fs::canonicalize(ALLIUM_LAUNCHER.as_path()).ok())
            .is_some_and(|(exe, launcher)| exe == launcher);
        let target = OsdTarget::for_foreground(self.menu_open, game_info.as_ref(), launcher);
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let event = OsdEvent::new(kind, fraction, direction, seq);
        present_foreground_osd(
            target, event, fraction, send_launcher_osd,
            |command| async move { command.send().await },
            |action| match action {
                FramebufferOsdAction::Retire => self.hide_osd(),
                FramebufferOsdAction::Show => self.show_framebuffer_osd(
                    kind, fraction, direction, target.repaints(),
                ),
            },
        ).await;
    }

    fn show_framebuffer_osd(&mut self, kind: OsdKind, fraction: f32, direction: i32, repainting: bool) {
        let opening_start = std::time::Instant::now();
        let title = self.locale.t(match (kind, direction.is_negative()) {
            (OsdKind::Volume, false) => "hotkeys-volume-up",
            (OsdKind::Volume, true) => "hotkeys-volume-down",
            (OsdKind::Brightness, false) => "hotkeys-brightness-up",
            (OsdKind::Brightness, true) => "hotkeys-brightness-down",
        });
        let theme = if self.osd.is_visible() {
            None
        } else {
            self.theme_runtime.as_mut().and_then(|runtime| {
                let activation = runtime.activate_osd();
                if let Some(message) = activation.diagnostic_once {
                    warn!("{message}");
                }
                activation.snapshot
            })
        };
        info!(target: "alliumd::osd", "osd opening theme_ms={} repainting={repainting}", opening_start.elapsed().as_millis());
        // Cosmetic only: a failed overlay must not take down the daemon
        if let Err(e) = self.osd.show(
            &mut self.platform,
            kind,
            &title,
            fraction,
            repainting,
            theme,
        ) {
            error!("failed to show OSD: {}", e);
        }
    }

    fn hide_osd(&mut self) {
        // Cosmetic only: must not block suspend or shutdown
        if let Err(e) = self.osd.hide() {
            error!("failed to hide OSD: {}", e);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OsdTarget {
    Launcher,
    RetroArch,
    Framebuffer,
}

impl OsdTarget {
    fn repaints(self) -> bool {
        self == Self::RetroArch
    }

    fn for_foreground(menu_open: bool, game_info: Option<&GameInfo>, launcher: bool) -> Self {
        if !menu_open && launcher {
            Self::Launcher
        } else if !menu_open && game_info.is_some_and(|info| info.has_menu) {
            Self::RetroArch
        } else {
            Self::Framebuffer
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FramebufferOsdAction {
    Retire,
    Show,
}

fn send_launcher_osd(event: OsdEvent) -> std::io::Result<()> {
    #[cfg(unix)]
    { event.send() }
    #[cfg(not(unix))]
    { let _ = event; Err(std::io::ErrorKind::Unsupported.into()) }
}

async fn present_foreground_osd<F, Fut>(
    target: OsdTarget,
    event: OsdEvent,
    fraction: f32,
    send_launcher: impl FnOnce(OsdEvent) -> std::io::Result<()>,
    send_retroarch: F,
    mut framebuffer: impl FnMut(FramebufferOsdAction),
) where
    F: FnOnce(RetroArchCommand) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if target != OsdTarget::Framebuffer {
        // Join the fallback writer before either foreground renderer can present.
        framebuffer(FramebufferOsdAction::Retire);
    }
    if target != OsdTarget::Launcher {
        present_osd(target, event.kind, fraction, send_retroarch,
            || framebuffer(FramebufferOsdAction::Show)).await;
        return;
    }
    let result = send_launcher(event);
    let sent = result.is_ok();
    info!(target: "alliumd::osd", "osd launcher_msg kind={:?} percent={} seq={} sent={sent}",
        event.kind, event.percent, event.seq);
    if let Err(error) = result {
        debug!(target: "alliumd::osd", "launcher OSD send failed: {error}");
        framebuffer(FramebufferOsdAction::Show);
    }
}

async fn present_osd<F, Fut>(
    target: OsdTarget,
    kind: OsdKind,
    fraction: f32,
    send: F,
    show_framebuffer: impl FnOnce(),
) where
    F: FnOnce(RetroArchCommand) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if target == OsdTarget::Framebuffer {
        show_framebuffer();
        return;
    }
    let percent = (fraction.clamp(0.0, 1.0) * 100.0).round() as u32;
    let label = match kind {
        OsdKind::Volume => "Volume",
        OsdKind::Brightness => "Brightness",
    };
    let command = RetroArchCommand::ShowMsg(format!("{label} {percent}%"));
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), send(command))
        .await;
    let sent = match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            debug!(target: "alliumd::osd", "RetroArch OSD send failed: {error:#}");
            false
        }
        Err(error) => {
            debug!(target: "alliumd::osd", "RetroArch OSD send timed out: {error}");
            false
        }
    };
    info!(target: "alliumd::osd", "osd retroarch_msg kind={kind:?} percent={percent} sent={sent}");
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
#[path = "osd_regression_tests.rs"]
mod osd_regression_tests;

#[cfg(test)]
mod retroarch_osd_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[tokio::test]
    async fn launcher_volume_change_sends_one_message_without_starting_overlay() {
        let messages = RefCell::new(Vec::new());
        let overlay_starts = Cell::new(0);
        let target = OsdTarget::for_foreground(false, None, true);
        assert_eq!(target, OsdTarget::Launcher);
        present_foreground_osd(
            target, OsdEvent::new(OsdKind::Volume, 0.4, 1, 7), 0.4,
            |event| { messages.borrow_mut().push(event); Ok(()) },
            |_| async { panic!("launcher OSD must not send RetroArch commands") },
            |action| if action == FramebufferOsdAction::Show { overlay_starts.set(overlay_starts.get() + 1); },
        ).await;
        assert_eq!((messages.borrow().len(), overlay_starts.get()), (1, 0));
        assert_eq!(messages.borrow()[0], OsdEvent {
            kind: OsdKind::Volume, percent: 40, direction: 1, seq: 7,
        });
    }

    #[tokio::test]
    async fn missing_launcher_listener_falls_back_and_menu_keeps_framebuffer() {
        let overlay_starts = Cell::new(0);
        for _ in 0..2 {
            present_foreground_osd(
                OsdTarget::Launcher, OsdEvent::new(OsdKind::Brightness, 0.5, -1, 1), 0.5,
                |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                |_| async { panic!("launcher OSD must not send RetroArch commands") },
                |action| if action == FramebufferOsdAction::Show { overlay_starts.set(overlay_starts.get() + 1); },
            ).await;
        }
        assert_eq!(overlay_starts.get(), 2);
        assert_eq!(OsdTarget::for_foreground(true, None, true), OsdTarget::Framebuffer);
    }

    #[tokio::test]
    async fn retroarch_volume_change_sends_one_message_without_starting_overlay() {
        let info = GameInfo {
            has_menu: true,
            ..GameInfo::default()
        };
        let commands = RefCell::new(Vec::new());
        let overlay_starts = Cell::new(0);
        let mut state = AlliumDState::new();
        state.volume = 7;
        state.volume += 1;
        present_osd(
            OsdTarget::for_foreground(false, Some(&info), false),
            OsdKind::Volume,
            state.volume as f32 / MAX_VOLUME as f32,
            |command| {
                commands.borrow_mut().push(command);
                std::future::ready(Ok(()))
            },
            || overlay_starts.set(overlay_starts.get() + 1),
        )
        .await;
        let commands = commands.borrow();
        assert_eq!(commands.len(), 1);
        assert!(matches!(&commands[0], RetroArchCommand::ShowMsg(text) if text == "Volume 40%"));
        assert_eq!(overlay_starts.get(), 0);
    }

    #[tokio::test]
    async fn brightness_changes_each_send_new_message_with_plate_rounding() {
        let commands = RefCell::new(Vec::new());
        for fraction in [0.55, 0.60, 0.575, -0.1, 1.1] {
            present_osd(
                OsdTarget::RetroArch,
                OsdKind::Brightness,
                fraction,
                |command| {
                    commands.borrow_mut().push(command);
                    std::future::ready(Ok(()))
                },
                || panic!("game OSD must not start the framebuffer writer"),
            )
            .await;
        }
        let commands = commands.borrow();
        assert_eq!(commands.len(), 5);
        for (command, percent) in commands.iter().zip([55, 60, 58, 0, 100]) {
            assert!(matches!(command, RetroArchCommand::ShowMsg(text)
                if text == &format!("Brightness {percent}%")));
        }
    }

    #[tokio::test]
    async fn other_apps_and_daemon_menu_keep_framebuffer_osd() {
        let retroarch = GameInfo {
            has_menu: true,
            ..GameInfo::default()
        };
        let other_game = GameInfo::default();
        for (menu_open, info) in [
            (false, None),
            (false, Some(&other_game)),
            (true, Some(&retroarch)),
        ] {
            let overlay_starts = Cell::new(0);
            let target = OsdTarget::for_foreground(menu_open, info, false);
            assert_eq!(target, OsdTarget::Framebuffer);
            present_osd(
                target,
                OsdKind::Volume,
                0.4,
                |_| async { panic!("non-game OSD must not send RetroArch commands") },
                || overlay_starts.set(overlay_starts.get() + 1),
            )
            .await;
            assert_eq!(overlay_starts.get(), 1);
        }
    }

    #[tokio::test]
    async fn failed_message_send_does_not_fall_back_to_framebuffer() {
        let sends = Cell::new(0);
        present_osd(
            OsdTarget::RetroArch,
            OsdKind::Volume,
            0.4,
            |_| {
                sends.set(sends.get() + 1);
                std::future::ready(Err(anyhow::anyhow!("not listening")))
            },
            || panic!("failed game OSD must not start the framebuffer writer"),
        )
        .await;
        assert_eq!(sends.get(), 1);
    }

    #[tokio::test]
    async fn stalled_message_send_is_bounded_without_framebuffer_fallback() {
        let sends = Cell::new(0);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            present_osd(
                OsdTarget::RetroArch,
                OsdKind::Brightness,
                0.5,
                |_| {
                    sends.set(sends.get() + 1);
                    std::future::pending::<Result<()>>()
                },
                || panic!("stalled game OSD must not start the framebuffer writer"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(sends.get(), 1);
    }
}

#[allow(clippy::needless_pass_by_ref_mut)]
async fn terminate(child: &mut Child) -> Result<()> {
    #[cfg(unix)]
    signal(child, Signal::SIGTERM)?;
    #[cfg(not(unix))]
    child.kill().await?;

    #[cfg(unix)]
    if let Err(_e) = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
        signal(child, Signal::SIGKILL)?;
    }
    #[cfg(feature = "miyoo")]
    restore_launcher_boost();
    Ok(())
}

#[cfg(unix)]
fn signal(child: &Child, signal: Signal) -> Result<()> {
    if let Some(pid) = child.id() {
        let pid = Pid::from_raw(pid as i32);
        kill(pid, signal)?;
    }
    Ok(())
}

async fn charging_startup<F, Fut>(charging: bool, keep_awake: &Path, suspend: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if !charging {
        return Ok(());
    }
    if keep_awake.exists() {
        info!("/tmp/stay_awake exists, skipping charging suspend");
        return Ok(());
    }
    suspend().await
}

/// Disable a failed menu exactly once; return whether an open menu needs unpausing.
fn disable_menu(available: &mut bool, open: &mut bool) -> bool {
    *available = false;
    std::mem::take(open)
}

fn failed_menu_send(available: &mut bool, open: &mut bool, paused: bool) -> bool {
    disable_menu(available, open);
    paused
}

#[cfg(test)]
mod perf6_tests {
    use super::*;

    struct HeldBattery {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        release: mpsc::Receiver<()>,
    }
    impl Battery for HeldBattery {
        fn update(&mut self) -> Result<()> {
            self.started.take().unwrap().send(()).unwrap();
            self.release.recv()?;
            Ok(())
        }
        fn percentage(&self) -> i32 {
            4
        }
        fn charging(&self) -> bool {
            false
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn battery_helper_does_not_block_input_runtime_and_keeps_low_sample() {
        let (started, wait) = tokio::sync::oneshot::channel();
        let (release, hold) = mpsc::channel();
        let mut monitor = BatteryMonitor::new(HeldBattery {
            started: Some(started),
            release: hold,
        })
        .unwrap();
        monitor.request();
        tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(monitor.readings.try_recv().is_err());
        release.send(()).unwrap();
        let reading =
            tokio::time::timeout(std::time::Duration::from_secs(1), monitor.readings.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(reading.percentage(), 4);
        assert!(!reading.charging());
        assert!(reading.percentage() <= BATTERY_WARNING_THRESHOLD);
        assert!(reading.percentage() <= BATTERY_SHUTDOWN_THRESHOLD);
    }

    #[test]
    fn preheld_chord_cannot_arm_menu_tap() {
        let mut keys = EnumMap::default();
        assert!(menu_press_is_unmodified(&keys));
        keys[Key::Right] = true;
        assert!(!menu_press_is_unmodified(&keys));
    }

    #[test]
    fn release_metadata_parser_retains_live_slot_and_disc() {
        let info = parse_retroarch_info("GET_INFO 3 2 -1".into());
        assert_eq!(
            info,
            RetroArchInfo {
                max_disk_slots: 3,
                disk_slot: 2,
                state_slot: Some(-1)
            }
        );
    }
}

#[cfg(test)]
mod menu_channel_tests {
    use super::{disable_menu, failed_menu_send};

    #[tokio::test]
    async fn dropped_channel_is_disabled_and_timer_remains_polled() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        drop(tx);
        let mut available = true;
        let mut open = false;
        let mut closed_events = 0;
        loop {
            tokio::select! {
                completion = rx.recv(), if available => {
                    assert_eq!(completion, None);
                    closed_events += 1;
                    assert!(!disable_menu(&mut available, &mut open));
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => break,
            }
        }
        assert_eq!(closed_events, 1);
    }

    #[tokio::test]
    async fn valid_completion_survives_channel_then_close() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(()).unwrap();
        drop(tx);
        assert_eq!(rx.recv().await, Some(()));
        assert_eq!(rx.recv().await, None);
        let mut available = true;
        let mut open = true;
        assert!(disable_menu(&mut available, &mut open));
        assert!(!available && !open);
        assert!(!disable_menu(&mut available, &mut open));
    }

    #[test]
    fn failed_send_race_clears_open_and_unpauses_only_actual_pause() {
        for paused in [false, true] {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            drop(rx);
            let mut available = true;
            let mut open = true;
            assert!(tx.send(()).is_err());
            let unpause_count = usize::from(failed_menu_send(&mut available, &mut open, paused));
            assert_eq!(unpause_count, if paused { 1 } else { 0 });
            assert!(!available && !open);
            // Later closed-channel notification must never issue a second unpause.
            assert!(!disable_menu(&mut available, &mut open));
        }
    }
}

#[cfg(test)]
mod charging_startup_tests {
    use super::charging_startup;
    use std::cell::Cell;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("allium-charging-{}-{name}", std::process::id()));
        assert!(!path.exists());
        path
    }

    #[tokio::test]
    async fn owned_awake_marker_prevents_charging_side_effect_dispatch() {
        let marker = fixture("protected");
        std::fs::write(&marker, b"").unwrap();
        let before = std::fs::metadata(&marker).unwrap();
        let dispatched = Cell::new(0);
        charging_startup(true, &marker, || async {
            dispatched.set(dispatched.get() + 1);
            anyhow::bail!("charging suspend must not run while protected")
        })
        .await
        .unwrap();
        assert_eq!(dispatched.get(), 0);
        let after = std::fs::metadata(&marker).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(std::fs::read(&marker).unwrap(), b"");
        std::fs::remove_file(marker).unwrap();
    }

    #[tokio::test]
    async fn ordinary_charging_keeps_one_suspend_dispatch_and_error_propagation() {
        let marker = fixture("ordinary");
        let dispatched = Cell::new(0);
        let result = charging_startup(true, &marker, || async {
            dispatched.set(dispatched.get() + 1);
            anyhow::bail!("original charging error")
        })
        .await;
        assert_eq!(dispatched.get(), 1);
        assert_eq!(result.unwrap_err().to_string(), "original charging error");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn noncharging_does_not_dispatch_or_create_keep_awake_marker() {
        let marker = fixture("noncharging");
        let dispatched = Cell::new(0);
        charging_startup(false, &marker, || async {
            dispatched.set(dispatched.get() + 1);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(dispatched.get(), 0);
        assert!(!marker.exists());
    }
}

#[cfg(test)]
mod theme_settings_tests {
    use common::theme::{Boundary, ThemeRuntime};
    #[test]
    fn daemon_menu_and_osd_resolve_launcher_settings_digest() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let state = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("alliumd-settings-{}", std::process::id()));
        std::fs::create_dir(&state).unwrap();
        let bind = || {
            ThemeRuntime::bind(
                &state,
                &repo.join("themes"),
                &repo.join("static/.allium/fonts"),
            )
            .unwrap()
        };
        let mut launcher = bind();
        launcher.choose("compact-plus").unwrap();
        let mut daemon = bind();
        for shape in ["square", "tall"] {
            launcher.choose_setting("cover_shape", shape).unwrap();
            for color in ["orange", "big-picture", "white", "black"] {
                let active = launcher
                    .choose_setting("color_scheme", color)
                    .unwrap()
                    .snapshot
                    .unwrap();
                launcher.flush_settings().unwrap();
                for boundary in [Boundary::MenuOpening, Boundary::OsdOpening] {
                    let loaded = daemon.activate(boundary);
                    assert!(loaded.status.diagnostic.is_none());
                    let loaded = loaded.snapshot.unwrap();
                    assert_eq!(loaded.digest(), active.digest());
                    assert_eq!(loaded.generation(), active.generation());
                    assert_eq!(loaded.settings(), active.settings());
                }
            }
        }
        std::fs::remove_dir_all(state).unwrap();
    }
}

#[cfg(feature = "miyoo")]
fn pause_launcher_boost() -> Option<common::platform::cpu_tuning::InputBoostPause> {
    match common::platform::cpu_tuning::pause_input_boost() {
        Ok(pause) => Some(pause),
        Err(error) => {
            warn!("Cannot inhibit launcher CPU boost: {error}");
            None
        }
    }
}

#[cfg(feature = "miyoo")]
fn restore_launcher_boost() {
    if let Err(error) = common::platform::cpu_tuning::restore_boost_lease() {
        warn!("Cannot recover launcher CPU boost: {error}");
    }
}
