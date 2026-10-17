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

pub(super) fn normalize_theme_models(
    handle: &common::theme::ThemeHandle,
    models: &mut [common::theme::frame::FrameInputs],
    res: &Resources,
    selected: usize,
    status: &common::theme::frame::Status,
) {
    prepare_theme_inputs(models, res, selected, status);
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

struct ParkedDetails {
    lower: common::theme::pack::ScreenId,
    frame: common::theme::render::PreparedFrame,
}
impl std::fmt::Debug for ParkedDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedDetails").field("lower", &self.lower).finish_non_exhaustive()
    }
}

const NEIGHBOR_LIMIT: usize = 5 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct NeighborGeneration<L = common::locale::LanguageIdentifier> {
    media: u64, names: u64, metadata: u64, adoption: u64, artwork: u64,
    locale: u64, consoles: u64, language: L,
}
impl<L> NeighborGeneration<L> {
    fn current(res: &Resources, language: L) -> Self {
        Self { media: common::media::generation(), names: common::media::name_generation(),
            metadata: super::details::metadata_generation(), adoption: super::orbit::metadata_completion_generation(),
            artwork: super::orbit::art_cache_revision(), locale: res.revision::<Locale>(),
            consoles: res.revision::<crate::consoles::ConsoleMapper>(), language }
    }
}
impl NeighborGeneration {
    fn borrowed(&self) -> NeighborGeneration<&common::locale::LanguageIdentifier> {
        let Self { media, names, metadata, adoption, artwork, locale, consoles, language } = self;
        NeighborGeneration { media: *media, names: *names, metadata: *metadata, adoption: *adoption,
            artwork: *artwork, locale: *locale, consoles: *consoles, language }
    }
}

#[derive(Default)]
struct NeighborGraveyard {
    base: Option<common::theme::render::RetiredFrame>,
    siblings: Vec<PreparedNeighbor>,
    packed: [Option<common::display::packed::PackedSpans>; 2],
    adopted_pack: Option<usize>,
    adopted_diagnostic: Option<common::display::packed::PackDiagnostic>,
    packed_reason: Option<&'static str>,
}
impl NeighborGraveyard {
    fn clear(&mut self) {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::FrameRetire);
        self.base = None;
        self.siblings.clear();
        self.packed = [None, None];
        self.adopted_pack = None;
        self.adopted_diagnostic = None;
        self.packed_reason = None;
    }
}
impl std::fmt::Debug for NeighborGraveyard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NeighborGraveyard").field("base", &self.base.is_some())
            .field("siblings", &self.siblings.len()).finish()
    }
}

struct PreparedNeighbor {
    base: common::theme::render::NavigationStamp,
    generation: NeighborGeneration,
    frame: common::theme::render::PreparedFrame,
    charge: usize,
    packed: Option<common::display::packed::PackedSpans>,
    pack_diagnostic: common::display::packed::PackDiagnostic,
}
fn retire_neighbor_packs(neighbors: &mut Vec<PreparedNeighbor>, graveyard: &mut NeighborGraveyard, reason: &'static str) {
    for neighbor in neighbors.iter_mut() {
        if retire_neighbor_pack(neighbor, graveyard).is_some() { graveyard.packed_reason = Some(reason); }
    }
    neighbors.clear();
}
fn retire_neighbor_pack(neighbor: &mut PreparedNeighbor, graveyard: &mut NeighborGraveyard) -> Option<usize> {
    let packed = neighbor.packed.take()?;
    let slot = graveyard.packed.iter().position(Option::is_none).expect("two neighbor pack slots");
    graveyard.packed[slot] = Some(packed);
    Some(slot)
}

// One candidate per idle turn; a refused priority candidate also blocks its sibling.
fn pack_neighbors(neighbors: &mut [PreparedNeighbor], display: &mut impl Display) -> Result<bool> {
    let available = NEIGHBOR_LIMIT.saturating_sub(neighbors.iter().map(|c| c.charge).sum());
    for candidate in neighbors {
        if candidate.packed.is_some() { continue; }
        if candidate.pack_diagnostic.reason != "no_pack_built" { return Ok(false); }
        candidate.packed = candidate.frame.pack_neighbor(display, available, &mut candidate.pack_diagnostic)?;
        if let Some(packed) = &candidate.packed {
            candidate.charge += packed.charge();
            candidate.pack_diagnostic = common::display::packed::PackDiagnostic::new("hit");
        }
        return Ok(true);
    }
    Ok(false)
}

impl std::fmt::Debug for PreparedNeighbor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedNeighbor").field("charge", &self.charge).finish_non_exhaustive()
    }
}

fn prepare_theme_frames(
    handle: &common::theme::ThemeHandle,
    models: &[common::theme::frame::FrameInputs],
    previous: Vec<common::theme::render::PreparedFrame>,
    parked: &mut Option<ParkedDetails>,
    neighbors: &mut Vec<PreparedNeighbor>,
    graveyard: &mut NeighborGraveyard,
    displayed: Option<&common::theme::render::NavigationStamp>,
    adopted: &mut bool,
) -> Result<Vec<common::theme::render::PreparedFrame>> {
    use common::theme::pack::ScreenId;
    use common::theme::render::PreparedFrame;
    *adopted = false;
    let Some((top, lower)) = models.split_last() else {
        *parked = None;
        return Ok(vec![]);
    };
    // Reuse only across the same single lower screen; other stacks retire the slot.
    let can_park = !crate::network_runtime::blocks_navigation_warm();
    let compatible_stack = can_park && (models.len() == 1
        || (models.len() == 2 && top.screen == ScreenId::GameDetail));
    if parked.as_ref().is_some_and(|saved| !compatible_stack
        || saved.lower != models[0].screen || !saved.frame.navigation_theme_matches(handle))
    {
        *parked = None;
    }
    let mut previous: Vec<_> = previous.into_iter().map(Some).collect();
    if can_park && models.len() == 1 && previous.len() == 2
        && previous[0].as_ref().is_some_and(|frame| frame.screen() == top.screen)
        && previous[1].as_ref().is_some_and(|frame| {
            frame.screen() == ScreenId::GameDetail && frame.navigation_theme_matches(handle)
        })
    {
        *parked = Some(ParkedDetails { lower: top.screen, frame: previous[1].take().unwrap() });
    }
    if models.len() == 2 && top.screen == ScreenId::GameDetail {
        if let Some(saved) = parked.take() {
            previous.resize_with(models.len().max(previous.len()), || None);
            if previous[1].as_ref().is_none_or(|frame| frame.screen() != ScreenId::GameDetail) {
                previous[1] = Some(saved.frame);
            }
        }
    }
    let old = previous.get_mut(lower.len()).and_then(Option::take);
    let tracing = common::perf_trace::recording();
    let mut rejection = if models.len() != 1 { "overlay_stack" }
        else if !can_park { "network_blocked" } else { "no_candidate" };
    let search = common::perf_trace::stage(common::perf_trace::Stage::NeighborSearch);
    let candidate = if models.len() == 1 && can_park {
        neighbors.iter().position(|candidate| {
            let Some(base) = &old else { rejection = "no_base"; return false; };
            if !displayed.is_some_and(|stamp| base.navigation_is_current(stamp))
                || !base.navigation_is_current(&candidate.base)
            { rejection = "base_mismatch"; return false; }
            if candidate.frame.neighbor_matches(handle, top) { return true; }
            if tracing { rejection = candidate.frame.neighbor_rejection_reason(handle, top); }
            false
        }).map(|index| neighbors.swap_remove(index))
    } else { None };
    drop(search);
    if tracing && top.screen == ScreenId::Recents {
        common::perf_trace::neighbor_adopt(if candidate.is_some() { "hit" } else { rejection });
    }
    if tracing && candidate.is_none() && !neighbors.is_empty() {
        common::perf_trace::neighbor_build("invalidated:prepare_mismatch");
    }
    {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::NeighborClear);
        if candidate.is_some() {
            // No new candidates can be built until idle has drained the graveyard.
            debug_assert!(graveyard.base.is_none() && graveyard.siblings.is_empty());
            std::mem::swap(neighbors, &mut graveyard.siblings);
        } else { retire_neighbor_packs(neighbors, graveyard, if models.len() != 1 { "pack_drop_overlay" }
            else if !can_park { "pack_drop_network" } else { "pack_drop_prepare" }); }
    }
    let top = if let Some(mut neighbor) = candidate {
        graveyard.adopted_diagnostic = Some(neighbor.pack_diagnostic);
        graveyard.adopted_pack = retire_neighbor_pack(&mut neighbor, graveyard);
        let mut candidate = neighbor.frame;
        graveyard.base = Some(candidate.recycle_neighbor_base(old.unwrap()));
        *adopted = true;
        common::perf_trace::count(common::perf_trace::Count::NeighborHit, 1);
        candidate
    } else {
        if top.screen == ScreenId::Recents {
            common::perf_trace::count(common::perf_trace::Count::NeighborMiss, 1);
        }
        PreparedFrame::prepare_reusing(handle.clone(), top, old)?
    };
    // PreparedFrame proves opacity across the entire 640 by 480 framebuffer.
    // Keep covered frames untouched; closing prepares them against current inputs.
    let covered = top.is_opaque() && lower.iter().enumerate().all(|(i, input)| {
        previous.get(i).and_then(Option::as_ref).is_some_and(|frame| {
            frame.screen() == input.screen && frame.navigation_theme_matches(handle)
        })
    });
    let mut frames = Vec::with_capacity(models.len());
    for (i, input) in lower.iter().enumerate() {
        let old = previous.get_mut(i).and_then(Option::take);
        frames.push(if covered {
            old.unwrap()
        } else {
            PreparedFrame::prepare_reusing(handle.clone(), input, old)?
        });
    }
    frames.push(top);
    Ok(frames)
}

