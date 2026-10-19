use std::collections::VecDeque;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::constants::RECENT_GAMES_LIMIT;
use common::database::Database;
use common::display::Display;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Image, ImageMode, Label, View};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::{Entry, display_names::DisplayNames};
use crate::entry::game::Game;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecentsCarouselState {
    pub selected: usize,
}

#[derive(Debug)]
pub struct RecentsCarousel {
    rect: Rect,
    res: Resources,
    games: Vec<Game>,
    selected: usize,
    screenshot: Image,
    game_name: Label<String>,
    button_hints: ButtonHints<String>,
    dirty: bool,
    name_generation: u64,
}

impl RecentsCarousel {
    pub fn new(rect: Rect, res: Resources, state: RecentsCarouselState) -> Result<Self> {
        let Rect { x, y, w, .. } = rect;

        let games = Self::load_games(&res)?;
        let selected = state.selected.min(games.len().saturating_sub(1));

        let styles = res.get::<Stylesheet>();

        let mut button_hints = {
            let locale = res.get::<Locale>();
            ButtonHints::new(
                res.clone(),
                vec![],
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
                        Key::X,
                        locale.t("sort-search"),
                        Alignment::Right,
                    ),
                ],
            )
        };

        let button_hints_rect = button_hints.bounding_box(&styles);
        let label_height = styles.ui.margin_y * 2 + styles.ui.ui_font.size as i32;
        // Calculate screenshot height based on where button hints start
        let available_height = (button_hints_rect.y - y) as u32;
        let screenshot_height = available_height.saturating_sub(label_height as u32);

        let mut screenshot =
            Image::empty(Rect::new(x, y, w, screenshot_height), ImageMode::Contain);
        screenshot.set_border_radius(12);
        screenshot.set_alignment(Alignment::Center);

        let game_name = Label::new(
            Point::new(
                x + w as i32 / 2,
                y + screenshot_height as i32 + styles.ui.margin_y,
            ),
            String::new(),
            Alignment::Center,
            Some(w - (styles.ui.margin_x * 2) as u32),
        );

        drop(styles);

        let mut carousel = Self {
            rect,
            res: res.clone(),
            games,
            selected,
            screenshot,
            game_name,
            button_hints,
            dirty: true,
            name_generation: common::media::name_generation(),
        };

        carousel.game_name.scroll(true);
        carousel.update_current_game()?;

        Ok(carousel)
    }

    pub fn load_or_new(
        rect: Rect,
        res: Resources,
        state: Option<RecentsCarouselState>,
    ) -> Result<Self> {
        let state = state.unwrap_or_default();
        Self::new(rect, res, state)
    }

    fn load_games(res: &Resources) -> Result<Vec<Game>> {
        let database = res.get::<Database>();
        #[cfg(test)]
        crate::entry::load_work::record(|work| work.sql += 1);
        let mut db_games = database.select_last_played(RECENT_GAMES_LIMIT)?;
        crate::entry::load_name_sources(db_games.iter().map(|game| game.path.as_path()));

        let mut games = Vec::new();
        db_games.retain(|game| crate::entry::listed_path(&game.path));
        let game_paths = db_games.iter().map(|game| game.path.clone()).collect();
        let mut identities = std::collections::HashSet::new();

        for game in db_games {
            let extension = game
                .path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_owned();

            let image =
                crate::entry::lazy_image::LazyImage::from_path(&game.path, game.image.clone());

            let mut game = Game {
                name: game.name.clone(),
                full_name: game.name,
                path: game.path,
                image,
                extension,
                core: game.core,
                rating: game.rating,
                release_date: game.release_date,
                developer: game.developer,
                publisher: game.publisher,
                genres: game.genres,
                favorite: game.favorite,
                screenshot_path: game.screenshot_path,
            };
            game.hydrate_name();
            if identities.insert(game.display_identity_in(&game_paths)) { games.push(game); }
        }

        Ok(games)
    }

    pub(crate) fn reload(&mut self) -> Result<()> {
        let selected_path = self.games.get(self.selected).map(|game| game.path.clone());
        let games = Self::load_games(&self.res)?;
        self.selected = selected_path.as_ref()
            .and_then(|path| games.iter().position(|game| &game.path == path))
            .unwrap_or(self.selected)
            .min(games.len().saturating_sub(1));
        self.games = games;
        self.update_current_game()?;
        self.set_should_draw();
        Ok(())
    }

    fn update_current_game(&mut self) -> Result<()> {
        if self.games.is_empty() {
            self.screenshot.set_path(None);
            self.game_name.scroll(false);
            self.game_name.suffix_len(0);
            self.game_name.set_text(String::new());
            return Ok(());
        }

        let entries: Vec<_> = self.games.iter().cloned().map(Entry::Game).collect();
        let names = DisplayNames::new(&entries);
        let name = names.name(&entries[self.selected]).to_owned();
        let game = &mut self.games[self.selected];

        self.screenshot.set_path(
            game.screenshot_path
                .clone()
                .or_else(|| game.image.image().map(Path::to_owned)),
        );
        self.screenshot.set_should_draw();
        self.game_name.suffix_len(names.suffix_len(&entries[self.selected]));
        self.game_name.set_text(name);
        self.game_name.scroll(true);
        self.button_hints.set_should_draw();

        self.dirty = true;
        Ok(())
    }

    pub fn save(&self) -> RecentsCarouselState {
        RecentsCarouselState { selected: 0 }
    }

    fn navigate_up(&mut self) -> Result<()> {
        if self.selected > 0 {
            self.selected -= 1;
            self.update_current_game()?;
        }
        Ok(())
    }

    fn navigate_down(&mut self) -> Result<()> {
        if self.selected < self.games.len().saturating_sub(1) {
            self.selected += 1;
            self.update_current_game()?;
        }
        Ok(())
    }

    async fn launch_game(&mut self, commands: Sender<Command>) -> Result<()> {
        if let Some(game) = self.games.get_mut(self.selected) {
            if crate::network_runtime::begin(game, false)? {
                return Ok(());
            }
            let command = crate::consoles::launch_feedback(
                self.res
                    .get::<ConsoleMapper>()
                    .launch_game(&self.res.get(), game, false));
            if let Some(cmd) = command {
                commands.send(cmd).await?;
            }
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl View for RecentsCarousel {
    fn update(&mut self, dt: std::time::Duration) {
        common::media::poll_name_indexes();
        let generation = common::media::name_generation();
        if self.name_generation != generation {
            for game in &mut self.games {
                game.hydrate_name();
                if matches!(game.image, crate::entry::lazy_image::LazyImage::NotFound) {
                    game.image = crate::entry::lazy_image::LazyImage::Unknown(game.path.clone());
                }
            }
            if let Err(error) = self.update_current_game() {
                log::error!("Refresh recent carousel names: {error:#}");
            }
            self.name_generation = generation;
        }
        self.screenshot.update(dt);
        self.game_name.update(dt);
        self.button_hints.update(dt);
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        if self.dirty {
            display.load(self.rect)?;
            self.dirty = false;
            drawn = true;
        }

        if self.screenshot.should_draw() {
            drawn |= self.screenshot.draw(display, styles)?;
        }

        if self.games.is_empty() {
            let locale = self.res.get::<Locale>();
            let mut empty_label = Label::new(
                Point::new(
                    self.rect.x + self.rect.w as i32 / 2,
                    self.rect.y + self.rect.h as i32 / 2,
                ),
                locale.t("no-recent-games"),
                Alignment::Center,
                None,
            );
            drawn |= empty_label.draw(display, styles)?;
        } else if self.game_name.should_draw() {
            display.load(self.game_name.bounding_box(styles))?;
            drawn |= self.game_name.draw(display, styles)?;
        }

        if self.button_hints.should_draw() {
            drawn |= self.button_hints.draw(display, styles)?;
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.dirty
            || self.screenshot.should_draw()
            || self.game_name.should_draw()
            || self.button_hints.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.dirty = true;
        self.screenshot.set_should_draw();
        self.game_name.set_should_draw();
        self.button_hints.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        _bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        match event {
            KeyEvent::Pressed(Key::Up) | KeyEvent::Autorepeat(Key::Up) => {
                self.navigate_up()?;
                Ok(true)
            }
            KeyEvent::Pressed(Key::Down) | KeyEvent::Autorepeat(Key::Down) => {
                self.navigate_down()?;
                Ok(true)
            }
            KeyEvent::Pressed(Key::A) => {
                self.launch_game(commands).await?;
                Ok(true)
            }
            KeyEvent::Pressed(Key::X) => {
                commands.send(Command::StartSearch).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.screenshot, &self.game_name, &self.button_hints]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![
            &mut self.screenshot,
            &mut self.game_name,
            &mut self.button_hints,
        ]
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, point: Point) {
        self.rect.x = point.x;
        self.rect.y = point.y;
    }
}

#[cfg(test)]
mod name_refresh_tests {
    use super::*;
    use crate::entry::lazy_image::LazyImage;

    #[test]
    fn carousel_reload_clears_removed_game_and_its_presentation() {
        let fixture = crate::entry::directory::list_tests::Fixture::new();
        let path = fixture.0.path.join("Recent.gb");
        std::fs::write(&path, b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        database.increment_play_count(&Game::new(path.clone()).into()).unwrap();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(database.clone());
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        let mut carousel = RecentsCarousel::new(Rect::new(0, 54, 640, 376),
            Resources::new(map), RecentsCarouselState::default()).unwrap();
        assert_eq!(carousel.games.len(), 1);
        database.reset_game(&path).unwrap();
        carousel.dirty = false;
        carousel.reload().unwrap();
        assert!(carousel.games.is_empty());
        assert_eq!(carousel.selected, 0);
        assert_eq!(carousel.game_name.text(), "");
        assert!(serde_json::to_value(&carousel.screenshot).unwrap()["path"].is_null());
        assert!(carousel.dirty);
    }

    #[test]
    fn carousel_filters_indexed_syncthing_conflicts_and_control_contents() {
        let fixture = crate::entry::directory::list_tests::Fixture::new();
        let database = Database::in_memory().unwrap();
        for name in ["Real.gb", "Game.sync-conflict-20250415-202527-JENTCHY.zip", ".stfolder/Control.gb", ".stversions/Version.gb"] {
            let path = fixture.0.path.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"rom").unwrap();
            database.increment_play_count(&Game::new(path).into()).unwrap();
        }
        let mut map = type_map::TypeMap::new();
        map.insert(database);
        let games = RecentsCarousel::load_games(&Resources::new(map)).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].path, fixture.0.path.join("Real.gb"));
    }

    #[test]
    fn carousel_build_loads_cold_gamelist_names_for_catalog_folder_rows() {
        common::media::invalidate();
        let fixture = crate::entry::directory::list_tests::Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        let folder = system.join("Clock Tower - The First Fear (Japan)");
        std::fs::create_dir_all(&folder).unwrap();
        let database = Database::in_memory().unwrap();
        let mut game = Game::new(folder.clone());
        game.name = "Clock Tower: The First Fear (Japan)".into();
        game.image = LazyImage::NotFound;
        database.increment_play_count(&game.into()).unwrap();
        std::fs::write(system.join("gamelist.xml"),
            "<gameList><game><path>./Clock Tower - The First Fear (Japan)</path><name>Clock Tower (Special Edition)</name></game></gameList>").unwrap();
        let mut map = type_map::TypeMap::new();
        map.insert(database.clone());
        let mut games = RecentsCarousel::load_games(&Resources::new(map)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            common::media::poll_name_indexes();
            games[0].hydrate_name();
            if games[0].name == "Clock Tower (Special Edition)" {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "carousel name did not hydrate");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        crate::entry::load_work::take();
        for _ in 0..8 {
            assert!(!games[0].hydrate_name());
        }
        assert_eq!(crate::entry::load_work::take(), Default::default());
        assert_eq!(database.select_game(&folder).unwrap().unwrap().name, "Clock Tower: The First Fear (Japan)");
        common::media::invalidate();
    }

    #[test]
    fn fix18_carousel_old_cue_uses_parent_title_and_keeps_history() {
        common::media::invalidate();
        let fixture = crate::entry::directory::list_tests::Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        let basename = "Clock Tower - The First Fear (Japan)";
        let folder = system.join(basename);
        std::fs::create_dir_all(&folder).unwrap();
        let cue = folder.join(format!("{basename}.cue"));
        std::fs::write(&cue, b"FILE \"disc.bin\" BINARY\n").unwrap();
        std::fs::write(folder.join("disc.bin"), b"rom").unwrap();
        std::fs::write(system.join("gamelist.xml"), format!(
            "<gameList><game><path>./{basename}</path><name>Clock Tower: The First Fear</name></game></gameList>"
        )).unwrap();
        let database = Database::in_memory().unwrap();
        let mut old = Game::new(cue.clone());
        old.name = basename.into();
        old.image = LazyImage::NotFound;
        database.increment_play_count(&old.into()).unwrap();
        let mut map = type_map::TypeMap::new();
        map.insert(database.clone());
        let mut games = RecentsCarousel::load_games(&Resources::new(map)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while common::media::cached_name(&folder).is_none() {
            common::media::poll_name_indexes();
            assert!(std::time::Instant::now() < deadline, "parent title never loaded");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(games.len(), 1);
        games[0].hydrate_name();
        assert_eq!(games[0].path, cue);
        assert_eq!(games[0].name, "Clock Tower: The First Fear");
        crate::entry::load_work::take();
        for _ in 0..8 { assert!(!games[0].hydrate_name()); }
        assert_eq!(crate::entry::load_work::take(), Default::default());
        assert_eq!(database.select_game(&cue).unwrap().unwrap().name, basename);
        common::media::invalidate();
    }

    #[test]
    fn fix20_carousel_folder_alias_does_not_leak_suffix() {
        let res = crate::view::entry_list::ux4_tests::fix20_recents_resources();
        let mut carousel = RecentsCarousel::new(Rect::new(0, 54, 640, 376), res, RecentsCarouselState::default()).unwrap();
        assert_eq!(carousel.game_name.text(), "Kowai Shashin: Shinrei shashin kitan");
        assert_eq!(carousel.games.len(), 9);
        carousel.reload().unwrap();
        assert_eq!(carousel.games.len(), 9);
        assert_eq!(carousel.game_name.text(), "Kowai Shashin: Shinrei shashin kitan");
    }

    #[test]
    fn fix20b_carousel_different_stem_and_nested_aliases_render_once() {
        for nested in [false, true] {
            for launch_most_recent in [false, true] {
                let (res, expected) = crate::view::entry_list::ux4_tests::fix20b_recents_resources(nested, launch_most_recent);
                let mut carousel = RecentsCarousel::new(Rect::new(0, 54, 640, 376), res, RecentsCarouselState::default()).unwrap();
                assert_eq!(carousel.game_name.text(), "Folder Title");
                assert_eq!(carousel.games.len(), 9);
                assert_eq!(carousel.games[0].path, expected);
                carousel.reload().unwrap();
                assert_eq!(carousel.games.len(), 9);
                assert_eq!(carousel.games[0].path, expected);
                assert_eq!(carousel.game_name.text(), "Folder Title");
            }
        }
        common::media::set_game_containers(std::iter::empty());
    }

    #[test]
    fn carousel_collision_labels_preserve_game_names_and_clear_after_removal() {
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(Database::in_memory().unwrap());
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        let mut carousel = RecentsCarousel::new(Rect::new(0, 54, 640, 376),
            Resources::new(map), RecentsCarouselState::default()).unwrap();
        carousel.games = ["Game (USA).nes", "Game (Europe).nes"].into_iter().map(|file| {
            let mut game = Game::new(file.into());
            game.image = LazyImage::NotFound;
            game
        }).collect();
        carousel.update_current_game().unwrap();
        assert_eq!(carousel.game_name.text(), "Game (USA)");
        carousel.navigate_down().unwrap();
        assert_eq!(carousel.game_name.text(), "Game (Europe)");
        assert!(carousel.games.iter().all(|game| game.name == "Game"));
        carousel.games.remove(0);
        carousel.selected = 0;
        carousel.update_current_game().unwrap();
        assert_eq!(carousel.game_name.text(), "Game");
    }

    #[test]
    fn name_refresh_retries_cold_carousel_cover_and_updates_selected_image() {
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        let database = Database::in_memory().unwrap();
        let root = std::env::temp_dir().join(format!("allium-carousel-cover-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Game.chd");
        let cover = root.join("unrelated-cover.png");
        let mut pixels = tiny_skia::Pixmap::new(16, 16).unwrap();
        pixels.fill(tiny_skia::Color::from_rgba8(35, 120, 210, 255));
        pixels.save_png(&cover).unwrap();
        let mut row: common::database::NewGame = Game::new(path.clone()).into();
        row.image = None;
        database.increment_play_count(&row).unwrap();
        map.insert(database);
        common::media::invalidate();
        std::fs::write(root.join("gamelist.xml"),
            "<gameList><game><path>Game.chd</path><name>Game</name><image>unrelated-cover.png</image></game></gameList>").unwrap();
        let mut carousel = RecentsCarousel::new(Rect::new(0, 54, 640, 376),
            Resources::new(map), RecentsCarouselState::default()).unwrap();
        assert_eq!(carousel.games[0].image, LazyImage::NotFound);
        assert!(serde_json::to_value(&carousel.screenshot).unwrap()["path"].is_null());
        let mut retained = Game::new(root.join("Other.chd"));
        retained.image = LazyImage::Found(cover.clone());
        carousel.games.push(retained);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while common::media::cached_name(&path).as_deref() != Some("Game") {
            common::media::poll_name_indexes();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        carousel.update(std::time::Duration::ZERO);
        assert_eq!(carousel.games[0].image, LazyImage::Found(cover.clone()));
        assert_eq!(carousel.games[1].image, LazyImage::Found(cover.clone()));
        assert_eq!(serde_json::to_value(&carousel.screenshot).unwrap()["path"], cover.to_str().unwrap());
        assert!(carousel.screenshot.should_draw());
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}
