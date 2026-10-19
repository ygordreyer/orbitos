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

                let game = Game {
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
                Entry::Game(game)
            })
            .collect::<Vec<_>>();
        crate::network_runtime::search(query, &mut entries);
        entries.retain(|entry| crate::entry::listed_path(entry.path()));
        crate::entry::load_name_sources(entries.iter().map(Entry::path));
        crate::entry::hydrate_names(&mut entries);
        let game_paths = entries.iter().map(|entry| entry.path().to_path_buf()).collect();
        let mut identities = HashMap::new();
        let mut unique: Vec<Entry> = Vec::with_capacity(entries.len());
        for entry in entries {
            let Entry::Game(game) = &entry else { unique.push(entry); continue; };
            let identity = game.display_identity_in(&game_paths);
            if let Some(&index) = identities.get(&identity) {
                // Keep the best-ranked slot, but launch the library's canonical row.
                if entry.path() == identity { unique[index] = entry; }
            } else {
                identities.insert(identity, unique.len());
                unique.push(entry);
            }
        }
        let mut entries = unique;
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
        for name in ["~Filter.miyoocmd", "Refresh.MIYOOCMD", "metadata.xml", "systeminfo.txt",
            "Conflict.sync-conflict-20250415-202527-JENTCHY.zip", ".stfolder/Control.gb", ".stversions/Version.gb"] {
            database.update_games(&[Game::new(std::path::PathBuf::from(name)).into()]).unwrap();
        }
        for name in ["Filter", "Refresh", "metadata", "systeminfo", "Conflict", "Control", "Version"] {
            let entries = SearchResultsSort::Relevance(name.into()).entries(
                &database, &ConsoleMapper::new(), &Locale::new("en-US")).unwrap();
            assert!(entries.is_empty(), "sidecar appeared in search: {name}");
        }
    }
}

#[cfg(test)]
mod search_alias_tests {
    use super::*;
    use crate::entry::directory::list_tests::Fixture;

    fn sorts(query: &str) -> [SearchResultsSort; 4] {
        [SearchResultsSort::Relevance(query.into()), SearchResultsSort::Alphabetical(query.into()),
            SearchResultsSort::LastPlayed(query.into()), SearchResultsSort::MostPlayed(query.into())]
    }

    #[test]
    fn search_deduplicates_folder_and_disc_aliases_preferring_folder() {
        let fixture = Fixture::new();
        let stem = "Kowai Shashin - Shinrei Shashin Kitan (Japan)";
        let folder = fixture.0.path.join("Roms/PS").join(stem);
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        for relative in [format!("{stem}.cue"), "disc.cue".into(), "nested/disc.cue".into()] {
            let disc = folder.join(relative);
            std::fs::write(&disc, b"fixture").unwrap();
            for paths in [[disc.clone(), folder.clone()], [folder.clone(), disc.clone()]] {
                let database = Database::in_memory().unwrap();
                for path in paths {
                    let mut game = Game::new(path);
                    game.name = "Kowai Shashin: Shinrei shashin kitan".into();
                    game.image = LazyImage::NotFound;
                    database.increment_play_count(&game.into()).unwrap();
                }
                assert_eq!(database.search("ko", RECENT_GAMES_LIMIT).unwrap().len(), 2);
                for sort in sorts("ko") {
                    let entries = sort.entries(&database, &ConsoleMapper::new(), &Locale::new("en-US")).unwrap();
                    assert_eq!(entries.len(), 1, "{sort:?}: {disc:?}");
                    assert_eq!(entries[0].path(), folder, "prefer the library's folder row");
                }
                assert_eq!(database.select_all_games().unwrap().len(), 2, "keep history rows");
            }
        }
    }

