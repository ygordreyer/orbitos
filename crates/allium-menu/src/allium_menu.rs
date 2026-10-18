use std::collections::VecDeque;

use anyhow::{Result, ensure};
use base32::encode;
use common::command::Command;
use common::constants::ALLIUM_SCREENSHOTS_DIR;
use common::database::Database;
use common::display::Display;
use common::game_info::GameInfo;
use common::geom;
use common::locale::{Locale, LocaleSettings};
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::theme::ThemeHandle;
use common::view::View;
use log::{info, warn};
use sha2::{Digest, Sha256};
use type_map::TypeMap;

use crate::retroarch_info::RetroArchInfo;
use crate::view::ingame_menu::IngameMenu;

// Constructors consume the same state file that prewarm and quit tests inspect.
#[cfg(test)]
pub(crate) static MENU_STATE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());


pub struct AlliumMenu<P>
where
    P: Platform,
{
    platform: P,
    display: P::Display,
    res: Resources,
    view: IngameMenu<P::Battery>,
    a_pressed: bool,
    b_pressed: bool,
    status: common::view::menu_status::MenuStatusCache,
    open_trace: Option<crate::open_trace::OpenTrace>,
    #[cfg(test)]
    menu_draws: usize,
    #[cfg(test)]
    theme_refreshes: usize,
}

impl AlliumMenu<DefaultPlatform> {
    pub async fn new(platform: DefaultPlatform, styles: Stylesheet) -> Result<Self> {
        Self::new_after_settle(platform, styles, None).await
    }

    pub async fn new_after_settle(
        mut platform: DefaultPlatform,
        styles: Stylesheet,
        deadline: Option<std::time::Instant>,
    ) -> Result<Self> {
        let display = display_after_settle(deadline, || platform.display()).await?;
        let battery = platform.battery()?;
        let status = common::view::menu_status::MenuStatusCache::new(platform.battery()?);
        let rect = display.bounding_box();

        let mut res = TypeMap::new();
        res.insert(Database::new()?);
        res.insert(GameInfo::default());
        res.insert(status.clone());
        res.insert(styles);
        res.insert(Locale::new(&LocaleSettings::load()?.lang));
        res.insert(Into::<geom::Size>::into(display.size()));
        res.insert(Option::<ThemeHandle>::None);
        let res = Resources::new(res);

        Ok(AlliumMenu {
            platform,
            display,
            res: res.clone(),
            view: IngameMenu::load_or_new(rect, res, battery, None).await?,
            a_pressed: false,
            b_pressed: false,
            status,
            open_trace: None,
            #[cfg(test)]
            menu_draws: 0,
            #[cfg(test)]
            theme_refreshes: 0,
        })
    }

    /// Prepare the menu for a new session with fresh RetroArchInfo.
    /// Call this before run_event_loop to refresh game info and state.
    pub async fn prepare(
        &mut self,
        info: Option<RetroArchInfo>,
        theme: Option<ThemeHandle>,
        game_info: GameInfo,
    ) -> Result<()> {
        if let Err(error) = self.platform.reset_input() {
            warn!("failed to reset menu input, keeping previous reader: {error:#}");
        }
        self.refresh_locale();
        self.res.insert(game_info);
        self.res.insert(theme);

        // Recreate the view with fresh info
        let rect = self.display.bounding_box();
        let battery = self.platform.battery()?;
        {
            let _phase = common::perf_trace::menu_phase("view_build");
            let mut next = IngameMenu::load_or_new(rect, self.res.clone(), battery, info).await?;
            next.reuse_theme_frame(&mut self.view);
            self.view = next;
        }

        {
            let _phase = common::perf_trace::menu_phase("first_frame_prepare");
            if let Err(error) = self.view.refresh_theme() {
                self.disable_theme(error);
            }
        }

        self.a_pressed = false;
        self.b_pressed = false;

        Ok(())
    }

