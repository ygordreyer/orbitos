use std::collections::VecDeque;
use std::fs::{self, File};
use std::marker::PhantomData;

use anyhow::Result;
use async_trait::async_trait;
use common::battery::Battery;
use common::command::Command;
use common::constants::ALLIUM_LAUNCHER_STATE;
use common::display::Display;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::{Stylesheet, StylesheetColor};
use common::view::{Label, Row, SearchView, StatusBar, View};
use log::{trace, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::view::Recents;
use crate::view::apps::AppsState;
use crate::view::games::GamesState;
use crate::view::recents::RecentsState;
use crate::view::search_results::SearchResultsView;
use crate::view::settings::SettingsState;
use crate::view::{Apps, Games, Settings};

fn prepare_theme_inputs(
    models: &mut [common::theme::frame::FrameInputs],
    res: &Resources,
    selected: usize,
    status: &common::theme::frame::Status,
) {
    for input in models.iter_mut() {
        crate::theme_adapter::shell(input, res, selected);
        input.status = status.clone();
    }

    let Some((top, underlays)) = models.split_last_mut() else {
        return;
    };
    if top.screen.is_dialog() {
        for input in underlays {
            input.hints.clear();
            input.suppress_hints = true;
        }
    }
}

pub(super) fn theme_content_complete(
    input: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
) -> bool {
    use common::theme::pack::{Arrangement, Binding, Kind};
    let has_rows = input.rows.values().any(|rows| !rows.is_empty());
    if !has_rows && input.selected.is_empty() {
        return true;
    }
    handle.pack().screens[&input.screen]
        .components
        .iter()
        .all(|c| {
            if c.layout == Some(Arrangement::Hidden) {
                return true;
            }
            let logo_suppressed = common::theme::logo_suppressed_by_cover(c, input);
            if c.logo && !logo_suppressed && input.art.selected_logo.is_none() && !input.art.completed_misses.selected_logo {
                return false;
            }
            if c.kind == Kind::Image {
                match c.binding {
                    Some(Binding::NextCover) => {
                        return !input.art.next_cover_expected || input.art.next_cover.is_some()
                            || input.art.completed_misses.next_cover;
                    }
                    Some(Binding::SelectedCover) => return input.art.selected_cover.is_some() || input.art.completed_misses.selected_cover,
                    Some(Binding::SelectedHero) => return input.art.selected_hero.is_some() || input.art.completed_misses.selected_hero,
                    Some(Binding::SelectedLogo) => return logo_suppressed || input.art.selected_logo.is_some() || input.art.completed_misses.selected_logo,
                    _ => {}
                }
            }
            if c.kind == Kind::Entries
                && c.layout == Some(Arrangement::Shelf)
                && c.paint.as_ref().is_some_and(|paint| paint.peek.is_some())
                && input.art.next_cover_expected
                && input.art.next_cover.is_none()
                && !input.art.completed_misses.next_cover
            {
                return false;
            }
            if c.kind == Kind::Entries
                && (c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0)
            {
                return c
                    .binding
                    .and_then(|binding| input.rows.get(&binding))
                    .is_none_or(|rows| {
                        rows.iter()
                            .all(|row| input.art.entries.contains_key(&row.id)
                                || input.art.completed_misses.entries.contains(&row.id))
                    });
            }
            true
        })
}

pub(crate) struct NavigationWarm {
    stamp: common::theme::render::NavigationStamp,
    media_generation: u64,
    name_generation: u64,
    direction: isize,
    stage: NavigationWarmStage,
}
enum NavigationWarmStage {
    Artwork(super::entry_list::NavigationWarm),
    Images(
        VecDeque<common::theme::render::NavigationImageWarm>,
        VecDeque<common::theme::render::NavigationRasterWarm>,
    ),
    Rasters(VecDeque<common::theme::render::NavigationRasterWarm>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppState {
    selected: usize,
    recents: RecentsState,
    games: GamesState,
    apps: AppsState,
    settings: SettingsState,
}

#[derive(Debug)]
pub struct App<B>
where
    B: Battery + 'static,
{
    rect: Rect,
    res: Resources,
    status_bar: StatusBar<B>,
    status_point: Point,
    views: (Recents, Games, Apps, Settings),
    selected: usize,
    tabs: Row<Label<String>>,
    search_results: Option<SearchResultsView>,
    search_view: SearchView,
    dirty: bool,
    theme_dirty: bool,
    navigation_direction: isize,
    content_complete: bool,
    theme_scene: Option<crate::theme_adapter::ThemeScene>,
    present_damage: Option<Vec<Rect>>,
    theme_was_layered: bool,
    theme_presented_screen: Option<common::theme::pack::ScreenId>,
    had_theme: bool,
    _phantom_battery: PhantomData<B>,
}

impl<B> App<B>
where
    B: Battery + 'static,
{
    pub fn new(
        rect: Rect,
        res: Resources,
        views: (Recents, Games, Apps, Settings),
        selected: usize,
        battery: B,
    ) -> Result<Self> {
        let Rect { x, y, w, h: _h } = rect;
        let styles = res.get::<Stylesheet>();
        let locale = res.get::<Locale>();

        let mirror = styles.ui.mirror_tabs_and_status;

        let mut status_bar = StatusBar::new(res.clone(), Point::zero(), battery);

        let mut tabs = Row::new(
            Point::zero(),
            {
                let mut tabs = vec![
                    Label::new(
                        Point::zero(),
                        locale.t("tab-recents"),
                        Alignment::Left,
                        None,
                    ),
                    Label::new(Point::zero(), locale.t("tab-games"), Alignment::Left, None),
                    Label::new(Point::zero(), locale.t("tab-apps"), Alignment::Left, None),
                    Label::new(
                        Point::zero(),
                        locale.t("tab-settings"),
                        Alignment::Left,
                        None,
                    ),
                ];
                for tab in tabs.iter_mut() {
                    tab.color(StylesheetColor::Tab);
                    tab.stroke_color(StylesheetColor::TabStroke);
                    tab.font_size(styles.ui.tab_font_size);
                }
                tabs
            },
            Alignment::Left,
            styles.ui.margin_x,
        );
        tabs.get_mut(selected)
            .unwrap()
            .color(StylesheetColor::TabSelected)
            .stroke_color(StylesheetColor::TabSelectedStroke);

        if mirror {
            let tabs_w = tabs.bounding_box(&styles).w as i32;
            tabs.set_position(Point::new(
                w as i32 - styles.ui.margin_x - tabs_w,
                y + styles.ui.margin_y,
            ));

            let status_w = status_bar.bounding_box(&styles).w as i32;
            status_bar.set_position(Point::new(
                x + styles.ui.margin_x + status_w,
                y + styles.ui.margin_y,
            ));
        } else {
            tabs.set_position(Point::new(x + styles.ui.margin_x, y + styles.ui.margin_y));
            status_bar.set_position(Point::new(
                w as i32 - styles.ui.margin_x,
                y + styles.ui.margin_y,
            ));
        }

        // let mut title = Label::new(
        //     Point::new(x + 24, y + styles.ui.margin_y),
        //     title(&locale, selected),
        //     Alignment::Left,
        //     None,
        // );
        // title.font_size(styles.ui.tab_font_size);

        let status_point = if mirror {
            Point::new(
                x + styles.ui.margin_x + status_bar.bounding_box(&styles).w as i32,
                y + styles.ui.margin_y,
            )
        } else {
            Point::new(w as i32 - styles.ui.margin_x, y + styles.ui.margin_y)
        };
        drop(styles);
        drop(locale);

        Ok(Self {
            rect,
            res: res.clone(),
            views,
            selected,
            status_bar,
            status_point,
            tabs,
            search_results: None,
            search_view: SearchView::new(res),
            // title,
            dirty: true,
            theme_dirty: true,
            navigation_direction: 1,
            content_complete: true,
            theme_scene: None,
            present_damage: None,
            theme_was_layered: false,
            theme_presented_screen: None,
            had_theme: false,
            _phantom_battery: PhantomData,
        })
    }

    pub fn load_or_new(rect: Rect, res: Resources, battery: B) -> Result<Self> {
        let tab_rect = {
            if crate::theme_adapter::active(&res) || crate::theme_adapter::legacy_orbit(&res) {
                Rect::new(
                    0,
                    super::orbit::HEADER_HEIGHT,
                    640,
                    super::orbit::CONTENT_HEIGHT,
                )
            } else {
                let styles = res.get::<Stylesheet>();
                let font_size = styles.tab_font_size().max(styles.status_bar_font_size()) as u32;
                Rect::new(
                    rect.x,
                    rect.y + font_size as i32 + styles.ui.margin_y + styles.ui.margin_y / 2,
                    rect.w,
                    rect.h - font_size - styles.ui.margin_y as u32 - styles.ui.margin_y as u32 / 2,
                )
            }
        };

        if ALLIUM_LAUNCHER_STATE.exists() {
            let file = File::open(ALLIUM_LAUNCHER_STATE.as_path())?;
            if let Ok(state) = serde_json::from_reader::<_, AppState>(file) {
                let views = (
                    Recents::load_or_new(tab_rect, res.clone(), Some(state.recents))?,
                    Games::load_or_new(tab_rect, res.clone(), Some(state.games)).unwrap_or_else(
                        |_| Games::load_or_new(tab_rect, res.clone(), None).unwrap(),
                    ),
                    Apps::load_or_new(tab_rect, res.clone(), Some(state.apps))?,
                    Settings::new(
                        tab_rect,
                        res.clone(),
                        if state.selected == 3 {
                            // Only load settings if it was the last selected tab
                            state.settings
                        } else {
                            Default::default()
                        },
                    )?,
                );
                return Self::new(rect, res, views, state.selected, battery);
            }
            warn!("failed to deserialize state file, deleting");
            fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path())?;
        }

        let views = (
            Recents::load_or_new(tab_rect, res.clone(), None)?,
            Games::load_or_new(tab_rect, res.clone(), None)?,
            Apps::load_or_new(tab_rect, res.clone(), None)?,
            Settings::new(tab_rect, res.clone(), Default::default())?,
        );
        let selected = 1;
        Self::new(rect, res, views, selected, battery)
    }

    #[cfg(test)]
    pub(crate) fn disc_picker_active(&self) -> bool {
        self.views.1.disc_picker_active()
    }

    #[cfg(test)]
    pub(crate) fn proof_navigation_scene(
        &self,
    ) -> (
        Vec<u8>,
        Vec<common::theme::render::Target>,
        Vec<(String, common::theme::pack::Rect)>,
    ) {
        let frame = &self.theme_scene.as_ref().unwrap().frames[0];
        (
            frame.pixels().data().to_vec(),
            frame.targets.clone(),
            frame.text_boxes.clone(),
        )
    }

    #[cfg(test)]
    pub(crate) fn proof_games_directory(&mut self, path: std::path::PathBuf) {
        use crate::view::entry_list::{EntryList, EntryListState};
        use crate::view::games::GamesSort;
        let list = EntryList::load(
            self.rect,
            self.res.clone(),
            EntryListState {
                sort: GamesSort::Alphabetical(crate::entry::directory::Directory::new(path)),
                selected: 2,
                selected_path: None,
                child: None,
            },
        )
        .unwrap();
        self.views.1 = Games::new(self.rect, self.res.clone(), list).unwrap();
        self.tab_change(1);
    }

    #[cfg(test)]
    pub(crate) fn proof_game_theme_inputs(&mut self) -> Vec<common::theme::frame::FrameInputs> {
        let handle = crate::theme_adapter::snapshot(&self.res).unwrap();
        self.views.1.theme_models(&handle).unwrap()
    }

    #[cfg(all(test, feature = "perf-bench"))]
    pub(crate) fn proof_fresh_game_pixels(&mut self) -> Vec<u8> {
        let handle = crate::theme_adapter::snapshot(&self.res).unwrap();
        let mut models = self.proof_game_theme_inputs();
        let status = if self
            .res
            .get::<Stylesheet>()
            .status_bar
            .hide_status_in_launcher
        {
            common::theme::frame::Status::default()
        } else {
            self.status_bar.theme_inputs()
        };
        prepare_theme_inputs(&mut models, &self.res, self.selected, &status);
        if handle.pack().ink()
            && models.len() > 1
            && models.last().is_some_and(|m| {
                handle.pack().screens[&m.screen]
                    .components
                    .iter()
                    .any(|c| c.kind == common::theme::pack::Kind::Hints)
            })
        {
            let underlay = models.len() - 1;
            for model in &mut models[..underlay] {
                model.hints.clear();
                model.suppress_hints = true;
            }
        }
        let mut display = <DefaultPlatform as Platform>::Display::default();
        for model in models {
            common::theme::render::PreparedFrame::prepare(handle.clone(), &model)
                .unwrap()
                .paint(&mut display);
        }
        display.pixmap().data().to_vec()
    }

    #[cfg(all(test, feature = "perf-bench"))]
    pub(crate) fn proof_theme_redraw(&mut self) {
        self.theme_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn prepared_status_boxes(&self) -> usize {
        let Some(handle) = crate::theme_adapter::snapshot(&self.res) else {
            return 0;
        };
        let ids = handle
            .pack()
            .screens
            .values()
            .flat_map(|screen| &screen.components)
            .filter(|c| c.kind == common::theme::pack::Kind::Status)
            .map(|c| &c.id)
            .collect::<Vec<_>>();
        self.theme_scene.as_ref().map_or(0, |scene| {
            scene
                .frames
                .iter()
                .flat_map(|frame| &frame.text_boxes)
                .filter(|(id, _)| ids.contains(&id))
                .count()
        })
    }

    #[cfg(test)]
    pub(crate) fn orbit_cache_counts(&self) -> (usize, usize) {
        self.views.1.orbit_cache_counts()
    }

    pub fn save(&self) -> Result<()> {
        let file = File::create(ALLIUM_LAUNCHER_STATE.as_path())?;
        let state = AppState {
            selected: self.selected,
            recents: self.views.0.save(),
            games: self.views.1.save(),
            apps: self.views.2.save(),
            settings: self.views.3.save(),
        };
        serde_json::to_writer(file, &state)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn proof_select_tab(&mut self, selected: usize) {
        self.tab_change(selected);
    }

    fn view(&self) -> &dyn View {
        match self.selected {
            0 => &self.views.0,
            1 => &self.views.1,
            2 => &self.views.2,
            3 => &self.views.3,
            _ => unreachable!(),
        }
    }

    fn view_mut(&mut self) -> &mut dyn View {
        match self.selected {
            0 => &mut self.views.0,
            1 => &mut self.views.1,
            2 => &mut self.views.2,
            3 => &mut self.views.3,
            _ => unreachable!(),
        }
    }

    fn tab_change(&mut self, selected: usize) {
        if self.selected == 1 && selected != 1 && crate::theme_adapter::legacy_orbit(&self.res) {
            self.status_bar.set_position(self.status_point);
            self.status_bar.set_should_draw();
        }
        self.tabs
            .get_mut(self.selected)
            .unwrap()
            .color(StylesheetColor::Tab)
            .stroke_color(StylesheetColor::TabStroke);
        self.selected = selected;
        self.view_mut().set_should_draw();
        if crate::theme_adapter::active(&self.res) && self.theme_scene.is_some() {
            self.theme_dirty = true;
        } else {
            self.set_should_draw();
        }
        self.tabs
            .get_mut(self.selected)
            .unwrap()
            .color(StylesheetColor::TabSelected)
            .stroke_color(StylesheetColor::TabSelectedStroke);
        // self.title.set_text(self.title());
    }

    fn root_tab_navigation_ready(&self) -> bool {
        crate::theme_adapter::active(&self.res)
            && self.search_results.is_none()
            && !self.search_view.is_active()
            && match self.selected {
                0 => self.views.0.root_tab_navigation_ready(),
                1 => self.views.1.root_tab_navigation_ready(),
                2 => self.views.2.root_tab_navigation_ready(),
                3 => self.views.3.root_tab_navigation_ready(),
                _ => false,
            }
    }

    fn next(&mut self) {
        let selected = (self.selected + 1).rem_euclid(4);
        self.tab_change(selected)
    }

    fn prev(&mut self) {
        let selected = (self.selected as isize - 1).rem_euclid(4);
        self.tab_change(selected as usize)
    }

    pub(crate) fn theme_fallback(&mut self) -> Result<()> {
        self.content_complete = true;
        self.theme_scene = None;
        self.theme_dirty = false;
        self.dirty = true;
        self.views.0.clear_theme()?;
        self.views.1.clear_theme()?;
        self.views.2.clear_theme()?;
        self.views.3.clear_theme()?;
        if let Some(search) = self.search_results.as_mut() {
            search.clear_theme()?;
        }
        Ok(())
    }
    pub(crate) fn force_full_present(&mut self) {
        self.dirty = true;
    }
    pub(crate) fn take_present_damage(&mut self) -> Option<Vec<Rect>> {
        self.present_damage.take()
    }
    fn single_game_list(&self) -> bool {
        use common::theme::pack::ScreenId;
        self.search_results.is_none()
            && !self.search_view.is_active()
            && self.theme_scene.as_ref().is_some_and(|scene| {
                scene.frames.len() == 1
                    && matches!(
                        scene.frames[0].screen(),
                        ScreenId::Library | ScreenId::Recents | ScreenId::Apps
                    )
            })
    }

    pub(crate) fn content_complete(&self) -> bool {
        self.content_complete
    }

    pub(crate) fn idle_media_step(&mut self) -> bool {
        let _task = common::perf_trace::task("media_adopt", common::perf_trace::generation());
        let completion = super::orbit::completion_generation();
        let Some(handle) = crate::theme_adapter::snapshot(&self.res) else {
            return false;
        };
        let worked = if let Some(search) = self.search_results.as_mut() {
            search.idle_media_step(&handle)
        } else {
            match self.selected {
                0 => self.views.0.idle_media_step(&handle),
                1 => self.views.1.idle_media_step(&handle),
                2 => self.views.2.idle_media_step(&handle),
                _ => false,
            }
        };
        if completion != super::orbit::completion_generation() {
            self.theme_dirty = true;
        }
        worked
    }

    pub(crate) fn navigation_warm_ready(&self) -> bool {
        self.single_game_list()
            && match self.selected {
                0 => self.views.0.navigation_warm_ready(),
                1 => self.views.1.navigation_warm_ready(),
                _ => false,
            }
            && !self.should_draw()
            && self.theme_scene.as_ref().is_some_and(|scene| {
                scene.frames.len() == 1 && scene.frames[0].can_prewarm_navigation()
            })
    }

    #[cfg(feature = "perf-bench")]
    pub(crate) fn navigation_warm_kind(&self, work: &Option<NavigationWarm>) -> &'static str {
        match work.as_ref().map(|work| &work.stage) {
            None => "plan",
            Some(NavigationWarmStage::Artwork(_)) => "artwork",
            Some(NavigationWarmStage::Images(_, _)) => "image_transform",
            Some(NavigationWarmStage::Rasters(_)) => "raster",
        }
    }

    pub(crate) fn prewarm_navigation_step(
        &mut self,
        work: &mut Option<NavigationWarm>,
        direction: isize,
    ) -> Result<bool> {
        self.prewarm_navigation_step_with_pending(work, direction, || false)
    }

    /// The launcher supplies a cheap queue check; each call runs at most one cache operation.
    pub(crate) fn prewarm_navigation_step_with_pending(
        &mut self,
        work: &mut Option<NavigationWarm>,
        direction: isize,
        mut input_pending: impl FnMut() -> bool,
    ) -> Result<bool> {
        if input_pending() {
            return Ok(false);
        }
        if direction != self.navigation_direction {
            *work = None;
            return Ok(true);
        }
        if !self.navigation_warm_ready() {
            *work = None;
            return Ok(true);
        }
        let Some(handle) = crate::theme_adapter::snapshot(&self.res) else {
            *work = None;
            return Ok(true);
        };
        let frame = &mut self.theme_scene.as_mut().unwrap().frames[0];
        if !frame.navigation_theme_matches(&handle) {
            *work = None;
            return Ok(true);
        }
        if work.as_ref().is_some_and(|work| {
            !frame.navigation_is_current(&work.stamp)
                || work.direction != direction
                || work.media_generation != common::media::generation()
                || work.name_generation != common::media::name_generation()
        }) {
            *work = None;
            return Ok(false);
        }
        let _task = common::perf_trace::task("navigation_prewarm", common::perf_trace::generation());
        let Some(mut current) = work.take() else {
            let Some(input) = frame.navigation_model() else {
                return Ok(true);
            };
            if input_pending() {
                return Ok(false);
            }
            let plan = match self.selected {
                0 => self.views.0.navigation_warm_model(&handle, direction, input)?,
                1 => self.views.1.navigation_warm_model(&handle, direction, input)?,
                _ => None,
            };
            let Some(plan) = plan else {
                return Ok(true);
            };
            common::perf_trace::count(common::perf_trace::Count::PlanBuilds, 1);
            *work = Some(NavigationWarm {
                stamp: frame.navigation_stamp(),
                media_generation: common::media::generation(),
                name_generation: common::media::name_generation(),
                direction,
                stage: NavigationWarmStage::Artwork(plan),
            });
            return Ok(false);
        };
        if input_pending() {
            *work = Some(current);
            return Ok(false);
        }
        current.stage = match current.stage {
            NavigationWarmStage::Artwork(mut plan) => {
                let ready = match self.selected {
                    0 => self.views.0.navigation_warm_art_step(&mut plan),
                    1 => self.views.1.navigation_warm_art_step(&mut plan),
                    _ => return Ok(true),
                };
                if ready {
                    NavigationWarmStage::Images(
                        frame.navigation_images(&plan.input),
                        frame.navigation_rasters(&plan.input),
                    )
                } else {
                    NavigationWarmStage::Artwork(plan)
                }
            }
            NavigationWarmStage::Images(mut images, rasters) => {
                if let Some(image) = images.pop_front() {
                    frame.prewarm_navigation_image(image)?;
                }
                if images.is_empty() {
                    NavigationWarmStage::Rasters(rasters)
                } else {
                    NavigationWarmStage::Images(images, rasters)
                }
            }
            NavigationWarmStage::Rasters(mut rasters) => {
                let Some(raster) = rasters.pop_front() else {
                    return Ok(true);
                };
                frame.prewarm_navigation_raster(raster)?;
                if rasters.is_empty() {
                    return Ok(true);
                }
                NavigationWarmStage::Rasters(rasters)
            }
        };
        *work = Some(current);
        Ok(false)
    }

    fn normalize_theme_inputs(
        &self,
        handle: &common::theme::ThemeHandle,
        models: &mut [common::theme::frame::FrameInputs],
    ) {
        let status = if self
            .res
            .get::<Stylesheet>()
            .status_bar
            .hide_status_in_launcher
        {
            common::theme::frame::Status::default()
        } else {
            self.status_bar.theme_inputs()
        };
        prepare_theme_inputs(models, &self.res, self.selected, &status);
        if handle.pack().ink()
            && models.len() > 1
            && models.last().is_some_and(|m| {
                handle.pack().screens[&m.screen]
                    .components
                    .iter()
                    .any(|c| c.kind == common::theme::pack::Kind::Hints)
            })
        {
            let underlay = models.len() - 1;
            for model in &mut models[..underlay] {
                model.hints.clear();
                model.suppress_hints = true;
            }
        }
    }

    pub(crate) fn prepare_theme(&mut self) -> Result<()> {
        use common::theme::render::PreparedFrame;
        let Some(handle) = crate::theme_adapter::snapshot(&self.res) else {
            self.content_complete = true;
            if self.had_theme {
                self.views.0.clear_theme()?;
                self.views.1.clear_theme()?;
                self.views.2.clear_theme()?;
                self.views.3.clear_theme()?;
                if let Some(search) = self.search_results.as_mut() {
                    search.clear_theme()?;
                }
                self.theme_scene = None;
                self.had_theme = false;
                self.dirty = true;
            }
            return Ok(());
        };
        self.had_theme = true;
        let models = self.theme_models(&handle)?;
        self.content_complete = models.as_ref().is_none_or(|models| {
            models.iter().all(|input| theme_content_complete(input, &handle))
        });
        self.theme_scene = if let Some(mut models) = models {
            self.normalize_theme_inputs(&handle, &mut models);
            let mut previous = self
                .theme_scene
                .take()
                .map(|scene| scene.frames)
                .unwrap_or_default()
                .into_iter();
            let mut frames = vec![];
            for input in models {
                frames.push(PreparedFrame::prepare_reusing(
                    handle.clone(),
                    &input,
                    previous.next(),
                )?);
            }
            Some(crate::theme_adapter::ThemeScene { frames })
        } else {
            None
        };
        Ok(())
    }

    async fn dispatch_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if crate::network_runtime::event(event)? {
            return Ok(true);
        }

        if let Some(search_results) = &mut self.search_results
            && search_results
                .handle_key_event(event, commands.clone(), bubble)
                .await?
        {
            let mut close_search = false;
            for cmd in bubble.iter() {
                match cmd {
                    Command::CloseView => {
                        close_search = true;
                    }
                    Command::Search(_) => {}
                    _ => {}
                }
            }
            if close_search {
                self.close_search_results();
            }
            bubble.clear();
            return Ok(true);
        }

        if self.search_view.is_active()
            && self
                .search_view
                .handle_key_event(event, commands.clone(), bubble)
                .await?
        {
            for cmd in bubble.iter() {
                if let Command::Search(query) = cmd {
                    self.search(query.clone())?;
                }
            }
            bubble.clear();
            return Ok(true);
        }

        if self.root_tab_navigation_ready() {
            match event {
                KeyEvent::Pressed(Key::L) => {
                    self.prev();
                    return Ok(true);
                }
                KeyEvent::Pressed(Key::R) => {
                    self.next();
                    return Ok(true);
                }
                _ => {}
            }
        }

        if self
            .view_mut()
            .handle_key_event(event, commands, bubble)
            .await?
        {
            return Ok(true);
        }
        if (crate::theme_adapter::active(&self.res)
            || crate::theme_adapter::legacy_orbit(&self.res))
            && self.search_results.is_none()
            && !self.search_view.is_active()
        {
            match event {
                KeyEvent::Pressed(Key::L) => {
                    self.prev();
                    return Ok(true);
                }
                KeyEvent::Pressed(Key::R) => {
                    self.next();
                    return Ok(true);
                }
                _ => {}
            }
        }

        match event {
            KeyEvent::Pressed(Key::Left) => {
                trace!("switch state prev");
                self.prev();
                Ok(true)
            }
            KeyEvent::Pressed(Key::Right) => {
                trace!("switch state next");
                self.next();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        let models = if let Some(keyboard) = self.search_view.theme_inputs() {
            Some(vec![crate::theme_adapter::keyboard_model(keyboard)])
        } else if let Some(search) = self.search_results.as_mut() {
            Some(search.theme_models(&handle)?)
        } else {
            match self.selected {
                0 => self.views.0.theme_models(&handle)?,
                1 => Some(self.views.1.theme_models(&handle)?),
                2 => Some(self.views.2.theme_models(&handle)?),
                3 => self.views.3.theme_models(&handle)?,
                _ => unreachable!(),
            }
        };
        Ok(models)
    }

    pub fn start_search(&mut self) {
        self.theme_dirty = true;
        self.search_view.activate();
        self.status_bar.set_should_draw();
    }

    pub fn search(&mut self, query: String) -> Result<()> {
        let search_view = SearchResultsView::new(self.rect, self.res.clone(), query)?;
        self.search_results = Some(search_view);
        self.theme_dirty = true;
        Ok(())
    }

    pub fn close_search_results(&mut self) {
        self.search_results = None;
        self.search_view.deactivate();
        self.set_should_draw();
    }

    // fn title(&self) -> String {
    //     title(&self.res.get::<Locale>(), self.selected)
    // }
}

#[async_trait(?Send)]
impl<B> View for App<B>
where
    B: Battery,
{
    fn update(&mut self, dt: std::time::Duration) {
        let before = self.status_bar.theme_inputs();
        for child in self.children_mut() {
            child.update(dt);
        }
        let after = self.status_bar.theme_inputs();
        if (
            before.clock,
            before.battery,
            before.charging,
            before.network,
        ) != (after.clock, after.battery, after.charging, after.network)
        {
            self.theme_dirty = true;
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        self.present_damage = None;
        if crate::theme_adapter::active(&self.res) {
            if let Some(scene) = &self.theme_scene {
                if !self.theme_dirty && !self.dirty {
                    return Ok(false);
                }
                let top = scene.frames.last().unwrap();
                let opaque_top = top.is_opaque();
                let same_top = self.theme_presented_screen == Some(top.screen());
                if !self.dirty && ((scene.frames.len() == 1 && !self.theme_was_layered)
                    || (opaque_top && same_top)) {
                    self.present_damage = Some(top.paint_damage(display));
                } else if opaque_top {
                    top.paint(display);
                } else {
                    for frame in &scene.frames {
                        frame.paint(display);
                    }
                }
                self.theme_presented_screen = Some(top.screen());
                self.theme_was_layered = scene.frames.len() != 1;
                self.theme_dirty = false;
                self.dirty = false;
                return Ok(self.present_damage.as_ref().is_none_or(|damage| !damage.is_empty()));
            }
        }
        let display_styles = styles.display_style();
        let styles = &display_styles;
        let app_dirty = self.dirty;
        if self.dirty {
            display.load(self.bounding_box(styles))?;
            self.dirty = false;
        }

        #[cfg(feature = "debug-ui-redraw")]
        {
            let bg_color = StylesheetColor::Background.to_color(styles);
            let full_rect =
                common::geom::Rect::new(0, 0, display.size().width, display.size().height);
            common::display::fill_rect(&mut display.pixmap_mut(), full_rect, bg_color);
        }

        let mut drawn = false;

        if crate::theme_adapter::legacy_orbit(&self.res)
            && self.search_results.is_none()
            && !self.search_view.is_active()
        {
            let list_dirty = self.view().should_draw();
            if app_dirty
                || list_dirty
                || (!styles.status_bar.hide_status_in_launcher && self.status_bar.should_draw())
            {
                let background = super::orbit::background();
                if app_dirty {
                    common::display::fill_rect(
                        &mut display.pixmap_mut(),
                        Rect::new(
                            0,
                            super::orbit::HEADER_HEIGHT,
                            640,
                            super::orbit::CONTENT_HEIGHT,
                        ),
                        background,
                    );
                    self.view_mut().set_should_draw();
                }
                common::display::fill_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(0, 0, 640, super::orbit::HEADER_HEIGHT as u32),
                    background,
                );
                common::display::fill_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(0, super::orbit::FOOTER_Y, 640, 50),
                    background,
                );
                let text = common::display::color::Color::new(35, 48, 51);
                let muted = common::display::color::Color::new(96, 118, 105);
                super::orbit::font(styles, 27, text).draw(
                    &mut display.pixmap_mut(),
                    "orbit",
                    Point::new(25, 14),
                );
                let locale = self.res.get::<Locale>();
                let names = [
                    locale.t("tab-recents"),
                    locale.t("tab-games"),
                    locale.t("tab-apps"),
                    locale.t("tab-settings"),
                ];
                drop(locale);
                let mut tab_size = (styles.tab_font_size() as u32).clamp(15, 22);
                while tab_size > 15 {
                    let font = super::orbit::font(styles, tab_size, muted);
                    if names.iter().map(|name| font.measure(name).w).sum::<u32>() + 42 <= 355 {
                        break;
                    }
                    tab_size -= 1;
                }
                let small = super::orbit::font(styles, tab_size, muted);
                let mut x = 130;
                for (index, name) in names.iter().enumerate() {
                    let label = super::orbit::wrapped(&small, name, 82, 1)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    let width = small.measure(&label).w;
                    small.draw(&mut display.pixmap_mut(), &label, Point::new(x, 20));
                    if index == self.selected {
                        common::display::fill_rect(
                            &mut display.pixmap_mut(),
                            Rect::new(x, 51, width, 3),
                            common::display::color::Color::new(22, 183, 187),
                        );
                    }
                    x += width as i32 + 14;
                }
                if self.selected == 1 {
                    super::orbit::font(styles, 16, muted).draw(
                        &mut display.pixmap_mut(),
                        "Left/Right Browse   A Select   B Back   X Search   L/R Tabs",
                        Point::new(18, 447),
                    );
                }
                let mut status_styles = styles.clone();
                status_styles.status_bar.text_color = muted;
                status_styles.status_bar.text_stroke_color =
                    common::display::color::Color::rgba(0, 0, 0, 0);
                if !styles.status_bar.hide_status_in_launcher {
                    self.status_bar.set_position(Point::new(615, 17));
                    self.status_bar.set_should_draw();
                    self.status_bar.draw(display, &status_styles)?;
                }
                drawn = true;
            }
            if list_dirty {
                drawn |= self.view_mut().draw(display, styles)?;
            }
            return Ok(drawn);
        }

        if self.search_results.is_none() {
            let hide_status = styles.status_bar.hide_status_in_launcher;
            let top_needs_draw =
                self.tabs.should_draw() || (!hide_status && self.status_bar.should_draw());
            let mut view_will_draw = !self.search_view.is_active() && self.view().should_draw();

            if top_needs_draw && !self.search_view.is_active() {
                self.view_mut().set_should_draw();
                view_will_draw = true;
            }

            if top_needs_draw || view_will_draw {
                display.load(
                    self.tabs
                        .bounding_box(styles)
                        .union(&self.status_bar.bounding_box(styles)),
                )?;
            }

            if view_will_draw {
                drawn |= self.view_mut().draw(display, styles)?;
            }

            if self.tabs.should_draw() || (!hide_status && self.status_bar.should_draw()) || drawn {
                if drawn {
                    self.tabs.set_should_draw();
                    if !hide_status {
                        self.status_bar.set_should_draw();
                    }
                }

                if self.tabs.should_draw() && styles.ui.tabs_backdrop_color.a() > 0 {
                    let mut bbox = self.tabs.bounding_box(styles);
                    if bbox.w > 0 && bbox.h > 0 {
                        let padding_x = styles.ui.padding_x;
                        let padding_y = styles.ui.padding_y;
                        bbox.x -= padding_x;
                        bbox.y -= padding_y;
                        bbox.w += (padding_x * 2) as u32;
                        bbox.h += (padding_y * 2) as u32;

                        if bbox.y < 0 {
                            bbox.h = (bbox.h as i32 + bbox.y) as u32;
                            bbox.y = 0;
                        }

                        common::display::fill_rounded_rect(
                            &mut display.pixmap_mut(),
                            bbox,
                            styles.ui.margin_x as u32,
                            styles.ui.tabs_backdrop_color,
                        );
                    }
                }
                drawn |= self.tabs.draw(display, styles)?;
                if !hide_status {
                    drawn |= self.status_bar.draw(display, styles)?;
                }
            }
        }

        drawn |= self.search_view.draw(display, styles)?;

        if let Some(search_results) = &mut self.search_results {
            let search_drawn =
                search_results.should_draw() && search_results.draw(display, styles)?;
            if search_drawn {
                self.status_bar.set_should_draw();
            }
            let mut status_styles = styles.clone();
            if crate::theme_adapter::legacy_orbit(&self.res) {
                status_styles.status_bar.text_color =
                    common::display::color::Color::new(96, 118, 105);
                status_styles.status_bar.text_stroke_color =
                    common::display::color::Color::rgba(0, 0, 0, 0);
            }
            drawn |= search_drawn;
            drawn |=
                self.status_bar.should_draw() && self.status_bar.draw(display, &status_styles)?;
        }

        #[cfg(feature = "debug-ui")]
        common::view::draw_debug_bounds(self, display, styles, 0)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        if crate::theme_adapter::active(&self.res) && self.theme_scene.is_some() {
            return self.theme_dirty || self.dirty;
        }

        if crate::theme_adapter::legacy_orbit(&self.res)
            && self.search_results.is_none()
            && !self.search_view.is_active()
        {
            return self.dirty
                || (!self
                    .res
                    .get::<Stylesheet>()
                    .status_bar
                    .hide_status_in_launcher
                    && self.status_bar.should_draw())
                || self.view().should_draw();
        }
        self.status_bar.should_draw()
            || self.view().should_draw()
            || self.tabs.should_draw()
            || self.search_view.should_draw()
            || self
                .search_results
                .as_ref()
                .is_some_and(|sr| sr.should_draw())
    }

    fn set_should_draw(&mut self) {
        self.theme_dirty = true;
        self.dirty = true;
        self.status_bar.set_should_draw();
        self.view_mut().set_should_draw();
        self.tabs.set_should_draw();
        self.search_view.set_should_draw();
        if let Some(search_results) = &mut self.search_results {
            search_results.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        let navigation = matches!(
            event,
            KeyEvent::Pressed(Key::L | Key::R | Key::Left | Key::Right | Key::Up | Key::Down)
                | KeyEvent::Autorepeat(Key::Left | Key::Right | Key::Up | Key::Down)
        );
        if self.single_game_list() && navigation {
            let handled = self.dispatch_key_event(event, commands, bubble).await?;
            if handled {
                match event {
                    KeyEvent::Pressed(Key::Left | Key::Up)
                    | KeyEvent::Autorepeat(Key::Left | Key::Up) => self.navigation_direction = -1,
                    KeyEvent::Pressed(Key::Right | Key::Down)
                    | KeyEvent::Autorepeat(Key::Right | Key::Down) => self.navigation_direction = 1,
                    _ => {}
                }
            }
            self.theme_dirty |= handled;
            return Ok(handled);
        }
        if self.single_game_list()
            && matches!(
                event,
                KeyEvent::Released(Key::L | Key::R | Key::Left | Key::Right | Key::Up | Key::Down)
            )
        {
            return Ok(false);
        }
        let handle =
            crate::theme_adapter::snapshot(&self.res).filter(|_| self.theme_scene.is_some());
        let retained_comparison = self.theme_scene.as_ref().is_some_and(|scene| {
            !scene.frames.is_empty() && scene.frames.iter().all(|frame| frame.has_retained_inputs())
        });
        let before = if !retained_comparison {
            if let Some(handle) = &handle {
                self.theme_models(handle)?
            } else {
                None
            }
        } else {
            None
        };
        let selected = self.selected;
        let dirty = (self.dirty, self.theme_dirty);
        let handled = self.dispatch_key_event(event, commands, bubble).await?;
        if let Some(handle) = handle {
            let same_theme = crate::theme_adapter::snapshot(&self.res).is_some_and(|current| {
                current.generation() == handle.generation() && current.digest() == handle.digest()
            });
            if retained_comparison || before.is_some() {
                let unchanged = if !same_theme || selected != self.selected {
                    false
                } else if retained_comparison {
                    if let Some(mut after) = self.theme_models(&handle)? {
                        self.normalize_theme_inputs(&handle, &mut after);
                        self.theme_scene.as_ref().is_some_and(|scene| {
                            scene.frames.len() == after.len()
                                && scene
                                    .frames
                                    .iter()
                                    .zip(&after)
                                    .all(|(frame, input)| frame.input_matches(input))
                        })
                    } else {
                        false
                    }
                } else {
                    self.theme_models(&handle)?.as_ref() == before.as_ref()
                };
                self.theme_dirty = dirty.1 || !unchanged;
                if unchanged {
                    self.dirty = dirty.0;
                }
            }
        }
        Ok(handled)
    }

    fn children(&self) -> Vec<&dyn View> {
        let mut children: Vec<&dyn View> = vec![&self.status_bar, self.view(), &self.tabs];
        if let Some(search_results) = &self.search_results {
            children.push(search_results);
        }
        children
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        let view: &mut dyn View = match self.selected {
            0 => &mut self.views.0,
            1 => &mut self.views.1,
            2 => &mut self.views.2,
            3 => &mut self.views.3,
            _ => unreachable!(),
        };
        let mut children: Vec<&mut dyn View> = vec![&mut self.status_bar, view, &mut self.tabs];
        if let Some(search_results) = &mut self.search_results {
            children.push(search_results);
        }
        children
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

// fn title(locale: &Locale, selected: usize) -> String {
//     match selected {
//         0 => locale.t("tab-recents"),
//         1 => locale.t("tab-games"),
//         2 => locale.t("tab-apps"),
//         3 => locale.t("tab-settings"),
//         _ => unreachable!(),
//     }
// }

#[cfg(test)]
mod tests {
    use super::prepare_theme_inputs;
    use common::locale::Locale;
    use common::resources::Resources;
    use common::theme::frame::{FrameInputs, Hint, Status};
    use common::theme::pack::{Binding, Pack, Rect, ScreenId};

    fn resources() -> Resources {
        let mut map = type_map::TypeMap::new();
        map.insert(Locale::new("en-US"));
        Resources::new(map)
    }

    fn authored(screen: ScreenId, button: &str, label: &str) -> FrameInputs {
        let mut input = FrameInputs::new(screen);
        input.hints.push(Hint {
            button: button.into(),
            label: label.into(),
        });
        input
    }

    fn hint_rect(pack: &Pack, screen: ScreenId) -> Rect {
        pack.screens[&screen]
            .components
            .iter()
            .find(|component| component.binding == Some(Binding::Hints))
            .unwrap()
            .rect
    }

    #[test]
    fn adapter_then_dialog_suppression_leaves_only_authored_top_controls() {
        let mut custom = Pack::builtin("orbit").unwrap();
        custom
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .iter_mut()
            .find(|component| component.binding == Some(Binding::Hints))
            .unwrap()
            .rect = Rect(8, 448, 624, 24);
        custom
            .screens
            .get_mut(&ScreenId::ContextMenu)
            .unwrap()
            .components
            .iter_mut()
            .find(|component| component.binding == Some(Binding::Hints))
            .unwrap()
            .rect = Rect(80, 401, 480, 30);
        assert_ne!(
            hint_rect(&custom, ScreenId::Library),
            hint_rect(&custom, ScreenId::ContextMenu)
        );

        let mut library = FrameInputs::new(ScreenId::Library);
        library.focus.component = "library-focus".into();
        let mut details = FrameInputs::new(ScreenId::GameDetail);
        details.progress = Some(0.5);
        let dialog = authored(ScreenId::ContextMenu, "X", "Download");
        let mut models = vec![library, details, dialog];
        let status = Status {
            clock: Some("12:34".into()),
            ..Status::default()
        };

        prepare_theme_inputs(&mut models, &resources(), 1, &status);

        assert!(models[0].hints.is_empty());
        assert!(models[1].hints.is_empty());
        assert_eq!(models[2].hints.len(), 1);
        assert_eq!(models[2].hints[0].button, "X");
        assert_eq!(models[2].hints[0].label, "Download");
        assert_eq!(models[0].focus.component, "library-focus");
        assert_eq!(models[1].progress, Some(0.5));
        assert!(
            models
                .iter()
                .all(|input| input.status.clock.as_deref() == Some("12:34"))
        );
    }

    #[test]
    fn adapter_defaults_remain_for_non_dialog_stack() {
        let mut models = vec![
            FrameInputs::new(ScreenId::Library),
            FrameInputs::new(ScreenId::GameDetail),
        ];

        prepare_theme_inputs(&mut models, &resources(), 1, &Status::default());

        assert_eq!(models[0].hints.len(), 3);
        assert_eq!(models[1].hints.len(), 3);
        assert_eq!(models[0].hints[0].button, "A");
        assert_eq!(models[1].hints[0].button, "A");
    }

    #[test]
    fn adapter_preserves_authored_library_hints_without_dialog() {
        let mut models = vec![authored(ScreenId::Library, "A", "Play")];

        prepare_theme_inputs(&mut models, &resources(), 1, &Status::default());

        assert_eq!(models[0].hints.len(), 1);
        assert_eq!(models[0].hints[0].label, "Play");
    }

    #[test]
    fn empty_model_stack_is_safe() {
        let mut models = vec![];

        prepare_theme_inputs(&mut models, &resources(), 1, &Status::default());

        assert!(models.is_empty());
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod integration_navigation_tests {
    use super::*;
    use crate::consoles::ConsoleMapper;
    use common::database::Database;
    use common::theme::{ThemeHandle, fonts::FontBank, runtime::ThemeSnapshot};

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(orbit_fixture)]
    async fn recents_warm_navigation_matches_full_compose_and_yields_to_pending_input() {
        let root = common::constants::ALLIUM_GAMES_DIR.join("integration-recents-warm");
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let database = Database::in_memory().unwrap();
        for index in 0..12 {
            let path = root.join(format!("Game {index:02}.chd"));
            std::fs::write(&path, []).unwrap();
            for (suffix, w, h) in [("orbit", 101, 101), ("hero", 360, 210)] {
                let mut art = tiny_skia::Pixmap::new(w, h).unwrap();
                art.fill(tiny_skia::Color::from_rgba8(index * 17, 90, 120, 255));
                art.save_png(root.join(format!("Imgs/Game {index:02}.{suffix}.png")))
                    .unwrap();
            }
            let game: common::database::NewGame = crate::entry::game::Game::new(path).into();
            database.update_games(&[game.clone()]).unwrap();
            database.increment_play_count(&game).unwrap();
        }
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        styles.recents.use_recents_carousel = false;
        let mut map = type_map::TypeMap::new();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(database);
        let res = Resources::new(map);
        let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
        let rect = Rect::new(0, 54, 640, 376);
        for (iteration, id) in ["compact", "orbit-hero", "big-picture", "compact-plus"]
            .iter()
            .enumerate()
        {
            let snapshot = if *id == "compact" {
                ThemeSnapshot::builtin(id).unwrap()
            } else {
                ThemeSnapshot::load(
                    &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("../../themes/{id}"))
                        .canonicalize()
                        .unwrap(),
                )
                .unwrap()
            };
            let handle = ThemeHandle::prepare(snapshot, &bank, iteration as u64 + 800).unwrap();
            res.insert(Some(handle.clone()));
            let views = (
                Recents::load_or_new(rect, res.clone(), None).unwrap(),
                Games::load_or_new(rect, res.clone(), None).unwrap(),
                Apps::load_or_new(rect, res.clone(), None).unwrap(),
                Settings::new(rect, res.clone(), Default::default()).unwrap(),
            );
            let mut app = App::new(
                Rect::new(0, 0, 640, 480),
                res.clone(),
                views,
                0,
                DefaultPlatform::new().unwrap().battery().unwrap(),
            )
            .unwrap();
            let mut display = <DefaultPlatform as Platform>::Display::default();
            let (tx, _rx) = tokio::sync::mpsc::channel(100);
            let style = res.get::<Stylesheet>().clone();
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
            for _ in 0..10 {
                assert!(app.navigation_warm_ready(), "{id}");
                let scene = app.proof_navigation_scene();
                let mut work = None;
                assert!(
                    !app.prewarm_navigation_step_with_pending(&mut work, 1, || true)
                        .unwrap()
                );
                assert!(work.is_none(), "pending input must prevent planning");
                assert!(!app.prewarm_navigation_step(&mut work, 1).unwrap());
                assert!(
                    work.is_some(),
                    "Recents must produce a real navigation plan"
                );
                if let NavigationWarmStage::Artwork(plan) = &work.as_ref().unwrap().stage {
                    let count = crate::theme_adapter::capacity(
                        &handle,
                        common::theme::pack::ScreenId::Recents,
                        common::theme::pack::Binding::Entries,
                    );
                    assert_eq!(
                        plan.input.rows[&common::theme::pack::Binding::Entries].len(),
                        count.min(12)
                    );
                } else {
                    panic!("missing artwork plan");
                }
                let mut checks = 0;
                assert!(
                    !app.prewarm_navigation_step_with_pending(&mut work, 1, || {
                        checks += 1;
                        checks == 2
                    })
                    .unwrap()
                );
                assert_eq!(checks, 2);
                assert!(
                    matches!(
                        work.as_ref().unwrap().stage,
                        NavigationWarmStage::Artwork(_)
                    ),
                    "pending input must leave queued work untouched"
                );
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !app.prewarm_navigation_step(&mut work, 1).unwrap() {
                    assert!(std::time::Instant::now() < deadline, "{id} warm timeout");
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert!(
                    app.proof_navigation_scene() == scene,
                    "idle warm changed the displayed frame"
                );
                app.handle_key_event(
                    KeyEvent::Pressed(Key::Down),
                    tx.clone(),
                    &mut VecDeque::new(),
                )
                .await
                .unwrap();
                app.prepare_theme().unwrap();
                let warmed = app.proof_navigation_scene();
                let mut models = app.theme_models(&handle).unwrap().unwrap();
                app.normalize_theme_inputs(&handle, &mut models);
                let fresh =
                    common::theme::render::PreparedFrame::prepare(handle.clone(), &models[0])
                        .unwrap();
                assert!(warmed.0 == fresh.pixels().data(), "{id} pixels");
                assert_eq!(warmed.1, fresh.targets, "{id} targets");
                app.draw(&mut display, &style).unwrap();
            }
        }
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }
}

#[cfg(test)]
mod content_readiness_tests {
    use super::*;
    use common::theme::{fonts::FontBank, frame::{FrameInputs, Row}, pack::{Binding, ScreenId}, runtime::ThemeSnapshot, ThemeHandle};
    #[test]
    fn covered_titles_skip_pending_logos_but_hero_titles_still_require_them() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for name in ["compact", "compact-plus", "orbit-hero", "orbit-row", "big-picture"] {
            let snapshot = if name == "compact" { ThemeSnapshot::builtin(name).unwrap() }
                else { ThemeSnapshot::load(&root.join("themes").join(name)).unwrap() };
            let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
            let mut input = FrameInputs::new(ScreenId::Library);
            input.rows.insert(Binding::Entries, vec![Row {
                id: "selected".into(), label: "Selected".into(), subtitle: String::new(),
                value: String::new(), enabled: true,
            }]);
            input.selected.insert(Binding::Entries, "selected".into());
            input.art.completed_misses = common::theme::frame::ArtMisses {
                selected_cover: true, selected_hero: true,
                entries: std::collections::BTreeSet::from(["selected".into()]),
                ..Default::default()
            };
            assert!(!theme_content_complete(&input, &theme), "{name} pending logo without cover");
            let cover = std::sync::Arc::new(tiny_skia::Pixmap::new(16, 16).unwrap());
            input.art.entries.insert("other".into(), cover.clone());
            assert!(!theme_content_complete(&input, &theme), "{name} unrelated cover");
            let suppressed = matches!(name, "compact" | "compact-plus");
            input.art.selected_cover = Some(cover.clone());
            assert_eq!(theme_content_complete(&input, &theme), suppressed, "{name} direct cover");
            input.art.selected_cover = None;
            input.art.entries.insert("selected".into(), cover.clone());
            assert_eq!(theme_content_complete(&input, &theme), suppressed, "{name} selected row cover");
            input.art.selected_logo = Some(cover);
            assert!(theme_content_complete(&input, &theme), "{name} loaded logo");
        }
    }

    #[test]
    fn completed_misses_settle_only_their_visible_art_bindings() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for name in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes").join(name)).unwrap(), &bank, 1).unwrap();
            let demand = super::super::art_queue::Demand::from_components(
                &theme.pack().screens[&ScreenId::Library].components, theme.pack().ink(),
            );
            let mut input = FrameInputs::new(ScreenId::Library);
            input.rows.insert(Binding::Entries, vec![Row {
                id: "missing".into(), label: "Missing".into(), subtitle: String::new(),
                value: String::new(), enabled: true,
            }]);
            input.art.next_cover_expected = true;
            let pixels = input.art.clone();
            input.art.completed_misses = common::theme::frame::ArtMisses {
                selected_cover: true, selected_hero: true, selected_logo: true, next_cover: true,
                entries: std::collections::BTreeSet::from(["missing".into()]),
            };
            assert!(input.art == pixels, "{name} readiness must not change pixel identity");
            assert!(theme_content_complete(&input, &theme), "{name} completed misses");
            for (binding, required) in [(Binding::SelectedCover, demand.cover),
                (Binding::SelectedHero, demand.hero), (Binding::SelectedLogo, demand.logo),
                (Binding::NextCover, demand.next), (Binding::Entries, demand.rows)] {
                let mut pending = input.clone();
                match binding {
                    Binding::SelectedCover => pending.art.completed_misses.selected_cover = false,
                    Binding::SelectedHero => pending.art.completed_misses.selected_hero = false,
                    Binding::SelectedLogo => pending.art.completed_misses.selected_logo = false,
                    Binding::NextCover => pending.art.completed_misses.next_cover = false,
                    Binding::Entries => pending.art.completed_misses.entries.clear(),
                    _ => unreachable!(),
                }
                assert_eq!(theme_content_complete(&pending, &theme), !required, "{name} {binding:?} pending");
            }
        }
    }

    #[test]
    fn preview_pending_is_incomplete_even_when_visible_rows_are_loaded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for name in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes").join(name).canonicalize().unwrap()).unwrap(), &bank, 1).unwrap();
            let mut input = FrameInputs::new(ScreenId::Library);
            let art = std::sync::Arc::new(tiny_skia::Pixmap::new(16, 16).unwrap());
            input.art.selected_cover = Some(art.clone());
            input.art.selected_hero = Some(art.clone());
            input.art.selected_logo = Some(art.clone());
            input.rows.insert(Binding::Entries, vec![Row {
                id: "last".into(), label: "Last".into(), subtitle: String::new(),
                value: String::new(), enabled: true,
            }]);
            input.art.entries.insert("last".into(), art.clone());
            input.art.next_cover_expected = true;
            // Compact Plus has no preview dependency.
            assert_eq!(theme_content_complete(&input, &theme), name == "compact-plus", "{name} pending preview");
            input.art.next_cover = Some(art.clone());
            assert!(theme_content_complete(&input, &theme), "{name} loaded preview");
            input.art.next_cover = None;
            assert_eq!(theme_content_complete(&input, &theme), name == "compact-plus", "{name} missing preview");
            input.art.next_cover_expected = false;
            assert!(theme_content_complete(&input, &theme), "{name} real end fallback");
            input.art.entries.clear();
            if name != "compact-plus" {
                assert!(!theme_content_complete(&input, &theme), "{name} missing fallback");
            }
        }
    }
}