    #[test]
    fn search_keeps_best_alias_position_when_replacing_it_with_folder() {
        let database = Database::in_memory().unwrap();
        let folder = std::path::PathBuf::from("Roms/PS/Kowai Shashin (Japan)");
        let cue = folder.join("Kowai Shashin (Japan).cue");
        let other = std::path::PathBuf::from("Roms/PS/Kowai Other.chd");
        for (path, plays) in [(&cue, 3), (&other, 2), (&folder, 1)] {
            let mut game = Game::new(path.clone());
            game.image = LazyImage::NotFound;
            for _ in 0..plays { database.increment_play_count(&game.clone().into()).unwrap(); }
        }
        for sort in [SearchResultsSort::Relevance("ko".into()), SearchResultsSort::MostPlayed("ko".into())] {
            let entries = sort.entries(&database, &ConsoleMapper::new(), &Locale::new("en-US")).unwrap();
            assert_eq!(entries.iter().map(Entry::path).collect::<Vec<_>>(), vec![folder.as_path(), other.as_path()]);
        }
    }

    #[test]
    fn search_keeps_regional_games_with_the_same_display_name() {
        let database = Database::in_memory().unwrap();
        let paths = ["Roms/SFC/Batman Returns (USA).sfc", "Roms/SFC/Batman Returns (Europe).sfc"];
        for path in paths {
            let mut game = Game::new(path.into());
            assert_eq!(game.name, "Batman Returns");
            game.image = LazyImage::NotFound;
            database.update_games(&[game.into()]).unwrap();
        }
        for sort in sorts("Batman") {
            let entries = sort.entries(&database, &ConsoleMapper::new(), &Locale::new("en-US")).unwrap();
            assert_eq!(entries.len(), 2);
            for path in paths { assert!(entries.iter().any(|entry| entry.path() == std::path::Path::new(path))); }
        }
    }
}

#[cfg(test)]
mod display_name_tests {
    use super::SearchResultsSort;
    use std::{fs, path::Path};
    use crate::entry::game::Game;
    use crate::consoles::ConsoleMapper;
    use crate::entry::{Entry, Sort, directory::{Directory, list_tests::Fixture}, load_work};
    use crate::entry::lazy_image::LazyImage;
    use crate::view::recents::RecentsSort;
    use common::{database::Database, locale::Locale, media};

    fn assert_models(database: &Database, system: &Path, expected: &[(&Path, &str)]) {
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        for model in 0..3 {
            media::invalidate();
            let mut entries = match model {
                0 => RecentsSort::LastPlayed.entries(database, &mapper, &locale).unwrap(),
                1 => SearchResultsSort::Relevance("Clock".into()).entries(database, &mapper, &locale).unwrap(),
                _ => Directory::new(system.into()).entries(database, &mapper, &locale).unwrap(),
            };
            assert_eq!(entries.len(), expected.len());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                media::poll_name_indexes();
                crate::entry::hydrate_names(&mut entries);
                if expected.iter().all(|(path, name)| entries.iter().any(|entry| entry.path() == *path && entry.name() == *name)) {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "model {model} names did not hydrate: {entries:?}");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            for (path, name) in expected {
                let entry = entries.iter().find(|entry| entry.path() == *path).unwrap();
                // Details and context menus receive this same Game, not the database row.
                let Entry::Game(game) = entry else { panic!("expected game") };
                assert_eq!(game.name, *name);
            }
            load_work::take();
            for _ in 0..8 {
                assert!(!crate::entry::hydrate_names(&mut entries));
            }
            assert_eq!(load_work::take(), Default::default(), "warm name refresh must not scan or query");
        }
    }

