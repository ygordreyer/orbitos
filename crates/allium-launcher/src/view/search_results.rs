use std::collections::HashMap;
use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::constants::RECENT_GAMES_LIMIT;
use common::database::Database;
use common::display::{Display, color::Color, fill_rect};
use common::geom::{Alignment, Point, Rect};
use common::locale::{Locale, LocaleFluentValue};
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Label, SearchView, View};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::directory::Directory;
use crate::entry::game::Game;
use crate::entry::lazy_image::LazyImage;
use crate::entry::{Entry, Sort};
use crate::view::entry_list::EntryList;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchResultsSort {
    Relevance(String),
    Alphabetical(String),
    LastPlayed(String),
    MostPlayed(String),
}

impl SearchResultsSort {
    fn query(&self) -> &str {
        match self {
            SearchResultsSort::Relevance(q) => q,
            SearchResultsSort::Alphabetical(q) => q,
            SearchResultsSort::LastPlayed(q) => q,
            SearchResultsSort::MostPlayed(q) => q,
        }
    }
}

impl Sort for SearchResultsSort {
    const HAS_BUTTON_HINTS: bool = true;

    fn button_hint(&self, locale: &Locale) -> String {
        match self {
            SearchResultsSort::Relevance(_) => locale.t("sort-relevance"),
            SearchResultsSort::Alphabetical(_) => locale.t("sort-alphabetical"),
            SearchResultsSort::LastPlayed(_) => locale.t("sort-last-played"),
            SearchResultsSort::MostPlayed(_) => locale.t("sort-most-played"),
        }
    }

    fn next(&self) -> Self {
        let query = self.query().to_string();
        match self {
            SearchResultsSort::Relevance(_) => SearchResultsSort::Alphabetical(query),
            SearchResultsSort::Alphabetical(_) => SearchResultsSort::LastPlayed(query),
            SearchResultsSort::LastPlayed(_) => SearchResultsSort::MostPlayed(query),
            SearchResultsSort::MostPlayed(_) => SearchResultsSort::Relevance(query),
        }
    }

    fn with_directory(&self, _directory: Directory) -> Self {
        self.clone()
    }

    fn entries(
        &self,
        database: &Database,
        _console_mapper: &ConsoleMapper,
        _locale: &Locale,
    ) -> Result<Vec<Entry>> {
        common::media::defer_cold_sources();
        let query = self.query();

        let mut games = database.search(query, RECENT_GAMES_LIMIT)?;

        match self {
            SearchResultsSort::Relevance(_) => {}
            SearchResultsSort::Alphabetical(_) => {
                games.sort_by_key(|a| a.name.to_lowercase());
            }
            SearchResultsSort::LastPlayed(_) => {
                games.sort_by_key(|b| std::cmp::Reverse(b.last_played));
            }
            SearchResultsSort::MostPlayed(_) => {
                games.sort_by_key(|b| std::cmp::Reverse(b.play_count));
            }
        }

        let mut entries = games
            .into_iter()
            .filter(|game| crate::entry::listed_path(&game.path))
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
            .collect::<Vec<_>>();
        crate::network_runtime::search(query, &mut entries);
        if matches!(self, Self::Alphabetical(_)) {
            entries.sort_by_key(|entry| entry.name().to_lowercase());
        }
        Ok(entries)
    }

    fn preserve_selection(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct SearchResultsView {
    rect: Rect,
    res: Resources,
    query: String,
    current_sort: SearchResultsSort,
    list: EntryList<SearchResultsSort>,
    header: Label<String>,
    result_count: Label<String>,
    button_hints: ButtonHints<String>,
    search_view: SearchView,
    name_generation: u64,
}

impl SearchResultsView {
    pub(crate) fn details_active(&self) -> bool {
        self.list.details_active()
    }

    pub(crate) fn idle_media_step(&mut self, handle: &common::theme::ThemeHandle) -> bool {
        self.list
            .idle_media_step(handle, common::theme::pack::ScreenId::SearchResults)
    }

    pub(crate) fn release_theme_art(&mut self) {
        self.list.release_theme_art();
    }
    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        self.list.clear_theme()
    }
    pub(crate) fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
        self.list
            .theme_models(common::theme::pack::ScreenId::SearchResults, handle)
    }

