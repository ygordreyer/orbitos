use std::cmp::Reverse;
use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::constants::ALLIUM_GAMES_DIR;
use common::database::Database;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, View};
use itertools::Itertools;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::directory::Directory;
use crate::entry::{Entry, Sort};
use crate::view::entry_list::{EntryList, EntryListState};

pub type GamesState = EntryListState<GamesSort>;

#[derive(Debug)]
pub struct Games {
    rect: Rect,
    res: Resources,
    list: EntryList<GamesSort>,
    button_hints: ButtonHints<String>,
    name_generation: u64,
}

impl Games {
    pub(crate) fn idle_media_step(&mut self, handle: &common::theme::ThemeHandle) -> bool {
        self.list
            .idle_media_step(handle, common::theme::pack::ScreenId::Library)
    }

    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        self.list.root_tab_navigation_ready()
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        self.list.clear_theme()
    }
    pub(crate) fn navigation_warm_model(
        &mut self,
        handle: &common::theme::ThemeHandle,
        direction: isize,
        input: common::theme::frame::FrameInputs,
    ) -> Result<Option<super::entry_list::NavigationWarm>> {
        self.list.navigation_warm_model(handle, direction, input)
    }
    pub(crate) fn navigation_warm_ready(&self) -> bool {
        self.list.navigation_warm_ready()
    }
    pub(crate) fn navigation_warm_art_step(
        &mut self,
        work: &mut super::entry_list::NavigationWarm,
    ) -> bool {
        self.list.navigation_warm_art_step(work)
    }

    pub(crate) fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
        self.list
            .theme_models(common::theme::pack::ScreenId::Library, handle)
    }

    pub fn new(rect: Rect, res: Resources, mut list: EntryList<GamesSort>) -> Result<Self> {
        if crate::theme_adapter::legacy_orbit(&res) {
            list.enable_orbit();
        }
        let button_hints = {
            let locale = res.get::<Locale>();
            ButtonHints::new(
                res.clone(),
                vec![ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::X,
                    locale.t("sort-search"),
                    Alignment::Left,
                )],
                vec![],
            )
        };

        Ok(Self {
            rect,
            res: res.clone(),
            list,
            button_hints,
            name_generation: common::media::name_generation(),
        })
    }

    pub fn load_or_new(rect: Rect, res: Resources, state: Option<GamesState>) -> Result<Self> {
        let rect = if crate::theme_adapter::legacy_orbit(&res) {
            Rect::new(
                0,
                super::orbit::HEADER_HEIGHT,
                640,
                super::orbit::CONTENT_HEIGHT,
            )
        } else {
            rect
        };
        let list = if let Some(state) = state {
            EntryList::load(rect, res.clone(), state)?
        } else {
            EntryList::new(
                rect,
                res.clone(),
                GamesSort::Alphabetical(Directory::new(ALLIUM_GAMES_DIR.clone())),
            )?
        };

        Self::new(rect, res, list)
    }

    #[cfg(test)]
    pub(crate) fn orbit_cache_counts(&self) -> (usize, usize) {
        self.list.orbit_cache_counts()
    }

    pub(crate) fn disc_picker_active(&self) -> bool {
        self.list.disc_picker_active()
    }

    pub fn save(&self) -> GamesState {
        self.list.save()
    }
}

#[async_trait(?Send)]
impl View for Games {
    fn update(&mut self, dt: std::time::Duration) {
        common::media::poll_name_indexes();
        let generation = common::media::name_generation();
        if self.name_generation != generation
            && !self.list.interaction_active()
            && !self.list.details_active()
            && !self.list.disc_picker_active()
        {
            let state = self.list.save();
            match common::media::with_name_indexes(|| {
                EntryList::load(self.rect, self.res.clone(), state)
            }) {
                Ok(mut list) => {
                    if crate::theme_adapter::legacy_orbit(&self.res) {
                        list.enable_orbit();
                    }
                    self.list = list;
                    self.name_generation = generation;
                    self.set_should_draw();
                }
                Err(error) => log::error!("Refresh Games names: {error:#}"),
            }
        }
        self.list.update(dt);
        self.button_hints.update(dt);
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if crate::theme_adapter::legacy_orbit(&self.res) {
            return self.list.draw(display, styles);
        }
        let mut drawn = false;

        if self.list.should_draw() {
            drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
            self.button_hints.set_should_draw();
        }
        drawn |= self.button_hints.should_draw() && self.button_hints.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.list.should_draw()
            || (!crate::theme_adapter::legacy_orbit(&self.res) && self.button_hints.should_draw())
    }

