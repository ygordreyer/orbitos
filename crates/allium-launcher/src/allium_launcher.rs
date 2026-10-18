use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::process;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use anyhow::Result;
use common::command::Command;
use common::constants::{ALLIUM_GAMES_DIR, ALLIUM_SD_ROOT, ALLIUM_THEMES_DIR};
use common::display::color::Color;
use common::geom;
use common::locale::{Locale, LocaleSettings};
use common::resources::Resources;
use common::view::View;
use enum_map::EnumMap;
use log::{debug, error, info, trace, warn};

use common::database::Database;
use common::display::Display;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::stylesheet::Stylesheet;
use type_map::TypeMap;

use crate::consoles::ConsoleMapper;
use crate::entry::directory::Directory;
use crate::entry::game::Game;
use crate::view::{App, Toast};

/// One-event lookahead preserves release/action boundaries while draining repeats.
#[derive(Default)]
pub(crate) struct PendingInput(Option<KeyEvent>);

impl PendingInput {
    /// NAV can pass `|| pending.is_ready(platform)` into bounded idle preparation.
    /// A ready event stays buffered for the next foreground iteration.
    pub(crate) fn is_ready(&mut self, platform: &mut DefaultPlatform) -> bool {
        if self.0.is_none() {
            let mut future = std::pin::pin!(poll_platform_input(platform));
            if let Poll::Ready(event) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                self.0 = Some(event);
            }
        }
        self.0.is_some()
    }

    fn take(&mut self, platform: &mut DefaultPlatform) -> Option<KeyEvent> {
        self.is_ready(platform);
        self.0.take()
    }
}

fn navigation_key(event: KeyEvent) -> Option<Key> {
    match event {
        KeyEvent::Pressed(key) | KeyEvent::Autorepeat(key)
            if matches!(key, Key::Up | Key::Down | Key::Left | Key::Right) =>
        {
            Some(key)
        }
        _ => None,
    }
}

#[cfg(any(feature = "miyoo", test))]
fn boosts_cpu(event: KeyEvent) -> bool {
    matches!(event, KeyEvent::Pressed(_) | KeyEvent::Autorepeat(_))
}

async fn poll_platform_input(platform: &mut DefaultPlatform) -> KeyEvent {
    #[cfg(all(
        test,
        feature = "perf-bench",
        not(any(feature = "miyoo", feature = "simulator"))
    ))]
    if perf_bench::has_loop_input() {
        return perf_bench::poll_loop_input().await;
    }
    #[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
    if navigation_warm_tests::has_input() {
        return navigation_warm_tests::poll_input().await;
    }
    platform.poll().await
}

#[derive(Debug)]
pub struct AlliumLauncher<P: Platform> {
    platform: P,
    display: P::Display,
    res: Resources,
    view: App<P::Battery>,
    toast: Option<Toast>,
    toast_deadline: Option<Instant>,
    first_flush: bool,
    display_revision: u64,
    osd: crate::osd::LauncherOsd,
}

impl AlliumLauncher<DefaultPlatform> {
    #[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
    pub(crate) fn proof_frame(&self) -> tiny_skia::Pixmap {
        self.display.pixmap().to_owned()
    }

    pub fn new(mut platform: DefaultPlatform) -> Result<Self> {
        common::perf_trace::lifecycle(common::perf_trace::LAUNCHER_ENTRY);
        let display = platform.display()?;
        if let Err(error) = crate::network_runtime::initialize(ALLIUM_SD_ROOT.to_path_buf()) {
            warn!(
                "Network library startup failed; configured PlayStation launch is blocked: {error}"
            );
        }
        let battery = platform.battery()?;

        let mut console_mapper = ConsoleMapper::new();
        console_mapper.load_config()?;

        let mut res = TypeMap::new();
        let database = Database::new()?;
        if let Err(error) = crate::media_refresh::consume(
            &ALLIUM_SD_ROOT,
            &database,
            &console_mapper,
            &Locale::new(&LocaleSettings::load()?.lang),
        ) {
            warn!("Media refresh will be retried: {error:#}");
        }
        crate::network_runtime::refresh_history(&database)?;
        res.insert(database);
        res.insert(console_mapper);
        res.insert(Stylesheet::load()?);
        res.insert(Locale::new(&LocaleSettings::load()?.lang));
        res.insert(Into::<geom::Size>::into(display.size()));
        let res = Resources::new(res);
        crate::theme_adapter::initialize(&res);

        let view = App::load_or_new(display.bounding_box(), res.clone(), battery)?;

        Ok(AlliumLauncher {
            platform,
            display,
            res,
            view,
            toast: None,
            toast_deadline: None,
            first_flush: true,
            display_revision: 0,
            osd: crate::osd::LauncherOsd::default(),
        })
    }

    pub async fn run_event_loop(&mut self) -> Result<()> {
        let _trace_flush = common::perf_trace::FlushPendingOnDrop;
        #[cfg(unix)]
        let mut osd_socket = crate::osd::bind().map_err(|error|
            warn!("Launcher OSD socket unavailable: {error}")).ok();
        {
            let styles = self.res.get::<Stylesheet>().display_style();

            if let Some(wallpaper) = styles.wallpaper.as_deref() {
                let path = resolve_wallpaper_path(wallpaper);
                if let Err(e) = set_wallpaper(&mut self.display, &path) {
                    error!("Failed to set wallpaper: {}", e);
                }
            }

            self.display.clear(styles.ui.background_color)?;
        }

        self.display.save()?;

        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);

        let mut keys: EnumMap<Key, bool> = EnumMap::default();