    pub fn new(rect: Rect, res: Resources, query: String) -> Result<Self> {
        let Rect { x, y, w, h } = rect;
        let styles = res.get::<Stylesheet>();

        let entry_count = SearchResultsSort::Relevance(query.clone())
            .entries(&res.get(), &res.get(), &res.get())?
            .len();

        let sort = SearchResultsSort::Relevance(query.clone());

        let result_text = {
            let locale = res.get::<Locale>();
            let mut map = HashMap::new();
            map.insert("count".into(), LocaleFluentValue::from(entry_count));
            locale.ta("search-games-found", &map)
        };

        let mut header = Label::new(
            Point::new(x + styles.ui.margin_x, y + styles.ui.margin_y),
            format!("Search: {}", query),
            Alignment::Left,
            Some(w - styles.ui.margin_x as u32 * 2),
        );
        header.font_size(styles.ui.tab_font_size);

        let mut result_count = Label::new(
            Point::new(
                x + styles.ui.margin_x,
                y + styles.ui.margin_y + styles.tab_font_size() as i32,
            ),
            result_text,
            Alignment::Left,
            Some(w - styles.ui.margin_x as u32 * 2),
        );
        result_count.font_size(styles.ui.tab_font_size);

        let list_y = y + styles.ui.margin_y + styles.tab_font_size() as i32 * 2;
        let list = common::media::with_name_indexes(|| {
            EntryList::new(
                Rect::new(x, y + list_y, w, h - list_y as u32),
                res.clone(),
                sort.clone(),
            )
        })?;

        let button_hints = {
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
                        Key::B,
                        locale.t("button-back"),
                        Alignment::Right,
                    ),
                    ButtonHint::new(
                        res.clone(),
                        Point::zero(),
                        Key::Y,
                        sort.button_hint(&locale),
                        Alignment::Right,
                    ),
                ],
            )
        };

        drop(styles);

        Ok(Self {
            rect,
            res: res.clone(),
            query,
            current_sort: sort,
            list,
            header,
            result_count,
            button_hints,
            search_view: SearchView::new(res),
            name_generation: common::media::name_generation(),
        })
    }

    pub fn update_query(&mut self, new_query: String) -> Result<()> {
        self.query = new_query.clone();
        self.header.set_text(format!("🔍 {}", new_query));

        let entry_count = SearchResultsSort::Relevance(new_query.clone())
            .entries(&self.res.get(), &self.res.get(), &self.res.get())?
            .len();

        let sort = SearchResultsSort::Relevance(new_query);
        self.current_sort = sort.clone();
        common::media::with_name_indexes(|| self.list.sort(sort))?;
        self.update_sort_button_hint();

        let result_text = {
            let locale = self.res.get::<Locale>();
            let mut map = HashMap::new();
            map.insert("count".into(), LocaleFluentValue::from(entry_count));
            locale.ta("search-games-found", &map)
        };
        self.result_count.set_text(result_text);

        Ok(())
    }

    fn update_sort_button_hint(&mut self) {
        let locale = self.res.get::<Locale>();
        let sort_text = self.current_sort.button_hint(&locale);
        self.button_hints
            .right_mut()
            .get_mut(2)
            .unwrap()
            .set_text(sort_text);
    }
}

#[async_trait(?Send)]
impl View for SearchResultsView {
    fn update(&mut self, dt: std::time::Duration) {
        common::media::poll_name_indexes();
        let generation = common::media::name_generation();
        if self.name_generation != generation
            && !self.list.interaction_active()
            && !self.list.details_active()
            && !self.list.disc_picker_active()
        {
            let state = self.list.save();
            match common::media::with_name_indexes(|| self.list.sort(self.current_sort.clone())) {
                Ok(()) => {
                    let entries = self
                        .current_sort
                        .entries(&self.res.get(), &self.res.get(), &self.res.get())
                        .unwrap_or_default();
                    let selected = state
                        .selected_path
                        .as_ref()
                        .and_then(|path| entries.iter().position(|entry| entry.path() == path))
                        .unwrap_or(state.selected);
                    self.list.select(selected);
                    let count = entries.len();
                    let mut values = HashMap::new();
                    values.insert("count".into(), LocaleFluentValue::from(count));
                    self.result_count
                        .set_text(self.res.get::<Locale>().ta("search-games-found", &values));
                    self.name_generation = generation;
                    self.set_should_draw();
                }
                Err(error) => log::error!("Refresh search names: {error:#}"),
            }
        }
        self.list.update(dt);
        self.header.update(dt);
        self.result_count.update(dt);
        self.button_hints.update(dt);
        self.search_view.update(dt);
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        let needs_full_redraw = self.header.should_draw() || self.result_count.should_draw();

        if needs_full_redraw {
            display.load(self.rect)?;
            if crate::theme_adapter::legacy_orbit(&self.res) {
                fill_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(0, 0, 640, 480),
                    super::orbit::background(),
                );
            }

            self.header.set_should_draw();
            self.result_count.set_should_draw();
            self.list.set_should_draw();
            self.button_hints.set_should_draw();
        }

