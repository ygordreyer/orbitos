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
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::directory::{Directory, DirectoryEntries, GameMetadata};
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

    pub(crate) fn details_active(&self) -> bool {
        self.list.details_active()
    }

    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        self.list.root_tab_navigation_ready()
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        self.list.clear_theme()
    }
    pub(crate) fn details_warm_model(&mut self, handle: &common::theme::ThemeHandle, screen: common::theme::pack::ScreenId) -> Option<super::entry_list::NavigationWarm> {
        self.list.details_warm_model(handle, screen)
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
            if self.list.refresh_names() {
                self.set_should_draw();
            }
            self.name_generation = generation;
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

impl GamesSort {
    fn sort_loaded(&self, loaded: DirectoryEntries, rng: &mut impl rand::Rng) -> Vec<Entry> {
        let DirectoryEntries {
            mut entries,
            mut games,
        } = loaded;
        if matches!(self, Self::Random(_)) {
            entries.shuffle(rng);
            return entries;
        }
        let (mut entries, game_entries) = partition_entries(entries);
        entries.sort();
        // Metadata keys are taken from the same rows that constructed the entries.
        let mut keyed: Vec<_> = game_entries
            .into_iter()
            .map(|game| {
                let metadata = games.remove(&game.path);
                (game, metadata)
            })
            .collect();
        drop(games);
        match self {
            Self::Alphabetical(_) => {
                keyed.sort_by(|(a, a_row), (b, b_row)| {
                    let favorite =
                        |row: &Option<GameMetadata>| row.as_ref().is_some_and(|row| row.favorite);
                    favorite(b_row).cmp(&favorite(a_row)).then_with(|| a.cmp(b))
                });
            }
            Self::LastPlayed(_) => {
                keyed.sort_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.last_played)))
            }
            Self::MostPlayed(_) => {
                keyed.sort_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.play_time)))
            }
            Self::Rating(_) => {
                keyed.sort_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.rating)))
            }
            Self::ReleaseDate(_) => {
                keyed.sort_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.release_date)))
            }
            Self::Random(_) => unreachable!(),
        }
        if entries.is_empty() {
            // Let collect reuse the consumed allocation for the common all-game list.
            keyed
                .into_iter()
                .map(|(game, _)| Entry::Game(game))
                .collect()
        } else {
            entries.extend(keyed.into_iter().map(|(game, _)| Entry::Game(game)));
            entries
        }
    }
}