        let mut pending_input = PendingInput::default();
        let mut input_commands_pending = false;
        let mut update_deadline = Instant::now();
        let mut animation_active = false;
        let mut last_frame = Instant::now();
        let mut overlay_was_drawn = true;
        let mut warm_steps = 0;
        let mut warm_work = None;
        let mut media_pending = true;
        let mut last_media_step = Instant::now();
        let mut name_generation = common::media::name_generation();
        let mut media_poll = Instant::now();
        loop {
            crate::network_runtime::trace_loop("loop_iteration", "before_poll");
            #[cfg(all(
                test,
                feature = "perf-bench",
                not(any(feature = "miyoo", feature = "simulator"))
            ))]
            perf_bench::loop_iteration();
            // Preserve commands from the preceding input action as its boundary.
            // Unrelated background commands must not overtake a ready input edge.
            let event = if input_commands_pending && !rx.is_empty() {
                None
            } else {
                pending_input.take(&mut self.platform)
            };
            let input_handled = event.is_some();
            if let Some(event) = event {
                self.apply_input_batch(event, &mut pending_input, &mut keys, &tx, &rx)
                    .await?;
                input_commands_pending = !rx.is_empty();
                media_pending = true;
                warm_work = None;
                warm_steps = 0;
            } else if let Ok(command) = rx.try_recv() {
                media_pending = true;
                warm_work = None;
                warm_steps = 0;
                self.handle_command(command).await?;
                input_commands_pending &= !rx.is_empty();
            }
            let network_job_active = crate::network_runtime::active();
            let network_dirty = crate::network_runtime::poll();
            if network_job_active && !crate::network_runtime::active() {
                crate::view::details::invalidate_metadata();
            }
            if network_dirty {
                self.view.set_should_draw();
                media_pending = true;
            }
            // Rendering can be slower than autorepeat: waiting for an input gap would
            // starve completion adoption for the entire hold. Poll once per bounded batch.
            let mut media_deferred = false;
            if input_handled
                && media_pending
                && rx.is_empty()
                && keys.values().any(|pressed| *pressed)
                && last_media_step.elapsed() >= Duration::from_millis(10)
            {
                media_deferred = self.toast.is_none() && !network_dirty && !overlay_was_drawn
                    && self.view.defer_media_step();
                if !media_deferred {
                    self.poll_media_step(&mut media_pending);
                    last_media_step = Instant::now();
                }
            }
            let dt = last_frame.elapsed();
            let update_due = Instant::now() >= update_deadline;
            let was_dirty = self.view.should_draw();
            self.update_view(dt, &mut name_generation, &mut media_pending);
            if update_due && !was_dirty {
                animation_active = self.view.should_draw();
            }
            last_frame = Instant::now();
            if last_frame >= update_deadline || animation_active {
                // Re-arm from now, never catch up missed frames after a long render.
                update_deadline = last_frame
                    + if animation_active {
                        common::constants::UI_FRAME_INTERVAL
                    } else {
                        Duration::from_secs(1)
                    };
            }

            self.present_frame(network_dirty, &mut overlay_was_drawn)
                .await?;
            if media_deferred {
                self.poll_media_step(&mut media_pending);
                last_media_step = Instant::now();
            }
            let mut handled_action = false;
            while let Some(action) = crate::network_runtime::action() {
                handled_action = true;
                crate::network_runtime::refresh_history(&self.res.get::<Database>())?;
                let result = (|| -> Result<Option<Command>> {
                    #[cfg(test)]
                    if crate::network_runtime::test_loop_event("resolve") {
                        return Ok(Some(Command::Exec(process::Command::new(
                            "/allium-test-intercept-only",
                        ))));
                    }
                    Ok(match action {
                        crate::network_runtime::Action::Launch(mut game, reset) => {
                            if !reset
                                && let Some(playlist) = crate::discs::discover(&game.path)?
                                && playlist.discs.len() > 1
                            {
                                crate::network_runtime::open_discs(game, playlist);
                                None
                            } else {
                                self.res.get::<ConsoleMapper>().launch_game(
                                    &self.res.get(),
                                    &mut game,
                                    reset,
                                )?
                            }
                        }
                        crate::network_runtime::Action::Disc(mut game, playlist, index) => self
                            .res
                            .get::<ConsoleMapper>()
                            .launch_game_with_disc(&self.res.get(), &mut game, &playlist, index)?,
                    })
                })();
                match result {
                    Ok(Some(command)) => {
                        crate::network_runtime::finish_action();
                        self.handle_command(command).await?;
                    }
                    Ok(None) => {}
                    Err(error) => crate::network_runtime::failure(error.to_string()),
                }
            }
            if handled_action {
                media_pending = true;
                warm_work = None;
                warm_steps = 0;
                continue;
            }
            let warm_allowed = self.navigation_warm_allowed()
                && !animation_active
                && keys.values().all(|pressed| !pressed)
                && rx.is_empty()
                && !pending_input.is_ready(&mut self.platform);
            if common::perf_trace::recording() {
                common::perf_trace::neighbor_warm(if animation_active { "animation" }
                    else if self.toast.is_some() { "toast" }
                    else if crate::network_runtime::blocks_navigation_warm() { "network_blocked" }
                    else if !self.view.navigation_warm_ready() { "view_not_ready" }
                    else if keys.values().any(|pressed| *pressed) { "held_key" }
                    else if !rx.is_empty() { "pending_command" }
                    else if !warm_allowed { "pending_input" } else { "allowed" });
            }
            if animation_active || self.toast.is_some() || crate::network_runtime::blocks_navigation_warm() {
                self.view.invalidate_neighbors_for(if animation_active { "invalidated:animation" }
                    else if self.toast.is_some() { "invalidated:toast" } else { "invalidated:network_blocked" });
            }
            if !warm_allowed {
                warm_work = None;
                warm_steps = 0;
            }

            #[cfg(all(
                test,
                feature = "perf-bench",
                not(any(feature = "miyoo", feature = "simulator"))
            ))]
            if !media_pending
                && warm_work.is_none()
                && (!warm_allowed || warm_steps >= 2)
                && keys.values().all(|pressed| !pressed)
                && !crate::media_refresh::pending()
            {
                perf_bench::loop_idle();
            }
            #[cfg(test)]
            crate::network_runtime::test_loop_event("wait");
            // Lookahead can contain a boundary event. Never sleep with buffered input.
            if pending_input.is_ready(&mut self.platform) {
                continue;
            }
            common::perf_trace::flush_pending();
            let view_deadline = self
                .toast_deadline
                .map_or(update_deadline, |toast| toast.min(update_deadline));
            let osd_deadline = self.osd.deadline();
            #[cfg(unix)]
            tokio::select! {
                biased;
                wake = crate::osd::wait(&mut osd_socket, osd_deadline) => {
                    self.handle_osd_wake(wake);
                }
                event = self.poll_input() => {
                    crate::network_runtime::trace_wake("input");
                    pending_input.0 = Some(event);
                }
                _ = sigterm.recv() => {
                    crate::network_runtime::trace_wake("signal");
                    self.handle_command(Command::Exit).await?;
                }
                cmd = rx.recv() => {
                    crate::network_runtime::trace_wake("command");
                    if let Some(cmd) = cmd {
                        media_pending = true;
                        warm_work = None;
                        warm_steps = 0;
                        self.handle_command(cmd).await?;
                    }
                }
                _ = crate::view::orbit::art_ready() => {
                    crate::network_runtime::trace_wake("art_completion");
                    self.poll_media_step(&mut media_pending);
                    last_media_step = Instant::now();
                }
                _ = tokio::time::sleep_until(view_deadline.into()) => {
                    crate::network_runtime::trace_wake("update_deadline");
                }
                _ = tokio::time::sleep(Duration::from_millis(33)), if crate::network_runtime::active() => {
                    crate::network_runtime::trace_wake("network_timer");
                }
                _ = tokio::time::sleep(if warm_work.is_some() {
                    std::time::Duration::from_millis(1)
                } else if media_pending || (warm_allowed && warm_steps < 2) || crate::media_refresh::pending() {
                    std::time::Duration::from_millis(10)
                } else {
                    if keys.values().any(|pressed| *pressed) {
                        Duration::from_secs(1)
                    } else {
                        Duration::from_secs(1).saturating_sub(media_poll.elapsed())
                    }
                }), if rx.is_empty() => {
                    if pending_input.is_ready(&mut self.platform) { continue; }
                    if rx.is_empty() && keys.values().all(|pressed| !pressed) {
                        self.view.idle_retire_neighbors();
                    }
                    if media_pending {
                        self.poll_media_step(&mut media_pending);
                        last_media_step = Instant::now();
                        if media_pending {continue;}
                    }
                    if pending_input.is_ready(&mut self.platform) { continue; }
                    if warm_allowed {
                        self.view.idle_navigation_step(&mut self.display, &mut warm_work, &mut warm_steps,
                            || pending_input.is_ready(&mut self.platform))?;
                    }
                    if pending_input.is_ready(&mut self.platform) { continue; }
                    if keys.values().all(|pressed| !pressed) && media_poll.elapsed()>=std::time::Duration::from_secs(1) {
                        let marker = if crate::media_refresh::pending() { Ok(Some(true)) } else { crate::media_refresh::marker_ready(&ALLIUM_SD_ROOT) };
                        match marker {
                            Ok(None) => { media_poll=Instant::now(); continue; },
                            Ok(Some(false)) => { media_poll=Instant::now(); continue; },
                            Err(error) => { warn!("Media marker: {error:#}"); media_poll=Instant::now(); continue; },
                            Ok(Some(true)) => {},
                        }
                        match crate::media_refresh::consume(&ALLIUM_SD_ROOT,&self.res.get::<Database>(),&self.res.get::<ConsoleMapper>(),&self.res.get::<Locale>()) {
                            Ok(true)=>{
                                self.view.save()?;
                                self.view=App::load_or_new(self.display.bounding_box(),self.res.clone(),self.platform.battery()?)?;
                                warm_work=None;warm_steps=0;overlay_was_drawn=true;
                                media_pending=true;
                                media_poll=Instant::now();
                            },
                            Ok(false)=>{if !crate::media_refresh::pending() {media_poll=Instant::now();}},
                            Err(error)=>{warn!("Media refresh: {error:#}");media_poll=Instant::now();},
                        }
                    }
                }
                else => { crate::network_runtime::trace_wake("other"); }
            }

            #[cfg(not(unix))]
            tokio::select! {
                event = self.poll_input() => {
                    crate::network_runtime::trace_wake("input");
                    let _phase = common::perf_trace::input(event);
                    let mut bubble = VecDeque::new();
                    self.view.handle_key_event(event, tx.clone(), &mut bubble).await?;
                }
                else => { crate::network_runtime::trace_wake("other"); }
            }
        }
    }

    fn poll_media_step(&mut self, media_pending: &mut bool) {
        *media_pending = self.view.idle_media_step();
        #[cfg(all(
            test,
            feature = "perf-bench",
            not(any(feature = "miyoo", feature = "simulator"))
        ))]
        perf_bench::loop_media_poll(*media_pending);
    }

    fn update_view(
        &mut self,
        dt: std::time::Duration,
        name_generation: &mut u64,
        media_pending: &mut bool,
    ) {
        self.view.update(dt);
        let observed = common::media::name_generation();
        if observed != *name_generation {
            *name_generation = observed;
            *media_pending = true;
            self.view.set_should_draw();
        }
    }

    fn navigation_warm_allowed(&self) -> bool {
        self.toast.is_none()
            && !self.osd.is_active()
            && !crate::network_runtime::blocks_navigation_warm()
            && self.view.navigation_warm_ready()
    }

    async fn poll_input(&mut self) -> KeyEvent {
        poll_platform_input(&mut self.platform).await
    }

    async fn apply_input_batch(
        &mut self,
        first: KeyEvent,
        pending: &mut PendingInput,
        keys: &mut EnumMap<Key, bool>,
        tx: &tokio::sync::mpsc::Sender<Command>,
        rx: &tokio::sync::mpsc::Receiver<Command>,
    ) -> Result<usize> {
        let trace_input = !matches!(first,
            KeyEvent::Released(Key::L | Key::R | Key::Left | Key::Right | Key::Up | Key::Down)
        );
        #[cfg(feature = "miyoo")]
        let _phase = if boosts_cpu(first) {
            Some(common::perf_trace::input_with_boost(first, common::platform::cpu_tuning::boost_input))
        } else {
            trace_input.then(|| common::perf_trace::input(first))
        };
        #[cfg(not(feature = "miyoo"))]
        let _phase = trace_input.then(|| common::perf_trace::input(first));
        let repeat_key = match first {
            KeyEvent::Autorepeat(_) => navigation_key(first),
            _ => None,
        };
        let mut event = first;
        let mut count = 0;
        let mut bubble = VecDeque::new();
        loop {
            match event {
                KeyEvent::Pressed(key) => keys[key] = true,
                KeyEvent::Released(key) => keys[key] = false,
                KeyEvent::Autorepeat(_) => {}
            }
            #[cfg(feature = "miyoo")]
            if count != 0 && boosts_cpu(event) {
                common::platform::cpu_tuning::boost_input();
            }
            #[cfg(all(
                test,
                feature = "perf-bench",
                not(any(feature = "miyoo", feature = "simulator"))
            ))]
            perf_bench::loop_input(keys);
            if !keys[Key::Menu] && !matches!(event, KeyEvent::Released(Key::Menu)) {
                if !crate::network_runtime::event(event)? {
                    self.view
                        .handle_key_event(event, tx.clone(), &mut bubble)
                        .await?;
                }
                #[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
                navigation_warm_tests::handled_input();
            }
            count += 1;
            // Bound a continuously arriving repeat stream so completed media still gets adopted.
            if repeat_key.is_none() || count == 64 || !rx.is_empty() {
                break;
            }
            let Some(next) = pending.take(&mut self.platform) else {
                break;
            };
            if matches!(next, KeyEvent::Autorepeat(key) if Some(key) == repeat_key) {
                event = next;
            } else {
                pending.0 = Some(next);
                break;
            }
        }
        Ok(count)
    }

    #[cfg(unix)]
    fn handle_osd_wake(&mut self, wake: crate::osd::Wake) {
        match wake {
            crate::osd::Wake::Event(event) => self.receive_osd(event),
            crate::osd::Wake::Expired => self.osd.expire(Instant::now()),
        }
    }

    fn receive_osd(&mut self, event: common::osd::OsdEvent) {
        let title = self.res.get::<Locale>().t(event.kind.title_key(event.direction));
        let styles = self.res.get::<Stylesheet>().display_style();
        if let Err(error) = self.osd.receive(event, Instant::now(),
            crate::theme_adapter::snapshot(&self.res), &title, &styles, &self.display) {
            warn!("Launcher OSD preparation failed: {error:#}");
        }
    }

    async fn present_frame(
        &mut self,
        network_dirty: bool,
        overlay_was_drawn: &mut bool,
    ) -> Result<()> {
        self.osd.expire(Instant::now());
        let osd_dirty = self.osd.take_dirty();
        let osd_active = self.osd.is_active();
        if self.toast.as_ref().is_some_and(Toast::has_expired) {
            self.toast = None;
            self.toast_deadline = None;
            self.handle_command(Command::Redraw).await?;
        }
        if self.toast.is_some() || network_dirty || *overlay_was_drawn
            || osd_dirty || (osd_active && self.view.should_draw()) {
            self.view.force_full_present();
        }
        let revision = self.display.composition_revision()?;
        if revision != self.display_revision {
            self.view.force_full_present();
            if common::perf_trace::recording() {
                common::perf_trace::neighbor_build("invalidated:display_contents_or_layout");
            }
            self.display_revision = revision;
        }
        common::perf_trace::begin_render();
        let component_phase = self
            .view
            .should_draw()
            .then(|| common::perf_trace::phase(common::perf_trace::Phase::ComponentPrep));
        if self.view.should_draw() {
            if let Err(error) = self.view.prepare_theme() {
                warn!("Theme presentation unavailable; using existing controller view: {error}");
                self.view.theme_fallback()?;
            }
        }
        drop(component_phase);
        let compose_phase = (self.view.should_draw() || self.toast.is_some() || network_dirty)
            .then(|| common::perf_trace::phase(common::perf_trace::Phase::Compose));
        let drawing_style = std::cell::OnceCell::new();
        let styles = || drawing_style.get_or_init(|| crate::theme_adapter::drawing_style(
            &self.res.get::<Stylesheet>(),
            crate::theme_adapter::snapshot(&self.res).as_ref(),
        ));
        let mut drawn = if self.view.should_draw() {
            let _stage = common::perf_trace::stage(common::perf_trace::Stage::ComposeDraw);
            match self.view.draw_prepared_theme(&mut self.display)? {
                Some(drawn) => drawn,
                None => self.view.draw(&mut self.display, styles())?,
            }
        } else { false };

        if let Some(toast) = self.toast.as_mut() {
            drawn |= toast.draw(&mut self.display, styles())?;
        }

        let network_drawn = if network_dirty || drawn {
            let _stage = common::perf_trace::stage(common::perf_trace::Stage::NetworkOverlay);
            crate::network_runtime::draw_with_style(&mut self.display, styles)
        } else { false };
        drawn |= network_drawn;
        *overlay_was_drawn = network_drawn || self.toast.is_some();
        let present_damage = self.view.take_present_damage();
        common::perf_trace::damage(
            present_damage
                .as_deref()
                .unwrap_or(&[self.display.bounding_box()]),
        );
        drop(compose_phase);
        let composition_changed = (drawn || network_dirty)
            && (network_dirty
                || *overlay_was_drawn
                || present_damage
                    .as_ref()
                    .is_none_or(|damage| !damage.is_empty()));
        let display_repair = !composition_changed && self.display.needs_present()?;
        // Page-history repair copies the unchanged logical pixmap, preserving neighbor damage.
        if composition_changed || display_repair {
            let present_damage = if display_repair {
                Some(present_damage.unwrap_or_default())
            } else {
                present_damage
            };
            crate::network_runtime::trace_loop("frame_drawn", "compose");
            {
                let _stage = common::perf_trace::stage(common::perf_trace::Stage::Present);
                let _phase = common::perf_trace::phase(common::perf_trace::Phase::Flush);
                if let Some(damage) =
                    present_damage.filter(|_| !*overlay_was_drawn && !network_dirty && !osd_active && !osd_dirty)
                {
                    common::perf_trace::present_reason("partial");
                    self.view.flush_prepared_damage(&mut self.display, &damage)?;
                } else {
                    if common::perf_trace::recording() { common::perf_trace::packed_span_reason("full_present_or_overlay"); }
                    common::perf_trace::present_reason(if network_dirty {
                        "network_dirty"
                    } else if *overlay_was_drawn {
                        "overlay"
                    } else {
                        "view_full"
                    });
                    self.osd.present(&mut self.display)?;
                }
                crate::network_runtime::frame_presented();
            }
            common::perf_trace::presented(common::perf_trace::generation(), self.view.content_complete());
            if self.first_flush {
                self.first_flush = false;
                common::perf_trace::lifecycle(common::perf_trace::FIRST_COMPLETED_FLUSH);
            }
            common::perf_trace::drawn_frame();
            #[cfg(all(
                test,
                feature = "perf-bench",
                not(any(feature = "miyoo", feature = "simulator"))
            ))]
            perf_bench::loop_present();
        } else if common::perf_trace::recording() && self.view.content_complete() {
            common::perf_trace::settled(common::perf_trace::generation());
        }

        common::perf_trace::end_render();
        Ok(())
    }

    fn restore_launcher(&mut self) -> Result<()> {
        self.display.load(self.display.bounding_box())?;
        self.view.set_should_draw();
        Ok(())
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        self.view.prepare_command(&command);
        #[cfg(test)]
        if matches!(&command, Command::Exec(_))
            && crate::network_runtime::test_loop_event("dispatch")
        {
            return Ok(());
        }
        match command {
            Command::Exit => {
                #[cfg(feature = "miyoo")]
                common::platform::cpu_tuning::restore_input_boost();
                info!("goodbye from allium launcher");
                self.view.save()?;
                self.view.stop_workers();
                common::perf_trace::lifecycle(common::perf_trace::WORKER_SHUTDOWN);
                self.display.clear(Color::new(0, 0, 0))?;
                self.display.flush()?;
                process::exit(0);
            }
            #[allow(unused_mut)]
            Command::Exec(mut cmd) => {
                common::perf_trace::lifecycle(common::perf_trace::LAUNCH_PREP);
                info!("executing command: {:?}", cmd);
                self.view.save()?;
                self.view.stop_workers();
                common::perf_trace::lifecycle(common::perf_trace::WORKER_SHUTDOWN);
                self.display.clear(Color::new(0, 0, 0))?;
                self.display.flush()?;
                #[cfg(feature = "miyoo")]
                {
                    use std::os::unix::process::CommandExt;
                    common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Game);
                    common::perf_trace::lifecycle(common::perf_trace::IMMINENT_EXEC);
                    let _ = cmd.exec();
                    common::platform::cpu_tuning::set(
                        common::platform::cpu_tuning::Workload::Frontend,
                    );
                }
                #[cfg(not(feature = "miyoo"))]
                {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::CommandExt;
                        common::perf_trace::lifecycle(common::perf_trace::IMMINENT_EXEC);
                        let _ = process::Command::new("/bin/sh")
                            .arg("-c")
                            .arg("make simulator bin=allium-menu")
                            .exec();
                    }

                    #[cfg(not(unix))]
                    process::exit(0);
                }
                self.restore_launcher()?;
            }
            Command::ReloadStylesheet(styles, needs_relayout) => {
                crate::view::details::invalidate_metadata();
                debug!("reloading stylesheet");

                let display_styles = styles.display_style();
                if let Some(wallpaper) = display_styles.wallpaper.as_deref() {
                    let path = resolve_wallpaper_path(wallpaper);
                    if let Err(e) = set_wallpaper(&mut self.display, &path) {
                        error!("Failed to set wallpaper: {}", e);
                    }
                }
                self.display.clear(display_styles.ui.background_color)?;
                self.display.save()?;

                self.res.insert(*styles);
                self.res.insert(Locale::new(&LocaleSettings::load()?.lang));

                if needs_relayout {
                    self.view.save()?;
                    self.view.stop_workers();
                common::perf_trace::lifecycle(common::perf_trace::WORKER_SHUTDOWN);
                    self.view = App::load_or_new(
                        self.display.bounding_box(),
                        self.res.clone(),
                        self.platform.battery()?,
                    )?;
                } else {
                    self.view.set_should_draw();
                }
            }
            Command::SaveDisplaySettings(mut settings) => {
                debug!("saving display settings");
                self.platform.set_display_settings(&mut settings)?;
                settings.save()?;
            }
            Command::SaveLocaleSettings(settings) => {
                debug!("saving locale settings");
                settings.save()?;
                self.res.insert(Locale::new(&settings.lang));
                self.view.save()?;
                self.view.stop_workers();
                common::perf_trace::lifecycle(common::perf_trace::WORKER_SHUTDOWN);
                self.view = App::load_or_new(
                    self.display.bounding_box(),
                    self.res.clone(),
                    self.platform.battery()?,
                )?;
            }
            Command::ClockChanged => self.view.set_should_draw(),
            Command::Redraw => {
                trace!("redrawing");
                self.restore_launcher()?;
            }
            Command::StartSearch => {
                debug!("starting search");
                self.view.start_search();
            }
            Command::Search(query) => {
                debug!("searching");
                self.view.search(query)?;
            }
            Command::Toast(text, duration) => {
                debug!("showing toast: {:?}", text);
                self.toast = Some(Toast::new(text, duration));
                self.toast_deadline = duration.map(|duration| Instant::now() + duration);
            }
            Command::ImageToast(image, text, duration) => {
                debug!("showing image toast: {:?}", text);
                self.toast = Some(Toast::with_image(image, text, duration));
                self.toast_deadline = duration.map(|duration| Instant::now() + duration);
            }
            Command::DismissToast => {
                debug!("dismissing toast");
                self.toast = None;
                self.toast_deadline = None;
                self.restore_launcher()?;
            }
            Command::PopulateDb => {
                crate::view::details::invalidate_metadata();
                #[cfg(feature = "miyoo")]
                {
                    std::process::Command::new("show")
                        .arg("--clear")
                        .spawn()?
                        .wait()?;
                    std::process::Command::new("say")
                        .arg(self.res.get::<Locale>().t("populating-database"))
                        .spawn()?
                        .wait()?;
                }

                let mut queue = VecDeque::with_capacity(10);
                queue.push_back(Directory::new(ALLIUM_GAMES_DIR.clone()));

                let database = self.res.get::<Database>();
                let console_mapper = self.res.get::<ConsoleMapper>();

                database.delete_all_directories()?;
                database.delete_all_unplayed_games()?;

                let mut games = database.select_all_games()?;
                for game in games.iter_mut() {
                    if let Some(old) = Game::resync(&mut game.path)? {
                        if let Err(e) = database.update_game_path(&old, &game.path) {
                            warn!("failed to update game path: {}", e);
                        }
                    } else if !game.path.exists() {
                        database.delete_game(&game.path)?;
                    }
                }

                while let Some(dir) = queue.pop_front() {
                    #[cfg(feature = "miyoo")]
                    {
                        std::process::Command::new("show")
                            .arg("--clear")
                            .spawn()?
                            .wait()?;
                        let mut map = std::collections::HashMap::new();
                        map.insert(
                            "directory".into(),
                            dir.path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into(),
                        );
                        std::process::Command::new("say")
                            .arg(self.res.get::<Locale>().ta("populating-games", &map))
                            .spawn()?
                            .wait()?;
                    }
                    dir.populate_db(&mut queue, &database, &console_mapper, &self.res.get())?;
                }

                database.set_has_indexed(true)?;

                self.view.save()?;
                self.view.stop_workers();
                common::perf_trace::lifecycle(common::perf_trace::WORKER_SHUTDOWN);
                self.view = App::load_or_new(
                    self.display.bounding_box(),
                    self.res.clone(),
                    self.platform.battery()?,
                )?;
            }
            command => {
                warn!("unhandled command: {:?}", command);
            }
        }
        Ok(())
    }
}