    fn refresh_locale(&mut self) {
        let settings = std::fs::read(common::constants::ALLIUM_LOCALE_SETTINGS.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LocaleSettings>(&bytes).ok())
            .unwrap_or_default();
        if self.res.get::<Locale>().lang.to_string() != settings.lang {
            self.res.insert(Locale::new(&settings.lang));
        }
    }

    pub fn set_open_trace(&mut self, trace: crate::open_trace::OpenTrace) {
        self.open_trace = Some(trace);
    }

    pub async fn prewarm(
        &mut self,
        info: Option<RetroArchInfo>,
        theme: Option<ThemeHandle>,
        game_info: GameInfo,
    ) -> Result<()> {
        let state = IngameMenu::<<DefaultPlatform as Platform>::Battery>::read_only_state();
        if !IngameMenu::<<DefaultPlatform as Platform>::Battery>::can_prewarm(&state) {
            return Ok(());
        }
        self.refresh_locale();
        self.res.insert(game_info);
        self.res.insert(theme);
        let mut next = IngameMenu::new(
            self.display.bounding_box(),
            state,
            self.res.clone(),
            self.platform.battery()?,
            info,
        );
        next.reuse_theme_frame(&mut self.view);
        self.view = next;
        self.view.refresh_theme()
    }

    /// Save the menu state. Call this after run_event_loop returns.
    pub fn save(&mut self) -> Result<()> {
        self.view.save()
    }

    pub async fn run_event_loop(&mut self) -> Result<()> {
        self.run_event_loop_after_settle(None).await
    }

