use std::collections::{HashMap, VecDeque};
use std::fs;
use std::fs::File;
use std::marker::PhantomData;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use base32::encode;
use common::battery::Battery;
use common::command::Command;
use common::constants::{ALLIUM_MENU_STATE, ALLIUM_SCREENSHOTS_DIR, SAVE_STATE_IMAGE_WIDTH};
use common::display::Display;
use common::game_info::GameInfo;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::retroarch::RetroArchCommand;
use common::stylesheet::Stylesheet;
use common::theme::{
    ThemeHandle,
    frame::{Focus, FrameInputs, Hint, Row as ThemeRow},
    pack::{Binding, ScreenId},
    render::PreparedFrame,
};
use common::view::{
    ButtonHint, ButtonHints, Image, ImageMode, Label, NullView, SettingsList, StatusBar, View,
};
use log::warn;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::Sender;

use crate::retroarch_info::RetroArchInfo;
use crate::view::guide_selector::GuideSelector;

#[derive(Serialize, Deserialize, Default)]
pub struct IngameMenuState {
    is_guide_open: bool,
    is_text_reader_open: bool,
    selected_guide_path: Option<PathBuf>,
}

#[derive(PartialEq)]
struct MenuFrameKey {
    digest: String,
    generation: u64,
    locale: String,
    game_path: PathBuf,
    input: FrameInputs,
}

pub struct IngameMenu<B>
where
    B: Battery + 'static,
{
    rect: Rect,
    res: Resources,
    name: Label<String>,
    status_bar: StatusBar<B>,
    menu: SettingsList,
    guide_selector: Option<GuideSelector>,
    button_hints: ButtonHints<String>,
    entries: Vec<MenuEntry>,
    retroarch_info: Option<RetroArchInfo>,
    path: PathBuf,
    image: Image,
    theme_frame: Option<PreparedFrame>,
    theme_key: Option<MenuFrameKey>,
    #[cfg(test)]
    theme_prepares: usize,
    theme_dirty: bool,
    _phantom_battery: PhantomData<B>,
}