    #[test]
    fn fix18_old_cue_rows_use_parent_gamelist_title_without_rewriting_history() {
        media::invalidate();
        let fixture = Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        let basename = "Kowai Shashin - Shinrei Shashin Kitan (Japan)";
        let folder = system.join(basename);
        fs::create_dir_all(&folder).unwrap();
        let cue = folder.join(format!("{basename}.cue"));
        fs::write(&cue, format!("FILE \"{basename}.bin\" BINARY\n")).unwrap();
        fs::write(folder.join(format!("{basename}.bin")), b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let old_name = "Kowai Shashin - Shinrei Shashin Kitan (Japan)";
        let mut old = Game::new(cue.clone());
        old.name = old_name.into();
        old.image = LazyImage::NotFound;
        database.increment_play_count(&old.into()).unwrap();
        fs::write(system.join("gamelist.xml"), format!(
            "<gameList><game><path>./{basename}</path><name>Kowai Shashin: Shinrei shashin kitan</name></game></gameList>"
        )).unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        let mut models = [
            RecentsSort::LastPlayed.entries(&database, &mapper, &locale).unwrap(),
            SearchResultsSort::Relevance("Kowai".into()).entries(&database, &mapper, &locale).unwrap(),
        ];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while media::cached_name(&folder).is_none() {
            media::poll_name_indexes();
            assert!(std::time::Instant::now() < deadline, "parent title never loaded");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for entries in &mut models {
            crate::entry::hydrate_names(entries);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path(), cue);
            assert_eq!(entries[0].name(), "Kowai Shashin: Shinrei shashin kitan");
            load_work::take();
            for _ in 0..8 { assert!(!crate::entry::hydrate_names(entries)); }
            assert_eq!(load_work::take(), Default::default());
        }
        let library = Directory::new(system.clone()).entries(&database, &mapper, &locale).unwrap();
        assert_eq!(library.len(), 1);
        assert_eq!(library[0].path(), folder);
        assert_eq!(library[0].name(), "Kowai Shashin: Shinrei shashin kitan");
        assert_eq!(database.select_game(&cue).unwrap().unwrap().name, old_name);
        assert_eq!(database.select_game(&cue).unwrap().unwrap().play_count, 1);
        media::invalidate();
    }

    #[test]
    fn raw_folder_names_use_library_cleanup_in_recents_and_search() {
        media::invalidate();
        let fixture = Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        // Dots in folder names must not be interpreted as a file extension.
        let folder = system.join("Clock Tower: The First Fear (Japan) [T-En v1.1]");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("disc.chd"), b"rom").unwrap();
        let single = system.join("Clock Single (USA).chd");
        fs::write(&single, b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let mut saved = Game::new(folder.clone());
        saved.name = folder.file_name().unwrap().to_str().unwrap().into();
        saved.image = LazyImage::NotFound;
        database.increment_play_count(&saved.clone().into()).unwrap();
        let mut single_game = Game::new(single.clone());
        single_game.image = LazyImage::NotFound;
        database.increment_play_count(&single_game.clone().into()).unwrap();
        assert_models(&database, &system, &[(&folder, "Clock Tower: The First Fear"), (&single, &single_game.name)]);
        let row = database.select_game(&folder).unwrap().unwrap();
        assert_eq!(row.name, saved.name, "display repair must preserve stored data");
        assert_eq!(row.play_count, 1);
        media::invalidate();
    }

    #[test]
    fn cold_gamelist_titles_override_catalog_rows_in_library_recents_and_search() {
        media::invalidate();
        let fixture = Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        let folder = system.join("Clock Tower - The First Fear (Japan)");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("disc.chd"), b"rom").unwrap();
        let single = system.join("Clock Single (USA).chd");
        fs::write(&single, b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        for (path, name) in [(&folder, "Clock Tower: The First Fear (Japan)"), (&single, "Clock Single (USA)")] {
            let mut game = Game::new(path.clone());
            game.name = name.into();
            game.image = LazyImage::NotFound;
            database.increment_play_count(&game.into()).unwrap();
        }
        let xml = system.join("gamelist.xml");
        fs::write(&xml, "<gameList><game><path>./Clock Tower - The First Fear (Japan)</path><name>Clock Tower: The First Fear</name></game><game><path>./Clock Single (USA).chd</path><name>Clock Single (Special Edition)</name></game></gameList>").unwrap();
        database.set_gamelist_fingerprint(&system, media::fingerprint(&fs::metadata(&xml).unwrap())).unwrap();
        assert_models(&database, &system, &[(&folder, "Clock Tower: The First Fear"), (&single, "Clock Single (Special Edition)")]);
        let row = database.select_game(&folder).unwrap().unwrap();
        assert_eq!(row.name, "Clock Tower: The First Fear (Japan)");
        assert_eq!(row.play_count, 1);
        media::invalidate();
    }

    #[test]
    fn catalog_rows_without_gamelists_use_library_cleanup() {
        media::invalidate();
        let fixture = Fixture::new();
        let system = fixture.0.path.join("Roms/PS");
        let folder = system.join("Clock Tower - The First Fear (Japan)");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("disc.chd"), b"rom").unwrap();
        let database = Database::in_memory().unwrap();
        let mut game = Game::new(folder.clone());
        game.name = "Clock Tower: The First Fear (Japan)".into();
        game.image = LazyImage::NotFound;
        database.increment_play_count(&game.into()).unwrap();
        for mut entries in [
            RecentsSort::LastPlayed.entries(&database, &ConsoleMapper::library_fixture(), &Locale::new("en-US")).unwrap(),
            SearchResultsSort::Relevance("Clock".into()).entries(&database, &ConsoleMapper::library_fixture(), &Locale::new("en-US")).unwrap(),
        ] {
            assert_eq!(entries[0].name(), "Clock Tower: The First Fear");
            load_work::take();
            assert!(!crate::entry::hydrate_names(&mut entries));
            assert_eq!(load_work::take(), Default::default());
        }
        assert_eq!(database.select_game(&folder).unwrap().unwrap().name, "Clock Tower: The First Fear (Japan)");
        media::invalidate();
    }