    pub async fn run_event_loop_after_settle(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<()> {
        let trace = *self
            .open_trace
            .get_or_insert_with(crate::open_trace::OpenTrace::start);
        {
            let _phase = trace.phase("remaining_settle_wait");
            if let Some(deadline) = deadline {
                tokio::time::sleep_until(deadline.into()).await;
            }
        }
        {
            let _phase = trace.phase("framebuffer_capture");
            let _scope = trace.scope();
            self.display.sync()?;
        }
        {
            let _phase = trace.phase("snapshot_clone");
            self.display.save()?;
        }
        if self.res.get::<Option<ThemeHandle>>().is_none() {
            let styles = self.res.get::<Stylesheet>().display_style();
            self.display.map_pixels(|pixel| {
                pixel.blend(styles.menu.background_color.overlay(pixel), 192)
            })?;
        }
        if self.res.get::<Option<ThemeHandle>>().is_some() {
            let _phase = trace.phase("snapshot_share");
            self.display.save_unchanged()?;
        } else {
            let _phase = trace.phase("snapshot_compare");
            self.display.save()?;
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        self.run_event_loop_with_queue(tx, rx).await
    }

    async fn run_event_loop_with_queue(
        &mut self,
        tx: tokio::sync::mpsc::Sender<Command>,
        mut rx: tokio::sync::mpsc::Receiver<Command>,
    ) -> Result<()> {
        let mut should_exit = false;
        let mut first_frame_flushed = false;

        loop {
            while let Ok(command) = rx.try_recv() {
                should_exit |= self.handle_command(command).await?;
            }
            if exit_released(should_exit, self.a_pressed, self.b_pressed) {
                return Ok(());
            }
            if !should_exit && self.view.should_draw() {
                #[cfg(test)]
                {
                    self.menu_draws += 1;
                }
                let drawn = {
                    let _scope = self.open_trace.map(|trace| trace.scope());
                    let _phase = common::perf_trace::open_phase("compose");
                    let _timing = common::perf_trace::menu_phase("compose");
                    self.view.draw(&mut self.display, &self.res.get())?
                };
                if drawn {
                    let _phase = common::perf_trace::menu_phase("flush");
                    let _scope = self.open_trace.map(|trace| trace.scope());
                    self.display.flush()?;
                    if !first_frame_flushed {
                        if let Some(trace) = self.open_trace.take() {
                            trace.first_present();
                        }
                        first_frame_flushed = true;
                        self.status.request_refresh();
                    }
                }
            }

            tokio::select! {
                Some(command) = rx.recv() => {
                    if self.handle_command(command).await? {
                        should_exit = true;
                    }
                }
                _ = self.status.changed(), if !should_exit && first_frame_flushed => {
                    if let Err(error) = self.view.refresh_status() {
                        self.disable_theme(error);
                        reset_active_background_for_legacy(&mut self.display, &self.res.get::<Stylesheet>())?;
                    }
                }
                event = self.platform.poll() => {
                    match event {
                        KeyEvent::Pressed(Key::A) => self.a_pressed = true,
                        KeyEvent::Released(Key::A) => self.a_pressed = false,
                        KeyEvent::Pressed(Key::B) => self.b_pressed = true,
                        KeyEvent::Released(Key::B) => self.b_pressed = false,
                        _ => {}
                    }
                    if should_exit { continue; }
                    should_exit |= self.handle_menu_event(event, tx.clone(), &mut rx).await?;
                }
                else => {}
            }

            // Exit only after A and B buttons are released
            if exit_released(should_exit, self.a_pressed, self.b_pressed) {
                info!("exiting menu, and b buttons released");
                return Ok(());
            }
        }
    }

    async fn handle_menu_event(
        &mut self,
        event: KeyEvent,
        tx: tokio::sync::mpsc::Sender<Command>,
        rx: &mut tokio::sync::mpsc::Receiver<Command>,
    ) -> Result<bool> {
        let mut bubble = VecDeque::new();
        self.view.handle_key_event(event, tx, &mut bubble).await?;
        let mut exiting = false;
        while let Ok(command) = rx.try_recv() {
            exiting |= self.handle_command(command).await?;
        }
        if !exiting {
            #[cfg(test)]
            {
                self.theme_refreshes += 1;
            }
            if let Err(error) = self.view.refresh_theme() {
                self.disable_theme(error);
                reset_active_background_for_legacy(
                    &mut self.display,
                    &self.res.get::<Stylesheet>(),
                )?;
            }
        }
        Ok(exiting)
    }

    /// Returns true if the menu should exit
    async fn handle_command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::Exit => {
                self.view.save()?;
                restore_display_after_menu(&mut self.display)?;
                return Ok(true);
            }
            Command::Redraw => {
                self.display.load(self.display.bounding_box())?;
                self.view.set_should_draw();
            }
            Command::SaveStateScreenshot { path, core, slot } => {
                if self.display.pop() {
                    self.display.load(self.display.bounding_box())?;
                    self.display.flush()?;

                    let mut hasher = Sha256::new();
                    hasher.update(&path);
                    hasher.update(&core);
                    hasher.update(slot.to_le_bytes());
                    let hash = hasher.finalize();
                    let base32 = encode(base32::Alphabet::Crockford, &hash);
                    let file_name = format!("{}.png", base32);

                    std::fs::create_dir_all(&*ALLIUM_SCREENSHOTS_DIR).ok();

                    let screenshot_path = ALLIUM_SCREENSHOTS_DIR.join(&file_name);
                    info!("saving screenshot to {:?}", screenshot_path);

                    {
                        let database = self.res.get::<Database>();
                        let game_path = std::path::Path::new(&path);
                        database
                            .update_screenshot_path(game_path, Some(&screenshot_path))
                            .ok();
                    }

                    #[cfg(feature = "miyoo")]
                    tokio::process::Command::new("screenshot")
                        .arg(screenshot_path)
                        .arg(format!(
                            "--width={}",
                            common::constants::SAVE_STATE_IMAGE_WIDTH
                        ))
                        .arg("--crop")
                        .spawn()?
                        .wait()
                        .await?;

                    #[cfg(feature = "simulator")]
                    std::fs::copy(
                        common::constants::ALLIUM_SD_ROOT.join("bg-640x480.png"),
                        screenshot_path,
                    )?;
                }
            }
            Command::RetroArchCommand(command) => {
                command.send().await?;
            }
            command => {
                warn!("unhandled command: {:?}", command);
            }
        }
        Ok(false)
    }