impl<B> IngameMenu<B>
where
    B: Battery + 'static,
{
    pub fn new(
        rect: Rect,
        state: IngameMenuState,
        res: Resources,
        battery: B,
        retroarch_info: Option<RetroArchInfo>,
    ) -> Self {
        let Rect { x, y, w, .. } = rect;

        let game_info = res.get::<GameInfo>();
        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();

        let name = Label::new(
            Point::new(x + styles.ui.margin_x, y + styles.ui.margin_y),
            game_info.name.clone(),
            Alignment::Left,
            None,
        );

        let status_phase = common::perf_trace::menu_phase("status_bar");
        let status_point = Point::new(w as i32 - styles.ui.margin_y, y + styles.ui.margin_y);
        let cache = res
            .0
            .borrow()
            .get::<common::view::menu_status::MenuStatusCache>()
            .cloned();
        let mut status_bar = if let Some(cache) = cache {
            StatusBar::new_cached(res.clone(), status_point, cache.clone())
        } else {
            StatusBar::new(res.clone(), status_point, battery)
        };

        drop(status_phase);
        let mut button_hints = ButtonHints::new(
            res.clone(),
            vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::Menu,
                locale.t("ingame-menu-continue"),
                Alignment::Left,
            )],
            vec![
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::A,
                    locale.t("button-select"),
                    Alignment::Right,
                ),
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::B,
                    locale.t("button-back"),
                    Alignment::Right,
                ),
            ],
        );

        let status_bar_rect = status_bar.bounding_box(&styles);
        let button_hints_rect = button_hints.bounding_box(&styles);
        let content_top = y
            + styles.ui.margin_y
            + styles.ui.ui_font.size.max(status_bar_rect.h) as i32
            + styles.ui.margin_y / 2;
        let content_height = (button_hints_rect.y - content_top) as u32;

        let entries = MenuEntry::entries(retroarch_info.as_ref(), !game_info.guides.is_empty());
        let mut menu = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                content_top,
                w - SAVE_STATE_IMAGE_WIDTH - styles.ui.margin_x as u32 * 3,
                content_height,
            ),
            entries.iter().map(|e| e.as_str(&locale)).collect(),
            entries
                .iter()
                .map(|_| Box::new(NullView) as Box<dyn View>)
                .collect(),
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        if let Some(info) = retroarch_info.as_ref()
            && info.max_disk_slots > 1
            && !state.is_text_reader_open
        {
            let mut map = HashMap::new();
            map.insert("disk".into(), (info.disk_slot + 1).into());
            menu.set_right(
                MenuEntry::Continue.index(retroarch_info.as_ref(), !game_info.guides.is_empty()),
                Box::new(Label::new(
                    Point::zero(),
                    locale.ta("ingame-menu-disk", &map),
                    Alignment::Right,
                    None,
                )),
            );
        }

        let mut image = Image::empty(
            Rect::new(
                x + w as i32 - SAVE_STATE_IMAGE_WIDTH as i32 - styles.ui.margin_y * 2,
                content_top,
                SAVE_STATE_IMAGE_WIDTH,
                content_height,
            ),
            ImageMode::Contain,
        );
        image.set_border_radius(12);
        image.set_alignment(Alignment::Right);

        let mut guide_selector = None;
        if state.is_guide_open || state.is_text_reader_open {
            menu.select(
                entries
                    .iter()
                    .position(|e| *e == MenuEntry::Guide)
                    .unwrap_or(0),
            );

            // Select the guide by path if previously selected
            let selected = if let Some(path) = &state.selected_guide_path
                && let Some(idx) = game_info.guides.iter().position(|p| p == path)
            {
                Some(idx)
            } else if game_info.guides.len() == 1 {
                Some(0)
            } else {
                None
            };

            if let Some(selected) = selected {
                let selector_rect = Rect::new(
                    x,
                    content_top,
                    rect.w,
                    (button_hints_rect.y - content_top) as u32,
                );
                guide_selector = Some(GuideSelector::new(
                    selector_rect,
                    res.clone(),
                    game_info.guides.clone(),
                    selected,
                    state.is_text_reader_open,
                ));
            }
        }

        let path = game_info.path.clone();

        drop(game_info);
        drop(locale);
        drop(styles);

        Self {
            rect,
            res,
            name,
            status_bar,
            menu,
            guide_selector,
            button_hints,
            entries,
            retroarch_info,
            path,
            image,
            theme_frame: None,
            theme_key: None,
            #[cfg(test)]
            theme_prepares: 0,
            theme_dirty: false,
            _phantom_battery: PhantomData,
        }
    }

    pub async fn load_or_new(
        rect: Rect,
        res: Resources,
        battery: B,
        info: Option<RetroArchInfo>,
    ) -> Result<Self> {
        if ALLIUM_MENU_STATE.exists() {
            let file = File::open(ALLIUM_MENU_STATE.as_path())?;
            if let Ok(state) = serde_json::from_reader::<_, IngameMenuState>(file) {
                return Ok(Self::new(rect, state, res, battery, info));
            }
            warn!("failed to deserialize state file, deleting");
            fs::remove_file(ALLIUM_MENU_STATE.as_path())?;
        }

        Ok(Self::new(rect, Default::default(), res, battery, info))
    }

    pub(crate) fn read_only_state() -> IngameMenuState {
        fs::read(ALLIUM_MENU_STATE.as_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub(crate) fn can_prewarm(state: &IngameMenuState) -> bool {
        !state.is_guide_open && !state.is_text_reader_open
    }

    pub fn save(&self) -> Result<()> {
        let file = File::create(ALLIUM_MENU_STATE.as_path())?;
        let state = IngameMenuState {
            is_guide_open: self.guide_selector.is_some(),
            is_text_reader_open: self
                .guide_selector
                .as_ref()
                .is_some_and(|s| s.is_text_reader_open()),
            selected_guide_path: self
                .guide_selector
                .as_ref()
                .and_then(|s| s.selected_path().map(|p| p.to_path_buf())),
        };
        if let Some(selector) = &self.guide_selector
            && let Some(reader) = selector.text_reader()
        {
            reader.save_cursor();
        }
        serde_json::to_writer(file, &state)?;
        Ok(())
    }

    pub(crate) fn reuse_theme_frame(&mut self, previous: &mut Self) {
        self.theme_frame = previous.theme_frame.take();
        self.theme_key = previous.theme_key.take();
    }

    pub(crate) fn refresh_status(&mut self) -> Result<()> {
        let previous = self.status_bar.theme_inputs();
        self.status_bar.update(std::time::Duration::ZERO);
        if self.status_bar.theme_inputs() != previous {
            self.refresh_theme()?;
        }
        Ok(())
    }

    pub fn refresh_theme(&mut self) -> Result<()> {
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let Some(theme) = theme else {
            self.theme_frame = None;
            self.theme_key = None;
            self.theme_dirty = false;
            self.menu.set_visible_count_override(None)?;
            return Ok(());
        };
        let status = self.status_bar.theme_inputs();
        if let Some(selector) = &mut self.guide_selector {
            selector.refresh_theme(status)?;
            self.theme_frame = None;
            self.theme_key = None;
            self.theme_dirty = false;
            return Ok(());
        }
        let count = theme_capacity(&theme, ScreenId::IngameMenu, Binding::Entries);
        self.menu.set_visible_count_override(Some(count))?;
        let first = self.menu.window_start();
        let selected = self.menu.selected();
        let locale = self.res.get::<Locale>();
        let game = self.res.get::<GameInfo>();
        let mut input = FrameInputs::new(ScreenId::IngameMenu);
        input.text.insert(Binding::Title, game.name.clone());
        super::game_context(&mut input, &self.res, &theme);
        let rows = self
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(count)
            .map(|(_, entry)| ThemeRow {
                subtitle: String::new(),
                id: entry.theme_id().into(),
                label: entry.as_str(&locale),
                value: self.entry_value(*entry, &locale),
                enabled: true,
            })
            .collect();
        input.rows.insert(Binding::Entries, rows);
        input.focus = Focus {
            component: theme_component(&theme, ScreenId::IngameMenu, Binding::Entries),
            item: self
                .entries
                .get(selected)
                .map(|entry| entry.theme_id().into()),
        };
        input
            .selected
            .insert(Binding::Entries, self.entries[selected].theme_id().into());
        input.text.insert(Binding::Subtitle, String::new());
        input.hints = vec![
            Hint {
                button: "MENU".into(),
                label: locale.t("ingame-menu-continue"),
            },
            Hint {
                button: "A".into(),
                label: locale.t("button-select"),
            },
            Hint {
                button: "B".into(),
                label: locale.t("button-back"),
            },
        ];
        input.status = status;
        let key = MenuFrameKey {
            digest: theme.digest().into(),
            generation: theme.generation(),
            locale: locale.lang.to_string(),
            game_path: game.path.clone(),
            input,
        };
        drop(game);
        drop(locale);
        if self.theme_key.as_ref() == Some(&key) && self.theme_frame.is_some() {
            let _phase = common::perf_trace::menu_phase("frame_cache_hit");
            self.theme_dirty = true;
            return Ok(());
        }
        self.theme_key = None;
        #[cfg(test)]
        {
            self.theme_prepares += 1;
        }
        let _phase = common::perf_trace::menu_phase("frame_prepare");
        self.theme_frame = Some(PreparedFrame::prepare_reusing(
            theme,
            &key.input,
            self.theme_frame.take(),
        )?);
        self.theme_key = Some(key);
        self.theme_dirty = true;
        Ok(())
    }

    pub(crate) fn disable_theme(&mut self) {
        self.theme_frame = None;
        self.theme_key = None;
        self.theme_dirty = false;
        if let Err(error) = self.menu.set_visible_count_override(None) {
            warn!("failed to reset Legacy menu viewport: {error:#}");
        }
        if let Some(selector) = &mut self.guide_selector {
            selector.disable_theme();
        }
        self.name.set_should_draw();
        self.status_bar.set_should_draw();
        self.menu.set_should_draw();
        self.image.set_should_draw();
        self.button_hints.set_should_draw();
    }

    fn entry_value(&self, entry: MenuEntry, locale: &Locale) -> String {
        let Some(info) = self.retroarch_info.as_ref() else {
            return String::new();
        };
        if entry == MenuEntry::Continue && info.max_disk_slots > 1 {
            let mut map = HashMap::new();
            map.insert("disk".into(), (info.disk_slot + 1).into());
            return locale.ta("ingame-menu-disk", &map);
        }
        if matches!(entry, MenuEntry::Save | MenuEntry::Load)
            && let Some(slot) = info.state_slot
        {
            if slot == -1 {
                return locale.t("ingame-menu-slot-auto");
            }
            let mut map = HashMap::new();
            map.insert("slot".into(), slot.into());
            return locale.ta("ingame-menu-slot", &map);
        }
        String::new()
    }

    async fn select_entry(&mut self, commands: Sender<Command>) -> Result<bool> {
        let selected = self.entries[self.menu.selected()];
        match selected {
            MenuEntry::Continue => {
                commands.send(Command::Exit).await?;
            }
            MenuEntry::Save => {
                let slot = self.retroarch_info.as_ref().unwrap().state_slot.unwrap();
                RetroArchCommand::SaveStateSlot(slot).send().await?;
                let core = self.res.get::<GameInfo>().core.to_owned();
                commands
                    .send(Command::SaveStateScreenshot {
                        path: self.path.canonicalize()?.to_string_lossy().to_string(),
                        core,
                        slot,
                    })
                    .await?;
                commands.send(Command::Exit).await?;
            }
            MenuEntry::Load => {
                RetroArchCommand::LoadStateSlot(
                    self.retroarch_info.as_ref().unwrap().state_slot.unwrap(),
                )
                .send()
                .await?;
                commands.send(Command::Exit).await?;
            }
            MenuEntry::Reset => {
                #[cfg(feature = "miyoo")]
                common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Game);
                RetroArchCommand::Unpause.send().await?;
                RetroArchCommand::Reset.send().await?;
                commands.send(Command::Exit).await?;
            }
            MenuEntry::Guide => {
                let game_info = self.res.get::<GameInfo>();
                let guides = game_info.guides.clone();
                drop(game_info);

                if !guides.is_empty() {
                    let styles = self.res.get::<Stylesheet>();
                    let status_bar_rect = self.status_bar.bounding_box(&styles);
                    let button_hints_rect = self.button_hints.bounding_box(&styles);
                    let content_top = self.rect.y
                        + styles.ui.margin_y
                        + styles.ui.ui_font.size.max(status_bar_rect.h) as i32
                        + styles.ui.margin_y / 2;
                    let selector_rect = Rect::new(
                        self.rect.x,
                        content_top,
                        self.rect.w,
                        (button_hints_rect.y - content_top) as u32,
                    );
                    self.guide_selector = Some(GuideSelector::new(
                        selector_rect,
                        self.res.clone(),
                        guides,
                        0,
                        false,
                    ));
                }
            }
            MenuEntry::Settings => {
                #[cfg(feature = "miyoo")]
                common::platform::cpu_tuning::set(common::platform::cpu_tuning::Workload::Game);
                RetroArchCommand::Unpause.send().await?;
                RetroArchCommand::MenuToggle.send().await?;
                commands.send(Command::Exit).await?;
            }
            MenuEntry::Quit => {
                if self.retroarch_info.is_some() {
                    let core = self.res.get::<GameInfo>().core.to_owned();
                    commands
                        .send(Command::SaveStateScreenshot {
                            path: self.path.canonicalize()?.to_string_lossy().to_string(),
                            core,
                            slot: -1,
                        })
                        .await?;
                    // Send this as a command so we are sure the screenshot succeeds first.
                    commands
                        .send(Command::RetroArchCommand(RetroArchCommand::Quit))
                        .await?;
                } else {
                    tokio::process::Command::new("pkill")
                        .arg("retroarch")
                        .spawn()?
                        .wait()
                        .await?;
                }
                commands.send(Command::Exit).await?;
            }
        }
        Ok(true)
    }

    fn update_state_slot_label(&mut self, state_slot: i8) {
        if state_slot == -1 {
            self.menu.set_right(
                self.menu.selected(),
                Box::new(Label::new(
                    Point::zero(),
                    self.res.get::<Locale>().t("ingame-menu-slot-auto"),
                    Alignment::Right,
                    None,
                )),
            );
        } else {
            let mut map = HashMap::new();
            map.insert("slot".into(), state_slot.into());
            self.menu.set_right(
                self.menu.selected(),
                Box::new(Label::new(
                    Point::zero(),
                    self.res.get::<Locale>().ta("ingame-menu-slot", &map),
                    Alignment::Right,
                    None,
                )),
            );
        }

        let path = self
            .path
            .canonicalize()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let slot = self.retroarch_info.as_ref().unwrap().state_slot.unwrap();

        let mut hasher = Sha256::new();
        hasher.update(&path);
        hasher.update(&self.res.get::<GameInfo>().core);
        hasher.update(slot.to_le_bytes());
        let hash = hasher.finalize();
        let base32 = encode(base32::Alphabet::Crockford, &hash);
        let file_name = format!("{}.png", base32);
        let mut screenshot_path = ALLIUM_SCREENSHOTS_DIR.join(file_name);

        // Previously, the hash did not include the core name. We try looking for that path as well.
        if !screenshot_path.exists() {
            let mut hasher = Sha256::new();
            hasher.update(&path);
            hasher.update(slot.to_le_bytes());
            let hash = hasher.finalize();
            let base32 = encode(base32::Alphabet::Crockford, &hash);
            let file_name = format!("{}.png", base32);
            screenshot_path = ALLIUM_SCREENSHOTS_DIR.join(file_name);
        }

        self.image.set_path(Some(screenshot_path));
    }
}