    fn set_should_draw(&mut self) {
        self.list.set_should_draw();
        self.button_hints.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        // A nested disc picker owns every key before Games-level shortcuts.
        if self.list.disc_picker_active() || self.list.details_active() {
            return self.list.handle_key_event(event, commands, bubble).await;
        }
        match event {
            KeyEvent::Pressed(Key::X) => {
                commands.send(Command::StartSearch).await?;
                return Ok(true);
            }
            _ => self.list.handle_key_event(event, commands, bubble).await,
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.list]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.list]
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GamesSort {
    Alphabetical(Directory),
    LastPlayed(Directory),
    MostPlayed(Directory),
    Rating(Directory),
    ReleaseDate(Directory),
    Random(Directory),
}

impl GamesSort {
    pub fn directory(&self) -> &Directory {
        match self {
            GamesSort::Alphabetical(d) => d,
            GamesSort::LastPlayed(d) => d,
            GamesSort::MostPlayed(d) => d,
            GamesSort::Rating(d) => d,
            GamesSort::ReleaseDate(d) => d,
            GamesSort::Random(d) => d,
        }
    }
}

impl Sort for GamesSort {
    fn button_hint(&self, locale: &Locale) -> String {
        match self {
            GamesSort::Alphabetical(_) => locale.t("sort-alphabetical"),
            GamesSort::LastPlayed(_) => locale.t("sort-last-played"),
            GamesSort::MostPlayed(_) => locale.t("sort-most-played"),
            GamesSort::Rating(_) => locale.t("sort-rating"),
            GamesSort::ReleaseDate(_) => locale.t("sort-release-date"),
            GamesSort::Random(_) => locale.t("sort-random"),
        }
    }

    fn next(&self) -> Self {
        match self {
            GamesSort::Alphabetical(d) => GamesSort::LastPlayed(d.clone()),
            GamesSort::LastPlayed(d) => GamesSort::MostPlayed(d.clone()),
            GamesSort::MostPlayed(d) => GamesSort::Rating(d.clone()),
            GamesSort::Rating(d) => GamesSort::ReleaseDate(d.clone()),
            GamesSort::ReleaseDate(d) => GamesSort::Random(d.clone()),
            GamesSort::Random(d) => GamesSort::Alphabetical(d.clone()),
        }
    }

    fn with_directory(&self, directory: Directory) -> Self {
        match self {
            GamesSort::Alphabetical(_) => GamesSort::Alphabetical(directory),
            GamesSort::LastPlayed(_) => GamesSort::LastPlayed(directory),
            GamesSort::MostPlayed(_) => GamesSort::MostPlayed(directory),
            GamesSort::Rating(_) => GamesSort::Rating(directory),
            GamesSort::ReleaseDate(_) => GamesSort::ReleaseDate(directory),
            GamesSort::Random(_) => GamesSort::Random(directory),
        }
    }