        if crate::theme_adapter::legacy_orbit(&self.res) {
            let mut header_styles = styles.clone();
            header_styles.ui.text_color = Color::new(35, 48, 51);
            header_styles.ui.stroke_width = 0;
            drawn |= self.header.should_draw() && self.header.draw(display, &header_styles)?;
            drawn |= self.result_count.should_draw()
                && self.result_count.draw(display, &header_styles)?;
        } else {
            drawn |= self.header.should_draw() && self.header.draw(display, styles)?;
            drawn |= self.result_count.should_draw() && self.result_count.draw(display, styles)?;
        }

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;

        if self.button_hints.should_draw() {
            display.load(Rect::new(
                0,
                display.size().h as i32 - 48,
                display.size().w,
                48,
            ))?;
            self.button_hints.set_should_draw();
            drawn |= self.button_hints.draw(display, styles)?;
            if crate::theme_adapter::legacy_orbit(&self.res) {
                fill_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(0, 425, 640, 55),
                    super::orbit::background(),
                );
                super::orbit::font(styles, 16, Color::new(35, 48, 51)).draw(
                    &mut display.pixmap_mut(),
                    "A Select    B Back    Y Sort",
                    Point::new(26, 455),
                );
            }
        }

        if self.search_view.is_active() {
            display.load(self.rect)?;
            self.search_view.set_should_draw();
        }
        drawn |= self.search_view.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.header.should_draw()
            || self.result_count.should_draw()
            || self.list.should_draw()
            || self.button_hints.should_draw()
            || self.search_view.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.header.set_should_draw();
        self.result_count.set_should_draw();
        self.list.set_should_draw();
        self.button_hints.set_should_draw();
        self.search_view.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if self.list.details_active() || self.list.disc_picker_active() {
            return self.list.handle_key_event(event, commands, bubble).await;
        }
        if self.search_view.is_active()
            && self
                .search_view
                .handle_key_event(event, commands.clone(), bubble)
                .await?
        {
            for cmd in bubble.iter() {
                if let Command::Search(new_query) = cmd {
                    self.update_query(new_query.clone())?;
                    commands.send(Command::Redraw).await?;
                    break;
                }
            }
            return Ok(true);
        }

        match event {
            KeyEvent::Pressed(Key::B) => {
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            KeyEvent::Pressed(Key::X) => {
                self.search_view.activate_with_value(self.query.clone());
                commands.send(Command::Redraw).await?;
                Ok(true)
            }
            KeyEvent::Pressed(Key::Y) => {
                if self.list.handle_key_event(event, commands, bubble).await? {
                    self.current_sort = self.current_sort.next();
                    self.update_sort_button_hint();
                    self.button_hints.set_should_draw();
                    Ok(true)
                } else {
                    Ok(false)
                }
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

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[tokio::test]
    async fn metadata_refresh_waits_for_context_menu_to_close() {
        common::media::invalidate();
        common::media::defer_cold_sources();
        let root = std::env::temp_dir().join(format!(
            "allium-search_results-menu-refresh-{}",
            std::process::id()
        ));
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
        let mut view =
            SearchResultsView::new(Rect::new(0, 0, 640, 480), res, "Match".into()).unwrap();
        view.current_sort = SearchResultsSort::Alphabetical("Match".into());
        common::media::with_name_indexes(|| view.list.sort(view.current_sort.clone())).unwrap();
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
    fn search_view_update_hydrates_existing_database_hits_and_preserves_fields() {
        common::media::invalidate();
        let root =
            std::env::temp_dir().join(format!("allium-search-refresh-{}", std::process::id()));
        let dir = root.join("Roms/TEST");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Raw.chd");
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::write(
            dir.join("gamelist.xml"),
            "<gameList><game><path>./Raw.chd</path><name>Hydrated search</name></game></gameList>",
        )
        .unwrap();
        let mut game = Game::new(path);
        game.favorite = true;
        game.rating = Some(7);
        game.developer = Some("Database studio".into());
        let database = Database::in_memory().unwrap();
        let row: common::database::NewGame = game.clone().into();
        database.update_games(&[row.clone()]).unwrap();
        database.set_favorite(&row).unwrap();
        common::media::invalidate();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let mut view =
            SearchResultsView::new(Rect::new(0, 0, 640, 480), res.clone(), "Raw".into()).unwrap();
        let generation = view.name_generation;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            view.update(std::time::Duration::from_millis(16));
            if view.name_generation != generation {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "search view did not refresh"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let entries = view
            .current_sort
            .entries(&res.get(), &res.get(), &res.get())
            .unwrap();
        let Entry::Game(hydrated) = &entries[0] else {
            panic!("game")
        };
        game.name = "Hydrated search".into();
        game.full_name = game.name.clone();
        assert_eq!(hydrated, &game);
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
    #[test]
    fn search_refresh_preserves_game_path_after_name_reorder_and_falls_back_if_removed() {
        common::media::invalidate();
        common::media::defer_cold_sources();
        let root =
            std::env::temp_dir().join(format!("allium-search-reorder-{}", std::process::id()));
        let dir = root.join("Roms/PS");
        std::fs::create_dir_all(&dir).unwrap();
        let paths = [
            dir.join("Match A.chd"),
            dir.join("Match B.chd"),
            dir.join("Match C.chd"),
        ];
        let database = Database::in_memory().unwrap();
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
            let mut game = Game::new(path.clone());
            game.image = crate::entry::lazy_image::LazyImage::NotFound;
            database.update_games(&[game.into()]).unwrap();
        }
        std::fs::write(dir.join("gamelist.xml"),
            "<gameList><game><path>Match A.chd</path><name>Zulu</name></game><game><path>Match B.chd</path><name>Alpha</name></game><game><path>Match C.chd</path><name>Middle</name></game></gameList>").unwrap();
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let mut view =
            SearchResultsView::new(Rect::new(0, 0, 640, 480), res.clone(), "Match".into()).unwrap();
        view.current_sort = SearchResultsSort::Alphabetical("Match".into());
        common::media::with_name_indexes(|| view.list.sort(view.current_sort.clone())).unwrap();
        view.list.select(0);
        assert_eq!(view.list.save().selected_path.as_ref(), Some(&paths[0]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            view.update(std::time::Duration::from_millis(16));
            if common::media::cached_name(&paths[0]).as_deref() == Some("Zulu") {
                view.update(std::time::Duration::from_millis(16));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "search names never hydrated"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let state = view.list.save();
        assert_eq!(state.selected_path.as_ref(), Some(&paths[0]));
        assert_eq!(state.selected, 2);
        view.list.select(1);
        assert_eq!(view.list.save().selected_path.as_ref(), Some(&paths[2]));
        res.get::<Database>().delete_game(&paths[2]).unwrap();
        view.name_generation = common::media::name_generation().wrapping_sub(1);
        view.update(std::time::Duration::from_millis(16));
        let state = view.list.save();
        assert_eq!(state.selected, 1);
        assert_eq!(state.selected_path.as_ref(), Some(&paths[0]));
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}

#[cfg(test)]
mod rom_filter_tests {
    use super::*;
    #[test]
    fn search_does_not_revive_indexed_commands_or_metadata_sidecars() {
        let database = Database::in_memory().unwrap();
        for name in ["~Filter.miyoocmd", "Refresh.MIYOOCMD", "metadata.xml", "systeminfo.txt"] {
            database.update_games(&[Game::new(std::path::PathBuf::from(name)).into()]).unwrap();
        }
        for name in ["Filter", "Refresh", "metadata", "systeminfo"] {
            let entries = SearchResultsSort::Relevance(name.into()).entries(
                &database, &ConsoleMapper::new(), &Locale::new("en-US")).unwrap();
            assert!(entries.is_empty(), "sidecar appeared in search: {name}");
        }
    }
}