fn partition_entries(entries: Vec<Entry>) -> (Vec<Entry>, Vec<crate::entry::game::Game>) {
    let mut others = Vec::new();
    let mut games = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            Entry::Game(game) => games.push(game),
            Entry::Directory(_) | Entry::App(_) => others.push(entry),
        }
    }
    (others, games)
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
        let loaded = self
            .directory()
            .entries_with_metadata(database, console_mapper, locale)?;
        let mut rng = rand::rng();
        Ok(self.sort_loaded(loaded, &mut rng))
    }

    fn refresh_names(&self, entries: &mut [Entry]) -> bool {
        let changed = crate::entry::hydrate_names(entries);
        if changed && matches!(self, Self::Alphabetical(_)) {
            entries.sort_by(|a, b| match (a, b) {
                (Entry::Game(a), Entry::Game(b)) => {
                    b.favorite.cmp(&a.favorite).then_with(|| a.cmp(b))
                }
                _ => a.cmp(b),
            });
        }
        changed
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

#[cfg(test)]
mod list_construction_tests {
    use super::*;
    use crate::entry::directory::list_tests::{Fixture, legacy_warm_load};
    use crate::entry::{app::App, game::Game, lazy_image::LazyImage, load_work};
    use rand::{SeedableRng, rngs::StdRng};
    use std::{hint::black_box, time::Instant};

    fn legacy_partition(mut entries: Vec<Entry>) -> (Vec<Entry>, Vec<Game>) {
        let mut games = Vec::with_capacity(entries.len());
        let mut i = 0;
        while i < entries.len() {
            if matches!(entries[i], Entry::Game(_)) {
                let Entry::Game(game) = entries.remove(i) else {
                    unreachable!()
                };
                games.push(game);
            } else {
                i += 1;
            }
        }
        (entries, games)
    }

    fn legacy_sort(sort: &GamesSort, loaded: DirectoryEntries) -> Vec<Entry> {
        let DirectoryEntries {
            mut entries,
            mut games,
        } = loaded;
        if matches!(sort, GamesSort::Random(_)) {
            entries.shuffle(&mut StdRng::seed_from_u64(42));
            return entries;
        }
        let (mut entries, game_entries) = legacy_partition(entries);
        let mut keyed: Vec<_> = game_entries
            .into_iter()
            .map(|game| {
                let metadata = games.remove(&game.path);
                (game, metadata)
            })
            .collect();
        match sort {
            GamesSort::Alphabetical(_) => {
                let (favorites, non_favorites): (Vec<_>, Vec<_>) = keyed
                    .into_iter()
                    .partition(|(_, row)| row.as_ref().is_some_and(|row| row.favorite));
                let mut favorites: Vec<_> = favorites.into_iter().map(|(game, _)| game).collect();
                let mut non_favorites: Vec<_> =
                    non_favorites.into_iter().map(|(game, _)| game).collect();
                favorites.sort_unstable();
                non_favorites.sort_unstable();
                entries.sort_unstable();
                entries.extend(favorites.into_iter().chain(non_favorites).map(Entry::Game));
                return entries;
            }
            GamesSort::LastPlayed(_) => keyed
                .sort_unstable_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.last_played))),
            GamesSort::MostPlayed(_) => keyed
                .sort_unstable_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.play_time))),
            GamesSort::Rating(_) => {
                keyed.sort_unstable_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.rating)))
            }
            GamesSort::ReleaseDate(_) => keyed
                .sort_unstable_by_key(|(_, row)| row.as_ref().map(|row| Reverse(row.release_date))),
            GamesSort::Random(_) => unreachable!(),
        }
        entries.sort_unstable();
        entries.extend(keyed.into_iter().map(|(game, _)| Entry::Game(game)));
        entries
    }

    fn synthetic(count: usize) -> Vec<Entry> {
        (0..count)
            .map(|i| {
                let mut game = Game::new(format!("/synthetic/Game {i:05}.chd").into());
                game.image = LazyImage::NotFound;
                Entry::Game(game)
            })
            .collect()
    }

    fn metadata(entries: Vec<Entry>) -> DirectoryEntries {
        let games = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let Entry::Game(game) = entry else {
                    return None;
                };
                // One missing row exercises the old Option-key ordering.
                if i == 0 {
                    return None;
                }
                Some((
                    game.path.clone(),
                    GameMetadata {
                        core: None,
                        favorite: i % 2 == 0,
                        last_played: i as i64,
                        play_time: chrono::Duration::seconds(i as i64),
                        rating: Some(i as u8),
                        release_date: chrono::NaiveDate::from_ymd_opt(2000 + i as i32, 1, 1),
                    },
                ))
            })
            .collect();
        DirectoryEntries { entries, games }
    }

    #[test]
    fn every_sort_matches_legacy_for_favorites_missing_rows_and_mixed_entries() {
        let mut entries = synthetic(8);
        entries.insert(
            1,
            Entry::Directory(Directory::new("/synthetic/Z folder".into())),
        );
        entries.insert(
            4,
            Entry::Directory(Directory::new("/synthetic/A folder".into())),
        );
        entries.insert(
            3,
            Entry::App(App {
                name: "Tool".into(),
                directory: "/synthetic/Tool.pak".into(),
                launch: "launch.sh".into(),
                image: None,
            }),
        );
        entries.swap(2, 8);
        let dir = Directory::default();
        for sort in [
            GamesSort::Alphabetical(dir.clone()),
            GamesSort::LastPlayed(dir.clone()),
            GamesSort::MostPlayed(dir.clone()),
            GamesSort::Rating(dir.clone()),
            GamesSort::ReleaseDate(dir.clone()),
            GamesSort::Random(dir),
        ] {
            let expected = legacy_sort(&sort, metadata(entries.clone()));
            let actual =
                sort.sort_loaded(metadata(entries.clone()), &mut StdRng::seed_from_u64(42));
            assert_eq!(actual, expected, "{sort:?}");
            let empty = sort.sort_loaded(metadata(Vec::new()), &mut StdRng::seed_from_u64(42));
            assert!(empty.is_empty());
        }
    }

    #[test]
    fn real_loader_matches_legacy_with_trailing_slash_and_persisted_sort_keys() {
        let fixture = Fixture::new();
        let database = Database::in_memory().unwrap();
        fixture.games(3, &database);
        let favorite_path = fixture.0.path.join("Game 00002.chd");
        database.set_core(&favorite_path, "custom-core").unwrap();
        let mut favorite: common::database::NewGame = Game::new(favorite_path.clone()).into();
        favorite.favorite = true;
        database.set_favorite(&favorite).unwrap();
        for i in 0..3 {
            let path = fixture.0.path.join(format!("Game {i:05}.chd"));
            database
                .add_play_time(&path, chrono::Duration::seconds((i + 1) * 100))
                .unwrap();
            database
                .increment_play_count(&Game::new(path).into())
                .unwrap();
        }
        let mapper = ConsoleMapper::new();
        let locale = Locale::new("en-US");
        let directory = Directory::new(fixture.0.path.join(""));
        assert!(directory.path.to_string_lossy().ends_with('/'));
        for sort in [
            GamesSort::Alphabetical(directory.clone()),
            GamesSort::LastPlayed(directory.clone()),
            GamesSort::MostPlayed(directory.clone()),
            GamesSort::Rating(directory.clone()),
            GamesSort::ReleaseDate(directory.clone()),
        ] {
            let expected = legacy_sort(&sort, legacy_warm_load(&fixture.0, &database));
            let actual = sort.entries(&database, &mapper, &locale).unwrap();
            assert_eq!(actual, expected, "{sort:?}");
            let favorite = actual
                .iter()
                .find_map(|entry| match entry {
                    Entry::Game(game) if game.path == favorite_path => Some(game),
                    _ => None,
                })
                .unwrap();
            assert_eq!(favorite.core.as_deref(), Some("custom-core"));
            assert!(favorite.favorite);
            if matches!(sort, GamesSort::MostPlayed(_)) {
                let paths: Vec<_> = actual.iter().map(Entry::path).collect();
                let expected: Vec<_> = (0..3)
                    .rev()
                    .map(|i| fixture.0.path.join(format!("Game {i:05}.chd")))
                    .collect();
                assert_eq!(paths, expected);
            }
        }
    }

    #[test]
    fn equal_sort_keys_keep_input_order() {
        let entries = synthetic(128).into_iter().rev().collect::<Vec<_>>();
        let mut loaded = metadata(entries.clone());
        for row in loaded.games.values_mut() {
            row.last_played = 0;
            row.play_time = chrono::Duration::zero();
            row.rating = None;
            row.release_date = None;
        }
        // Include the first entry in the tie rather than treating it as an absent DB row.
        loaded.games.insert(
            entries[0].path().to_path_buf(),
            loaded.games.values().next().unwrap().clone(),
        );
        let dir = Directory::default();
        for sort in [
            GamesSort::LastPlayed(dir.clone()),
            GamesSort::MostPlayed(dir.clone()),
            GamesSort::Rating(dir.clone()),
            GamesSort::ReleaseDate(dir),
        ] {
            let actual = sort.sort_loaded(
                DirectoryEntries {
                    entries: entries.clone(),
                    games: loaded.games.clone(),
                },
                &mut StdRng::seed_from_u64(42),
            );
            assert_eq!(actual, entries, "{sort:?}");
        }
    }

    #[test]
    fn name_refresh_keeps_images_and_only_reorders_alphabetical_games() {
        let fixture = Fixture::new();
        let path_a = fixture.0.path.join("A.chd");
        let path_b = fixture.0.path.join("B.chd");
        std::fs::write(&path_a, b"fixture").unwrap();
        std::fs::write(&path_b, b"fixture").unwrap();
        std::fs::write(fixture.0.path.join("gamelist.xml"), "<gameList><game><path>./A.chd</path><name>Zulu</name></game><game><path>./B.chd</path><name>Alpha</name></game></gameList>").unwrap();
        common::media::check_directory(&fixture.0.path);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while common::media::cached_name(&path_a).as_deref() != Some("Zulu")
            || common::media::cached_name(&path_b).as_deref() != Some("Alpha")
        {
            common::media::poll_name_indexes();
            assert!(Instant::now() < deadline, "name index did not complete");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut a = Game::new(path_a.clone());
        a.image = LazyImage::Found("retained-art.png".into());
        let b = Game::new(path_b.clone());
        let original = vec![Entry::Game(a), Entry::Game(b)];
        let selected_path = original[0].path().to_path_buf();
        let dir = fixture.0.clone();
        for sort in [
            GamesSort::Alphabetical(dir.clone()),
            GamesSort::LastPlayed(dir.clone()),
            GamesSort::MostPlayed(dir.clone()),
            GamesSort::Rating(dir.clone()),
            GamesSort::ReleaseDate(dir.clone()),
            GamesSort::Random(dir),
        ] {
            let mut entries = original.clone();
            load_work::take();
            assert!(common::media::with_name_indexes(
                || sort.refresh_names(&mut entries)
            ));
            assert_eq!(load_work::take(), load_work::Counts::default());
            let selected = entries
                .iter()
                .position(|entry| entry.path() == selected_path)
                .unwrap();
            assert_eq!(
                selected,
                usize::from(matches!(sort, GamesSort::Alphabetical(_)))
            );
            let Entry::Game(game) = &entries[selected] else {
                unreachable!()
            };
            assert_eq!(game.name, "Zulu");
            assert_eq!(game.image, LazyImage::Found("retained-art.png".into()));
            assert!(!common::media::with_name_indexes(
                || sort.refresh_names(&mut entries)
            ));
        }
    }

    #[test]
    #[ignore = "host list construction benchmark; run with --ignored --nocapture"]
    fn benchmark_list_construction() {
        let locale = Locale::new("en-US");
        let mapper = ConsoleMapper::new();
        println!(
            "Warm owned-pipeline counts exclude media indexing/app parsing; file_types count DirEntry lookups (these may stat on filesystems without cached types)."
        );
        for count in [500, 2_000, 5_000] {
            let entries = synthetic(count);
            let mut old_times = Vec::new();
            let mut new_times = Vec::new();
            for _ in 0..7 {
                let input = entries.clone();
                let start = Instant::now();
                let old = black_box(legacy_partition(black_box(input)));
                old_times.push(start.elapsed());
                let input = entries.clone();
                let start = Instant::now();
                let new = black_box(partition_entries(black_box(input)));
                new_times.push(start.elapsed());
                assert_eq!(old, new);
            }
            old_times.sort_unstable();
            new_times.sort_unstable();
            let fixture = Fixture::new();
            let database = Database::in_memory().unwrap();
            fixture.games(count, &database);
            load_work::take();
            let before = legacy_warm_load(&fixture.0, &database);
            let old_work = load_work::take();
            let after = fixture
                .0
                .entries_with_metadata(&database, &mapper, &locale)
                .unwrap();
            let new_work = load_work::take();
            assert_eq!(before.entries, after.entries);
            assert_eq!(old_work.sql, count * 2 + 2);
            assert_eq!(new_work.sql, 2);
            assert_eq!(new_work.metadata, 0);
            println!(
                "n={count} partition median_ms before={:.6} after={:.6}; load counts before={old_work:?} after={new_work:?}",
                old_times[3].as_secs_f64() * 1000.0,
                new_times[3].as_secs_f64() * 1000.0
            );
        }
    }
    #[test]
    #[ignore = "host name-generation refresh benchmark"]
    fn benchmark_name_generation_refresh() {
        for count in [500, 2_000] {
            let fixture = Fixture::new();
            let database = Database::in_memory().unwrap();
            fixture.games(count, &database);
            let mut map = type_map::TypeMap::new();
            let mut styles = Stylesheet::new();
            styles.load_fonts().unwrap();
            map.insert(styles);
            map.insert(Locale::new("en-US"));
            map.insert(ConsoleMapper::new());
            map.insert(common::geom::Size::new(640, 480));
            map.insert(database);
            let res = Resources::new(map);
            let list = EntryList::new(
                Rect::new(0, 54, 640, 376),
                res.clone(),
                GamesSort::Alphabetical(fixture.0.clone()),
            )
            .unwrap();
            let mut view = Games::new(Rect::new(0, 54, 640, 376), res, list).unwrap();
            let mut samples = Vec::new();
            let mut reload = Vec::new();
            for _ in 0..7 {
                let state = view.list.save();
                let start = Instant::now();
                let old = common::media::with_name_indexes(|| {
                    EntryList::load(view.rect, view.res.clone(), state)
                })
                .unwrap();
                reload.push(start.elapsed().as_nanos());
                drop(old);
            }
            reload.sort_unstable();
            load_work::take();
            for _ in 0..7 {
                view.name_generation = common::media::name_generation().wrapping_sub(1);
                let start = Instant::now();
                view.update(std::time::Duration::ZERO);
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            println!(
                "REFRESH_BENCH n={count} reload_median_ns={} refresh_median_ns={} refresh_work={:?}",
                reload[3],
                samples[3],
                load_work::take()
            );
        }
    }

}