    fn entries(
        &self,
        database: &Database,
        console_mapper: &ConsoleMapper,
        locale: &Locale,
    ) -> Result<Vec<Entry>> {
        let mut entries = self.directory().entries(database, console_mapper, locale)?;

        match self {
            GamesSort::Alphabetical(_) => {
                let mut games = Vec::with_capacity(entries.len());
                let mut i = 0;
                while i < entries.len() {
                    if matches!(entries[i], Entry::Game(_)) {
                        match entries.remove(i) {
                            Entry::Game(game) => games.push(game),
                            _ => unreachable!(),
                        }
                    } else {
                        i += 1;
                    }
                }

                let db_games = database
                    .select_games(&games.iter().map(|g| g.path.as_path()).collect::<Vec<_>>())?;

                let games = games.into_iter().zip(db_games).collect::<Vec<_>>();
                let (favorites, non_favorites): (Vec<_>, Vec<_>) =
                    games.into_iter().partition(|(_, db_game)| {
                        db_game.as_ref().map(|g| g.favorite).unwrap_or_default()
                    });
                let favorites = favorites.into_iter().map(|(g, _)| g).sorted_unstable();
                let non_favorites = non_favorites.into_iter().map(|(g, _)| g).sorted_unstable();
                entries.retain(|e| matches!(e, Entry::Directory(_) | Entry::App(_)));
                entries.sort_unstable();
                entries.extend(favorites.map(Entry::Game));
                entries.extend(non_favorites.map(Entry::Game));
            }
            GamesSort::LastPlayed(_) => {
                // With this current implementation, apps will appear before games.
                // TOOD: think about whether this is OK?
                let mut games = Vec::with_capacity(entries.len());
                let mut i = 0;
                while i < entries.len() {
                    if matches!(entries[i], Entry::Game(_)) {
                        match entries.remove(i) {
                            Entry::Game(game) => games.push(game),
                            _ => unreachable!(),
                        }
                    } else {
                        i += 1;
                    }
                }

                let db_games = database
                    .select_games(&games.iter().map(|g| g.path.as_path()).collect::<Vec<_>>())?;

                let mut games = games.into_iter().zip(db_games).collect::<Vec<_>>();
                games.sort_unstable_by_key(|(_, db_game)| {
                    db_game.as_ref().map(|g| Reverse(g.last_played))
                });
                entries.retain(|e| matches!(e, Entry::Directory(_) | Entry::App(_)));
                entries.sort_unstable();
                entries.extend(games.into_iter().map(|(game, _)| Entry::Game(game)));
            }
            GamesSort::MostPlayed(_) => {
                let mut games = Vec::with_capacity(entries.len());
                let mut i = 0;
                while i < entries.len() {
                    if matches!(entries[i], Entry::Game(_)) {
                        match entries.remove(i) {
                            Entry::Game(game) => games.push(game),
                            _ => unreachable!(),
                        }
                    } else {
                        i += 1;
                    }
                }

                let db_games = database
                    .select_games(&games.iter().map(|g| g.path.as_path()).collect::<Vec<_>>())?;

                let mut games = games.into_iter().zip(db_games).collect::<Vec<_>>();
                games.sort_unstable_by_key(|(_, db_game)| {
                    db_game.as_ref().map(|g| Reverse(g.play_time))
                });
                entries.retain(|e| matches!(e, Entry::Directory(_) | Entry::App(_)));
                entries.sort_unstable();
                entries.extend(games.into_iter().map(|(game, _)| Entry::Game(game)));
            }
            GamesSort::Rating(_) => {
                let mut games = Vec::with_capacity(entries.len());
                let mut i = 0;
                while i < entries.len() {
                    if matches!(entries[i], Entry::Game(_)) {
                        match entries.remove(i) {
                            Entry::Game(game) => games.push(game),
                            _ => unreachable!(),
                        }
                    } else {
                        i += 1;
                    }
                }

                let db_games = database
                    .select_games(&games.iter().map(|g| g.path.as_path()).collect::<Vec<_>>())?;

                let mut games = games.into_iter().zip(db_games).collect::<Vec<_>>();
                games.sort_unstable_by_key(|(_, db_game)| {
                    db_game.as_ref().map(|g| Reverse(g.rating))
                });
                entries.retain(|e| matches!(e, Entry::Directory(_) | Entry::App(_)));
                entries.sort_unstable();
                entries.extend(games.into_iter().map(|(game, _)| Entry::Game(game)));
            }
            GamesSort::ReleaseDate(_) => {
                let mut games = Vec::with_capacity(entries.len());
                let mut i = 0;
                while i < entries.len() {
                    if matches!(entries[i], Entry::Game(_)) {
                        match entries.remove(i) {
                            Entry::Game(game) => games.push(game),
                            _ => unreachable!(),
                        }
                    } else {
                        i += 1;
                    }
                }

                let db_games = database
                    .select_games(&games.iter().map(|g| g.path.as_path()).collect::<Vec<_>>())?;

                let mut games = games.into_iter().zip(db_games).collect::<Vec<_>>();
                games.sort_unstable_by_key(|(_, db_game)| {
                    db_game.as_ref().map(|g| Reverse(g.release_date))
                });
                entries.retain(|e| matches!(e, Entry::Directory(_) | Entry::App(_)));
                entries.sort_unstable();
                entries.extend(games.into_iter().map(|(game, _)| Entry::Game(game)));
            }
            GamesSort::Random(_) => {
                entries.shuffle(&mut rand::rng());
            }
        }

        Ok(entries)
    }