fn resolve_wallpaper_path(wallpaper: &Path) -> std::path::PathBuf {
    // If wallpaper path is absolute, use it as-is
    if wallpaper.is_absolute() {
        return wallpaper.to_path_buf();
    }

    // Load the current theme and check if wallpaper exists in theme directory
    let theme = common::stylesheet::Theme::load();
    let theme_wallpaper = ALLIUM_THEMES_DIR.join(&theme.0).join(wallpaper);
    if theme_wallpaper.exists() {
        return theme_wallpaper;
    }

    // Fall back to SD root
    ALLIUM_SD_ROOT.join(wallpaper)
}

fn set_wallpaper(display: &mut impl Display, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let rect = display.size();

    let image = ::image::open(path)?;
    let image = image.resize_to_fill(rect.w, rect.h, image::imageops::FilterType::Lanczos3);
    let image = image.into_rgba8();

    common::display::image::draw_image(
        &mut display.pixmap_mut(),
        &image,
        common::geom::Point::new(0, 0),
    );

    Ok(())
}

#[cfg(test)]
mod input_boost_tests {
    use super::*;

    #[test]
    fn presses_and_repeats_boost_cpu_but_releases_do_not() {
        for key in [Key::B, Key::Start, Key::Up, Key::A, Key::Menu, Key::Power] {
            assert!(boosts_cpu(KeyEvent::Pressed(key)), "{key:?}");
            assert!(boosts_cpu(KeyEvent::Autorepeat(key)), "{key:?}");
            assert!(!boosts_cpu(KeyEvent::Released(key)), "{key:?}");
        }
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod orbit_proof {
    include!("parity_capture_tests.rs");
    use super::*;
    use common::constants::{ALLIUM_BASE_DIR, ALLIUM_LAUNCHER_STATE};
    use serde_json::Value;
    use std::fs;

    fn state(app: &AlliumLauncher<DefaultPlatform>) -> Value {
        app.view.save().unwrap();
        serde_json::from_reader(std::fs::File::open(ALLIUM_LAUNCHER_STATE.as_path()).unwrap())
            .unwrap()
    }

    async fn press(
        app: &mut AlliumLauncher<DefaultPlatform>,
        key: Key,
        tx: &tokio::sync::mpsc::Sender<Command>,
    ) {
        let mut bubble = VecDeque::new();
        assert!(
            app.view
                .handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut bubble)
                .await
                .unwrap()
        );
    }

    fn prepare_art() {
        let source = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_ART").unwrap());
        let target = ALLIUM_GAMES_DIR.join("PS/Imgs");
        std::fs::create_dir_all(&target).unwrap();
        for (stem, asset) in [
            ("R4 - Ridge Racer Type 4 (USA)", "r4"),
            ("Silent Hill (USA)", "silent"),
        ] {
            let cover = image::open(source.join(format!("{asset}-cover.png")))
                .unwrap()
                .thumbnail(93, 93)
                .to_rgba8();
            let mut canvas =
                image::RgbaImage::from_pixel(101, 101, image::Rgba([255, 255, 255, 255]));
            image::imageops::replace(
                &mut canvas,
                &cover,
                (101 - cover.width()) as i64 / 2,
                (101 - cover.height()) as i64 / 2,
            );
            common::display::image::round(&mut canvas, 9);
            canvas
                .save(target.join(format!("{stem}.orbit.png")))
                .unwrap();
            image::open(source.join(format!("{asset}-hero.png")))
                .unwrap()
                .resize_to_fill(360, 210, image::imageops::FilterType::Lanczos3)
                .save(target.join(format!("{stem}.hero.png")))
                .unwrap();
            std::fs::write(
                ALLIUM_GAMES_DIR.join("PS").join(format!("{stem}.chd")),
                b"host fixture, not a ROM",
            )
            .unwrap();
        }
    }

    fn render_theme(app: &mut AlliumLauncher<DefaultPlatform>) {
        app.view.prepare_theme().unwrap();
        let style = crate::theme_adapter::drawing_style(
            &app.res.get::<Stylesheet>(),
            crate::theme_adapter::snapshot(&app.res).as_ref(),
        );
        app.view.draw(&mut app.display, &style).unwrap();
    }
    fn activate_orbit_fixture(app: &mut AlliumLauncher<DefaultPlatform>) {
        let raw = serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap();
        crate::theme_adapter::choose(&app.res, None).unwrap();
        assert!(crate::theme_adapter::snapshot(&app.res).is_none());
        assert_eq!(
            app.res.get::<Stylesheet>().display_style().ui.text_color,
            app.res.get::<Stylesheet>().ui.text_color
        );
        crate::theme_adapter::choose(&app.res, Some("orbit")).unwrap();
        assert_eq!(
            crate::theme_adapter::snapshot(&app.res).unwrap().pack().id,
            "orbit"
        );
        assert_eq!(
            serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
            raw
        );
        app.view.set_should_draw();
        render_theme(app);
    }
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn themed_root_shoulders_do_not_depend_on_art_cache() {
        assert!(ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(!ALLIUM_SD_ROOT.starts_with("/Volumes"));
        prepare_art();
        let asset_source =
            std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_ART").unwrap());
        let ctr = ALLIUM_GAMES_DIR.join("PS/Crash Team Racing (USA).chd");
        fs::write(&ctr, b"host routing fixture, not a ROM").unwrap();
        image::open(asset_source.join("ctr-cover.png"))
            .unwrap()
            .resize_exact(101, 101, image::imageops::FilterType::Lanczos3)
            .save(ALLIUM_GAMES_DIR.join("PS/Imgs/Crash Team Racing (USA).orbit.png"))
            .unwrap();
        image::open(asset_source.join("ctr-hero.png"))
            .unwrap()
            .resize_to_fill(360, 210, image::imageops::FilterType::Lanczos3)
            .save(ALLIUM_GAMES_DIR.join("PS/Imgs/Crash Team Racing (USA).hero.png"))
            .unwrap();
        if ALLIUM_LAUNCHER_STATE.exists() {
            fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
        }
        let apps = common::constants::ALLIUM_APPS_DIR.as_path();
        fs::create_dir_all(apps).unwrap();
        for index in 0..8 {
            let path = apps.join(format!("Routing {index}.pak"));
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("config.json"),
                format!("{{\"label\":\"Routing {index}\",\"launch\":\"not-executable\"}}"),
            )
            .unwrap();
        }
        let external = ALLIUM_THEMES_DIR.join("routing-external");
        fs::create_dir_all(external.join("assets")).unwrap();
        let pack_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../common/src/theme/packs");
        let mut pack: serde_json::Value =
            serde_json::from_slice(&fs::read(pack_dir.join("compact.json")).unwrap()).unwrap();
        pack["id"] = "routing-external".into();
        pack["name"] = "Routing external fixture".into();
        pack["revision"] = "routing-fixture".into();
        for asset in pack["assets"].as_object().unwrap().values() {
            let relative = asset["path"].as_str().unwrap();
            let target = external.join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(
                pack_dir.join(std::path::Path::new(relative).file_name().unwrap()),
                target,
            )
            .unwrap();
        }
        fs::write(
            external.join("theme.json"),
            serde_json::to_vec(&pack).unwrap(),
        )
        .unwrap();
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        let raw = serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap();
        let database = fs::read(common::constants::ALLIUM_DATABASE.as_path()).unwrap();
        let game_info = format!("{:?}", common::game_info::GameInfo::load().ok());
        fn saves_snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files.extend(saves_snapshot(&path));
                } else {
                    files.push((path.clone(), fs::read(path).unwrap()));
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        }
        let saves = saves_snapshot(&ALLIUM_SD_ROOT.join("Saves"));
        for preset in ["orbit", "compact", "routing-external"] {
            for prepared_art in [false, true] {
                crate::theme_adapter::choose(&app.res, None).unwrap();
                render_theme(&mut app);
                crate::theme_adapter::choose(&app.res, Some(preset)).unwrap();
                app.view.proof_select_tab(2);
                if prepared_art {
                    render_theme(&mut app);
                    app.display
                        .pixmap()
                        .to_owned()
                        .save_png(
                            std::path::PathBuf::from(
                                std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap(),
                            )
                            .join(format!("root-tabs-{preset}-apps.png")),
                        )
                        .unwrap();
                }
                let selected = state(&app)["apps"]["selected"].clone();
                press(&mut app, Key::R, &tx).await;
                assert_eq!(
                    state(&app)["selected"],
                    3,
                    "{preset} Apps R prepared={prepared_art}"
                );
                assert_eq!(state(&app)["apps"]["selected"], selected);
                if prepared_art {
                    render_theme(&mut app);
                    app.display
                        .pixmap()
                        .to_owned()
                        .save_png(
                            std::path::PathBuf::from(
                                std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap(),
                            )
                            .join(format!("root-tabs-{preset}-settings.png")),
                        )
                        .unwrap();
                }
                press(&mut app, Key::L, &tx).await;
                assert_eq!(state(&app)["selected"], 2);
            }
            for tab in 0..4 {
                app.view.proof_select_tab(tab);
                render_theme(&mut app);
                press(&mut app, Key::R, &tx).await;
                assert_eq!(
                    state(&app)["selected"],
                    (tab + 1) % 4,
                    "{preset} root {tab} R"
                );
                press(&mut app, Key::L, &tx).await;
                assert_eq!(state(&app)["selected"], tab);
            }
        }
        crate::theme_adapter::choose(&app.res, Some("orbit")).unwrap();
        app.view.proof_select_tab(1);
        press(&mut app, Key::A, &tx).await;
        render_theme(&mut app);
        assert!(state(&app)["games"]["child"].is_object());
        while app.view.idle_media_step() {}
        render_theme(&mut app);
        assert_eq!(app.view.proof_game_theme_inputs()[0].art.entries.len(), 3);
        let library = state(&app)["games"].clone();
        assert!(
            app.view.proof_game_theme_inputs()[0]
                .hints
                .iter()
                .any(|hint| hint.button == "L/R" && hint.label == "Mode")
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(
                std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                    .join("root-tabs-native-ps-library.png"),
            )
            .unwrap();
        press(&mut app, Key::R, &tx).await;
        assert_eq!(
            state(&app)["selected"],
            2,
            "actual PS library shoulder advances Apps"
        );
        press(&mut app, Key::L, &tx).await;
        assert_eq!(state(&app)["selected"], 1);
        assert_eq!(
            state(&app)["games"],
            library,
            "actual PS game/window preserved"
        );
        press(&mut app, Key::B, &tx).await;
        while let Ok(command) = rx.try_recv() {
            assert!(
                matches!(command, Command::Redraw),
                "directory Back emits only presentation refresh"
            );
        }
        app.view.proof_select_tab(2);
        press(&mut app, Key::Select, &tx).await;
        render_theme(&mut app);
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 2, "context owns shoulders");
        press(&mut app, Key::B, &tx).await;
        assert!(matches!(rx.try_recv().unwrap(), Command::Redraw));
        app.view.proof_select_tab(3);
        let theme_row = if DefaultPlatform::has_wifi() { 5 } else { 3 };
        for _ in 0..theme_row {
            press(&mut app, Key::Down, &tx).await;
        }
        press(&mut app, Key::A, &tx).await;
        assert!(state(&app)["settings"]["child"].is_object());
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 3, "Appearance child owns paging");
        press(&mut app, Key::B, &tx).await;
        assert!(state(&app)["settings"]["child"].is_null());
        assert!(
            rx.try_recv().is_err(),
            "Appearance B closes internally, no queued command"
        );
        app.view.proof_select_tab(1);
        app.view.start_search();
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 1, "search keyboard owns shoulders");
        app.view.close_search_results();
        crate::theme_adapter::choose(&app.res, None).unwrap();
        render_theme(&mut app);
        app.view.proof_select_tab(2);
        let before = state(&app)["apps"]["selected"].as_u64().unwrap();
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 2, "Legacy root paging preserved");
        assert_eq!(state(&app)["apps"]["selected"], (before + 5).min(7));
        assert_eq!(
            serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
            raw
        );
        assert_eq!(
            fs::read(common::constants::ALLIUM_DATABASE.as_path()).unwrap(),
            database
        );
        assert_eq!(
            format!("{:?}", common::game_info::GameInfo::load().ok()),
            game_info
        );
        assert!(
            rx.try_recv().is_err(),
            "no Exec/Search/cache/domain commands"
        );
        assert_eq!(saves_snapshot(&ALLIUM_SD_ROOT.join("Saves")), saves);
        for index in 0..8 {
            let path = apps.join(format!("Routing {index}.pak"));
            fs::remove_file(path.join("config.json")).unwrap();
            fs::remove_dir(path).unwrap();
        }
        fs::remove_dir_all(external).unwrap();
        fs::remove_file(ctr).unwrap();
        for suffix in ["orbit", "hero"] {
            fs::remove_file(
                ALLIUM_GAMES_DIR.join(format!("PS/Imgs/Crash Team Racing (USA).{suffix}.png")),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn native_launcher_theme_controller_real_render_and_legacy() {
        assert!(ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(ALLIUM_SD_ROOT.starts_with("/private/tmp"));
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut installed = Vec::new();
        for id in common::theme::choices::LAUNCHER_PACKS {
            let target = ALLIUM_THEMES_DIR.join(id);
            if !target.exists() {
                fs::create_dir_all(target.join("assets")).unwrap();
                fs::copy(
                    repo.join("themes").join(id).join("theme.json"),
                    target.join("theme.json"),
                )
                .unwrap();
                for asset in fs::read_dir(repo.join("themes").join(id).join("assets")).unwrap() {
                    let asset = asset.unwrap();
                    fs::copy(asset.path(), target.join("assets").join(asset.file_name())).unwrap();
                }
                installed.push(target);
            }
        }
        prepare_art();
        if ALLIUM_LAUNCHER_STATE.exists() {
            fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
        }
        let evidence = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        let raw = serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap();
        crate::theme_adapter::choose(&app.res, None).unwrap();
        assert!(crate::theme_adapter::snapshot(&app.res).is_none());
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        app.view.proof_select_tab(1);
        press(&mut app, Key::A, &tx).await;
        let identity = state(&app)["games"].clone();
        // Cold frames only consume cached sidecars; this proof compares loaded metadata.
        let selected = identity["child"]["selected_path"].as_str().unwrap();
        let details = crate::view::details::Details::new(&Game::new(selected.into()), "PlayStation");
        assert!(details.blurb.contains("Real Racing Roots"));
        let database_path = common::constants::ALLIUM_DATABASE.as_path();
        let db_before = fs::read(database_path).unwrap();
        for id in ["orbit", "compact", "orbit-row"] {
            crate::theme_adapter::choose(&app.res, Some(id)).unwrap();
            app.view.set_should_draw();
            render_theme(&mut app);
            let models = app.view.proof_game_theme_inputs();
            let semantic = &models[0].text;
            if id == "orbit" {
                assert!(semantic[&common::theme::pack::Binding::SelectedMetadata].is_empty());
                assert!(semantic[&common::theme::pack::Binding::SelectedBlurb].is_empty());
            } else {
                assert!(semantic[&common::theme::pack::Binding::SelectedMetadata].contains("PlayStation"));
                assert!(semantic[&common::theme::pack::Binding::SelectedBlurb].contains("Real Racing Roots"));
            }
            let handle = crate::theme_adapter::snapshot(&app.res).unwrap();
            if id == "orbit" {
                assert!(
                    !handle.pack().screens[&common::theme::pack::ScreenId::Library]
                        .components
                        .iter()
                        .any(|c| matches!(
                            c.binding,
                            Some(
                                common::theme::pack::Binding::SelectedMetadata
                                    | common::theme::pack::Binding::SelectedBlurb
                            )
                        ))
                );
            } else {
                assert!(
                    handle.pack().screens[&common::theme::pack::ScreenId::Library]
                        .components
                        .iter()
                        .any(|c| c.binding == Some(common::theme::pack::Binding::SelectedMetadata))
                );
            }
            assert_eq!(state(&app)["games"], identity);
            assert_eq!(
                serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
                raw
            );
            assert!(!app.view.should_draw(), "cached themed frame must settle");
            app.display
                .pixmap()
                .to_owned()
                .save_png(evidence.join(format!("launcher-{id}-library.png")))
                .unwrap();
            if id == "orbit" {
                assert!(
                    app.view.prepared_status_boxes() > 0,
                    "real boxed status must reach App frame"
                );
                let original = app.res.get::<Stylesheet>().clone();
                let mut hidden = original.clone();
                hidden.status_bar.hide_status_in_launcher = true;
                app.res.insert(hidden);
                app.view.set_should_draw();
                render_theme(&mut app);
                assert_eq!(
                    app.view.prepared_status_boxes(),
                    0,
                    "hide-status preference suppresses launcher status only"
                );
                app.display
                    .pixmap()
                    .to_owned()
                    .save_png(evidence.join("launcher-orbit-status-hidden.png"))
                    .unwrap();
                app.res.insert(original);
                app.view.set_should_draw();
                render_theme(&mut app);
                assert!(app.view.prepared_status_boxes() > 0);
                assert_eq!(
                    serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
                    raw
                );
            }
            app.view.proof_select_tab(3);
            app.view.set_should_draw();
            render_theme(&mut app);
            app.display
                .pixmap()
                .to_owned()
                .save_png(evidence.join(format!("launcher-{id}-settings.png")))
                .unwrap();
            app.view.proof_select_tab(1);
            app.view.set_should_draw();
            render_theme(&mut app);
        }
        assert_eq!(fs::read(database_path).unwrap(), db_before);
        assert!(
            rx.try_recv().is_err(),
            "theme changes/navigation cannot emit Exec"
        );
        crate::theme_adapter::choose(&app.res, Some("orbit")).unwrap();
        app.view.proof_select_tab(3);
        app.view.set_should_draw();
        render_theme(&mut app);
        for _ in 0..if DefaultPlatform::has_wifi() { 5 } else { 3 } {
            press(&mut app, Key::Down, &tx).await;
        }
        press(&mut app, Key::A, &tx).await;
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("launcher-appearance.png"))
            .unwrap();
        press(&mut app, Key::Right, &tx).await;
        render_theme(&mut app);
        assert_eq!(
            crate::theme_adapter::snapshot(&app.res).unwrap().pack().id,
            "orbit-hero"
        );
        press(&mut app, Key::Down, &tx).await;
        press(&mut app, Key::Right, &tx).await;
        render_theme(&mut app);
        assert_eq!(
            crate::theme_adapter::snapshot(&app.res).unwrap().settings()["cover_shape"],
            "tall"
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("launcher-appearance-tall.png"))
            .unwrap();
        press(&mut app, Key::B, &tx).await;
        assert!(
            rx.try_recv().is_err(),
            "Appearance choose/back emits no command"
        );
        app.view.proof_select_tab(1);
        app.view.set_should_draw();
        render_theme(&mut app);
        press(&mut app, Key::Select, &tx).await;
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("launcher-theme-settings-context.png"))
            .unwrap();
        press(&mut app, Key::B, &tx).await;
        let context_command = rx.try_recv().expect("context cancel requests redraw");
        eprintln!("CONTEXT_CANCEL_COMMAND={context_command:?}");
        assert!(matches!(context_command, Command::Redraw));
        assert!(rx.try_recv().is_err(), "context cancel emits only Redraw");
        render_theme(&mut app);
        app.view.proof_select_tab(3);
        press(&mut app, Key::A, &tx).await;
        press(&mut app, Key::Left, &tx).await;
        assert_eq!(
            crate::theme_adapter::snapshot(&app.res).unwrap().pack().id,
            "compact-plus"
        );
        press(&mut app, Key::Down, &tx).await;
        press(&mut app, Key::Down, &tx).await;
        press(&mut app, Key::Left, &tx).await;
        assert_eq!(
            crate::theme_adapter::snapshot(&app.res).unwrap().settings()["color_scheme"],
            "black"
        );
        press(&mut app, Key::B, &tx).await;
        crate::theme_adapter::choose(&app.res, None).unwrap();
        app.view.set_should_draw();
        render_theme(&mut app);
        assert!(crate::theme_adapter::snapshot(&app.res).is_none());
        assert_eq!(
            serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
            raw
        );
        assert_eq!(fs::read(database_path).unwrap(), db_before);
        assert_eq!(state(&app)["games"], identity);
        assert!(rx.try_recv().is_err(), "Theme options emit no command");
        eprintln!("THEME_OPTION_COMMANDS=[]");
        assert_eq!(
            app.res.get::<Stylesheet>().display_style().ui.text_color,
            app.res.get::<Stylesheet>().ui.text_color
        );
        for path in installed {
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn native_orbit_global_pages_preserve_raw_theme_and_game_database() {
        use common::database::Database;
        use common::display::color::Color;
        use common::geom::{Alignment, Point};
        use common::view::{ColorPicker, Keyboard};
        assert!(ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(!ALLIUM_SD_ROOT.starts_with("/Volumes"));
        let evidence = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        std::fs::create_dir_all(&evidence).unwrap();
        let apps = common::constants::ALLIUM_APPS_DIR.join("Orbit Render Fixture.pak");
        assert!(!apps.exists());
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(
            apps.join("config.json"),
            br#"{"label":"Orbit Render Fixture","launch":"not-executable"}"#,
        )
        .unwrap();
        struct OwnedApp(std::path::PathBuf);
        impl Drop for OwnedApp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("config.json"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let _owned_app = OwnedApp(apps);
        if ALLIUM_LAUNCHER_STATE.exists() {
            std::fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
        }
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        activate_orbit_fixture(&mut app);
        let raw_theme = serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap();
        let database_path = common::constants::ALLIUM_DATABASE.as_path();
        let database_before = std::fs::read(database_path).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        for (index, name) in ["recents", "library-root", "apps", "settings"]
            .iter()
            .enumerate()
        {
            app.view.proof_select_tab(index);
            assert_eq!(state(&app)["selected"], index);
            app.view.set_should_draw();
            render_theme(&mut app);
            let frame = app.display.pixmap().to_owned();
            assert_eq!((frame.width(), frame.height()), (640, 480));
            assert_eq!(frame.pixel(0, 0).unwrap(), {
                let color = crate::theme_adapter::drawing_style(
                    &app.res.get::<Stylesheet>(),
                    crate::theme_adapter::snapshot(&app.res).as_ref(),
                )
                .ui
                .background_color;
                tiny_skia::PremultipliedColorU8::from_rgba(
                    color.r(),
                    color.g(),
                    color.b(),
                    color.a(),
                )
                .unwrap()
            });
            frame
                .save_png(evidence.join(format!("native-orbit-global-{name}.png")))
                .unwrap();
        }
        press(&mut app, Key::Down, &tx).await;
        assert!(
            app.view.should_draw(),
            "active Settings selection must wake Orbit shell"
        );
        render_theme(&mut app);
        press(&mut app, Key::Up, &tx).await;
        press(&mut app, Key::A, &tx).await;
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-global-settings-nested.png"))
            .unwrap();
        press(&mut app, Key::B, &tx).await;
        app.view.search("Ridge".into()).unwrap();
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-global-search.png"))
            .unwrap();
        app.view.close_search_results();
        let display_styles = crate::theme_adapter::drawing_style(
            &app.res.get::<Stylesheet>(),
            crate::theme_adapter::snapshot(&app.res).as_ref(),
        );
        let mut keyboard = Keyboard::new(app.res.clone(), "controller navigation".into(), false);
        app.display
            .clear(display_styles.ui.background_color)
            .unwrap();
        keyboard.draw(&mut app.display, &display_styles).unwrap();
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-global-keyboard.png"))
            .unwrap();
        let value = Color::new(45, 190, 200);
        let mut picker = ColorPicker::new(Point::new(520, 120), value, Alignment::Right);
        app.display
            .clear(display_styles.ui.background_color)
            .unwrap();
        picker.draw(&mut app.display, &display_styles).unwrap();
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-global-color-picker.png"))
            .unwrap();
        assert_eq!(picker.value(), value);
        assert_eq!(
            serde_json::to_vec(&*app.res.get::<Stylesheet>()).unwrap(),
            raw_theme
        );
        assert_eq!(std::fs::read(database_path).unwrap(), database_before);
        assert!(
            app.res
                .get::<Database>()
                .select_games(&[])
                .unwrap()
                .is_empty()
        );
        assert!(
            rx.try_recv().is_err(),
            "drawing and navigation must not emit launch commands"
        );
    }

    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn native_orbit_frame_selection_launch_and_tabs() {
        assert!(crate::view::orbit::enabled());
        let evidence = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        std::fs::create_dir_all(&evidence).unwrap();
        assert!(ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(!ALLIUM_SD_ROOT.starts_with("/Volumes"));
        if ALLIUM_LAUNCHER_STATE.exists() {
            std::fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
        }
        common::game_info::GameInfo::delete().unwrap();
        prepare_art();
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        activate_orbit_fixture(&mut app);
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        press(&mut app, Key::A, &tx).await;
        assert!(state(&app)["games"]["child"].is_object());
        render_theme(&mut app);
        let (cached, ancestors) = app.view.orbit_cache_counts();
        assert!(cached <= 16);
        assert_eq!(ancestors, 0, "ancestor artwork must release on descent");
        let frame = app.display.pixmap().to_owned();
        assert_eq!((frame.width(), frame.height()), (640, 480));
        frame
            .save_png(evidence.join("native-orbit-library.png"))
            .unwrap();
        assert!(
            !app.view.should_draw(),
            "static library must settle without an animation loop"
        );
        fn detail_save_snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files.extend(detail_save_snapshot(&path));
                } else {
                    files.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        }
        let saves_before_details = detail_save_snapshot(&ALLIUM_SD_ROOT.join("Saves"));
        let selection_before_details = state(&app);
        let db_before_details = fs::read(common::constants::ALLIUM_DATABASE.as_path()).unwrap();
        let game_info_before_details = common::game_info::GameInfo::load().ok();
        press(&mut app, Key::Start, &tx).await;
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-details.png"))
            .unwrap();
        assert!(
            !app.view.should_draw(),
            "Details must settle without a render loop"
        );
        for page in 2..=12 {
            press(&mut app, Key::Down, &tx).await;
            render_theme(&mut app);
            app.display
                .pixmap()
                .to_owned()
                .save_png(evidence.join(format!("native-orbit-details-page-{page}.png")))
                .unwrap();
        }
        for _ in 0..12 {
            press(&mut app, Key::Up, &tx).await;
        }
        render_theme(&mut app);
        for key in [
            Key::X,
            Key::Y,
            Key::Select,
            Key::L,
            Key::R,
            Key::L2,
            Key::R2,
        ] {
            press(&mut app, key, &tx).await;
            assert_eq!(state(&app), selection_before_details);
        }
        assert_eq!(
            fs::read(common::constants::ALLIUM_DATABASE.as_path()).unwrap(),
            db_before_details
        );
        assert_eq!(
            format!("{:?}", common::game_info::GameInfo::load().ok()),
            format!("{:?}", game_info_before_details)
        );
        assert_eq!(
            detail_save_snapshot(&ALLIUM_SD_ROOT.join("Saves")),
            saves_before_details
        );
        assert!(
            rx.try_recv().is_err(),
            "Details inspection must emit no commands"
        );
        press(&mut app, Key::B, &tx).await;
        render_theme(&mut app);
        assert_eq!(state(&app), selection_before_details);
        crate::view::orbit::Orbit::proof_long_title(
            &mut app.display,
            &app.res.get::<Stylesheet>(),
            ALLIUM_GAMES_DIR.join("PS/R4 - Ridge Racer Type 4 (USA).chd"),
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-long-title.png"))
            .unwrap();
        press(&mut app, Key::Right, &tx).await;
        let next = state(&app);
        assert_eq!(next["selected"], 1);
        assert_eq!(next["games"]["child"]["selected"], 1);
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-selected.png"))
            .unwrap();
        press(&mut app, Key::Left, &tx).await;
        assert_eq!(state(&app)["games"]["child"]["selected"], 0);
        press(&mut app, Key::Select, &tx).await;
        render_theme(&mut app);
        app.view.set_should_draw();
        render_theme(&mut app);
        assert!(!app.view.should_draw(), "Orbit menu underlay must settle");
        assert!(!app.view.should_draw());
        press(&mut app, Key::L, &tx).await;
        assert_eq!(
            state(&app)["selected"],
            1,
            "context menu owns shoulder navigation"
        );
        press(&mut app, Key::B, &tx).await;
        assert!(matches!(rx.try_recv().unwrap(), Command::Redraw));
        press(&mut app, Key::A, &tx).await;
        let Command::Exec(launch) = rx.try_recv().unwrap() else {
            panic!("A must emit the existing Exec command");
        };
        let arguments = launch.get_args().collect::<Vec<_>>();
        assert_eq!(
            launch.get_program(),
            ALLIUM_BASE_DIR.join("cores/retroarch/launch.sh")
        );
        assert_eq!(arguments[0], "pcsx_rearmed");
        assert_eq!(
            arguments[1],
            ALLIUM_GAMES_DIR.join("PS/R4 - Ridge Racer Type 4 (USA).chd")
        );
        press(&mut app, Key::B, &tx).await;
        assert!(state(&app)["games"]["child"].is_null());
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 2);
        press(&mut app, Key::Right, &tx).await;
        assert_eq!(state(&app)["selected"], 2, "Apps owns directional selection");
        press(&mut app, Key::R, &tx).await;
        assert_eq!(state(&app)["selected"], 3);
        press(&mut app, Key::L, &tx).await;
        assert_eq!(
            state(&app)["selected"],
            2,
            "themed root Settings uses explicit shoulder tabs"
        );
        press(&mut app, Key::Left, &tx).await;
        assert_eq!(state(&app)["selected"], 2, "Apps owns directional selection");
        press(&mut app, Key::L, &tx).await;
        assert_eq!(state(&app)["selected"], 1);
        press(&mut app, Key::X, &tx).await;
        assert!(matches!(rx.try_recv().unwrap(), Command::StartSearch));
        app.view.start_search();
        render_theme(&mut app);
        app.view.close_search_results();
        render_theme(&mut app);
        assert!(!app.view.should_draw(), "Orbit search return must settle");
        assert!(!app.view.should_draw());
    }
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn native_orbit_disc_popup_cancel_and_shared_launch_identity() {
        use crate::entry::Entry;
        use common::constants::ALLIUM_GAME_INFO;
        use common::database::Database;
        use std::fs;
        assert!(ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(!ALLIUM_SD_ROOT.starts_with("/Volumes"));
        prepare_art();
        let outer = ALLIUM_GAMES_DIR.join("PS/ZZ Multi (USA).m3u");
        fs::create_dir_all(&outer).unwrap();
        let inner = outer.join("ZZ Multi (USA).m3u");
        fs::write(&inner, "#EXTM3U\nDisc 1.chd\nDisc 2.chd\n").unwrap();
        for name in ["Disc 1.chd", "Disc 2.chd"] {
            fs::write(outer.join(name), b"host fixture, not a ROM").unwrap();
        }
        let saves = ALLIUM_SD_ROOT.join("Saves/CurrentProfile/saves");
        fs::create_dir_all(&saves).unwrap();
        let config = ALLIUM_SD_ROOT.join("RetroArch/.retroarch");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("retroarch.cfg"), format!(
            "savefile_directory = \"{}\"\nsort_savefiles_enable = false\nsort_savefiles_by_content_enable = false\nsavestate_auto_load = true\n", saves.display()
        )).unwrap();
        let record = saves.join("ZZ Multi (USA).ldci");
        fs::write(&record, b"existing disc-state fixture").unwrap();
        let sram = saves.join("ZZ Multi (USA).srm");
        fs::write(&sram, b"shared SRAM sentinel").unwrap();
        let states = ALLIUM_SD_ROOT.join("Saves/CurrentProfile/states/PCSX-ReARMed");
        fs::create_dir_all(&states).unwrap();
        let saved_state = states.join("ZZ Multi (USA).state.auto");
        fs::write(&saved_state, b"autoload state sentinel").unwrap();
        if ALLIUM_LAUNCHER_STATE.exists() {
            fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
        }
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        activate_orbit_fixture(&mut app);
        assert!(
            matches!(
                Entry::new(
                    outer.clone(),
                    &app.res.get::<crate::consoles::ConsoleMapper>()
                )
                .unwrap(),
                Some(Entry::Game(_))
            ),
            "Stock classification treats extension-bearing folder as a game"
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        press(&mut app, Key::A, &tx).await;
        press(&mut app, Key::Right, &tx).await;
        press(&mut app, Key::Right, &tx).await;
        assert_eq!(state(&app)["games"]["child"]["selected"], 2);
        let before_count = app.res.get::<Database>().select_games(&[&outer]).unwrap()[0]
            .as_ref()
            .map(|game| game.play_count);
        let before_info = fs::read(ALLIUM_GAME_INFO.as_path()).ok();
        press(&mut app, Key::Start, &tx).await;
        assert!(rx.try_recv().is_err());
        press(&mut app, Key::A, &tx).await;
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(
                std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                    .join("native-disc-picker.png"),
            )
            .unwrap();
        assert!(!app.view.should_draw());
        let modal_games_state = state(&app)["games"].clone();
        for key in [
            Key::X,
            Key::Y,
            Key::L,
            Key::R,
            Key::L2,
            Key::R2,
            Key::Left,
            Key::Right,
            Key::Start,
            Key::Menu,
            Key::Unknown,
        ] {
            press(&mut app, key, &tx).await;
            assert!(
                rx.try_recv().is_err(),
                "Modal shortcut {key:?} must emit no Search/Exec/other command"
            );
            assert!(
                app.view.disc_picker_active(),
                "Modal shortcut {key:?} must leave the nested picker active"
            );
            assert_eq!(state(&app)["selected"], 1, "Modal owns tab keys");
            assert_eq!(
                state(&app)["games"],
                modal_games_state,
                "Modal shortcut must not change underlying game or sort"
            );
            assert!(
                !app.view.should_draw(),
                "Ignored shortcut must not dirty underlying views"
            );
        }
        press(&mut app, Key::B, &tx).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(fs::read(&record).unwrap(), b"existing disc-state fixture");
        assert_eq!(fs::read(ALLIUM_GAME_INFO.as_path()).ok(), before_info);
        assert_eq!(
            app.res.get::<Database>().select_games(&[&outer]).unwrap()[0]
                .as_ref()
                .map(|game| game.play_count),
            before_count
        );
        press(&mut app, Key::A, &tx).await;
        press(&mut app, Key::Down, &tx).await;
        press(&mut app, Key::A, &tx).await;
        let Command::Exec(command) = rx.try_recv().unwrap() else {
            panic!("Disc selection must emit Exec");
        };
        let arguments = command.get_args().collect::<Vec<_>>();
        assert_eq!(
            arguments,
            vec![
                std::ffi::OsStr::new("pcsx_rearmed"),
                inner.as_os_str(),
                std::ffi::OsStr::new("--choose-disc")
            ]
        );
        let info: Value =
            serde_json::from_slice(&fs::read(ALLIUM_GAME_INFO.as_path()).unwrap()).unwrap();
        assert_eq!(info["path"], outer.to_str().unwrap());
        assert_eq!(info["args"], serde_json::json!(["pcsx_rearmed", inner]));
        assert!(
            !info["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "--choose-disc"),
            "Daemon resume keeps normal runner args"
        );
        let disk: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        assert_eq!(disk["image_index"], 1);
        assert_eq!(
            disk["image_path"],
            outer.join("Disc 2.chd").to_str().unwrap()
        );
        assert_eq!(fs::read(&sram).unwrap(), b"shared SRAM sentinel");
        assert_eq!(fs::read(&saved_state).unwrap(), b"autoload state sentinel");
        // A failed confirmation retains the popup and cannot update launch metadata.
        press(&mut app, Key::A, &tx).await;
        fs::remove_file(outer.join("Disc 1.chd")).unwrap();
        let prior_record = fs::read(&record).unwrap();
        let prior_info = fs::read(ALLIUM_GAME_INFO.as_path()).unwrap();
        let prior_count = app.res.get::<Database>().select_games(&[&outer]).unwrap()[0]
            .as_ref()
            .unwrap()
            .play_count;
        press(&mut app, Key::A, &tx).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(fs::read(&record).unwrap(), prior_record);
        assert_eq!(fs::read(ALLIUM_GAME_INFO.as_path()).unwrap(), prior_info);
        assert_eq!(
            app.res.get::<Database>().select_games(&[&outer]).unwrap()[0]
                .as_ref()
                .unwrap()
                .play_count,
            prior_count
        );
        press(&mut app, Key::B, &tx).await;
        fs::write(outer.join("Disc 1.chd"), b"host fixture, not a ROM").unwrap();
        let playlist = crate::discs::discover(&outer).unwrap().unwrap();
        let temporary = crate::discs::StartRecord::apply(&playlist, 0).unwrap();
        temporary.rollback().unwrap();
        assert_eq!(fs::read(&record).unwrap(), prior_record);
        fs::write(&inner, "\u{feff}Disc 1.chd\nDisc 2.chd\n").unwrap();
        let bad = crate::discs::discover(&outer).unwrap().unwrap();
        assert!(crate::discs::StartRecord::apply(&bad, 0).is_err());
        assert_eq!(fs::read(&record).unwrap(), prior_record);
        fs::write(&inner, "Disc 1.chd\nDisc 2.chd\n").unwrap();
        let overrides = config.join("config/PCSX-ReARMed");
        fs::create_dir_all(&overrides).unwrap();
        let override_path = overrides.join("ZZ Multi (USA).cfg");
        fs::write(&override_path, "savefile_directory = custom").unwrap();
        assert!(crate::discs::StartRecord::apply(&playlist, 0).is_err());
        assert_eq!(fs::read(&record).unwrap(), prior_record);
        fs::remove_file(override_path).unwrap();
        let single_dir = ALLIUM_GAMES_DIR.join("PS/_single-disc-proof");
        fs::create_dir_all(&single_dir).unwrap();
        let single_path = single_dir.join("Single.m3u");
        fs::write(&single_path, "single.chd\n").unwrap();
        fs::write(single_dir.join("single.chd"), b"host fixture, not a ROM").unwrap();
        assert_eq!(
            crate::discs::discover(&single_path)
                .unwrap()
                .unwrap()
                .discs
                .len(),
            1
        );
        let mut single = crate::entry::game::Game::new(single_path.clone());
        let Command::Exec(single_command) = app
            .res
            .get::<crate::consoles::ConsoleMapper>()
            .launch_game(&app.res.get(), &mut single, false)
            .unwrap()
            .unwrap()
        else {
            panic!("Single disc must use existing Exec");
        };
        assert_eq!(
            single_command.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("pcsx_rearmed"),
                single_path.as_os_str()
            ]
        );
        assert!(!saves.join("Single.ldci").exists());
        fs::remove_file(single_path).unwrap();
        fs::remove_file(single_dir.join("single.chd")).unwrap();
        fs::remove_dir(single_dir).unwrap();

        press(&mut app, Key::B, &tx).await;
        let (cached, ancestors) = app.view.orbit_cache_counts();
        assert!(cached <= 16);
        assert_eq!(ancestors, 0);
    }
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn native_orbit_network_cards_cancel_verify_exec_and_storage() {
        use crate::network_library::{Catalog, NetworkGame, Payload};
        use crate::network_runtime as network;
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let root = ALLIUM_SD_ROOT.to_path_buf();
        assert!(root.ends_with("native-orbit-fixture") && !root.starts_with("/Volumes"));
        let state = root.join(".allium/state/network-library");
        assert!(
            !state.exists(),
            "network proof requires fresh owned fixture state"
        );
        let path = root.join("Roms/PS/Network Fixture.chd");
        assert!(!path.exists());
        struct Cleanup(std::path::PathBuf, std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.0.exists() {
                    std::fs::remove_dir_all(&self.0).unwrap();
                }
                if self.1.exists() {
                    std::fs::remove_file(&self.1).unwrap();
                }
                network::initialize(ALLIUM_SD_ROOT.to_path_buf()).unwrap();
            }
        }
        let _cleanup = Cleanup(state.clone(), path.clone());
        let body = vec![b'f'; 160 * 1024];
        let sha = crate::network_transfer::digest_hex(Sha256::digest(&body).as_slice());
        let revision = "b".repeat(64);
        let descriptor = NetworkGame {
            id: "network-fixture".into(),
            display_title: "Network Fixture".into(),
            platform: "PS".into(),
            bundle_revision: revision.clone(),
            game_path: "Roms/PS/Network Fixture.chd".into(),
            cache_path: "Roms/PS/Network Fixture.chd".into(),
            install_kind: "file".into(),
            launch_path: "Roms/PS/Network Fixture.chd".into(),
            files: vec![Payload {
                id: "disc".into(),
                relative_path: String::new(),
                bytes: body.len() as u64,
                sha256: sha.clone(),
                role: "rom".into(),
                http_route: format!("/v1/files/network-fixture/{revision}/disc"),
            }],
            discs: vec![],
        };
        let catalog = Catalog {
            version: 1,
            library_id: "proof".into(),
            revision: "a".repeat(64),
            games: vec![descriptor.clone()],
        };
        catalog.validate().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (chunk_sent, chunk_ready) = std::sync::mpsc::channel();
        let (cancel_sent, cancel_ready) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let offset = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("range") {
                            return None;
                        }
                        value
                            .trim()
                            .strip_prefix("bytes=")
                            .and_then(|v| v.strip_suffix('-'))
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let range = if offset > 0 {
                    format!(
                        "Content-Range: bytes {offset}-{}/{}\r\n",
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    String::new()
                };
                let header = format!(
                    "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nETag: \"{}\"\r\n{range}Connection: close\r\n\r\n",
                    if offset > 0 { 206 } else { 200 },
                    body.len() - offset,
                    sha
                );
                stream.write_all(header.as_bytes()).unwrap();
                if attempt == 0 {
                    stream.write_all(&body[..65536]).unwrap();
                    chunk_sent.send(()).unwrap();
                    cancel_ready
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .unwrap();
                    let _ = stream.write_all(&body[65536..]);
                } else {
                    let _ = stream.write_all(&body[offset..]);
                }
            }
        });
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.json"),serde_json::to_vec(&serde_json::json!({"version":1,"library_id":"proof","address":address.to_string()})).unwrap()).unwrap();
        std::fs::write(
            state.join("catalog.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        activate_orbit_fixture(&mut app);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while network::active() {
                network::poll();
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let mut entries = Vec::new();
        network::merge_directory(&root.join("Roms/PS"), &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), path);
        assert!(!path.exists());
        let mut search = Vec::new();
        network::search("Network", &mut search);
        assert_eq!(search.len(), 1);
        assert_eq!(network::badge(&path).as_deref(), Some("NAS"));
        assert_eq!(
            network::download_id(&path).as_deref(),
            Some("network-fixture")
        );
        render_theme(&mut app);
        let mut cloud_frame = crate::view::orbit::Orbit::new();
        cloud_frame.prepare(&entries, 0);
        cloud_frame.draw(
            &entries,
            0,
            &mut app.display,
            &crate::theme_adapter::drawing_style(
                &app.res.get::<Stylesheet>(),
                crate::theme_adapter::snapshot(&app.res).as_ref(),
            ),
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(
                std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                    .join("native-network-card.png"),
            )
            .unwrap();
        app.view.search("Network Fixture".into()).unwrap();
        let (context_tx, mut context_rx) = tokio::sync::mpsc::channel(8);
        let mut bubble = VecDeque::new();
        app.view
            .handle_key_event(
                KeyEvent::Pressed(Key::Start),
                context_tx.clone(),
                &mut bubble,
            )
            .await
            .unwrap();
        render_theme(&mut app);
        app.display
            .pixmap()
            .to_owned()
            .save_png(
                std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                    .join("native-network-details.png"),
            )
            .unwrap();
        for key in [Key::X, Key::Y, Key::Select, Key::L, Key::R] {
            app.view
                .handle_key_event(KeyEvent::Pressed(key), context_tx.clone(), &mut bubble)
                .await
                .unwrap();
        }
        assert!(context_rx.try_recv().is_err());
        assert!(!network::active(), "Details must not download until Play");
        app.view
            .handle_key_event(KeyEvent::Pressed(Key::B), context_tx.clone(), &mut bubble)
            .await
            .unwrap();
        app.view
            .handle_key_event(
                KeyEvent::Pressed(Key::Select),
                context_tx.clone(),
                &mut bubble,
            )
            .await
            .unwrap();
        for _ in 0..5 {
            app.view
                .handle_key_event(
                    KeyEvent::Pressed(Key::Down),
                    context_tx.clone(),
                    &mut bubble,
                )
                .await
                .unwrap();
        }
        assert!(context_rx.try_recv().is_err());
        app.view.set_should_draw();
        render_theme(&mut app);
        let context_path =
            std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap())
                .join("native-network-download-context.png");
        app.display
            .pixmap()
            .to_owned()
            .save_png(&context_path)
            .unwrap();
        let context_frame = image::open(&context_path).unwrap().to_rgba8();
        assert_eq!(context_frame.dimensions(), (640, 480));
        let handle = crate::theme_adapter::snapshot(&app.res).unwrap();
        let color =
            crate::theme_adapter::drawing_style(&app.res.get::<Stylesheet>(), Some(&handle))
                .ui
                .background_color;
        // The declared full-screen scrim blends over the prepared underlying page.
        let scrim: common::display::color::Color = serde_json::from_value(
            serde_json::to_value(
                &handle.pack().tokens.colors[&common::theme::pack::ColorRole::Scrim],
            )
            .unwrap(),
        )
        .unwrap();
        let blend = |base: u8, overlay: u8| {
            ((base as f32 * (255 - scrim.a()) as f32 + overlay as f32 * scrim.a() as f32) / 255.0)
                .round() as u8
        };
        let background = [
            blend(color.r(), scrim.r()),
            blend(color.g(), scrim.g()),
            blend(color.b(), scrim.b()),
            255,
        ];
        assert_eq!(context_frame.get_pixel(639, 479).0, background);
        let context_components =
            &handle.pack().screens[&common::theme::pack::ScreenId::ContextMenu].components;
        let title_rect = context_components
            .iter()
            .find(|c| c.binding == Some(common::theme::pack::Binding::Title))
            .unwrap()
            .rect;
        let hints_rect = context_components
            .iter()
            .find(|c| c.kind == common::theme::pack::Kind::Hints)
            .unwrap()
            .rect;
        let dark_pixels = |x0, x1, y0, y1| {
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let pixel = context_frame.get_pixel(x, y);
                    pixel[0] < background[0].saturating_sub(5)
                        && pixel[1] < background[1].saturating_sub(5)
                        && pixel[2] < background[2].saturating_sub(5)
                })
                .count()
        };
        assert!(
            dark_pixels(
                title_rect.0,
                title_rect.0 + title_rect.2,
                title_rect.1,
                title_rect.1 + title_rect.3
            ) > 200,
            "selected game context title must be legible inside active descriptor"
        );
        assert!(
            dark_pixels(
                hints_rect.0,
                hints_rect.0 + hints_rect.2,
                hints_rect.1,
                hints_rect.1 + hints_rect.3
            ) > 50,
            "context hints must be legible inside active descriptor"
        );
        app.view.close_search_results();
        let mut game = Game::new(path.clone());
        let before = app.res.get::<Database>().select_games(&[&path]).unwrap()[0]
            .as_ref()
            .map_or(0, |g| g.play_count);
        assert!(
            app.res
                .get::<ConsoleMapper>()
                .launch_game(&app.res.get(), &mut game, false)
                .is_err()
        );
        fn file_snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        files.extend(file_snapshot(&path));
                    } else {
                        files.push((path.clone(), std::fs::read(path).unwrap()));
                    }
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        }
        let saves = root.join("Saves");
        let saves_before = file_snapshot(&saves);
        let game_info_before = std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok();
        let database_before = app.res.get::<Database>().select_games(&[&path]).unwrap();
        network::prefill("network-fixture").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while chunk_ready.try_recv().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "first download chunk was not sent"
            );
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(20);
        for key in [
            Key::X,
            Key::L,
            Key::R,
            Key::Select,
            Key::Start,
            Key::Left,
            Key::A,
        ] {
            let mut bubble = VecDeque::new();
            assert!(
                app.view
                    .handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut bubble)
                    .await
                    .unwrap()
            );
            assert!(rx.try_recv().is_err());
        }
        for _ in 0..20 {
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        render_theme(&mut app);
        assert!(network::draw(
            &mut app.display,
            &crate::theme_adapter::drawing_style(
                &app.res.get::<Stylesheet>(),
                crate::theme_adapter::snapshot(&app.res).as_ref()
            )
        ));
        let evidence = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-network-progress.png"))
            .unwrap();
        assert!(
            network::active(),
            "prefill cancellation needs a live worker"
        );
        network::event(KeyEvent::Pressed(Key::B)).unwrap();
        cancel_sent.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while network::active() {
            assert!(std::time::Instant::now() < deadline);
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(network::action().is_none());
        assert!(!path.exists());
        assert!(network::guard_launch(&path).is_err());
        assert_eq!(file_snapshot(&saves), saves_before);
        assert_eq!(
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok(),
            game_info_before
        );
        assert_eq!(
            app.res.get::<Database>().select_games(&[&path]).unwrap(),
            database_before
        );
        assert_eq!(
            app.res.get::<Database>().select_games(&[&path]).unwrap()[0]
                .as_ref()
                .map_or(0, |g| g.play_count),
            before
        );
        assert!(network::prefill("unknown-catalog-id").is_err());
        network::prefill("network-fixture").unwrap();
        assert!(network::action().is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while network::active() {
            assert!(std::time::Instant::now() < deadline);
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        network::poll();
        assert!(path.exists());
        assert_eq!(network::badge(&path).as_deref(), Some("Cached"));
        assert!(network::download_id(&path).is_none());
        assert!(network::action().is_none());
        assert!(network::guard_launch(&path).is_err());
        assert_eq!(
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok(),
            game_info_before
        );
        assert_eq!(
            app.res.get::<Database>().select_games(&[&path]).unwrap(),
            database_before
        );
        assert_eq!(file_snapshot(&saves), saves_before);
        render_theme(&mut app);
        assert!(network::draw(
            &mut app.display,
            &crate::theme_adapter::drawing_style(
                &app.res.get::<Stylesheet>(),
                crate::theme_adapter::snapshot(&app.res).as_ref()
            )
        ));
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-network-download-complete.png"))
            .unwrap();
        network::event(KeyEvent::Pressed(Key::B)).unwrap();
        assert!(network::begin(&game, false).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while network::active() {
            assert!(std::time::Instant::now() < deadline);
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        server.join().unwrap();
        let Some(network::Action::Launch(mut ready, reset)) = network::action() else {
            panic!("verified bundle must emit one UI launch continuation")
        };
        assert!(!reset);
        assert!(path.exists());
        let Some(Command::Exec(command)) = app
            .res
            .get::<ConsoleMapper>()
            .launch_game(&app.res.get(), &mut ready, reset)
            .unwrap()
        else {
            panic!("expected Exec payload")
        };
        assert!(command.get_args().any(|arg| arg == path.as_os_str()));
        network::finish_action();
        assert!(network::guard_launch(&path).is_err());
        assert!(network::action().is_none());
        common::game_info::GameInfo::delete().unwrap();
        let game_info_after_play =
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok();
        assert_eq!(
            app.res.get::<Database>().select_games(&[&path]).unwrap()[0]
                .as_ref()
                .unwrap()
                .play_count,
            before + 1
        );
        app.res.insert(Database::default());
        app.res.insert(Database::new().unwrap());
        assert_eq!(
            app.res.get::<Database>().select_games(&[&path]).unwrap()[0]
                .as_ref()
                .unwrap()
                .play_count,
            before + 1
        );
        let saves_after_play = file_snapshot(&saves);
        network::storage().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while network::active() {
            assert!(std::time::Instant::now() < deadline);
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        network::event(KeyEvent::Pressed(Key::X)).unwrap();
        app.view.set_should_draw();
        render_theme(&mut app);
        network::draw(
            &mut app.display,
            &crate::theme_adapter::drawing_style(
                &app.res.get::<Stylesheet>(),
                crate::theme_adapter::snapshot(&app.res).as_ref(),
            ),
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-network-storage.png"))
            .unwrap();
        network::event(KeyEvent::Pressed(Key::A)).unwrap();
        network::event(KeyEvent::Pressed(Key::B)).unwrap();
        assert!(path.exists());
        assert!(network::action().is_none());
        network::storage().unwrap();
        while network::active() {
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        network::event(KeyEvent::Pressed(Key::A)).unwrap();
        network::event(KeyEvent::Pressed(Key::A)).unwrap();
        while network::active() {
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!path.exists());
        assert_eq!(network::badge(&path).as_deref(), Some("NAS"));
        assert!(network::action().is_none());
        // A closed tiny loopback fixture proves the offline/error route never emits a continuation.
        network::prefill("network-fixture").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while network::active() {
            assert!(std::time::Instant::now() < deadline);
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(network::action().is_none());
        assert!(!path.exists());
        assert!(network::guard_launch(&path).is_err());
        assert_eq!(network::badge(&path).as_deref(), Some("Unavailable"));
        assert_eq!(file_snapshot(&saves), saves_after_play);
        assert_eq!(
            std::fs::read(common::constants::ALLIUM_GAME_INFO.as_path()).ok(),
            game_info_after_play
        );
        network::event(KeyEvent::Pressed(Key::B)).unwrap();
        // The backend's deterministic insufficient-space fixtures prove the space decision;
        // this render/input check proves that failure offers explicit storage rather than autoplay.
        network::failure(
            "Insufficient space. Keep 2 GiB free; choose a registered game to unload.".into(),
        );
        app.view.set_should_draw();
        render_theme(&mut app);
        network::draw(
            &mut app.display,
            &crate::theme_adapter::drawing_style(
                &app.res.get::<Stylesheet>(),
                crate::theme_adapter::snapshot(&app.res).as_ref(),
            ),
        );
        app.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-network-space.png"))
            .unwrap();
        network::event(KeyEvent::Pressed(Key::A)).unwrap();
        while network::active() {
            network::poll();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        network::event(KeyEvent::Pressed(Key::B)).unwrap();
        assert!(network::action().is_none());
        network::poll();
        assert!(!network::poll());
    }
}

#[cfg(all(
    test,
    feature = "perf-bench",
    not(any(feature = "miyoo", feature = "simulator"))
))]
#[path = "perf_bench.rs"]
mod perf_bench;

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
#[path = "present_dl15b_tests.rs"]
pub(crate) mod present_dl15b_tests;

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod launcher_osd_tests {
    use super::*;
    use common::{osd::{OSD_DURATION, OsdEvent, OsdKind}, theme::{frame::{FrameInputs, Hint},
        pack::Binding, render::PreparedFrame}};

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn socket_and_expiry_wakes_present_plate_then_restore_once_without_view_dirtying() {
        let (mut app, baseline, mut overlay) = super::present_dl15b_tests::launcher().await;
        assert!(!app.view.should_draw());
        let path = std::env::temp_dir().join(format!("allium-osd-loop-{}.sock", std::process::id()));
        let mut socket = Some(crate::osd::bind_at(&path).unwrap());
        // Unlink even if an assertion unwinds.
        struct SocketPath(std::path::PathBuf);
        impl Drop for SocketPath {
            fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
        }
        let _path = SocketPath(path.clone());
        for kind in [OsdKind::Volume, OsdKind::Brightness] {
            let event = OsdEvent::new(kind, 0.6, -1, 42);
            event.send_to(&path).unwrap();
            let wake = tokio::time::timeout(Duration::from_millis(250),
                crate::osd::wait(&mut socket, app.osd.deadline())).await.unwrap();
            assert!(matches!(wake, crate::osd::Wake::Event(received) if received == event));
            app.handle_osd_wake(wake);
            assert!(!app.view.should_draw(), "socket wake must dirty only the OSD");
            assert!(app.osd.is_active());
            let mut expected = DefaultPlatform::new().unwrap().display().unwrap();
            expected.pixmap_mut().data_mut().copy_from_slice(baseline.data());
            expected.record_presentations();
            app.osd.present(&mut expected).unwrap();
            let expected = expected.take_presented_frames().pop().unwrap();
            assert_ne!(expected.data(), baseline.data());
            app.present_frame(false, &mut overlay).await.unwrap();
            let frames = app.display.take_presented_frames();
            assert_eq!(frames.len(), 1, "the socket wake must cause a present");
            super::present_dl15b_tests::assert_frames(&frames, &expected, "OSD present");
            app.present_frame(false, &mut overlay).await.unwrap();
            assert!(app.display.take_presented_frames().is_empty());

            let wake = tokio::time::timeout(OSD_DURATION + Duration::from_millis(250),
                crate::osd::wait(&mut socket, app.osd.deadline())).await.unwrap();
            assert!(matches!(wake, crate::osd::Wake::Expired));
            app.handle_osd_wake(wake);
            assert!(!app.osd.is_active(), "the timer arm must expire the plate");
            app.present_frame(false, &mut overlay).await.unwrap();
            let frames = app.display.take_presented_frames();
            assert_eq!(frames.len(), 1, "expiry must present the clean frame exactly once");
            super::present_dl15b_tests::assert_frames(&frames, &baseline, "OSD expiry");
            assert!(tokio::time::timeout(Duration::from_millis(30),
                crate::osd::wait(&mut socket, app.osd.deadline())).await.is_err(),
                "idle OSD must have no timer wake");
            for _ in 0..3 {
                app.present_frame(false, &mut overlay).await.unwrap();
                assert!(app.display.take_presented_frames().is_empty(), "idle must not present");
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn themed_osd_is_last_in_each_present_survives_repaint_and_expires_once() {
        let (mut app, _, mut overlay) = super::present_dl15b_tests::launcher().await;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes/compact-plus").canonicalize().unwrap();
        let bank = common::theme::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
        let theme = common::theme::ThemeHandle::prepare(
            common::theme::runtime::ThemeSnapshot::load(&root).unwrap(), &bank, 100).unwrap();
        app.res.insert(Some(theme));
        app.view.set_should_draw();
        app.present_frame(false, &mut overlay).await.unwrap();
        let baseline = app.display.take_presented_frames().pop().unwrap();
        for kind in [OsdKind::Volume, OsdKind::Brightness] {
            let event = OsdEvent::new(kind, 0.6, -1, 42);
            app.receive_osd(event);
            assert!(!app.navigation_warm_allowed());
            let mut input = FrameInputs::new(kind.screen());
            input.text.insert(Binding::Title, app.res.get::<Locale>().t(kind.title_key(-1)));
            input.progress = Some(0.6);
            input.hints.push(Hint { button: "B".into(), label: "Back".into() });
            let theme = crate::theme_adapter::snapshot(&app.res).unwrap();
            let prepared = PreparedFrame::prepare_osd_overlay(theme, &input, false).unwrap();
            assert_eq!(prepared.bounds(), common::theme::pack::Rect(0, 0, 640, 480));
            let mut expected = baseline.clone();
            prepared.compose(&mut expected.as_mut());
            assert_ne!(expected.data(), baseline.data());
            for _ in 0..2 {
                app.view.set_should_draw();
                app.present_frame(false, &mut overlay).await.unwrap();
                let frames = app.display.take_presented_frames();
                assert_eq!(frames.len(), 1);
                super::present_dl15b_tests::assert_frames(&frames, &expected, "OSD present");
                assert!(app.display.pixmap().data() == baseline.data(), "logical frame must remain clean");
            }
            app.present_frame(false, &mut overlay).await.unwrap();
            assert!(app.display.take_presented_frames().is_empty(), "active OSD must not repaint on idle wakes");
            let wake = tokio::time::timeout(OSD_DURATION + Duration::from_millis(250),
                crate::osd::wait(&mut None, app.osd.deadline())).await.unwrap();
            assert!(matches!(wake, crate::osd::Wake::Expired));
            app.handle_osd_wake(wake);
            assert!(!app.osd.is_active(), "the timer arm must expire the plate");
            app.present_frame(false, &mut overlay).await.unwrap();
            let frames = app.display.take_presented_frames();
            assert_eq!(frames.len(), 1);
            super::present_dl15b_tests::assert_frames(&frames, &baseline, "OSD expiry");
            app.present_frame(false, &mut overlay).await.unwrap();
            assert!(app.display.take_presented_frames().is_empty(), "expiry must repaint only once");
        }
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
#[path = "navigation_warm_tests.rs"]
pub(crate) mod navigation_warm_tests;

#[cfg(all(
    test,
    feature = "perf-bench",
    not(any(feature = "miyoo", feature = "simulator"))
))]
mod integration_measurement_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(orbit_fixture)]
    async fn real_input_and_completed_flush_emit_generation_and_final_totals() {
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        crate::theme_adapter::choose(&app.res, Some("compact")).unwrap();
        for stem in ["R4 - Ridge Racer Type 4 (USA)", "Silent Hill (USA)"] {
            let game: common::database::NewGame =
                Game::new(ALLIUM_GAMES_DIR.join(format!("PS/{stem}.chd"))).into();
            app.res
                .get::<Database>()
                .update_games(&[game.clone()])
                .unwrap();
            app.res
                .get::<Database>()
                .increment_play_count(&game)
                .unwrap();
        }
        app.view = App::load_or_new(
            app.display.bounding_box(),
            app.res.clone(),
            app.platform.battery().unwrap(),
        )
        .unwrap();
        app.view.proof_select_tab(0);
        let mut overlay = false;
        app.present_frame(false, &mut overlay).await.unwrap();
        let path = std::env::temp_dir().join(format!(
            "allium-production-trace-{}.log",
            std::process::id()
        ));
        common::perf_trace::start_probe(path.to_str().unwrap());
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        app.apply_input_batch(
            KeyEvent::Pressed(Key::Down),
            &mut PendingInput::default(),
            &mut EnumMap::default(),
            &tx,
            &rx,
        )
        .await
        .unwrap();
        app.present_frame(false, &mut overlay).await.unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let feedback = rows
            .iter()
            .find(|row| row["event"] == "first_feedback")
            .unwrap();
        let final_content = rows
            .iter()
            .find(|row| row["event"] == "final_content")
            .unwrap();
        assert_eq!(feedback["selection_generation"], 1);
        assert_eq!(final_content["selection_generation"], 1);
        assert_eq!(feedback["measurement"]["input_key"], "Down");
        assert_eq!(feedback["measurement"]["input_edge"], "press");
        assert_eq!(feedback["measurement"]["input_generation"], 1);
        assert_eq!(
            final_content["measurement"]["first_feedback_monotonic_us"],
            final_content["measurement"]["final_content_monotonic_us"]
        );
        assert!(
            final_content["measurement"]["input"]["cpu_ns"][0]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            final_content["measurement"]["input"]["counters"][5]
                .as_u64()
                .unwrap()
                > 0
        );
        let frozen = final_content["measurement"]["input"].clone();
        app.view.idle_media_step();
        assert_eq!(
            common::perf_trace::probe_counts()["measurement"]["input"],
            frozen
        );
        for (event, edge, generation) in [
            (KeyEvent::Pressed(Key::B), "press", 2),
            (KeyEvent::Released(Key::B), "release", 3),
        ] {
            app.apply_input_batch(event, &mut PendingInput::default(),
                &mut EnumMap::default(), &tx, &rx).await.unwrap();
            let measurement = common::perf_trace::probe_counts()["measurement"].clone();
            assert_eq!(measurement["input_key"], "B");
            assert_eq!(measurement["input_edge"], edge);
            assert_eq!(measurement["input_generation"], generation);
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
#[cfg(feature = "perf-bench")]
#[path = "final_content_tests.rs"]
mod final_content_tests;