fn visible_theme_models<'a>(
    models: &'a [common::theme::frame::FrameInputs],
    frames: &[common::theme::render::PreparedFrame],
) -> &'a [common::theme::frame::FrameInputs] {
    if frames.last().is_some_and(|frame| frame.is_opaque()) {
        &models[models.len() - 1..]
    } else { models }
}

pub(super) fn theme_content_complete(
    input: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
) -> bool {
    theme_content_pending::<false>(input, handle).is_none()
}

fn theme_content_pending<const DIAGNOSE_ROWS: bool>(
    input: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
) -> Option<(&'static str, usize)> {
    visit_theme_content_pending::<DIAGNOSE_ROWS>(input, handle, |_, _| true)
}

fn visit_theme_content_pending<const DIAGNOSE_ROWS: bool>(
    input: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
    mut stop: impl FnMut(&'static str, usize) -> bool,
) -> Option<(&'static str, usize)> {
    use common::theme::pack::{Arrangement, Binding, Kind};
    let has_rows = input.rows.values().any(|rows| !rows.is_empty());
    if !has_rows && input.screen != common::theme::pack::ScreenId::GameDetail
        && input.selected.keys().all(|binding| *binding == Binding::Navigation)
    {
        return None;
    }
    handle.pack().screens[&input.screen]
        .components
        .iter()
        .find_map(|c| {
            if c.layout == Some(Arrangement::Hidden) {
                return None;
            }
            let logo_suppressed = common::theme::logo_suppressed_by_cover(c, input);
            if c.logo && !logo_suppressed && input.art.selected_logo.is_none() && !input.art.completed_misses.selected_logo {
                if stop("logo", 0) { return Some(("logo", 0)); }
            }
            if c.kind == Kind::Image {
                let (ready, reason) = match c.binding {
                    Some(Binding::NextCover) => (!input.art.next_cover_expected || input.art.next_cover.is_some()
                        || input.art.completed_misses.next_cover, "next_cover"),
                    Some(Binding::SelectedCover) => (input.art.selected_cover.is_some() || input.art.completed_misses.selected_cover, "selected_cover"),
                    Some(Binding::SelectedHero) => (input.art.selected_hero.is_some() || input.art.completed_misses.selected_hero, "selected_hero"),
                    Some(Binding::SelectedLogo) => (logo_suppressed || input.art.selected_logo.is_some() || input.art.completed_misses.selected_logo, "selected_logo"),
                    _ => (true, ""),
                };
                return (!ready && stop(reason, 0)).then_some((reason, 0));
            }
            if c.kind == Kind::Entries
                && c.layout == Some(Arrangement::Shelf)
                && c.paint.as_ref().is_some_and(|paint| paint.peek.is_some())
                && input.art.next_cover_expected
                && input.art.next_cover.is_none()
                && !input.art.completed_misses.next_cover
            {
                if stop("peek", 0) { return Some(("peek", 0)); }
            }
            if c.kind == Kind::Entries
                && (c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0)
            {
                if let Some(rows) = c.binding.and_then(|binding| input.rows.get(&binding)) {
                    let mut missing = rows.iter().filter(|row| !input.art.entries.contains_key(&row.id)
                        && !input.art.completed_misses.entries.contains(&row.id));
                    let count = if DIAGNOSE_ROWS { missing.count() } else { usize::from(missing.next().is_some()) };
                    if count != 0 && stop("rows", count) { return Some(("rows", count)); }
                }
            }
            None
        })
}

fn theme_content_pending_reasons(
    models: &[common::theme::frame::FrameInputs],
    handle: &common::theme::ThemeHandle,
) -> Vec<(&'static str, usize)> {
    let mut reasons = Vec::new();
    for input in models {
        visit_theme_content_pending::<true>(input, handle, |reason, count| {
            if !reasons.contains(&(reason, count)) { reasons.push((reason, count)); }
            false
        });
    }
    reasons
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
    NeighborArtwork(super::entry_list::NavigationWarm),
    NeighborFrame(isize, bool),
    DetailsPlan,
    DetailsArtwork(super::entry_list::NavigationWarm),
    DetailsImages(common::theme::render::DetailImageWarm, super::entry_list::NavigationWarm),
    DetailsText(common::theme::render::DetailTextWarm),
    DetailShadows(Option<common::theme::render::DetailHeroWarm>),
    DetailsHero(common::theme::render::DetailHeroWarm),
}

#[cfg(test)]
thread_local! { static THEME_MODEL_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

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
    // Static Details geometry outlives selection-specific navigation plans.
    detail_shadows: Option<common::theme::render::DetailShadowWarm>,
    content_complete: bool,
    theme_scene: Option<crate::theme_adapter::ThemeScene>,
    // At most one closed Details frame: ~3.56 MB of framebuffer and operation underlays.
    parked_details: Option<ParkedDetails>,
    neighbors: Vec<PreparedNeighbor>,
    neighbor_graveyard: NeighborGraveyard,
    neighbor_base: Option<common::theme::render::NavigationStamp>,
    neighbor_adopted: bool,
    present_packed: Option<usize>,
    present_pack_diagnostic: common::display::packed::PackDiagnostic,
    media_deferred: bool,
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
            detail_shadows: None,
            content_complete: true,
            theme_scene: None,
            parked_details: None,
            neighbors: Vec::with_capacity(2),
            neighbor_graveyard: NeighborGraveyard { base: None, siblings: Vec::with_capacity(2), packed: [None, None], ..Default::default() },
            neighbor_base: None,
            neighbor_adopted: false,
            present_packed: None,
            present_pack_diagnostic: common::display::packed::PackDiagnostic::new("not_adopted"),
            media_deferred: false,
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
    pub(crate) fn proof_detail_shadow_warming(&self, work: &Option<NavigationWarm>) -> bool {
        work.is_some() && self.detail_shadows.is_some()
    }

    #[cfg(test)]
    pub(crate) fn proof_retired_neighbors(&self) -> bool {
        self.neighbor_graveyard.base.is_some() || !self.neighbor_graveyard.siblings.is_empty()
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

    #[cfg(all(test, feature = "perf-bench"))]
    pub(crate) fn proof_fresh_navigation_pixels(&mut self) -> Vec<u8> {
        let handle = crate::theme_adapter::snapshot(&self.res).unwrap();
        let mut models = self.theme_models(&handle).unwrap().unwrap();
        self.normalize_theme_inputs(&handle, &mut models);
        common::theme::render::PreparedFrame::prepare(handle, &models[0]).unwrap().pixels().data().to_vec()
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
        self.release_details_frame();
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
    pub(crate) fn release_details_frame(&mut self) {
        self.parked_details = None;
        self.invalidate_neighbors_for("invalidated:release_details");
    }
    pub(crate) fn prepare_command(&mut self, command: &Command) {
        if matches!(command, Command::Exec(_) | Command::ReloadStylesheet(..)) {
            self.release_details_frame();
            self.views.0.release_theme_art();
            self.views.1.release_theme_art();
            self.views.2.release_theme_art();
            if let Some(search) = self.search_results.as_mut() { search.release_theme_art(); }
        }
    }
    pub(crate) fn force_full_present(&mut self) {
        self.invalidate_neighbors_for("invalidated:full_present");
        self.dirty = true;
    }
    pub(crate) fn invalidate_neighbors(&mut self) {
        self.invalidate_neighbors_for("invalidated:view_state");
    }
    pub(crate) fn invalidate_neighbors_for(&mut self, reason: &'static str) {
        if common::perf_trace::recording() && (self.neighbor_base.is_some() || !self.neighbors.is_empty()) {
            common::perf_trace::neighbor_build(reason);
        }
        retire_neighbor_packs(&mut self.neighbors, &mut self.neighbor_graveyard, match reason {
            "invalidated:status" => "pack_drop_status",
            "invalidated:artwork_completion" => "pack_drop_artwork",
            "invalidated:release_details" => "pack_drop_details",
            "invalidated:full_present" => "pack_drop_full",
            "invalidated:animation" => "pack_drop_animation",
            "invalidated:toast" => "pack_drop_toast",
            "invalidated:network_blocked" => "pack_drop_network",
            "invalidated:redraw" => "pack_drop_redraw",
            _ => "pack_drop_view",
        });
        self.neighbor_base = None;
        self.neighbor_adopted = false;
    }
    pub(crate) fn take_present_damage(&mut self) -> Option<Vec<Rect>> {
        self.present_damage.take()
    }
    pub(crate) fn flush_prepared_damage(&self, display: &mut impl Display, damage: &[Rect]) -> Result<()> {
        if let Some(slot) = self.present_packed {
            if let Some(packed) = &self.neighbor_graveyard.packed[slot] {
                return display.flush_packed_rects(damage, packed);
            }
            if common::perf_trace::recording() { common::perf_trace::packed_span_reason("pack_missing_slot"); }
        } else if common::perf_trace::recording() {
            self.present_pack_diagnostic.record();
        }
        display.flush_rects(damage)
    }

    pub(crate) fn prepack_neighbors(&mut self, display: &mut impl Display, mut input_pending: impl FnMut() -> bool) -> Result<bool> {
        if input_pending() { return Ok(false); }
        pack_neighbors(&mut self.neighbors, display)
    }

    pub(crate) fn idle_navigation_step(&mut self, display: &mut impl Display,
        work: &mut Option<NavigationWarm>, steps: &mut usize, mut input_pending: impl FnMut() -> bool,
    ) -> Result<()> {
        if self.prepack_neighbors(display, &mut input_pending)? { return Ok(()); }
        if *steps < 2 && self.prewarm_navigation_step_with_pending(work,
            if *steps == 0 { 1 } else { -1 }, input_pending)? { *steps += 1; }
        Ok(())
    }

    fn draw_prepared_theme(&mut self, display: &mut impl Display) -> Result<Option<bool>> {
        self.present_damage = None;
        self.present_packed = None;
        self.present_pack_diagnostic = common::display::packed::PackDiagnostic::new(self.neighbor_graveyard.packed_reason.unwrap_or("not_adopted"));
        if crate::theme_adapter::active(&self.res) {
            if self.theme_scene.as_ref().is_some_and(|scene|
                scene.frames.iter().any(|frame| !frame.pixels_usable()))
            {
                self.theme_scene = None;
                self.invalidate_neighbors();
                self.dirty = true;
                self.prepare_theme()?;
            }
            if let Some(scene) = &mut self.theme_scene {
                if !self.theme_dirty && !self.dirty {
                    return Ok(Some(false));
                }
                let layered = scene.frames.len() != 1;
                let top = scene.frames.last_mut().unwrap();
                let opaque_top = top.is_opaque();
                let same_top = self.theme_presented_screen == Some(top.screen());
                if !self.dirty && ((!layered && !self.theme_was_layered)
                    || (opaque_top && same_top)) {
                    self.present_damage = Some(if std::mem::take(&mut self.neighbor_adopted) {
                        self.present_pack_diagnostic = self.neighbor_graveyard.adopted_diagnostic.unwrap_or_default();
                        if let Some(slot) = self.neighbor_graveyard.adopted_pack {
                            if self.neighbor_graveyard.packed[slot].as_ref().is_some_and(|packed| display.packed_base_matches(packed)) {
                                self.present_packed = Some(slot);
                            } else { self.present_pack_diagnostic = common::display::packed::PackDiagnostic::new(if self.neighbor_graveyard.packed[slot].is_none() {
                                "pack_missing_slot"
                            } else { "base_mismatch" }); }
                        }
                        top.adopt_neighbor_damage(display)
                    } else { top.paint_damage(display) }.ok_or_else(|| anyhow::anyhow!("expired prepared frame"))?);
                } else if opaque_top {
                    top.paint(display);
                } else {
                    for frame in &scene.frames {
                        frame.paint(display);
                    }
                }
                self.neighbor_adopted = false;
                let top = scene.frames.last().unwrap();
                if common::perf_trace::recording() && !self.neighbors.is_empty() {
                    common::perf_trace::neighbor_build("invalidated:draw");
                }
                retire_neighbor_packs(&mut self.neighbors, &mut self.neighbor_graveyard, "pack_drop_draw");
                self.neighbor_base = (scene.frames.len() == 1 && top.can_prepare_neighbor()
                    && display.width() == 640 && display.height() == 480).then(|| top.navigation_stamp());
                self.theme_presented_screen = Some(top.screen());
                self.theme_was_layered = scene.frames.len() != 1;
                self.theme_dirty = false;
                self.dirty = false;
                return Ok(Some(self.present_damage.as_ref().is_none_or(|damage| !damage.is_empty())));
            }
        }
        Ok(None)
    }

    fn details_active(&self) -> bool {
        if self.search_view.is_active() {
            return false;
        }
        if let Some(search) = &self.search_results {
            return search.details_active();
        }
        match self.selected {
            0 => self.views.0.details_active(),
            1 => self.views.1.details_active(),
            _ => false,
        }
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

    pub(crate) fn idle_retire_neighbors(&mut self) {
        self.neighbor_graveyard.clear();
    }

    pub(crate) fn defer_media_step(&mut self) -> bool {
        self.media_deferred = !super::orbit::art_result_queued() && !self.dirty
            && self.selected == 0 && self.search_results.is_none() && !self.search_view.is_active()
            && self.rect.w == 640 && self.rect.h == 480 && self.views.0.navigation_warm_ready()
            && !crate::network_runtime::blocks_navigation_warm() && !self.neighbors.is_empty();
        if self.media_deferred && common::perf_trace::recording() {
            common::perf_trace::neighbor_warm("media_no_result");
        }
        self.media_deferred
    }

    pub(crate) fn idle_media_step(&mut self) -> bool {
        self.media_deferred = false;
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
            self.invalidate_neighbors_for("invalidated:artwork_completion");
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

    #[cfg(any(test, feature = "perf-bench"))]
    pub(crate) fn navigation_warm_kind(&self, work: &Option<NavigationWarm>) -> &'static str {
        match work.as_ref().map(|work| &work.stage) {
            None => "plan",
            Some(NavigationWarmStage::Artwork(_)) => "artwork",
            Some(NavigationWarmStage::Images(_, _)) => "image_transform",
            Some(NavigationWarmStage::DetailsArtwork(_)) => "details_artwork",
            Some(NavigationWarmStage::DetailsImages(..)) => "details_image_transform",
            Some(NavigationWarmStage::DetailsText(_)) => "details_text",
            Some(NavigationWarmStage::DetailShadows(_)) => "detail_shadow",
            Some(NavigationWarmStage::DetailsHero(_)) => "details_hero",
            Some(NavigationWarmStage::Rasters(_)) => "raster",
            Some(NavigationWarmStage::NeighborArtwork(_)) => "neighbor_artwork",
            Some(NavigationWarmStage::NeighborFrame(..)) => "neighbor_frame",
            Some(NavigationWarmStage::DetailsPlan) => "details_plan",
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
            if common::perf_trace::recording() { common::perf_trace::neighbor_warm("pending_input"); }
            return Ok(false);
        }
        self.idle_retire_neighbors();
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
        let locale = self.res.get::<Locale>();
        let generation = NeighborGeneration::current(&self.res, &locale.lang);
        let resident = self.neighbors.len();
        self.neighbors.retain_mut(|candidate| {
            let keep = candidate.generation.borrowed() == generation && candidate.frame.navigation_theme_matches(&handle);
            if !keep && retire_neighbor_pack(candidate, &mut self.neighbor_graveyard).is_some() {
                self.neighbor_graveyard.packed_reason = Some("pack_drop_generation");
            }
            keep
        });
        drop(locale);
        self.neighbor_graveyard.clear();
        if common::perf_trace::recording() && self.neighbors.len() != resident {
            common::perf_trace::neighbor_build("invalidated:generation_or_theme");
        }
        let status = if self.res.get::<Stylesheet>().status_bar.hide_status_in_launcher {
            Default::default()
        } else { self.status_bar.theme_inputs() };
        let frame = &mut self.theme_scene.as_mut().unwrap().frames[0];
        frame.restore_pixel_storage();
        if common::perf_trace::recording() && !frame.can_prepare_neighbor() {
            common::perf_trace::neighbor_build(frame.neighbor_ineligible_reason());
        }
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
            common::perf_trace::count(common::perf_trace::Count::PlanBuilds, 1);
            if self.detail_shadows.as_ref().is_none_or(|shadows| !shadows.theme_matches(&handle)) {
                self.detail_shadows = Some(frame.detail_shadow_warm());
            }
            *work = Some(NavigationWarm {
                stamp: frame.navigation_stamp(),
                media_generation: common::media::generation(),
                name_generation: common::media::name_generation(),
                direction,
                stage: plan.map(NavigationWarmStage::Artwork)
                    .unwrap_or_else(|| NavigationWarmStage::Rasters(VecDeque::new())),
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
                    normalize_theme_models(&handle, std::slice::from_mut(&mut plan.input), &self.res, self.selected, &status);
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
                    current.stage = if self.selected == 0 && frame.can_prepare_neighbor() {
                        if let Some(input) = frame.navigation_model() {
                            self.views.0.navigation_warm_model(&handle, -self.navigation_direction, input)?
                                .map(NavigationWarmStage::NeighborArtwork)
                                .unwrap_or(NavigationWarmStage::NeighborFrame(self.navigation_direction, false))
                        } else { NavigationWarmStage::DetailsPlan }
                    } else { NavigationWarmStage::DetailsPlan };
                    *work = Some(current);
                    return Ok(false);
                };
                frame.prewarm_navigation_raster(raster)?;
                NavigationWarmStage::Rasters(rasters)
            }
            NavigationWarmStage::NeighborArtwork(mut plan) => {
                if self.views.0.navigation_warm_art_step(&mut plan) {
                    NavigationWarmStage::NeighborFrame(self.navigation_direction, false)
                } else { NavigationWarmStage::NeighborArtwork(plan) }
            }
            NavigationWarmStage::NeighborFrame(target_direction, final_pass) => {
                if self.selected == 0 && self.rect.w == 640 && self.rect.h == 480
                    && self.neighbor_base.as_ref().is_some_and(|base| frame.navigation_is_current(base))
                    && frame.can_prepare_neighbor() && self.neighbors.len() < 2
                    && !crate::network_runtime::blocks_navigation_warm()
                {
                    if let Some(mut input) = self.views.0.navigation_frame_model(&handle, target_direction) {
                        normalize_theme_models(&handle, std::slice::from_mut(&mut input), &self.res, self.selected, &status);
                        let duplicate = self.neighbors.iter().any(|candidate| candidate.frame.input_matches(&input));
                        if input_pending() { current.stage = NavigationWarmStage::NeighborFrame(target_direction, final_pass);
                            *work = Some(current); return Ok(false); }
                        let available = NEIGHBOR_LIMIT - self.neighbors.iter().map(|c| c.charge).sum::<usize>();
                        if !duplicate && let Some((candidate, charge)) = frame.prepare_neighbor(&input, available.saturating_sub(std::mem::size_of::<PreparedNeighbor>()))? {
                            self.neighbors.push(PreparedNeighbor { base: frame.navigation_stamp(),
                                generation: NeighborGeneration::current(&self.res, self.res.get::<Locale>().lang.clone()), frame: candidate, charge: charge + std::mem::size_of::<PreparedNeighbor>(), packed: None, pack_diagnostic: Default::default() });
                        }
                    } else if common::perf_trace::recording() {
                        common::perf_trace::neighbor_build("not_eligible:no_target");
                    }
                } else if common::perf_trace::recording() && self.neighbors.len() < 2 {
                    common::perf_trace::neighbor_build(if self.selected != 0 { "not_eligible:screen" }
                        else if self.rect.w != 640 || self.rect.h != 480 { "not_eligible:size" }
                        else if !frame.can_prepare_neighbor() { frame.neighbor_ineligible_reason() }
                        else if crate::network_runtime::blocks_navigation_warm() { "not_eligible:network_blocked" }
                        else { "not_eligible:no_displayed_base" });
                }
                if target_direction == self.navigation_direction && self.selected == 0 && frame.can_prepare_neighbor() {
                    NavigationWarmStage::NeighborFrame(-target_direction, final_pass)
                } else if final_pass {
                    current.stage = NavigationWarmStage::NeighborFrame(target_direction, true);
                    *work = Some(current);
                    return Ok(true);
                } else { NavigationWarmStage::DetailsPlan }
            }
            NavigationWarmStage::DetailsPlan => {
                let plan = match self.selected {
                    0 => self.views.0.details_warm_model(&handle, frame.screen()),
                    1 => self.views.1.details_warm_model(&handle, frame.screen()),
                    _ => None,
                };
                plan.map(NavigationWarmStage::DetailsArtwork).unwrap_or(NavigationWarmStage::DetailShadows(None))
            }
            NavigationWarmStage::DetailsArtwork(mut plan) => {
                let ready = match self.selected {
                    0 => self.views.0.navigation_warm_art_step(&mut plan),
                    1 => self.views.1.navigation_warm_art_step(&mut plan),
                    _ => return Ok(true),
                };
                if ready { NavigationWarmStage::DetailsImages(frame.detail_images(&plan.input), plan) }
                else { NavigationWarmStage::DetailsArtwork(plan) }
            }
            NavigationWarmStage::DetailsImages(mut images, mut plan) => {
                if frame.prewarm_detail_image_step(&mut images)? {
                    if let Some(model) = plan.details_text.take() {
                        let status = if self.res.get::<Stylesheet>().status_bar.hide_status_in_launcher {
                            Default::default()
                        } else { self.status_bar.theme_inputs() };
                        prepare_theme_inputs(std::slice::from_mut(&mut plan.input), &self.res, self.selected, &status);
                        NavigationWarmStage::DetailsText(frame.detail_texts(move || model(plan.input)))
                    } else { NavigationWarmStage::DetailShadows(None) }
                } else { NavigationWarmStage::DetailsImages(images, plan) }
            }
            NavigationWarmStage::DetailsText(mut text) => {
                if frame.prewarm_detail_text_step(&mut text)? {
                    NavigationWarmStage::DetailShadows(frame.detail_hero_after_text(&mut text))
                }
                else { NavigationWarmStage::DetailsText(text) }
            }
            NavigationWarmStage::DetailShadows(hero) => {
                let done = self.detail_shadows.as_mut()
                    .is_none_or(|shadows| frame.prewarm_detail_shadow_step(shadows));
                let complete = done && hero.is_none();
                current.stage = if done { hero.map(NavigationWarmStage::DetailsHero)
                    .unwrap_or(NavigationWarmStage::DetailShadows(None)) }
                    else { NavigationWarmStage::DetailShadows(hero) };
                *work = Some(current);
                if complete && self.selected == 0 && frame.can_prepare_neighbor() {
                    work.as_mut().unwrap().stage = NavigationWarmStage::NeighborFrame(self.navigation_direction, true);
                    return Ok(false);
                }
                return Ok(complete);
            }
            NavigationWarmStage::DetailsHero(mut hero) => {
                let done = frame.prewarm_detail_hero_step(&mut hero)?;
                current.stage = NavigationWarmStage::DetailsHero(hero);
                *work = Some(current);
                if done && self.selected == 0 && frame.can_prepare_neighbor() {
                    work.as_mut().unwrap().stage = NavigationWarmStage::NeighborFrame(self.navigation_direction, true);
                    return Ok(false);
                }
                return Ok(done);
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
        normalize_theme_models(handle, models, &self.res, self.selected, &status);
    }

    pub(crate) fn prepare_theme(&mut self) -> Result<()> {
        // Recheck after input/update/layout work, later than the ordinary media poll.
        if super::orbit::art_result_queued() {
            self.idle_media_step();
        }
        self.neighbor_adopted = false;
        let Some(handle) = crate::theme_adapter::snapshot(&self.res) else {
            self.release_details_frame();
            self.content_complete = true;
            if common::perf_trace::recording() {
                common::perf_trace::content_readiness("ready", false, 0);
            }
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
        let models = {
            let _stage = common::perf_trace::stage(common::perf_trace::Stage::FrameModel);
            self.theme_models(&handle)?
        };
        self.theme_scene = if let Some(mut models) = models {
            {
                let _stage = common::perf_trace::stage(common::perf_trace::Stage::FrameNormalize);
                self.normalize_theme_inputs(&handle, &mut models);
            }
            let locale = self.res.get::<Locale>();
            let generation = {
                let _stage = common::perf_trace::stage(common::perf_trace::Stage::NeighborGeneration);
                NeighborGeneration::current(&self.res, &locale.lang)
            };
            let resident = self.neighbors.len();
            {
                let _stage = common::perf_trace::stage(common::perf_trace::Stage::NeighborFilter);
                self.neighbors.retain_mut(|candidate| {
                    let keep = candidate.generation.borrowed() == generation;
                    if !keep && retire_neighbor_pack(candidate, &mut self.neighbor_graveyard).is_some() {
                        self.neighbor_graveyard.packed_reason = Some("pack_drop_generation");
                    }
                    keep
                });
            }
            drop(locale);
            if common::perf_trace::recording() && self.neighbors.len() != resident {
                common::perf_trace::neighbor_build("invalidated:generation");
            }
            if self.dirty || self.selected != 0 || self.search_results.is_some() || self.search_view.is_active()
                || self.rect.w != 640 || self.rect.h != 480 || !self.views.0.navigation_warm_ready()
            { self.invalidate_neighbors(); }
            if self.media_deferred {
                let eligible = models.len() == 1 && self.theme_scene.as_ref().is_some_and(|scene| {
                    scene.frames.len() == 1 && self.neighbor_base.as_ref().is_some_and(|stamp|
                        scene.frames[0].navigation_is_current(stamp))
                        && self.neighbors.iter().any(|candidate|
                            scene.frames[0].navigation_is_current(&candidate.base)
                                && candidate.frame.neighbor_matches(&handle, &models[0]))
                });
                if !eligible {
                    self.idle_media_step();
                    return self.prepare_theme();
                }
            }
            let previous = self.theme_scene.take().map(|scene| scene.frames).unwrap_or_default();
            let frames = prepare_theme_frames(&handle, &models, previous, &mut self.parked_details,
                &mut self.neighbors, &mut self.neighbor_graveyard, self.neighbor_base.as_ref(), &mut self.neighbor_adopted)?;
            let _readiness = common::perf_trace::stage(common::perf_trace::Stage::FrameReadiness);
            let visible = visible_theme_models(&models, &frames);
            self.content_complete = visible.iter().all(|input| theme_content_complete(input, &handle));
            if common::perf_trace::recording() {
                let pending = theme_content_pending_reasons(visible, &handle);
                let next_cover_expected = visible.iter().any(|input| input.art.next_cover_expected);
                common::perf_trace::content_readiness_reasons(pending, next_cover_expected);
            }
            Some(crate::theme_adapter::ThemeScene { frames })
        } else {
            self.release_details_frame();
            self.content_complete = true;
            if common::perf_trace::recording() {
                common::perf_trace::content_readiness("ready", false, 0);
            }
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
        #[cfg(test)]
        THEME_MODEL_BUILDS.with(|count| count.set(count.get() + 1));
        let models = if let Some(keyboard) = self.search_view.theme_inputs() {
            Some(vec![crate::theme_adapter::keyboard_model(keyboard)])
        } else if let Some(search) = self.search_results.as_mut() {
            Some(search.theme_models(&handle)?)
        } else {
            match self.selected {
                0 => {
                    let covered = self.theme_scene.as_ref().is_some_and(|scene| {
                        scene.frames.len() == 2
                            && scene.frames[0].screen() == common::theme::pack::ScreenId::Recents
                            && scene.frames.iter().all(|frame| frame.navigation_theme_matches(handle))
                            && scene.frames[1].screen() == common::theme::pack::ScreenId::GameDetail
                            && scene.frames[1].is_opaque()
                    });
                    self.views.0.theme_models(&handle, covered)?
                },
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
            self.invalidate_neighbors_for("invalidated:status");
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(drawn) = self.draw_prepared_theme(display)? { return Ok(drawn); }
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
        self.invalidate_neighbors_for("invalidated:redraw");
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
            if self.selected != 0 || !self.views.0.navigation_warm_ready()
                || crate::network_runtime::blocks_navigation_warm() { self.invalidate_neighbors(); }
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
        let details_active = self.details_active();
        let selected = self.selected;
        let dirty = (self.dirty, self.theme_dirty);
        let handled = self.dispatch_key_event(event, commands, bubble).await?;
        if crate::network_runtime::blocks_navigation_warm() {
            self.release_details_frame();
        }
        if self.selected != 0 || !self.views.0.navigation_warm_ready()
            || self.search_results.is_some() || self.search_view.is_active() { self.invalidate_neighbors(); }
        if let Some(handle) = handle {
            let same_theme = crate::theme_adapter::snapshot(&self.res).is_some_and(|current| {
                current.generation() == handle.generation() && current.digest() == handle.digest()
            });
            if retained_comparison || before.is_some() {
                let unchanged = if !same_theme || selected != self.selected
                    || details_active != self.details_active()
                {
                    // Opening/closing Details changes the scene stack unconditionally.
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
                                    .skip(if scene.frames.last().is_some_and(|top| top.is_opaque()) {
                                        after.len().saturating_sub(1)
                                    } else { 0 })
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
mod details_frame_tests;

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
    fn normalized_empty_lists_are_content_complete() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, 1).unwrap();
        for screen in [ScreenId::Recents, ScreenId::Library, ScreenId::SearchResults] {
            let mut input = FrameInputs::new(screen);
            input.rows.insert(Binding::Entries, vec![]);
            prepare_theme_inputs(std::slice::from_mut(&mut input), &resources(), 0, &Status::default());
            assert_eq!(input.selected.len(), 1);
            assert!(input.selected.contains_key(&Binding::Navigation));
            assert!(super::theme_content_complete(&input, &theme), "{screen:?}");
            assert!(super::theme_content_pending_reasons(&[input], &theme).is_empty(), "{screen:?}");
        }
    }

    #[test]
    fn normalized_opaque_details_wait_for_their_own_artwork() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, 1).unwrap();
        let mut models = vec![FrameInputs::new(ScreenId::Recents), FrameInputs::new(ScreenId::GameDetail)];
        models[0].selected.insert(Binding::Entries, "missing-underlay-art".into());
        models[1].text.insert(Binding::SelectedTitle, "Selected game".into());
        prepare_theme_inputs(&mut models, &resources(), 0, &Status::default());
        let frames = super::prepare_theme_frames(&theme, &models, vec![], &mut None, &mut vec![], &mut super::NeighborGraveyard::default(), None, &mut false).unwrap();
        assert!(frames.last().unwrap().is_opaque());
        let visible = super::visible_theme_models(&models, &frames);
        assert_eq!(visible.len(), 1);
        assert!(!super::theme_content_complete(&visible[0], &theme));
        let reasons = super::theme_content_pending_reasons(visible, &theme);
        for reason in ["selected_hero", "selected_logo", "selected_cover"] {
            assert!(reasons.contains(&(reason, 0)), "{reasons:?}");
        }
        models[1].art.completed_misses = common::theme::frame::ArtMisses {
            selected_hero: true, selected_logo: true, selected_cover: true, ..Default::default()
        };
        let visible = super::visible_theme_models(&models, &frames);
        assert!(visible.iter().all(|input| super::theme_content_complete(input, &theme)));
        assert!(super::theme_content_pending_reasons(visible, &theme).is_empty());
        assert!(!super::theme_content_pending_reasons(&models, &theme).is_empty());
    }

    #[test]
    fn adapter_preserves_authored_library_hints_without_dialog() {
        let mut models = vec![authored(ScreenId::Library, "A", "Play")];

        prepare_theme_inputs(&mut models, &resources(), 1, &Status::default());

        assert_eq!(models[0].hints.len(), 1);
        assert_eq!(models[0].hints[0].label, "Play");
    }

    #[test]
    fn opaque_details_preserve_underlay_and_transparent_dialog_prepares_it() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, render::PreparedFrame};
        use common::{display::Display, platform::{DefaultPlatform, Platform}};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, 1).unwrap();
        let mut lower = authored(ScreenId::Recents, "A", "Play");
        lower.text.insert(Binding::SelectedTitle, "AV To é 日本語".into());
        lower.status.clock = Some("12:34".into());
        let mut optimized = vec![];
        let mut parked = None;
        let mut reference = vec![];
        for top_screen in [None, Some(ScreenId::GameDetail), None, Some(ScreenId::ContextMenu), None] {
            let mut models = vec![lower.clone()];
            if let Some(screen) = top_screen {
                models[0].suppress_hints = true;
                models[0].hints.clear();
                let mut top = authored(screen, "B", "Back");
                top.text.insert(Binding::SelectedTitle, "AV To é 日本語".into());
                top.art.completed_misses = common::theme::frame::ArtMisses {
                    selected_hero: true, selected_logo: true, selected_cover: true, ..Default::default()
                };
                models.push(top);
            }
            let before = optimized.first().map(|frame: &PreparedFrame| frame.pixels().data().to_vec());
            optimized = super::prepare_theme_frames(&theme, &models, optimized, &mut parked, &mut vec![], &mut super::NeighborGraveyard::default(), None, &mut false).unwrap();
            if top_screen.is_some() {
                let mut pending = models.clone();
                pending[0].selected.insert(Binding::Entries, "missing-underlay-art".into());
                assert!(!super::theme_content_pending_reasons(&pending, &theme).is_empty());
                assert_eq!(super::theme_content_pending_reasons(
                    super::visible_theme_models(&pending, &optimized), &theme,
                ).is_empty(), top_screen == Some(ScreenId::GameDetail));
            }
            let mut old = reference.into_iter();
            reference = models.iter().map(|input| PreparedFrame::prepare_reusing(theme.clone(), input, old.next()).unwrap()).collect::<Vec<_>>();
            let mut actual = <DefaultPlatform as Platform>::Display::default();
            let mut expected = <DefaultPlatform as Platform>::Display::default();
            for frame in &optimized { frame.paint(&mut actual); }
            for frame in &reference { frame.paint(&mut expected); }
            assert_eq!(actual.pixmap().data(), expected.pixmap().data(), "{top_screen:?}");
            if top_screen == Some(ScreenId::GameDetail) {
                assert!(optimized.last().unwrap().is_opaque());
                assert_eq!(optimized[0].pixels().data(), before.unwrap());
                assert!(!optimized[0].input_matches(&models[0]), "covered frame was prepared");
                lower.status.clock = Some("12:35".into());
                lower.status.battery = Some(41);
            } else {
                if top_screen == Some(ScreenId::ContextMenu) { assert!(!optimized.last().unwrap().is_opaque()); }
                assert!(optimized[0].input_matches(&models[0]));
            }
        }
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
    async fn selected_details_idle_images_and_text_match_cold_open_and_drop_on_selection_change() {
        let root = common::constants::ALLIUM_GAMES_DIR.join("integration-details-warm");
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        std::fs::write(root.join("gamelist.xml"), format!("<gameList>{}</gameList>", (0..2).map(|index| format!(
            "<game><path>./Network {index}</path><marquee>./Imgs/Details {index}.logo.png</marquee><desc>{}</desc><developer>Human Entertainment</developer><publisher>ASCII</publisher><genre>Adventure, Horror</genre><releasedate>19970930T000000</releasedate><players>1</players><scraper>Fixture scraper</scraper></game>",
            "A young heroine explores a mansion and searches for her friends. A shadow follows her through the rooms. ".repeat(6)
        )).collect::<String>())).unwrap();
        let database = Database::in_memory().unwrap();
        for index in 0..2 {
            let path = root.join(format!("Network {index}/Discs/Details {index}.chd"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, []).unwrap();
            for (suffix, w, h) in [("orbit", 101, 101), ("hero", 360, 210), ("logo", 220, 96)] {
                let mut art = tiny_skia::Pixmap::new(w, h).unwrap();
                for (at, pixel) in art.data_mut().chunks_exact_mut(4).enumerate() {
                    pixel.copy_from_slice(&[((at + index * 19) % 173) as u8, (at / 31 % 173) as u8, 91, 173]);
                }
                art.save_png(root.join(format!("Imgs/Details {index}.{suffix}.png"))).unwrap();
                if suffix == "orbit" { art.save_png(root.join(format!("Imgs/Details {index}.png"))).unwrap(); }
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
        let pack = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../themes/compact-plus").canonicalize().unwrap();
        let handle = ThemeHandle::prepare(ThemeSnapshot::load(&pack).unwrap(), &bank, 901).unwrap();
        res.insert(Some(handle.clone()));
        let rect = Rect::new(0, 54, 640, 376);
        let views = (
            Recents::load_or_new(rect, res.clone(), None).unwrap(),
            Games::load_or_new(rect, res.clone(), None).unwrap(),
            Apps::load_or_new(rect, res.clone(), None).unwrap(),
            Settings::new(rect, res.clone(), Default::default()).unwrap(),
        );
        let mut app = App::new(Rect::new(0, 0, 640, 480), res.clone(), views, 0,
            DefaultPlatform::new().unwrap().battery().unwrap()).unwrap();
        let mut display = <DefaultPlatform as Platform>::Display::default();
        let style = res.get::<Stylesheet>().clone();
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        common::media::invalidate();
        common::media::set_game_containers((0..2).map(|index| (
            root.join(format!("Network {index}/Discs/Details {index}.chd")),
            root.join(format!("Network {index}")),
        )));
        #[cfg(feature = "perf-bench")]
        let io = common::perf_trace::frontend_io_ops();
        let cold_plan = app.views.0.details_warm_model(&handle, common::theme::pack::ScreenId::Recents).unwrap();
        assert!(cold_plan.details_text.is_none(), "cold metadata produced a text plan");
        #[cfg(feature = "perf-bench")]
        assert_eq!(common::perf_trace::frontend_io_ops(), io, "planning performed cold IO");
        drop(cold_plan);
        // Populate the normal UI-thread cache, including the explicit container aliases.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        for index in 0..2 {
            loop {
                common::media::poll_name_indexes();
                let data = common::media::metadata(&root.join(format!("Network {index}/Discs/Details {index}.chd")));
                if data.developer == "Human Entertainment" { break; }
                assert!(std::time::Instant::now() < deadline, "UI metadata did not become resident");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }
        let mut snapshot_plan = app.views.0.details_warm_model(&handle, common::theme::pack::ScreenId::Recents).unwrap();
        let model = snapshot_plan.details_text.take().unwrap();
        let warmed = std::thread::spawn(move || {
            #[cfg(feature = "perf-bench")]
            let io = common::perf_trace::frontend_io_ops();
            let input = model(snapshot_plan.input).unwrap();
            #[cfg(feature = "perf-bench")]
            assert_eq!(common::perf_trace::frontend_io_ops(), io, "worker performed metadata IO");
            input
        }).join().unwrap();
        assert!(warmed.facts.iter().any(|(key, value)| key == "Developer" && value == "Human Entertainment"));
        app.prepare_theme().unwrap();
        app.draw(&mut display, &style).unwrap();
        let mut work = None;
        let mut saw_details = false;
        let mut saw_text = false;
        let mut saw_hero = false;
        loop {
            // Artwork notifications are consumed by the normal media poll in the real loop.
            app.idle_media_step();
            if app.should_draw() {
                app.prepare_theme().unwrap();
                app.draw(&mut display, &style).unwrap();
            }
            let done = app.prewarm_navigation_step(&mut work, 1).unwrap();
            saw_details |= matches!(work.as_ref().map(|w| &w.stage), Some(NavigationWarmStage::DetailsImages(..)));
            saw_text |= matches!(work.as_ref().map(|w| &w.stage), Some(NavigationWarmStage::DetailsText(_)));
            saw_hero |= matches!(work.as_ref().map(|w| &w.stage), Some(NavigationWarmStage::DetailsHero(_)));
            if done { break; }
            assert!(std::time::Instant::now() < deadline, "Details warm timeout");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(saw_details && saw_text && saw_hero);
        let recents_art = app.theme_models(&handle).unwrap().unwrap()[0].art.clone();
        assert!(recents_art.selected_hero.is_some() && recents_art.selected_logo.is_some()
            && recents_art.selected_cover.is_some(), "recents sizes: {:?}, {:?}, {:?}; requests {}",
            recents_art.selected_hero.as_ref().map(|p| (p.width(), p.height())),
            recents_art.selected_logo.as_ref().map(|p| (p.width(), p.height())),
            recents_art.selected_cover.as_ref().map(|p| (p.width(), p.height())),
            super::super::orbit::test_art_requests());
        app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
        let mut models = app.theme_models(&handle).unwrap().unwrap();
        app.normalize_theme_inputs(&handle, &mut models);
        let details = models.last().unwrap();
        assert_eq!(details.screen, common::theme::pack::ScreenId::GameDetail);
        assert_eq!(details.text, warmed.text);
        assert_eq!(details.metadata, warmed.metadata);
        assert_eq!(details.facts, warmed.facts);
        assert_eq!(details.detail_lines, warmed.detail_lines);
        assert_eq!(details.pagination, warmed.pagination);
        assert_eq!(details.art.selected_cover.as_ref().unwrap().width(), 176);
        assert_eq!(details.art.selected_hero.as_ref().unwrap().width(), 256);
        #[cfg(feature = "perf-bench")]
        {
            common::perf_trace::start_probe(root.join("trace.log").to_str().unwrap());
            common::perf_trace::begin_render();
        }
        let hot = common::theme::render::PreparedFrame::prepare(handle.clone(), details).unwrap();
        #[cfg(feature = "perf-bench")]
        {
            assert_eq!(common::perf_trace::probe_counts()["image_misses"], 0);
            assert_eq!(common::perf_trace::probe_counts()["text_misses"], 0);
            assert_eq!(common::perf_trace::probe_counts()["hero_hits"], 1);
            assert_eq!(common::perf_trace::probe_counts()["hero_misses"], 0);
        }
        let cold_theme = ThemeHandle::prepare(ThemeSnapshot::load(&pack).unwrap(), &bank, 902).unwrap();
        let cold = common::theme::render::PreparedFrame::prepare(cold_theme, details).unwrap();
        assert_eq!(hot.pixels().data(), cold.pixels().data());
        let initial = details.clone();
        app.prepare_theme().unwrap();
        app.draw(&mut display, &style).unwrap();
        let semantic_builds = super::super::entry_list::RECENTS_SEMANTIC_BUILDS.with(std::cell::Cell::get);
        app.prepare_theme().unwrap();
        assert_eq!(super::super::entry_list::RECENTS_SEMANTIC_BUILDS.with(std::cell::Cell::get), semantic_builds,
            "prepared opaque Details must reuse Recents semantics");
        for key in [Key::Down, Key::Right] {
            app.handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
        }
        let changed = app.theme_models(&handle).unwrap().unwrap();
        assert_ne!(changed.last().unwrap().pagination, initial.pagination);
        assert_ne!(changed.last().unwrap().focus, initial.focus);
        for cycle in 0..4 {
            let idle_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while app.idle_media_step() {
                if app.should_draw() {
                    app.prepare_theme().unwrap();
                    app.draw(&mut display, &style).unwrap();
                }
                assert!(std::time::Instant::now() < idle_deadline, "Details idle timeout");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let requests = super::super::orbit::test_art_requests();
            app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.idle_media_step();
            let models = app.theme_models(&handle).unwrap().unwrap();
            assert_eq!(models.len(), 1);
            for (before, after) in [
                (&recents_art.selected_hero, &models[0].art.selected_hero),
                (&recents_art.selected_logo, &models[0].art.selected_logo),
                (&recents_art.selected_cover, &models[0].art.selected_cover),
            ] {
                assert!(std::sync::Arc::ptr_eq(before.as_ref().unwrap(), after.as_ref().unwrap()));
            }
            assert!(theme_content_pending_reasons(&models, &handle).is_empty());
            app.prepare_theme().unwrap();
            assert!(app.content_complete, "first close frame pending in cycle {cycle}");
            app.draw(&mut display, &style).unwrap();
            assert_eq!(super::super::orbit::test_art_requests(), requests, "close requested art");
            let mut fresh_models = models;
            app.normalize_theme_inputs(&handle, &mut fresh_models);
            let fresh = common::theme::render::PreparedFrame::prepare(
                ThemeHandle::prepare(ThemeSnapshot::load(&pack).unwrap(), &bank, 912).unwrap(),
                &fresh_models[0],
            ).unwrap();
            assert_eq!(display.pixmap().data(), fresh.pixels().data(), "close pixels {cycle}");
            assert!(app.parked_details.is_some());
            if cycle < 3 {
                app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
                app.prepare_theme().unwrap();
                assert!(app.content_complete);
                app.draw(&mut display, &style).unwrap();
            }
        }
        app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.prepare_theme().unwrap();
        app.draw(&mut display, &style).unwrap();
        assert!(app.parked_details.is_none());
        let mut reopened = app.theme_models(&handle).unwrap().unwrap();
        app.normalize_theme_inputs(&handle, &mut reopened);
        let reopened = reopened.last().unwrap();
        assert_eq!(reopened.pagination, initial.pagination);
        assert_eq!(reopened.focus, initial.focus);
        let fresh = common::theme::render::PreparedFrame::prepare(
            ThemeHandle::prepare(ThemeSnapshot::load(&pack).unwrap(), &bank, 903).unwrap(), reopened,
        ).unwrap();
        assert_eq!(display.pixmap().data(), fresh.pixels().data());
        app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.prepare_theme().unwrap();
        app.draw(&mut display, &style).unwrap();
        work = None;
        while !matches!(work.as_ref().map(|w: &NavigationWarm| &w.stage), Some(NavigationWarmStage::DetailsImages(..))) {
            assert!(!app.prewarm_navigation_step(&mut work, 1).unwrap());
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(!app.prewarm_navigation_step_with_pending(&mut work, 1, || true).unwrap());
        assert!(matches!(work.as_ref().unwrap().stage, NavigationWarmStage::DetailsImages(..)));
        while !matches!(work.as_ref().map(|w| &w.stage), Some(NavigationWarmStage::DetailsText(_))) {
            assert!(!app.prewarm_navigation_step(&mut work, 1).unwrap());
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(!app.prewarm_navigation_step_with_pending(&mut work, 1, || true).unwrap());
        assert!(matches!(work.as_ref().unwrap().stage, NavigationWarmStage::DetailsText(_)));
        app.prewarm_navigation_step(&mut work, 1).unwrap();
        let stale = work.as_ref().unwrap().media_generation;
        common::media::invalidate();
        assert_ne!(common::media::generation(), stale);
        app.prewarm_navigation_step(&mut work, 1).unwrap();
        assert!(work.is_none(), "media invalidation retained in-flight Details work");
        // A new plan still expires when the selected game changes.
        app.prewarm_navigation_step(&mut work, 1).unwrap();
        assert!(work.is_some());
        app.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.prewarm_navigation_step(&mut work, 1).unwrap();
        assert!(work.is_none(), "selection change retained speculative Details work");
        app.prepare_theme().unwrap();
        app.draw(&mut display, &style).unwrap();
        assert!(app.parked_details.is_some(), "selection changes must preserve reusable Details");
        app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.prepare_theme().unwrap();
        let mut selected = app.theme_models(&handle).unwrap().unwrap();
        app.normalize_theme_inputs(&handle, &mut selected);
        let selected = selected.last().unwrap();
        assert_ne!(selected.text, initial.text);
        let fresh = common::theme::render::PreparedFrame::prepare(
            ThemeHandle::prepare(ThemeSnapshot::load(&pack).unwrap(), &bank, 904).unwrap(), selected,
        ).unwrap();
        assert_eq!(app.theme_scene.as_ref().unwrap().frames.last().unwrap().pixels().data(), fresh.pixels().data());
        for command in [Command::Exec(std::process::Command::new("unused-game-launch")),
            Command::Exec(std::process::Command::new("unused-app-launch")),
            Command::ReloadStylesheet(Box::new(style.clone()), false)]
        {
            app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
            assert!(app.parked_details.is_some());
            app.prepare_command(&Command::Redraw);
            assert!(app.parked_details.is_some());
            app.prepare_command(&command);
            assert!(app.parked_details.is_none(), "{command:?} retained closed Details");
            let released = app.theme_models(&handle).unwrap().unwrap();
            assert!(released.iter().all(|input| input.art.selected_hero.is_none()
                && input.art.selected_logo.is_none() && input.art.selected_cover.is_none()),
                "{command:?} retained artwork sources");
            app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
        }
        app.handle_key_event(KeyEvent::Pressed(Key::B), tx, &mut VecDeque::new()).await.unwrap();
        app.prepare_theme().unwrap();
        assert!(app.parked_details.is_some());
        app.theme_fallback().unwrap();
        assert!(app.parked_details.is_none());
        std::fs::remove_dir_all(root).unwrap();
        common::media::set_game_containers(std::iter::empty());
        common::media::invalidate();
    }

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
                let selected = app.theme_models(&handle).unwrap().unwrap()[0]
                    .selected.get(&common::theme::pack::Binding::Entries).cloned();
                let mut work = None;
                let retired = app.neighbor_graveyard.base.is_some();
                assert!(
                    !app.prewarm_navigation_step_with_pending(&mut work, 1, || true)
                        .unwrap()
                );
                assert!(work.is_none(), "pending input must prevent planning");
                assert_eq!(app.neighbor_graveyard.base.is_some(), retired, "pending input must defer destruction");
                assert!(!app.prewarm_navigation_step(&mut work, 1).unwrap());
                assert!(app.neighbor_graveyard.base.is_none() && app.neighbor_graveyard.siblings.is_empty(),
                    "idle must retire heavy data before building any neighbors");
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
                    let kind = app.navigation_warm_kind(&work);
                    assert!(!app.prewarm_navigation_step_with_pending(&mut work, 1, || true).unwrap());
                    assert_eq!(app.navigation_warm_kind(&work), kind);
                    assert!(app.proof_navigation_scene() == scene, "pending input changed displayed frame");
                    assert!(std::time::Instant::now() < deadline, "{id} warm timeout");
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert!(
                    app.proof_navigation_scene() == scene,
                    "idle warm changed the displayed frame"
                );
                let predicted = app.neighbors.iter().find(|candidate| {
                    candidate.frame.navigation_model().as_ref().is_some_and(|model|
                        model.selected.get(&common::theme::pack::Binding::Entries)
                            != selected.as_ref())
                }).map(|candidate| candidate.frame.navigation_stamp());
                if *id == "compact-plus" { assert_eq!(app.neighbors.len(), 2, "both directions must be resident"); }
                app.handle_key_event(
                    KeyEvent::Pressed(Key::Down),
                    tx.clone(),
                    &mut VecDeque::new(),
                )
                .await
                .unwrap();
                let plans = super::super::entry_list::MEDIA_PLANS.with(std::cell::Cell::get);
                if *id == "compact-plus" { assert!(app.defer_media_step()); }
                app.prepare_theme().unwrap();
                if *id == "compact-plus" {
                    assert!(app.media_deferred, "no-result prepared hit must defer media planning");
                    assert_eq!(super::super::entry_list::MEDIA_PLANS.with(std::cell::Cell::get), plans,
                        "no-result press ran selection planning before presentation");
                    assert!(app.theme_scene.as_ref().unwrap().frames[0].navigation_is_current(predicted.as_ref().unwrap()),
                        "matching press must adopt the full frame");
                    assert!(app.neighbor_graveyard.base.is_some());
                    assert_eq!(app.neighbor_graveyard.siblings.len(), 1);
                }
                let warmed = app.proof_navigation_scene();
                let mut models = app.theme_models(&handle).unwrap().unwrap();
                app.normalize_theme_inputs(&handle, &mut models);
                let fresh =
                    common::theme::render::PreparedFrame::prepare(handle.clone(), &models[0])
                        .unwrap();
                assert!(warmed.0 == fresh.pixels().data(), "{id} pixels");
                assert_eq!(warmed.1, fresh.targets, "{id} targets");
                app.draw(&mut display, &style).unwrap();
                if app.media_deferred {
                    app.idle_media_step();
                    assert!(!app.media_deferred);
                    assert!(super::super::entry_list::MEDIA_PLANS.with(std::cell::Cell::get) > plans,
                        "deferred selection planning must run after presentation");
                }
            }
            if *id == "compact-plus" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while app.idle_media_step() {
                    assert!(std::time::Instant::now() < deadline);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                app.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
                app.views.0.release_theme_art();
                app.invalidate_neighbors();
                let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
                super::super::orbit::proof_art_gate(Some(gate.clone()));
                app.idle_media_step();
                assert!(!super::super::orbit::art_result_queued());
                let completion = super::super::orbit::completion_generation();
                gate.wait();
                while !super::super::orbit::art_result_queued() {
                    assert!(std::time::Instant::now() < deadline);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                app.prepare_theme().unwrap();
                assert_ne!(super::super::orbit::completion_generation(), completion,
                    "result arriving after input and the media poll must be adopted before render");
                let mut models = app.theme_models(&handle).unwrap().unwrap();
                app.normalize_theme_inputs(&handle, &mut models);
                assert!(app.proof_navigation_scene().0
                    == common::theme::render::PreparedFrame::prepare(handle.clone(), &models[0]).unwrap().pixels().data());
                assert_eq!(app.content_complete(), models.iter().all(|input| theme_content_complete(input, &handle)));
                app.draw(&mut display, &style).unwrap();
                while app.idle_media_step() {
                    assert!(std::time::Instant::now() < deadline);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                app.prepare_theme().unwrap();
                app.draw(&mut display, &style).unwrap();
                println!("MEDIANOOP prepared_hit_pre_present_plans=0 post_present_planning=true racing_completion_before_render=true pixels_and_readiness=identical");
                for cycle in 0..3 {
                    for key in [Key::Start, Key::B] {
                        let builds = THEME_MODEL_BUILDS.with(|n| n.get());
                        app.handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut VecDeque::new()).await.unwrap();
                        assert_eq!(THEME_MODEL_BUILDS.with(|n| n.get()), builds,
                            "Details structural transition rebuilt comparison models");
                        assert!(app.theme_dirty);
                        // Updates arriving after input must be reflected at presentation.
                        let mut updated = res.get::<Stylesheet>().clone();
                        updated.status_bar.hide_status_in_launcher = cycle % 2 == 1;
                        res.insert(updated);
                        common::media::invalidate();
                        app.idle_media_step();
                        app.prepare_theme().unwrap();
                        assert_eq!(THEME_MODEL_BUILDS.with(|n| n.get()), builds + 1);
                        let mut models = app.theme_models(&handle).unwrap().unwrap();
                        app.normalize_theme_inputs(&handle, &mut models);
                        assert!(app.theme_scene.as_ref().unwrap().frames.last().unwrap().input_matches(models.last().unwrap()));
                        app.draw(&mut display, &style).unwrap();
                        let mut fresh = <DefaultPlatform as Platform>::Display::default();
                        for model in models {
                            common::theme::render::PreparedFrame::prepare(handle.clone(), &model).unwrap().paint(&mut fresh);
                        }
                        assert_eq!(display.pixmap().data(), fresh.pixmap().data(), "cycle {cycle}, {key:?}");
                        for ignored in [KeyEvent::Released(key), KeyEvent::Pressed(Key::X)] {
                            app.handle_key_event(ignored, tx.clone(), &mut VecDeque::new()).await.unwrap();
                            assert!(!app.theme_dirty && !app.dirty, "ignored input dirtied the scene: {ignored:?}");
                        }
                    }
                }
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
    fn diagnostics_include_every_blocker_and_keep_the_original_first_reason() {
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
            let reasons = theme_content_pending_reasons(&[input.clone()], &theme);
            assert_eq!(reasons.first().copied(), theme_content_pending::<true>(&input, &theme));
            for (required, names) in [
                (demand.cover, &["selected_cover"][..]),
                (demand.hero, &["selected_hero"][..]),
                (demand.logo, &["selected_logo", "logo"][..]),
                (demand.next, &["next_cover", "peek"][..]),
                (demand.rows, &["rows"][..]),
            ] {
                assert_eq!(reasons.iter().any(|(reason, _)| names.contains(reason)), required, "{name}: {reasons:?}");
            }
            input.art.completed_misses = common::theme::frame::ArtMisses {
                selected_cover: true, selected_hero: true, selected_logo: true, next_cover: true,
                entries: std::collections::BTreeSet::from(["missing".into()]),
            };
            assert!(theme_content_pending_reasons(&[input], &theme).is_empty());
        }
    }

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

#[cfg(test)]
mod neighbor_frame_tests;
