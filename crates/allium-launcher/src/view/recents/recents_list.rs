use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::constants::RECENT_GAMES_LIMIT;
use common::database::Database;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, View};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::directory::Directory;
use crate::entry::game::Game;
use crate::entry::lazy_image::LazyImage;
use crate::entry::{Entry, Sort};
use crate::view::entry_list::{EntryList, EntryListState};

pub type RecentsListState = EntryListState<RecentsSort>;

#[derive(Debug)]
pub struct RecentsList {
    rect: Rect,
    list: EntryList<RecentsSort>,
    button_hints: ButtonHints<String>,
    name_generation: u64,
}

impl RecentsList {
    pub(crate) fn idle_media_step(&mut self, handle: &common::theme::ThemeHandle) -> bool {
        self.list
            .idle_media_step(handle, common::theme::pack::ScreenId::Recents)
    }

    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        self.list.root_tab_navigation_ready()
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        self.list.clear_theme()
    }
    pub(crate) fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
        self.list
            .theme_models(common::theme::pack::ScreenId::Recents, handle)
    }

    pub fn new(rect: Rect, res: Resources, list: EntryList<RecentsSort>) -> Result<Self> {
        let styles = res.get::<Stylesheet>();

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

        drop(styles);

        Ok(Self {
            rect,
            list,
            button_hints,
            name_generation: common::media::name_generation(),
        })
    }

    pub fn load_or_new(
        rect: Rect,
        res: Resources,
        state: Option<RecentsListState>,
    ) -> Result<Self> {
        let list = if let Some(state) = state {
            common::media::with_name_indexes(|| EntryList::load(rect, res.clone(), state))?
        } else {
            common::media::with_name_indexes(|| {
                EntryList::new(rect, res.clone(), RecentsSort::LastPlayed)
            })?
        };

        Self::new(rect, res, list)
    }

    pub fn save(&self) -> RecentsListState {
        self.list.save()
    }
}

#[async_trait(?Send)]
impl View for RecentsList {
    fn update(&mut self, dt: std::time::Duration) {
        common::media::poll_name_indexes();
        let generation = common::media::name_generation();
        if self.name_generation != generation
            && !self.list.details_active()
            && !self.list.disc_picker_active()
        {
            let state = self.list.save();
            match common::media::with_name_indexes(|| self.list.sort(state.sort)) {
                Ok(()) => {
                    self.list.select(state.selected);
                    self.name_generation = generation;
                }
                Err(error) => log::error!("Refresh recent names: {error:#}"),
            }
        }
        self.list.update(dt);
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        if self.list.should_draw() {
            drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
            self.button_hints.set_should_draw();
        }
        drawn |= self.button_hints.should_draw() && self.button_hints.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.list.should_draw() || self.button_hints.should_draw()
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
pub enum RecentsSort {
    LastPlayed,
    MostPlayed,
    Favorites,
    Random,
    Search(String),
}

impl Sort for RecentsSort {
    fn button_hint(&self, locale: &Locale) -> String {
        match self {
            RecentsSort::LastPlayed => locale.t("sort-last-played"),
            RecentsSort::MostPlayed => locale.t("sort-most-played"),
            RecentsSort::Favorites => locale.t("sort-favorites"),
            RecentsSort::Random => locale.t("sort-random"),
            RecentsSort::Search(_) => locale.t("sort-search"),
        }
    }

    fn next(&self) -> Self {
        match self {
            RecentsSort::LastPlayed => RecentsSort::MostPlayed,
            RecentsSort::MostPlayed => RecentsSort::Favorites,
            RecentsSort::Favorites => RecentsSort::Random,
            RecentsSort::Random => RecentsSort::LastPlayed,
            RecentsSort::Search(_) => RecentsSort::LastPlayed,
        }
    }

    fn with_directory(&self, _directory: Directory) -> Self {
        unimplemented!();
    }

    fn entries(
        &self,
        database: &Database,
        _console_mapper: &ConsoleMapper,
        _locale: &Locale,
    ) -> Result<Vec<Entry>> {
        common::media::defer_cold_sources();
        let games = match self {
            RecentsSort::LastPlayed => database.select_last_played(RECENT_GAMES_LIMIT),
            RecentsSort::MostPlayed => database.select_most_played(RECENT_GAMES_LIMIT),
            RecentsSort::Favorites => database.select_favorites(RECENT_GAMES_LIMIT),
            RecentsSort::Random => database.select_random(RECENT_GAMES_LIMIT),
            RecentsSort::Search(query) => database.search(query, RECENT_GAMES_LIMIT),
        };

        let games = match games {
            Ok(games) => games,
            Err(err) => {
                log::error!("Failed to select games: {}", err);
                return Err(err);
            }
        };

        Ok(games
            .into_iter()
            .map(|game| {
                let extension = game
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_owned();

                let full_name = game.name.clone();

                let image = LazyImage::from_path(&game.path, game.image);

                let mut game = Game {
                    name: game.name,
                    full_name,
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
                Entry::Game(game)
            })
            .collect())
    }

    fn preserve_selection(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn recent_view_update_adopts_cold_names_without_waiting_for_input() {
        common::media::invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-recents-refresh-{}", std::process::id()));
        let dir = root.join("Roms/TEST");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Raw.chd");
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::write(
            dir.join("gamelist.xml"),
            "<gameList><game><path>./Raw.chd</path><name>Hydrated recent</name></game></gameList>",
        )
        .unwrap();
        let database = Database::in_memory().unwrap();
        assert!(
            RecentsSort::LastPlayed
                .entries(&database, &ConsoleMapper::new(), &Locale::new("en-US"))
                .unwrap()
                .is_empty()
        );
        common::media::check_directory(&dir);
        assert!(common::media::cached_metadata(&path).name.is_empty());
        let row: common::database::NewGame = Game::new(path).into();
        database.update_games(&[row.clone()]).unwrap();
        database.increment_play_count(&row).unwrap();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let rect = Rect::new(0, 54, 640, 376);
        let list = EntryList::new(rect, res.clone(), RecentsSort::LastPlayed).unwrap();
        let view = RecentsList::new(rect, res.clone(), list).unwrap();
        let mut recents = crate::view::recents::Recents::List(Box::new(view));
        let generation = common::media::name_generation();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            recents.update(std::time::Duration::from_millis(16));
            let crate::view::recents::Recents::List(view) = &recents else {
                unreachable!()
            };
            let entries = RecentsSort::LastPlayed
                .entries(&res.get(), &res.get(), &res.get())
                .unwrap();
            if view.name_generation != generation && entries[0].name() == "Hydrated recent" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "recent view did not refresh"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let entries = RecentsSort::LastPlayed
            .entries(&res.get(), &res.get(), &res.get())
            .unwrap();
        assert_eq!(entries[0].name(), "Hydrated recent");
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}