    fn disable_theme(&mut self, error: anyhow::Error) {
        warn!("failed to prepare themed menu frame, using Legacy for this session: {error:#}");
        // refresh_theme returns only after dropping every Resources guard. Mutate the shared
        // theme slot here, at the owning session boundary, then clear every active child frame.
        self.res.insert(Option::<ThemeHandle>::None);
        self.view.disable_theme();
    }
}

fn exit_released(exiting: bool, a_pressed: bool, b_pressed: bool) -> bool {
    exiting && !a_pressed && !b_pressed
}

async fn display_after_settle<D>(
    deadline: Option<std::time::Instant>,
    construct: impl FnOnce() -> Result<D>,
) -> Result<D> {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    }
    construct()
}

#[cfg(test)]
async fn capture_after_settle(
    display: &mut impl Display,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    }
    let _phase = common::perf_trace::menu_phase("framebuffer_capture");
    display.sync()
}

fn reset_active_background_for_legacy(
    display: &mut impl Display,
    styles: &Stylesheet,
) -> Result<()> {
    let bounds = display.bounding_box();
    // The active menu owns two saves: the original game frame and its drawing background.
    // Replace only the latter with the Legacy dim so the normal exit path still restores game.
    display.load(bounds)?;
    ensure!(display.pop(), "menu display background save is missing");
    display.load(bounds)?;
    let styles = styles.display_style();
    display.map_pixels(|pixel| pixel.blend(styles.menu.background_color.overlay(pixel), 192))?;
    display.save()?;
    Ok(())
}

fn restore_display_after_menu(display: &mut impl Display) -> Result<()> {
    display.pop();
    display.load(display.bounding_box())?;
    display.flush()?;
    display.pop();
    Ok(())
}

#[cfg(test)]
mod orbit_global_proof {
    use super::*;
    use crate::view::ingame_menu::IngameMenuState;
    use common::display::color::Color;
    use common::game_info::GameInfo;
    use tiny_skia::{Pixmap, PixmapMut, PixmapRef};

    struct StackDisplay {
        pixmap: Pixmap,
        saved: Vec<Pixmap>,
        flushes: usize,
        captured_at: Option<std::time::Instant>,
    }

    impl StackDisplay {
        fn new() -> Self {
            let mut pixmap = Pixmap::new(8, 8).unwrap();
            pixmap.fill(tiny_skia::Color::from_rgba8(180, 120, 60, 255));
            Self {
                pixmap,
                saved: Vec::new(),
                flushes: 0,
                captured_at: None,
            }
        }
    }

    impl Display for StackDisplay {
        fn width(&self) -> u32 {
            self.pixmap.width()
        }

        fn height(&self) -> u32 {
            self.pixmap.height()
        }

        fn pixmap(&self) -> PixmapRef<'_> {
            self.pixmap.as_ref()
        }