#[async_trait(?Send)]
impl<B> View for IngameMenu<B>
where
    B: Battery,
{
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(frame) = &self.theme_frame {
            if !self.theme_dirty {
                return Ok(false);
            }
            if !frame.is_opaque() {
                // The saved background is pristine; blending over the last menu compounds its scrim.
                display.load(display.bounding_box())?;
            }
            frame.paint(display);
            self.theme_dirty = false;
            return Ok(true);
        }
        let display_styles = styles.display_style();
        let styles = &display_styles;
        let mut drawn = false;

        drawn |= self.name.should_draw() && self.name.draw(display, styles)?;
        drawn |= self.status_bar.should_draw() && self.status_bar.draw(display, styles)?;
        if let Some(selector) = &mut self.guide_selector {
            drawn |= selector.should_draw() && selector.draw(display, styles)?;
        } else {
            drawn |= self.menu.should_draw() && self.menu.draw(display, styles)?;
            drawn |= self.image.should_draw() && self.image.draw(display, styles)?;
            if self.button_hints.should_draw() {
                display.load(self.button_hints.bounding_box(styles))?;
                drawn |= self.button_hints.draw(display, styles)?;
            }
        }

        #[cfg(feature = "debug-ui")]
        if drawn {
            common::view::draw_debug_bounds(self, display, styles, 0)?;
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        if self.theme_frame.is_some() {
            return self.theme_dirty;
        }
        self.name.should_draw()
            || self.status_bar.should_draw()
            || if let Some(selector) = &self.guide_selector {
                selector.should_draw()
            } else {
                self.menu.should_draw()
                    || self.image.should_draw()
                    || self.button_hints.should_draw()
            }
    }

    fn set_should_draw(&mut self) {
        if self.theme_frame.is_some() {
            self.theme_dirty = true;
            return;
        }
        self.name.set_should_draw();
        self.status_bar.set_should_draw();
        if let Some(selector) = &mut self.guide_selector {
            selector.set_should_draw();
        } else {
            self.menu.set_should_draw();
            self.image.set_should_draw();
            self.button_hints.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if event == KeyEvent::Pressed(Key::Menu) {
            commands.send(Command::Exit).await?;
            return Ok(true);
        }

        if let Some(selector) = &mut self.guide_selector
            && selector
                .handle_key_event(event, commands.clone(), bubble)
                .await?
        {
            bubble.retain(|cmd| match cmd {
                Command::CloseView => {
                    self.guide_selector = None;
                    self.set_should_draw();
                    false
                }
                _ => true,
            });
            return Ok(true);
        }

        let selected = self.menu.selected();

        // Handle disk slot selection
        if let Some(ref mut info) = self.retroarch_info {
            if info.max_disk_slots > 1 && selected == MenuEntry::Continue as usize {
                match event {
                    KeyEvent::Pressed(Key::Left) | KeyEvent::Autorepeat(Key::Left) => {
                        info.disk_slot = info.disk_slot.saturating_sub(1);
                        RetroArchCommand::SetDiskSlot(info.disk_slot).send().await?;

                        let mut map = HashMap::new();
                        map.insert("disk".into(), (info.disk_slot + 1).into());
                        self.menu.set_right(
                            self.menu.selected(),
                            Box::new(Label::new(
                                Point::zero(),
                                self.res.get::<Locale>().ta("ingame-menu-disk", &map),
                                Alignment::Right,
                                None,
                            )),
                        );
                        return Ok(true);
                    }
                    KeyEvent::Pressed(Key::Right) | KeyEvent::Autorepeat(Key::Right) => {
                        info.disk_slot = (info.disk_slot + 1).min(info.max_disk_slots - 1);
                        RetroArchCommand::SetDiskSlot(info.disk_slot).send().await?;

                        let mut map = HashMap::new();
                        map.insert("disk".into(), (info.disk_slot + 1).into());
                        self.menu.set_right(
                            self.menu.selected(),
                            Box::new(Label::new(
                                Point::zero(),
                                self.res.get::<Locale>().ta("ingame-menu-disk", &map),
                                Alignment::Right,
                                None,
                            )),
                        );
                        return Ok(true);
                    }
                    _ => {}
                }
            }

            // Handle state slot selection
            if (selected == MenuEntry::Save as usize || selected == MenuEntry::Load as usize)
                && let Some(state_slot) = info.state_slot.as_mut()
            {
                match event {
                    KeyEvent::Pressed(Key::Left) | KeyEvent::Autorepeat(Key::Left) => {
                        *state_slot = (*state_slot - 1).max(-1);
                        let state_slot = *state_slot;
                        RetroArchCommand::SetStateSlot(state_slot).send().await?;
                        self.update_state_slot_label(state_slot);
                        return Ok(true);
                    }
                    KeyEvent::Pressed(Key::Right) | KeyEvent::Autorepeat(Key::Right) => {
                        *state_slot = state_slot.saturating_add(1);
                        let state_slot = *state_slot;
                        RetroArchCommand::SetStateSlot(state_slot).send().await?;
                        self.update_state_slot_label(state_slot);
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        }

        match event {
            KeyEvent::Pressed(Key::A) => self.select_entry(commands).await,
            KeyEvent::Pressed(Key::Left | Key::Right)
            | KeyEvent::Autorepeat(Key::Left | Key::Right) => {
                // Don't scroll with left/right
                Ok(true)
            }
            event => {
                let prev = self.menu.selected();
                let consumed = self
                    .menu
                    .handle_key_event(event, commands.clone(), bubble)
                    .await?;
                let curr = self.menu.selected();
                if consumed
                    && prev != curr
                    && let Some(info) = self.retroarch_info.as_ref()
                {
                    if info.max_disk_slots > 1 {
                        if prev == MenuEntry::Continue as usize {
                            self.menu.set_right(prev, Box::new(NullView));
                        }
                        if curr == MenuEntry::Continue as usize {
                            let mut map = HashMap::new();
                            map.insert("disk".into(), (info.disk_slot + 1).into());
                            self.menu.set_right(
                                curr,
                                Box::new(Label::new(
                                    Point::zero(),
                                    self.res.get::<Locale>().ta("ingame-menu-disk", &map),
                                    Alignment::Right,
                                    None,
                                )),
                            );
                        }
                    }

                    if let Some(state_slot) = info.state_slot {
                        if prev == MenuEntry::Save as usize || prev == MenuEntry::Load as usize {
                            self.menu.set_right(prev, Box::new(NullView));
                        }
                        if curr == MenuEntry::Save as usize || curr == MenuEntry::Load as usize {
                            self.update_state_slot_label(state_slot);
                        } else {
                            self.image.set_path(None);
                        }
                    }
                }
                if !consumed && matches!(event, KeyEvent::Pressed(Key::B)) {
                    commands.send(Command::Exit).await?;
                }
                Ok(consumed)
            }
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        let mut children: Vec<&dyn View> = vec![&self.name, &self.status_bar, &self.button_hints];
        if let Some(selector) = &self.guide_selector {
            children.push(selector);
        } else {
            children.push(&self.menu);
            children.push(&self.image);
        }
        children
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        let mut children: Vec<&mut dyn View> =
            vec![&mut self.name, &mut self.status_bar, &mut self.button_hints];
        if let Some(selector) = &mut self.guide_selector {
            children.push(selector);
        } else {
            children.push(&mut self.menu);
            children.push(&mut self.image);
        }
        children
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, point: Point) {
        self.rect.x = point.x;
        self.rect.y = point.y;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MenuEntry {
    Continue,
    Save,
    Load,
    Reset,
    Guide,
    Settings,
    Quit,
}

impl MenuEntry {
    fn theme_id(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Save => "save",
            Self::Load => "load",
            Self::Reset => "reset",
            Self::Guide => "guide",
            Self::Settings => "settings",
            Self::Quit => "quit",
        }
    }

    fn as_str(&self, locale: &Locale) -> String {
        match self {
            MenuEntry::Continue => locale.t("ingame-menu-continue"),
            MenuEntry::Save => locale.t("ingame-menu-save"),
            MenuEntry::Load => locale.t("ingame-menu-load"),
            MenuEntry::Reset => locale.t("ingame-menu-reset"),
            MenuEntry::Guide => locale.t("ingame-menu-guide"),
            MenuEntry::Settings => locale.t("ingame-menu-settings"),
            MenuEntry::Quit => locale.t("ingame-menu-quit"),
        }
    }

    fn entries(info: Option<&RetroArchInfo>, has_guides: bool) -> Vec<Self> {
        match info {
            Some(RetroArchInfo {
                state_slot: Some(_),
                ..
            }) => {
                let mut entries = vec![MenuEntry::Continue, MenuEntry::Save, MenuEntry::Load];
                if has_guides {
                    entries.push(MenuEntry::Guide);
                }
                entries.extend([MenuEntry::Settings, MenuEntry::Reset, MenuEntry::Quit]);
                entries
            }
            Some(_) => {
                let mut entries = vec![MenuEntry::Continue, MenuEntry::Reset];
                if has_guides {
                    entries.push(MenuEntry::Guide);
                }
                entries.extend([MenuEntry::Settings, MenuEntry::Quit]);
                entries
            }
            None => {
                let mut entries = vec![MenuEntry::Continue];
                if has_guides {
                    entries.push(MenuEntry::Guide);
                }
                entries.push(MenuEntry::Quit);
                entries
            }
        }
    }

    fn index(&self, info: Option<&RetroArchInfo>, has_guides: bool) -> usize {
        let entries = MenuEntry::entries(info, has_guides);
        entries.iter().position(|e| e == self).unwrap_or(0)
    }
}

fn theme_capacity(theme: &ThemeHandle, screen: ScreenId, binding: Binding) -> usize {
    theme.pack().screens[&screen]
        .components
        .iter()
        .find(|component| component.binding == Some(binding))
        .and_then(|component| component.visible_count)
        .unwrap_or(1)
        .clamp(1, 64) as usize
}

fn theme_component(theme: &ThemeHandle, screen: ScreenId, binding: Binding) -> String {
    theme.pack().screens[&screen]
        .components
        .iter()
        .find(|component| component.binding == Some(binding))
        .map(|component| component.id.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod theme_adapter_tests {
    use super::*;
    use common::{
        constants::ALLIUM_FONTS_DIR,
        geom::Size,
        theme::{FontBank, runtime::ThemeSnapshot},
    };
    use type_map::TypeMap;

    #[derive(Clone)]
    pub(super) struct TestBattery;

    impl Battery for TestBattery {
        fn update(&mut self) -> Result<()> {
            Ok(())
        }

        fn percentage(&self) -> i32 {
            73
        }

        fn charging(&self) -> bool {
            false
        }
    }

    struct SlowBattery;
    impl Battery for SlowBattery {
        fn update(&mut self) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(())
        }
        fn percentage(&self) -> i32 {
            73
        }
        fn charging(&self) -> bool {
            false
        }
    }

    #[test]
    #[ignore = "host menu opening benchmark with injected 100 ms battery sample"]
    fn perf5_host_menu_open_timing() {
        let (theme, root) = orbit_handle();
        for cached in [false, true] {
            let cache = common::view::menu_status::MenuStatusCache::new(SlowBattery);
            let mut samples = Vec::new();
            for _ in 0..5 {
                let mut map = TypeMap::new();
                map.insert(Stylesheet::load().unwrap());
                map.insert(Locale::new("en-US"));
                map.insert(Size { w: 640, h: 480 });
                map.insert(GameInfo {
                    name: "Kowai Shashin".into(),
                    has_menu: true,
                    ..Default::default()
                });
                map.insert(Some(theme.clone()));
                if cached {
                    map.insert(cache.clone());
                }
                let start = std::time::Instant::now();
                let mut menu = IngameMenu::new(
                    Rect::new(0, 0, 640, 480),
                    IngameMenuState::default(),
                    Resources::new(map),
                    SlowBattery,
                    None,
                );
                menu.refresh_theme().unwrap();
                let mut platform = DefaultPlatform::new().unwrap();
                let mut display = platform.display().unwrap();
                menu.draw(&mut display, &Stylesheet::load().unwrap())
                    .unwrap();
                display.flush().unwrap();
                samples.push(start.elapsed().as_micros());
                if cached {
                    cache.request_refresh();
                }
            }
            println!(
                "PERF5_HOST_MENU {}",
                serde_json::json!({"cached": cached, "open_to_flush_us": samples, "injected_battery_ms": 100, "host_only": true})
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    pub(super) fn perf6_resources(theme: ThemeHandle, clock: bool) -> Resources {
        let mut map = TypeMap::new();
        let mut styles = Stylesheet::load().unwrap();
        styles.status_bar.show_clock = clock;
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(Size { w: 640, h: 480 });
        map.insert(GameInfo {
            name: "Kowai Shashin".into(),
            has_menu: true,
            ..Default::default()
        });
        map.insert(Some(theme));
        map.insert(common::view::menu_status::MenuStatusCache::new(TestBattery));
        Resources::new(map)
    }

    pub(super) fn perf6_info() -> Option<RetroArchInfo> {
        Some(RetroArchInfo {
            max_disk_slots: 1,
            disk_slot: 0,
            state_slot: Some(0),
        })
    }

    #[tokio::test]
    #[ignore = "release host benchmark; real state file and six rows"]
    async fn perf6_representative_host_open() {
        let _state_guard = crate::allium_menu::MENU_STATE_TEST_LOCK.lock().await;
        let (theme, root) = orbit_handle();
        let res = perf6_resources(theme, true);
        let rect = Rect::new(0, 0, 640, 480);
        let mut previous = IngameMenu::new(
            rect,
            IngameMenuState::default(),
            res.clone(),
            TestBattery,
            perf6_info(),
        );
        previous.save().unwrap();
        previous.refresh_theme().unwrap();
        let mut platform = DefaultPlatform::new().unwrap();
        let mut display = platform.display().unwrap();
        let styles = Stylesheet::load().unwrap();
        let mut samples = Vec::new();
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut menu = IngameMenu::load_or_new(rect, res.clone(), TestBattery, perf6_info())
                .await
                .unwrap();
            assert_eq!(menu.entries.len(), 6);
            menu.reuse_theme_frame(&mut previous);
            menu.refresh_theme().unwrap();
            menu.draw(&mut display, &styles).unwrap();
            display.flush().unwrap();
            samples.push(start.elapsed().as_micros());
            previous = menu;
        }
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        println!(
            "PERF6_HOST {}",
            serde_json::json!({"samples_us":samples,"median_us":sorted[2],"rows":6,"real_state_load":true,"release":!cfg!(debug_assertions),"mock_capture_and_flush":true})
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn perf6_frame_parity_each_selection() {
        let (theme, root) = orbit_handle();
        let mut hashes = Vec::new();
        for six_rows in [false, true] {
            let res = perf6_resources(theme.clone(), false);
            let mut menu = IngameMenu::new(
                Rect::new(0, 0, 640, 480),
                IngameMenuState::default(),
                res,
                TestBattery,
                if six_rows { perf6_info() } else { None },
            );
            for selected in 0..menu.entries.len() {
                menu.menu.select(selected);
                menu.refresh_theme().unwrap();
                let bytes = menu.theme_frame.as_ref().unwrap().pixels().data();
                hashes.push(serde_json::json!({"rows":menu.entries.len(),"selection":selected,"sha256":Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect::<String>()}));
            }
        }
        let result = serde_json::to_string_pretty(&hashes).unwrap();
        if let Ok(path) = std::env::var("PERF6_FRAME_HASHES") {
            fs::write(path, &result).unwrap();
        }
        println!("PERF6_FRAMES {result}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn perf6_cached_frame_invalidates_and_matches_uncached_pixels() {
        let (theme, root) = orbit_handle();
        let res = perf6_resources(theme.clone(), false);
        let rect = Rect::new(0, 0, 640, 480);
        let mut menu = IngameMenu::new(
            rect,
            IngameMenuState::default(),
            res.clone(),
            TestBattery,
            perf6_info(),
        );
        menu.refresh_theme().unwrap();
        menu.refresh_theme().unwrap();
        assert_eq!(menu.theme_prepares, 1);
        for change in 0..6 {
            match change {
                0 => menu.retroarch_info.as_mut().unwrap().state_slot = Some(7),
                1 => menu.retroarch_info.as_mut().unwrap().max_disk_slots = 3,
                2 => menu.menu.select(2),
                3 => res.insert(Locale::new("pt-BR")),
                4 => res.insert(GameInfo {
                    name: "Fresh title".into(),
                    ..Default::default()
                }),
                _ => {
                    let bank = common::theme::FontBank::bind(&ALLIUM_FONTS_DIR).unwrap();
                    let base = common::theme::runtime::ThemeSnapshot::builtin("orbit").unwrap();
                    res.insert(Some(ThemeHandle::prepare(base, &bank, 999).unwrap()));
                }
            }
            menu.refresh_theme().unwrap();
            assert_eq!(menu.theme_prepares, change + 2);
            let cached = menu.theme_frame.as_ref().unwrap().pixels().data().to_vec();
            let key = menu.theme_key.as_ref().unwrap();
            let current_theme = res.get::<Option<ThemeHandle>>().clone().unwrap();
            let reference = PreparedFrame::prepare(current_theme, &key.input).unwrap();
            assert_eq!(
                cached,
                reference.pixels().data(),
                "invalidated frame {change}"
            );
            menu.refresh_theme().unwrap();
            assert_eq!(menu.theme_prepares, change + 2);
        }
        menu.retroarch_info = None;
        menu.entries = MenuEntry::entries(None, false);
        menu.menu.select(0);
        menu.refresh_theme().unwrap();
        assert_eq!(
            menu.theme_key.as_ref().unwrap().input.rows[&Binding::Entries].len(),
            2
        );
        menu.disable_theme();
        assert!(menu.theme_key.is_none() && menu.theme_frame.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn perf5_quit_keeps_screenshot_quit_exit_order() {
        let _state_guard = crate::allium_menu::MENU_STATE_TEST_LOCK.lock().await;
        let mut map = TypeMap::new();
        map.insert(Stylesheet::load().unwrap());
        map.insert(Locale::new("en-US"));
        map.insert(Size { w: 640, h: 480 });
        map.insert(GameInfo {
            path: std::env::current_dir().unwrap(),
            ..Default::default()
        });
        map.insert(Option::<ThemeHandle>::None);
        let mut menu = IngameMenu::new(
            Rect::new(0, 0, 640, 480),
            IngameMenuState::default(),
            Resources::new(map),
            TestBattery,
            Some(RetroArchInfo {
                max_disk_slots: 1,
                disk_slot: 0,
                state_slot: Some(0),
            }),
        );
        menu.menu.select(
            menu.entries
                .iter()
                .position(|e| *e == MenuEntry::Quit)
                .unwrap(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        menu.select_entry(tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            Command::SaveStateScreenshot { slot: -1, .. }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Command::RetroArchCommand(RetroArchCommand::Quit)
        ));
        assert!(matches!(rx.try_recv().unwrap(), Command::Exit));
        assert!(rx.try_recv().is_err());
    }

    fn orbit_handle() -> (ThemeHandle, PathBuf) {
        let root = std::env::current_dir().unwrap().join("out").join(format!(
            "allium-menu-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        let bank = FontBank::bind(&ALLIUM_FONTS_DIR).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
        (theme, root)
    }

    #[tokio::test]
    async fn ux4_menu_shows_all_actions_without_scroll_and_reuses_frame() {
        let (builtin, root) = orbit_handle();
        let bank = common::theme::FontBank::bind(&ALLIUM_FONTS_DIR).unwrap();
        let packs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../themes").canonicalize().unwrap();
        let mut themes = vec![builtin, ThemeHandle::prepare(ThemeSnapshot::builtin("compact").unwrap(), &bank, 1).unwrap()];
        for name in ["compact-plus", "big-picture", "orbit-hero", "orbit-row"] {
            themes.push(ThemeHandle::prepare(ThemeSnapshot::load(&packs.join(name)).unwrap(), &bank, 1).unwrap());
        }
        for theme in themes {
            for guide in [false, true] {
                let mut map = TypeMap::new();
                map.insert(Stylesheet::load().unwrap());
                map.insert(Locale::new("en-US"));
                map.insert(Size { w: 640, h: 480 });
                map.insert(common::database::Database::in_memory().unwrap());
                map.insert(GameInfo {
                    name: "Kowai Shashin".into(),
                    has_menu: true,
                    guides: if guide {
                        vec![root.join("Guide.txt")]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                });
                map.insert(Some(theme.clone()));
                let res = Resources::new(map);
                let info = || {
                    Some(RetroArchInfo {
                        max_disk_slots: 1,
                        disk_slot: 0,
                        state_slot: Some(0),
                    })
                };
                let mut menu = IngameMenu::new(
                    Rect::new(0, 0, 640, 480),
                    IngameMenuState::default(),
                    res.clone(),
                    TestBattery,
                    info(),
                );
                menu.refresh_theme().unwrap();
                let expected = if guide { 7 } else { 6 };
                assert_eq!(menu.entries.len(), expected);
                let frame = menu.theme_frame.as_ref().unwrap();
                assert_eq!(
                    frame
                        .targets
                        .iter()
                        .filter(|t| t.component == "rows")
                        .count(),
                    expected
                );
                let storage = frame.pixels().data().as_ptr();
                let (tx, _) = tokio::sync::mpsc::channel(16);
                for _ in 1..expected {
                    menu.handle_key_event(
                        KeyEvent::Pressed(Key::Down),
                        tx.clone(),
                        &mut VecDeque::new(),
                    )
                    .await
                    .unwrap();
                    menu.refresh_theme().unwrap();
                    assert_eq!(menu.menu.window_start(), 0);
                    assert_eq!(
                        menu.theme_frame.as_ref().unwrap().pixels().data().as_ptr(),
                        storage
                    );
                }
                if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
                    fs::create_dir_all(&proof).unwrap();
                    menu.theme_frame
                        .as_ref()
                        .unwrap()
                        .pixels()
                        .save_png(std::path::Path::new(&proof).join(format!(
                            "{}-ingame-{}-actions.png",
                            theme.pack().id,
                            expected
                        )))
                        .unwrap();
                }
                let mut next = IngameMenu::new(
                    Rect::new(0, 0, 640, 480),
                    IngameMenuState::default(),
                    res,
                    TestBattery,
                    info(),
                );
                next.reuse_theme_frame(&mut menu);
                next.refresh_theme().unwrap();
                assert_eq!(
                    next.theme_frame.as_ref().unwrap().pixels().data().as_ptr(),
                    storage
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn themed_menu_renders_controller_rows_and_b_still_exits() {
        let (theme, root) = orbit_handle();
        let mut map = TypeMap::new();
        map.insert(Stylesheet::load().unwrap());
        map.insert(Locale::new("en-US"));
        map.insert(Size { w: 640, h: 480 });
        map.insert(common::database::Database::in_memory().unwrap());
        map.insert(GameInfo {
            name: "Controller Truth".into(),
            has_menu: true,
            ..Default::default()
        });
        map.insert(Some(theme));
        let res = Resources::new(map);
        let mut menu = IngameMenu::new(
            Rect::new(0, 0, 640, 480),
            IngameMenuState::default(),
            res,
            TestBattery,
            None,
        );
        menu.refresh_theme().unwrap();

        let frame = menu.theme_frame.as_ref().expect("themed frame");
        assert_eq!(frame.pixels().width(), 640);
        assert_eq!(frame.pixels().height(), 480);
        assert!(frame.pixels().data().iter().any(|byte| *byte != 0));
        if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
            fs::create_dir_all(&proof).unwrap();
            frame
                .pixels()
                .save_png(PathBuf::from(&proof).join("native-orbit-ingame-menu.png"))
                .unwrap();
        }

        let guide_a = root.join("guide-a.txt");
        let guide_b = root.join("guide-b.txt");
        fs::write(&guide_a, "Guide A").unwrap();
        fs::write(&guide_b, "Guide B").unwrap();
        menu.guide_selector = Some(GuideSelector::new(
            Rect::new(0, 0, 640, 430),
            menu.res.clone(),
            vec![guide_a, guide_b],
            1,
            false,
        ));
        menu.refresh_theme().unwrap();
        let guide_frame = menu
            .guide_selector
            .as_ref()
            .and_then(GuideSelector::prepared_theme_frame)
            .expect("themed guide frame");
        assert!(guide_frame.pixels().data().iter().any(|byte| *byte != 0));
        if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
            guide_frame
                .pixels()
                .save_png(PathBuf::from(proof).join("native-orbit-guide-selector.png"))
                .unwrap();
        }
        menu.guide_selector = None;
        menu.refresh_theme().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut bubble = VecDeque::new();
        assert!(
            !menu
                .handle_key_event(KeyEvent::Pressed(Key::B), tx, &mut bubble)
                .await
                .unwrap()
        );
        assert!(matches!(rx.recv().await, Some(Command::Exit)));

        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn nested_prepare_failure_falls_back_after_guards_and_menu_still_exits() {
        let (theme, root) = orbit_handle();
        let guide = root.join("nested-guide.txt");
        fs::write(&guide, "nested guide").unwrap();
        let mut map = TypeMap::new();
        map.insert(Stylesheet::load().unwrap());
        map.insert(Locale::new("en-US"));
        map.insert(Size { w: 640, h: 480 });
        map.insert(common::database::Database::in_memory().unwrap());
        map.insert(GameInfo {
            name: "Fallback Fixture".into(),
            guides: vec![guide.clone()],
            has_menu: true,
            ..Default::default()
        });
        map.insert(Some(theme));
        let res = Resources::new(map);

        // Nested construction under an ancestor resource borrow must not prepare or mutate the
        // shared theme. Preparation is owned by the outer menu boundary after guards are gone.
        let locale_guard = res.get::<Locale>();
        let selector = GuideSelector::new(
            Rect::new(0, 40, 640, 400),
            res.clone(),
            vec![guide],
            0,
            true,
        );
        assert!(res.get::<Option<ThemeHandle>>().is_some());
        drop(locale_guard);

        let mut menu = IngameMenu::new(
            Rect::new(0, 0, 640, 480),
            IngameMenuState::default(),
            res.clone(),
            TestBattery,
            None,
        );
        menu.guide_selector = Some(selector);
        menu.refresh_theme().unwrap();
        menu.guide_selector
            .as_mut()
            .and_then(GuideSelector::text_reader_mut)
            .unwrap()
            .fail_next_theme_prepare();
        assert!(menu.refresh_theme().is_err());

        // AlliumMenu performs these operations in this order at the owning safe boundary.
        res.insert(Option::<ThemeHandle>::None);
        menu.disable_theme();
        assert!(res.get::<Option<ThemeHandle>>().is_none());
        assert!(
            menu.guide_selector
                .as_ref()
                .unwrap()
                .theme_is_disabled_recursively()
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let mut bubble = VecDeque::new();
        assert!(
            menu.handle_key_event(KeyEvent::Pressed(Key::Menu), tx, &mut bubble)
                .await
                .unwrap()
        );
        assert!(matches!(rx.recv().await, Some(Command::Exit)));
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod parity_b_tests {
    use super::theme_adapter_tests::{TestBattery, perf6_info, perf6_resources};
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/parity-b-support.rs"
    ));
    #[tokio::test]
    async fn parity_b_menu_and_guides() {
        let temp =
            std::env::temp_dir().join(format!("allium-parity-b-menu-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let guide = temp.join("Guide Watch.txt");
        fs::write(
            &guide,
            "First bookmark.\nYour saved notes for this game appear here.\n",
        )
        .unwrap();
        let image = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-fixtures/synthetic-art/sample-b-cover.png");
        let tall_path = temp.join("PS/media/covers-tall/r4.png");
        fs::create_dir_all(tall_path.parent().unwrap()).unwrap();
        fs::write(temp.join("PS/gamelist.xml"), "<gameList><game><path>./r4.chd</path><name>R4 - Ridge Racer Type 4</name><releasedate>19981203T000000</releasedate></game></gameList>").unwrap();
        image::open(&image)
            .unwrap()
            .resize_exact(120, 180, image::imageops::FilterType::Lanczos3)
            .save(&tall_path)
            .unwrap();
        for (variant, theme) in parity_variants() {
            let res = perf6_resources(theme.clone(), false);
            res.insert(common::database::Database::in_memory().unwrap());
            res.insert(GameInfo {
                name: "R4 - Ridge Racer Type 4".into(),
                path: temp.join("PS/r4.chd"),
                image: Some(image.clone()),
                has_menu: true,
                ..Default::default()
            });
            res.get::<common::database::Database>()
                .update_guide_cursor(&guide, 16)
                .unwrap();
            let mut input = FrameInputs::new(ScreenId::GuideSelector);
            super::super::game_context(&mut input, &res, &theme);
            super::super::guide_shell(&mut input, &res.get::<Locale>());
            assert_eq!(
                input.navigation[&common::theme::pack::NavGroup::Primary].len(),
                4
            );
            assert_eq!(input.selected[&Binding::Navigation], "tab-2");
            assert_eq!(
                input.metadata,
                vec![
                    ("Platform".into(), "PS".into()),
                    ("Released".into(), "1998".into())
                ]
            );
            assert_eq!(
                input.text[&Binding::SelectedTitle],
                "R4 - Ridge Racer Type 4"
            );
            let cover = input.art.selected_cover.as_ref().unwrap();
            assert_eq!(
                cover.height() > cover.width(),
                theme.pack().alignment.as_ref().unwrap().cover_shape == "tall"
            );
            let mut again = FrameInputs::new(ScreenId::IngameMenu);
            super::super::game_context(&mut again, &res, &theme);
            assert!(std::sync::Arc::ptr_eq(
                input.art.selected_cover.as_ref().unwrap(),
                again.art.selected_cover.as_ref().unwrap()
            ));
            let mut menu = IngameMenu::new(
                Rect::new(0, 0, 640, 480),
                IngameMenuState::default(),
                res,
                TestBattery,
                perf6_info(),
            );
            menu.refresh_theme().unwrap();
            parity_save_overlay(&variant, "ingame_menu", menu.theme_frame.as_ref().unwrap());
            menu.guide_selector = Some(GuideSelector::new(
                Rect::new(0, 0, 640, 480),
                menu.res.clone(),
                vec![guide.clone(), guide.clone()],
                0,
                false,
            ));
            menu.refresh_theme().unwrap();
            menu.guide_selector.as_mut().unwrap().refresh_theme(common::theme::frame::Status {
                clock: Some("12:34".into()), battery: Some(82), network: Some("online".into()), ..Default::default()
            }).unwrap();
            parity_save(
                &variant,
                "guide_selector",
                menu.guide_selector
                    .as_ref()
                    .unwrap()
                    .prepared_theme_frame()
                    .unwrap()
                    .pixels(),
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }
}