    #[test]
    fn cloud_virtual_games_use_clean_catalog_titles_in_library_and_search() {
        media::invalidate();
        let fixture = Fixture::new();
        let root = &fixture.0.path;
        let state = root.join(".allium/state/network-library");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(root.join("Roms/PS")).unwrap();
        let revision = "a".repeat(64);
        let catalog = serde_json::json!({
            "version": 1, "library_id": "names", "revision": revision,
            "games": [{
                "id": "aconcagua", "display_title": "Aconcagua (Japan)", "platform": "PS",
                "bundle_revision": revision, "game_path": "Roms/PS/Aconcagua (Japan).chd",
                "cache_path": "Roms/PS/Aconcagua (Japan).chd", "install_kind": "file",
                "launch_path": "Roms/PS/Aconcagua (Japan).chd", "discs": [],
                "files": [{"id": "disc", "relative_path": "", "bytes": 1, "sha256": revision,
                    "role": "rom", "http_route": format!("/v1/files/aconcagua/{revision}/disc")}]
            }]
        });
        fs::write(state.join("config.json"), r#"{"version":1,"library_id":"names","address":"127.0.0.1:9"}"#).unwrap();
        fs::write(state.join("catalog.json"), serde_json::to_vec(&catalog).unwrap()).unwrap();
        crate::network_runtime::initialize(root.clone()).unwrap();
        let database = Database::in_memory().unwrap();
        let mapper = ConsoleMapper::library_fixture();
        let locale = Locale::new("en-US");
        let path = root.join("Roms/PS/Aconcagua (Japan).chd");
        assert!(!path.exists());
        for entries in [
            Directory::new(root.join("Roms/PS")).entries(&database, &mapper, &locale).unwrap(),
            SearchResultsSort::Relevance("Aconcagua".into()).entries(&database, &mapper, &locale).unwrap(),
        ] {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path(), path);
            assert_eq!(entries[0].name(), "Aconcagua");
        }
        assert!(database.select_game(&path).unwrap().is_none());
        fs::remove_file(state.join("config.json")).unwrap();
        crate::network_runtime::initialize(root.clone()).unwrap();
        media::invalidate();
    }
}