    fn preserve_selection(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::entry::game::Game;

    #[tokio::test]
    async fn metadata_refresh_waits_for_context_menu_to_close() {
        common::media::invalidate();
        common::media::defer_cold_sources();
        let root =
            std::env::temp_dir().join(format!("allium-games-menu-refresh-{}", std::process::id()));
        let dir = root.join("Roms/PS");
        std::fs::create_dir_all(&dir).unwrap();
        let paths = [dir.join("Match A.chd"), dir.join("Match B.chd")];
        let database = Database::in_memory().unwrap();
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
            let mut game = crate::entry::game::Game::new(path.clone());
            game.image = crate::entry::lazy_image::LazyImage::NotFound;
            database.update_games(&[game.into()]).unwrap();
        }
        std::fs::write(root.join("Roms/gamelist.xml"),
            "<gameList><game><path>PS/Match A.chd</path><name>Zulu</name></game><game><path>PS/Match B.chd</path><name>Alpha</name></game></gameList>").unwrap();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        let mut mapper = ConsoleMapper::new();
        mapper.load_config().unwrap();
        assert!(!mapper.get_console(&paths[0]).unwrap().cores.is_empty());
        map.insert(mapper);
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let list = common::media::with_name_indexes(|| {
            EntryList::new(
                Rect::new(0, 54, 640, 376),
                res.clone(),
                GamesSort::Alphabetical(Directory::new(dir)),
            )
        })
        .unwrap();
        let mut view = Games::new(Rect::new(0, 54, 640, 376), res, list).unwrap();
        let before = view.list.save();
        let generation = view.name_generation;
        let (send, mut recv) = tokio::sync::mpsc::channel(16);
        view.handle_key_event(
            KeyEvent::Pressed(Key::Select),
            send.clone(),
            &mut VecDeque::new(),
        )
        .await
        .unwrap();
        assert!(view.list.interaction_active());
        view.handle_key_event(
            KeyEvent::Pressed(Key::Down),
            send.clone(),
            &mut VecDeque::new(),
        )
        .await
        .unwrap();
        view.handle_key_event(
            KeyEvent::Pressed(Key::Right),
            send.clone(),
            &mut VecDeque::new(),
        )
        .await
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            common::media::poll_name_indexes();
            let _ = common::media::cached_name(&paths[0]);
            if common::media::cached_name(&paths[0]).as_deref() == Some("Zulu") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "index never completed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        view.update(std::time::Duration::from_millis(16));
        assert!(
            view.list.interaction_active(),
            "background refresh closed context menu"
        );
        assert_eq!(view.name_generation, generation);
        assert_eq!(view.list.save().selected, before.selected);
        assert_eq!(view.list.save().selected_path, before.selected_path);
        view.handle_key_event(KeyEvent::Pressed(Key::Select), send, &mut VecDeque::new())
            .await
            .unwrap();
        assert!(!view.list.interaction_active());
        assert!(matches!(recv.try_recv(), Ok(Command::Redraw)));
        assert!(
            recv.try_recv().is_err(),
            "menu cancellation launched a game"
        );
        view.update(std::time::Duration::from_millis(16));
        assert_eq!(view.name_generation, common::media::name_generation());
        assert_eq!(view.list.save().selected_path.as_ref(), Some(&paths[0]));
        assert_eq!(
            view.list.save().selected,
            1,
            "names did not refresh after menu closed"
        );
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    fn games_update_retries_cold_sources_and_preserves_nested_selection_by_path() {
        common::media::invalidate();
        common::media::defer_cold_sources();
        let root =
            std::env::temp_dir().join(format!("allium-games-refresh-{}", std::process::id()));
        let system = root.join("Roms/PS");
        let nested = system.join("Pack");
        std::fs::create_dir_all(&nested).unwrap();
        let paths = [nested.join("RawA.chd"), nested.join("RawB.chd")];
        let database = Database::in_memory().unwrap();
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
            let mut game = Game::new(path.clone());
            game.image = crate::entry::lazy_image::LazyImage::NotFound;
            database.update_games(&[game.into()]).unwrap();
        }
        std::fs::write(root.join("Roms/gamelist.xml"),
            "<gameList><game><path>PS/Pack/RawA.chd</path><name>Zulu</name></game><game><path>PS/Pack/RawB.chd</path><name>Alpha</name></game></gameList>").unwrap();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let state = GamesState {
            sort: GamesSort::Alphabetical(Directory::new(system)),
            selected: 0,
            selected_path: Some(nested.clone()),
            child: Some(Box::new(GamesState {
                sort: GamesSort::Alphabetical(Directory::new(nested.clone())),
                selected: 0,
                selected_path: Some(paths[0].clone()),
                child: None,
            })),
        };
        let list = common::media::with_name_indexes(|| {
            EntryList::load(Rect::new(0, 54, 640, 376), res.clone(), state)
        })
        .unwrap();
        let mut view = Games::new(Rect::new(0, 54, 640, 376), res, list).unwrap();
        assert_eq!(view.save().child.unwrap().selected, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            view.update(std::time::Duration::from_millis(16));
            let state = view.save();
            assert_eq!(state.selected_path.as_ref(), Some(&nested));
            let child = state.child.unwrap();
            assert_eq!(child.sort.directory().path, nested);
            assert_eq!(child.selected_path.as_ref(), Some(&paths[0]));
            if child.selected == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Games entries never rehydrated and reordered"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            common::media::cached_name(&paths[0]).as_deref(),
            Some("Zulu")
        );
        assert_eq!(
            common::media::cached_name(&paths[1]).as_deref(),
            Some("Alpha")
        );
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}