        fn pixmap_mut(&mut self) -> PixmapMut<'_> {
            self.pixmap.as_mut()
        }

        fn map_pixels<F>(&mut self, mut f: F) -> Result<()>
        where
            F: FnMut(Color) -> Color,
        {
            for pixel in self.pixmap.pixels_mut() {
                *pixel = f((*pixel).into()).into();
            }
            Ok(())
        }

        fn sync(&mut self) -> Result<()> {
            self.captured_at = Some(std::time::Instant::now());
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            self.flushes += 1;
            Ok(())
        }

        fn save(&mut self) -> Result<()> {
            self.saved.push(self.pixmap.clone());
            Ok(())
        }

        fn load(&mut self, _area: geom::Rect) -> Result<()> {
            self.pixmap = self.saved.last().unwrap().clone();
            Ok(())
        }

        fn pop(&mut self) -> bool {
            self.saved.pop();
            !self.saved.is_empty()
        }
    }

    #[tokio::test]
    async fn perf6_release_metadata_replaces_seed_before_any_menu_draw() {
        let _state_guard = MENU_STATE_TEST_LOCK.lock().await;
        let root =
            std::env::temp_dir().join(format!("perf6-fresh-metadata-{}", std::process::id()));
        std::fs::create_dir_all(root.join("state")).unwrap();
        let mut runtime = common::theme::ThemeRuntime::bind(
            &root.join("state"),
            &root.join("themes"),
            &common::constants::ALLIUM_FONTS_DIR,
        )
        .unwrap();
        runtime.choose("orbit").unwrap();
        let theme = runtime
            .activate(common::theme::Boundary::MenuOpening)
            .snapshot;
        let mut styles = Stylesheet::load().unwrap();
        styles.status_bar.show_clock = false;
        let mut app = AlliumMenu::new(DefaultPlatform::new().unwrap(), styles.clone())
            .await
            .unwrap();
        let mut reference = AlliumMenu::new(DefaultPlatform::new().unwrap(), styles.clone())
            .await
            .unwrap();
        let game = || GameInfo {
            name: "Metadata guard".into(),
            ..Default::default()
        };
        let info = |slot| {
            Some(crate::RetroArchInfo {
                max_disk_slots: 1,
                disk_slot: 0,
                state_slot: Some(slot),
            })
        };
        app.prewarm(info(0), theme.clone(), game()).await.unwrap();
        assert_eq!(app.menu_draws, 0);
        app.prepare(info(7), theme.clone(), game()).await.unwrap();
        app.view.draw(&mut app.display, &styles).unwrap();
        reference
            .prepare(info(7), theme.clone(), game())
            .await
            .unwrap();
        let background = reference.display.pixmap().data().to_vec();
        reference
            .view
            .draw(&mut reference.display, &styles)
            .unwrap();
        assert_eq!(
            app.display.pixmap().data(),
            reference.display.pixmap().data()
        );
        reference.prepare(info(0), theme, game()).await.unwrap();
        reference
            .display
            .pixmap_mut()
            .data_mut()
            .copy_from_slice(&background);
        reference
            .view
            .draw(&mut reference.display, &styles)
            .unwrap();
        assert_ne!(
            app.display.pixmap().data(),
            reference.display.pixmap().data()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn perf6_prewarm_keeps_display_and_state_read_only() {
        let _state_guard = MENU_STATE_TEST_LOCK.lock().await;
        let mut app = AlliumMenu::new(DefaultPlatform::new().unwrap(), Stylesheet::load().unwrap())
            .await
            .unwrap();
        let path = common::constants::ALLIUM_MENU_STATE.as_path();
        let saved = std::fs::read(path).ok();
        let invalid = b"invalid speculative state";
        std::fs::write(path, invalid).unwrap();
        let pixels = app.display.pixmap().data().to_vec();
        app.prewarm(
            Some(crate::RetroArchInfo {
                max_disk_slots: 1,
                disk_slot: 0,
                state_slot: Some(3),
            }),
            None,
            GameInfo {
                name: "Prewarm".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), invalid);
        assert_eq!(app.display.pixmap().data(), pixels);
        assert_eq!(app.menu_draws, 0);
        assert!(app.open_trace.is_none());
        match saved {
            Some(bytes) => std::fs::write(path, bytes).unwrap(),
            None => std::fs::remove_file(path).unwrap(),
        }
    }

    #[tokio::test]
    async fn perf5_pending_exit_restores_without_menu_draw_or_flush() {
        let _state_guard = MENU_STATE_TEST_LOCK.lock().await;
        let mut menu =
            AlliumMenu::new(DefaultPlatform::new().unwrap(), Stylesheet::load().unwrap())
                .await
                .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        tx.send(Command::Exit).await.unwrap();
        menu.run_event_loop_with_queue(tx, rx).await.unwrap();
        assert_eq!(
            menu.menu_draws, 0,
            "queued Exit drew a menu frame before restore"
        );
    }

    #[tokio::test]
    async fn perf5_menu_press_skips_theme_refresh_and_keeps_release_barrier() {
        let _state_guard = MENU_STATE_TEST_LOCK.lock().await;
        let mut menu =
            AlliumMenu::new(DefaultPlatform::new().unwrap(), Stylesheet::load().unwrap())
                .await
                .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        assert!(
            menu.handle_menu_event(KeyEvent::Pressed(Key::Menu), tx, &mut rx)
                .await
                .unwrap()
        );
        assert_eq!(menu.menu_draws, 0);
        assert_eq!(menu.theme_refreshes, 0);
        for (a, b) in [(false, true), (true, false), (true, true)] {
            assert!(!exit_released(true, a, b));
        }
        assert!(exit_released(true, false, false));
        assert!(!exit_released(false, false, false));
    }

    #[tokio::test]
    async fn perf5b_standalone_construction_capture_and_flush_wait_for_settle() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        let mut display = display_after_settle(Some(deadline), || {
            let mut display = StackDisplay::new();
            display.sync()?;
            Ok(display)
        })
        .await
        .unwrap();
        assert!(
            display.captured_at.unwrap() >= deadline,
            "construction captured during settle"
        );
        capture_after_settle(&mut display, Some(deadline))
            .await
            .unwrap();
        display.flush().unwrap();
        assert!(display.captured_at.unwrap() >= deadline);
        assert!(std::time::Instant::now() >= deadline);
    }

    #[tokio::test]
    async fn perf5_capture_and_flush_wait_for_settle_deadline() {
        let mut display = StackDisplay::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        let prepared = std::time::Instant::now();
        assert!(
            prepared < deadline,
            "offscreen preparation can start during settle"
        );
        capture_after_settle(&mut display, Some(deadline))
            .await
            .unwrap();
        display.flush().unwrap();
        assert!(
            display.captured_at.unwrap() >= deadline,
            "capture preceded settle deadline"
        );
        assert!(
            std::time::Instant::now() >= deadline,
            "flush preceded settle deadline"
        );
    }

    #[test]
    fn late_theme_fallback_keeps_normal_two_level_display_unwind() {
        let styles = Stylesheet::load().unwrap();
        let mut display = StackDisplay::new();
        let original = display.pixmap.data().to_vec();
        display.save().unwrap();
        display.save().unwrap();
        display.clear(Color::new(255, 0, 0)).unwrap();

        reset_active_background_for_legacy(&mut display, &styles).unwrap();
        assert_eq!(display.saved.len(), 2);
        assert_ne!(display.pixmap.data(), original);

        restore_display_after_menu(&mut display).unwrap();
        assert_eq!(display.saved.len(), 0);
        assert_eq!(display.pixmap.data(), original);
        assert_eq!(display.flushes, 1);
    }

    #[tokio::test]
    async fn native_orbit_global_menu_preserves_raw_theme() {
        let _state_guard = MENU_STATE_TEST_LOCK.lock().await;
        assert!(common::constants::ALLIUM_SD_ROOT.ends_with("native-orbit-fixture"));
        assert!(!common::constants::ALLIUM_SD_ROOT.starts_with("/Volumes"));
        let styles = Stylesheet::load().unwrap();
        let raw = serde_json::to_vec(&styles).unwrap();
        let mut menu = AlliumMenu::new(DefaultPlatform::new().unwrap(), styles)
            .await
            .unwrap();
        menu.res.insert(GameInfo {
            name: "Ridge Racer Type 4".into(),
            has_menu: true,
            ..Default::default()
        });
        menu.view = IngameMenu::new(
            menu.display.bounding_box(),
            IngameMenuState::default(),
            menu.res.clone(),
            menu.platform.battery().unwrap(),
            Some(crate::RetroArchInfo {
                max_disk_slots: 4,
                disk_slot: 1,
                state_slot: Some(0),
            }),
        );
        let display_styles = menu.res.get::<Stylesheet>().display_style();
        menu.display
            .clear(display_styles.menu.background_color)
            .unwrap();
        menu.view
            .draw(&mut menu.display, &menu.res.get::<Stylesheet>())
            .unwrap();
        let evidence = std::path::PathBuf::from(std::env::var("ALLIUM_ORBIT_PROOF_DIR").unwrap());
        std::fs::create_dir_all(&evidence).unwrap();
        menu.display
            .pixmap()
            .to_owned()
            .save_png(evidence.join("native-orbit-global-ingame-menu.png"))
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&*menu.res.get::<Stylesheet>()).unwrap(),
            raw
        );
    }
}
