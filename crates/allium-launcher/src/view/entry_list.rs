use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::database::Database;
use common::display::Display;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::{Stylesheet, StylesheetColor};
use common::view::{ButtonHint, ButtonHints, Image, ImageMode, ScrollList, View};
use log::{debug, trace};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::consoles::ConsoleMapper;
use crate::entry::display_names::DisplayNames;
use crate::entry::{Entry, Sort};
use crate::view::disc_picker::DiscPicker;
use crate::view::orbit::Orbit;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryListState<S> {
    pub sort: S,
    pub selected: usize,
    #[serde(default)]
    pub selected_path: Option<std::path::PathBuf>,
    pub child: Option<Box<EntryListState<S>>>,
}

pub(crate) struct NavigationWarm {
    pub input: common::theme::frame::FrameInputs,
    artwork: VecDeque<super::orbit::WarmArt>,
    selected: usize,
    first: usize,
    count: usize,
    metadata: bool,
    ink: bool,
    targets: Option<common::theme::art::Targets>,
    pub details_text: Option<Box<dyn FnOnce(common::theme::frame::FrameInputs) -> Result<common::theme::frame::FrameInputs> + Send>>,
}

#[derive(Debug)]
pub struct CoreSelection {
    core: usize,
    cores: Vec<String>,
}

#[derive(Debug, Clone)]
struct SelectedInfo {
    path: std::path::PathBuf,
    generation: u64,
    media_generation: u64,
    adoption_generation: u64,
    platform: String,
    metadata: String,
    metadata_runs: Vec<(String, String)>,
    blurb: String,
}
impl SelectedInfo {
    fn get<'a>(
        slot: &'a mut Option<Self>,
        game: &crate::entry::game::Game,
        platform: &str,
    ) -> &'a Self {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::SelectedInfo);
        let generation = crate::view::details::metadata_generation();
        if slot.as_ref().is_none_or(|info| {
            info.path != game.path
                || info.platform != platform
                || info.generation != generation
                || info.media_generation != common::media::generation()
                || info.adoption_generation != super::orbit::metadata_completion_generation()
        }) {
            let details = crate::view::details::Details::cached(game, platform);
            *slot = Some(Self {
                path: game.path.clone(),
                generation,
                media_generation: common::media::generation(),
                adoption_generation: crate::view::orbit::metadata_completion_generation(),
                platform: platform.to_owned(),
                metadata: details.metadata,
                metadata_runs: details.metadata_runs,
                blurb: details.blurb,
            });
        }
        slot.as_ref().unwrap()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecentsSemanticKey {
    selected: usize,
    path: Option<std::path::PathBuf>,
    first: usize,
    count: usize,
    image_focus: Option<common::theme::frame::Focus>,
    cloud_rows: Vec<bool>,
    revisions: RecentsRevisions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecentsRevisions {
    list_revision: u64,
    media_generation: u64,
    name_generation: u64,
    metadata_generation: u64,
    adoption_generation: u64,
    locale_revision: u64,
    console_revision: u64,
    language: String,
    theme_generation: u64,
    theme_digest: std::sync::Arc<str>,
    screen: common::theme::pack::ScreenId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NormalizedRecentsKey {
    semantic: RecentsSemanticKey,
    artwork: u64,
    completion: u64,
    stylesheet: u64,
    theme: u64,
    size: u64,
    rect: Rect,
    status: common::theme::frame::Status,
}

#[derive(Default)]
struct NormalizedRecentsCache(VecDeque<(NormalizedRecentsKey, Vec<common::theme::frame::FrameInputs>, Option<super::orbit::ArtSnapshot>)>);
impl std::fmt::Debug for NormalizedRecentsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NormalizedRecentsCache").field("len", &self.0.len()).finish()
    }
}

const RECENTS_MODEL_LIMIT: usize = 8;
// ScrollList accepts at most 64 visible rows, so retain two maximum windows.
const RECENTS_ROW_LIMIT: usize = 128;

#[derive(Debug)]
struct RecentsRows {
    revisions: RecentsRevisions,
    rows: VecDeque<(std::path::PathBuf, common::theme::frame::Row)>,
}

struct RecentsSemantic {
    key: RecentsSemanticKey,
    // Raw semantics only: artwork is acquired anew and normalization edits a clone.
    input: common::theme::frame::FrameInputs,
}
impl std::fmt::Debug for RecentsSemantic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentsSemantic").field("key", &self.key).finish_non_exhaustive()
    }
}
#[cfg(test)]
thread_local! {
    pub(crate) static RECENTS_SEMANTIC_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static MEDIA_PLANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MEDIA_CONFIG_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEMANTIC_ROW_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug)]
struct MediaConfig {
    generation: u64,
    digest: std::sync::Arc<str>,
    screen: common::theme::pack::ScreenId,
    stack: [Option<common::theme::pack::ScreenId>; 3],
    count: usize,
    ink: bool,
    tall: bool,
    demand: super::art_queue::Demand,
    lower: super::art_queue::Demand,
    targets: common::theme::art::Targets,
    lower_targets: common::theme::art::Targets,
    visible: super::art_queue::Demand,
    metadata: bool,
    blurb: bool,
}
impl MediaConfig {
    fn new(handle: &common::theme::ThemeHandle, screen: common::theme::pack::ScreenId,
        stack: [Option<common::theme::pack::ScreenId>; 3]) -> Self {
        use common::theme::pack::{Arrangement, Binding, Kind};
        #[cfg(test)]
        MEDIA_CONFIG_BUILDS.with(|n| n.set(n.get() + 1));
        let ink = handle.pack().ink();
        let components: Vec<_> = stack.iter().flatten()
            .flat_map(|screen| &handle.pack().screens[screen].components).collect();
        let image = |binding| components.iter().any(|c| c.kind == Kind::Image && c.binding == Some(binding));
        Self {
            generation: handle.generation(), digest: handle.digest().into(), screen, stack,
            count: crate::theme_adapter::capacity(handle, screen, Binding::Entries), ink,
            tall: handle.pack().alignment.as_ref().is_some_and(|a| a.cover_shape == "tall"),
            demand: super::art_queue::Demand::from_components(components.iter().copied(), ink),
            lower: super::art_queue::Demand::from_components(&handle.pack().screens[&screen].components, ink),
            targets: common::theme::art::Targets::from_components(handle.pack(), components.iter().copied()),
            lower_targets: common::theme::art::Targets::for_screens(handle.pack(), &[screen]),
            visible: super::art_queue::Demand {
                hero: image(Binding::SelectedHero), cover: image(Binding::SelectedCover),
                logo: image(Binding::SelectedLogo) || components.iter().any(|c| c.logo),
                next: image(Binding::NextCover) || components.iter().any(|c| c.layout == Some(Arrangement::Shelf)
                    && (!ink || c.paint.as_ref().is_some_and(|p| p.peek.is_some()))),
                rows: components.iter().any(|c| c.kind == Kind::Entries
                    && (c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0)),
                metadata: false,
            },
            metadata: components.iter().any(|c| c.binding == Some(Binding::SelectedMetadata)),
            blurb: components.iter().any(|c| c.binding == Some(Binding::SelectedBlurb)),
        }
    }
}

#[derive(Debug)]
pub struct EntryList<S>
where
    S: Sort,
{
    rect: Rect,
    res: Resources,
    entries: Vec<Entry>,
    display_names: DisplayNames,
    media_generation: u64,
    list_revision: u64,
    recents_semantic: VecDeque<RecentsSemantic>,
    recents_rows: Option<RecentsRows>,
    normalized_recents: NormalizedRecentsCache,
    sort: S,
    list: ScrollList,
    image: Image,
    menu: Option<ScrollList>,
    menu_entries: Vec<MenuEntry>,
    core: Option<CoreSelection>,
    button_hints: ButtonHints<String>,
    pub child: Option<Box<EntryList<S>>>,
    orbit: Option<Orbit>,
    disc_picker: Option<DiscPicker>,
    details: Option<crate::view::details::Details>,
    semantic_info: Option<SelectedInfo>,
    media_config: Option<MediaConfig>,
    idle_media_key: Option<(usize, usize, usize, u64, bool, bool, bool, super::art_queue::Demand, common::theme::art::Targets)>,
    idle_media_work: VecDeque<super::orbit::WarmArt>,
    idle_lower_work: VecDeque<super::orbit::WarmArt>,
    detail_action: usize,
    image_focus: Option<common::theme::frame::Focus>,
    theme_screen: Option<common::theme::pack::ScreenId>,
}

impl<S> EntryList<S>
where
    S: Sort,
{
    fn image_actions(handle: &common::theme::ThemeHandle, screen: common::theme::pack::ScreenId) -> Vec<common::theme::frame::Focus> {
        handle.pack().screens[&screen].components.iter().filter_map(|c| {
            let action=c.paint.as_ref()?.action.as_deref()?;
            (matches!(c.kind,common::theme::pack::Kind::Image|common::theme::pack::Kind::AssetImage) && action=="play").then(|| common::theme::frame::Focus {component:c.id.clone(),item:Some(action.into())})
        }).collect()
    }
    fn detail_targets(handle: &common::theme::ThemeHandle) -> Vec<common::theme::frame::Focus> {
        use common::theme::{frame::Focus,pack::{ScreenId,Binding}};
        let component=crate::theme_adapter::component_id(handle,ScreenId::GameDetail,Binding::Actions);
        let mut targets=vec![Focus{component:component.clone(),item:Some("play".into())},Focus{component,item:Some("back".into())}];
        targets.extend(Self::image_actions(handle,ScreenId::GameDetail));targets
    }
    pub fn new(rect: Rect, res: Resources, sort: S) -> Result<Self> {
        let Rect { x, y, w, .. } = rect;

        let styles = res.get::<Stylesheet>();

        let mut button_hints = {
            let locale = res.get::<Locale>();
            let mut hints = vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::A,
                locale.t("button-select"),
                Alignment::Right,
            )];
            if S::HAS_BUTTON_HINTS {
                hints.push(ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::Y,
                    sort.button_hint(&locale),
                    Alignment::Right,
                ));
            }
            ButtonHints::new(res.clone(), vec![], hints)
        };

        let button_hints_rect = button_hints.bounding_box(&styles);
        let list_height = (button_hints_rect.y - y) as u32;

        let list = ScrollList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                if styles.games.boxart_width > 0 {
                    w - styles.games.boxart_width - styles.ui.margin_x as u32 * 3
                } else {
                    w - styles.ui.margin_x as u32 * 2
                },
                list_height,
            ),
            Vec::new(),
            Alignment::Left,
            res.get::<Stylesheet>().ui.ui_font.size + styles.ui.padding_y as u32,
        );

        let (image_y, image_h) = if styles.games.boxart_underlay {
            (0, (y + rect.h as i32) as u32)
        } else {
            (y, list_height)
        };

        let image_margin = if styles.games.boxart_underlay {
            0
        } else {
            styles.ui.margin_x
        };

        let mut image = Image::empty(
            Rect::new(
                x + w as i32 - styles.games.boxart_width as i32 - image_margin,
                image_y,
                styles.games.boxart_width,
                image_h,
            ),
            ImageMode::Contain,
        );
        image.set_border_radius(
            (styles.ui.margin_x as f32 * styles.games.boxart_border_radius) as u32,
        );
        image.set_alignment(Alignment::Right);

        drop(styles);

        let mut this = Self {
            rect,
            res,
            entries: vec![],
            display_names: DisplayNames::default(),
            media_generation: common::media::generation(),
            list_revision: 0,
            recents_semantic: VecDeque::new(),
            recents_rows: None,
            normalized_recents: NormalizedRecentsCache::default(),
            sort,
            list,
            image,
            menu: None,
            menu_entries: vec![],
            core: None,
            button_hints,
            child: None,
            orbit: None,
            disc_picker: None,
            details: None,
            semantic_info: None,
            media_config: None,
            idle_media_key: None,
            idle_media_work: VecDeque::new(),
            idle_lower_work: VecDeque::new(),
            detail_action: 0,
            image_focus: None,
            theme_screen: None,
        };

        this.load_entries()?;

        Ok(this)
    }

    pub fn enable_orbit(&mut self) {
        self.orbit = Some(Orbit::new());
        self.orbit.as_mut().unwrap().set_display_names(self.display_names.clone());
        if let Some(child) = self.child.as_mut() {
            child.enable_orbit();
        } else {
            self.prepare_orbit();
        }
    }

    pub(crate) fn disc_picker_active(&self) -> bool {
        self.disc_picker.is_some()
            || self
                .child
                .as_ref()
                .is_some_and(|child| child.disc_picker_active())
    }

    pub fn interaction_active(&self) -> bool {
        // Core selection is owned by the menu; its saved state outlives cancellation.
        self.menu.is_some() || self.child.as_ref().is_some_and(|c| c.interaction_active())
    }

    pub(crate) fn details_active(&self) -> bool {
        self.details.is_some() || self.child.as_ref().is_some_and(|c| c.details_active())
    }

    #[cfg(test)]
    pub(crate) fn orbit_cache_counts(&self) -> (usize, usize) {
        let own = self.orbit.as_ref().map_or(0, Orbit::cache_entries);
        if let Some(child) = &self.child {
            let (total, ancestors) = child.orbit_cache_counts();
            (own + total, own + ancestors)
        } else {
            (own, 0)
        }
    }

    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        self.child.is_none()
            && self.menu.is_none()
            && self.details.is_none()
            && self.disc_picker.is_none()
    }

    fn prepare_orbit(&mut self) {
        if !matches!(self.entries.get(self.list.selected()), Some(Entry::Game(_))) {
            self.semantic_info = None;
        }
        if self.child.is_some() {
            self.semantic_info = None;
            if let Some(orbit) = self.orbit.as_mut() {
                orbit.release_art();
            }
            return;
        }
        let themed = crate::theme_adapter::active(&self.res) && self.disc_picker.is_none();
        if let Some(orbit) = self.orbit.as_mut() {
            if themed {
                orbit.defer_legacy();
            } else {
                orbit.prepare(&self.entries, self.list.selected());
            }
        }
    }

    fn media_configuration(&mut self, handle: &common::theme::ThemeHandle,
        screen: common::theme::pack::ScreenId) -> &MediaConfig {
        use common::theme::pack::ScreenId;
        let stack = [Some(if self.details.is_some() { ScreenId::GameDetail } else { screen }),
            self.menu.as_ref().map(|_| ScreenId::ContextMenu),
            self.disc_picker.as_ref().map(|_| ScreenId::DiscPicker)];
        if self.media_config.as_ref().is_none_or(|config| config.generation != handle.generation()
            || config.digest.as_ref() != handle.digest() || config.screen != screen || config.stack != stack)
        {
            self.media_config = Some(MediaConfig::new(handle, screen, stack));
        }
        self.media_config.as_ref().unwrap()
    }

    pub(crate) fn idle_media_step(
        &mut self,
        handle: &common::theme::ThemeHandle,
        screen: common::theme::pack::ScreenId,
    ) -> bool {
        if let Some(child) = self.child.as_mut() {
            return child.idle_media_step(handle, screen);
        }
        if self.entries.is_empty() {
            return false;
        }
        let selected = self.list.selected();
        let first = self.list.window_start();
        let config = self.media_configuration(handle, screen);
        let neighbours = config.stack[0] != Some(common::theme::pack::ScreenId::GameDetail);
        let (count, ink, tall, demand, targets, lower_targets, lower, visible, metadata, blurb) =
            (config.count, config.ink, config.tall, config.demand, config.targets, config.lower_targets,
                config.lower, config.visible, config.metadata, config.blurb);
        if self.orbit.get_or_insert_with(Orbit::new).set_art_theme(handle) {
            self.idle_media_key = None;
            self.idle_media_work.clear();
            self.idle_lower_work.clear();
        }
        let key = (
            selected,
            first,
            count,
            common::media::generation(),
            ink,
            tall,
            neighbours,
            demand,
            targets,
        );
        if self.idle_media_key != Some(key) {
            #[cfg(test)]
            MEDIA_PLANS.with(|n| n.set(n.get() + 1));
            let direction = self.idle_media_key.map_or(1, |old| {
                let forward = (selected + self.entries.len() - old.0.min(self.entries.len() - 1))
                    % self.entries.len();
                if forward > self.entries.len() / 2 {
                    -1
                } else {
                    1
                }
            });
            self.idle_media_key = Some(key);
            self.idle_media_work =
                super::art_queue::plan(&self.entries, selected, first, count, direction, demand, neighbours);
            self.idle_lower_work.clear();
            if self.details.is_some() {
                self.idle_lower_work = super::art_queue::plan(
                    &self.entries, selected, first, count, direction, lower, true);
                self.idle_lower_work.retain(|task| {
                    let path = task.entry().path();
                    path == self.entries[selected].path()
                        || (lower.rows && self.entries.iter().skip(first).take(count)
                            .any(|entry| entry.path() == path))
                        || (lower.next && super::orbit::peek_game(&self.entries, first, count)
                            .is_some_and(|i| self.entries[i].path() == path))
                });
            }
            self.orbit.get_or_insert_with(Orbit::new).cancel_selection();
        }
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        orbit.set_ink_art(ink);
        orbit.set_cover_shape(tall);
        orbit.set_art_targets(targets);
        orbit.protect_covered(self.entries.get(selected).filter(|_| self.details.is_some()), lower_targets);
        orbit.protect_window(
            &self.entries,
            selected,
            first,
            count,
            demand.rows,
            demand.next,
        );
        let visible_art = |mut art: common::theme::frame::Art| {
            if !visible.hero {
                art.selected_hero = None;
                art.completed_misses.selected_hero = false;
            }
            if !visible.cover {
                art.selected_cover = None;
                art.completed_misses.selected_cover = false;
            }
            if !visible.logo {
                art.selected_logo = None;
                art.completed_misses.selected_logo = false;
            }
            if !visible.next {
                art.next_cover = None;
                art.completed_misses.next_cover = false;
            }
            if !visible.rows {
                art.entries.clear();
                art.completed_misses.entries.clear();
            }
            art
        };
        let has_result = super::orbit::art_result_queued();
        let before = has_result.then(|| visible_art(orbit.theme_art(&self.entries, selected, first, count)));
        let lower_before = self.details.as_ref().filter(|_| has_result).map(|_| {
            orbit.theme_art_for_targets(&self.entries, selected, first, count, lower_targets)
        });
        // A result arriving after the probe stays queued for the next adoption pass.
        // Submission must not consume it without the corresponding before snapshot.
        let mut completed = has_result && orbit.adopt_art_result();
        orbit.submit_art_step_for_targets(&mut self.idle_media_work, targets);
        // Visible Details work takes priority; lower variants also serve transparent Details packs.
        if self.idle_media_work.is_empty() && !orbit.art_pending() && !self.idle_lower_work.is_empty() {
            completed |= has_result && orbit.adopt_art_result();
            orbit.submit_art_step_for_targets(&mut self.idle_lower_work, lower_targets);
        }
        let pending = !self.idle_media_work.is_empty() || !self.idle_lower_work.is_empty() || orbit.art_pending();
        if !completed {
            return pending;
        }
        let before = before.unwrap();
        let after = visible_art(orbit.theme_art(&self.entries, selected, first, count));
        if common::perf_trace::recording() {
            common::perf_trace::artwork_readiness(&before, &after);
        }
        let lower_after = lower_before.as_ref().map(|_| {
            orbit.theme_art_for_targets(&self.entries, selected, first, count, lower_targets)
        });
        let art_changed = before != after || lower_before != lower_after;
        let readiness_changed = before.completed_misses != after.completed_misses
            || lower_before.as_ref().map(|art| &art.completed_misses)
                != lower_after.as_ref().map(|art| &art.completed_misses);
        let mut metadata_changed = false;
        if let Some(Entry::Game(game)) = self.entries.get(selected) {
            if metadata || blurb {
                let before = self.semantic_info.take();
                let platform = self
                    .res
                    .get::<ConsoleMapper>()
                    .get_console(&game.path)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let after = SelectedInfo::get(&mut self.semantic_info, game, &platform);
                metadata_changed = before.as_ref().is_none_or(|before| {
                    (metadata
                        && (before.metadata != after.metadata
                            || (ink && before.metadata_runs != after.metadata_runs)))
                        || (blurb && before.blurb != after.blurb)
                });
            } else {
                self.semantic_info = None;
            }
        }
        let changed = art_changed || metadata_changed;
        if changed || readiness_changed {
            super::orbit::visible_completion();
        }
        if changed {
            self.set_should_draw();
        }
        pending || changed || readiness_changed
    }

    pub(crate) fn navigation_warm_ready(&self) -> bool {
        self.menu.is_none()
            && self.core.is_none()
            && self.details.is_none()
            && self.disc_picker.is_none()
            && self
                .child
                .as_ref()
                .is_none_or(|child| child.navigation_warm_ready())
    }

    pub(crate) fn navigation_warm_art_step(&mut self, work: &mut NavigationWarm) -> bool {
        if let Some(child) = self.child.as_mut() {
            return child.navigation_warm_art_step(work);
        }
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        if let Some(targets) = work.targets {
            orbit.warm_art_step_for_targets(&mut work.artwork, targets);
        } else {
            orbit.warm_art_step(&mut work.artwork);
        }
        if !work.artwork.is_empty() || orbit.art_pending() {
            return false;
        }
        work.input.art = if let Some(targets) = work.targets {
            orbit.theme_art_for_targets(&self.entries, work.selected, work.first, work.count, targets)
        } else { orbit.theme_art(&self.entries, work.selected, work.first, work.count) };
        if let Some(Entry::Game(game)) = self.entries.get(work.selected)
            && work.metadata
        {
            use common::theme::pack::Binding;
            let platform = self
                .res
                .get::<ConsoleMapper>()
                .get_console(&game.path)
                .map(|console| console.name.clone())
                .unwrap_or_default();
            let info = SelectedInfo::get(&mut self.semantic_info, game, &platform);
            work.input
                .text
                .insert(Binding::SelectedMetadata, info.metadata.clone());
            work.input
                .text
                .insert(Binding::SelectedBlurb, info.blurb.clone());
            if work.ink {
                work.input.metadata = info.metadata_runs.clone();
            }
        }
        true
    }

    pub(crate) fn details_warm_model(&mut self, handle: &common::theme::ThemeHandle, _screen: common::theme::pack::ScreenId) -> Option<NavigationWarm> {
        use common::theme::{frame::FrameInputs, pack::ScreenId};
        if let Some(child) = self.child.as_mut() { return child.details_warm_model(handle, _screen); }
        if !handle.pack().ink() || !self.root_tab_navigation_ready() { return None; }
        let selected = self.list.selected();
        let Some(Entry::Game(game)) = self.entries.get(selected) else { return None; };
        let components = &handle.pack().screens.get(&ScreenId::GameDetail)?.components;
        let mut demand = super::art_queue::Demand::from_components(components, true);
        demand.metadata = false;
        let mut artwork = super::art_queue::plan(&self.entries, selected, selected, 1, 1, demand, true);
        artwork.retain(|task| task.entry().path() == self.entries[selected].path());
        let details_text = components.iter().any(|c| c.binding == Some(common::theme::pack::Binding::SelectedBlurb)
            && c.paint.as_ref().is_some_and(|p| p.section_style.is_some())).then(||
                crate::view::details::DetailsSnapshot::cached(game)).flatten().map(|snapshot| {
            let game = game.clone();
            let title = self.entries[selected].name().to_owned();
            let platform = self.res.get::<ConsoleMapper>().get_console(&game.path)
                .map(|c| c.name.clone()).unwrap_or_default();
            let theme = handle.clone();
            let focus = Self::detail_targets(handle)[0].clone();
            Box::new(move |base: FrameInputs| {
                // Section pagination excludes history, so no UI-thread database query is needed.
                let mut details = crate::view::details::Details::from_snapshot(&game, &platform, snapshot);
                details.reflow(&theme);
                let mut input = details.frame_input(&theme, &title, focus);
                input.art = base.art;
                input.status = base.status;
                input.navigation = base.navigation;
                input.selected = base.selected;
                Ok(input)
            }) as Box<dyn FnOnce(FrameInputs) -> Result<FrameInputs> + Send>
        });
        Some(NavigationWarm {
            input: FrameInputs::new(ScreenId::GameDetail), artwork, selected, first: selected, count: 1,
            metadata: false, ink: true, details_text,
            targets: Some(common::theme::art::Targets::for_screens(handle.pack(), &[ScreenId::GameDetail])),
        })
    }

    pub(crate) fn navigation_warm_model(
        &mut self,
        handle: &common::theme::ThemeHandle,
        direction: isize,
        mut input: common::theme::frame::FrameInputs,
    ) -> Result<Option<NavigationWarm>> {
        use common::theme::pack::Binding;
        if let Some(child) = self.child.as_mut() {
            return child.navigation_warm_model(handle, direction, input);
        }
        if self.entries.len() < 2 || !self.root_tab_navigation_ready() {
            return Ok(None);
        }
        let count = crate::theme_adapter::capacity(handle, input.screen, Binding::Entries);
        let Some((selected, first)) = self.list.navigation_selection(direction) else { return Ok(None); };
        input = self.recents_theme_model(input.screen, handle, selected, first, count);
        let metadata = handle.pack().screens[&input.screen].components.iter().any(|c| {
            matches!(c.binding, Some(Binding::SelectedMetadata | Binding::SelectedBlurb))
        });
        let ink = handle.pack().ink();
        let demand = super::art_queue::Demand::from_components(
            &handle.pack().screens[&input.screen].components,
            handle.pack().ink(),
        );
        let mut artwork =
            super::art_queue::plan(&self.entries, selected, first, count, direction, demand, true);
        // Warm only art used by the predicted frame; App prepares one raster neighbor.
        artwork.retain(|task| {
            let path = task.entry().path();
            path == self.entries[selected].path()
                || (demand.rows && self.entries[first..(first + count).min(self.entries.len())]
                    .iter().any(|entry| entry.path() == path))
                || (demand.next && super::orbit::peek_game(&self.entries, first, count)
                    .is_some_and(|i| self.entries[i].path() == path))
        });
        Ok(Some(NavigationWarm {
            input,
            artwork,
            selected,
            first,
            count,
            metadata,
            ink,
            targets: None,
            details_text: None,
        }))
    }

    pub(crate) fn navigation_frame_model(
        &mut self, handle: &common::theme::ThemeHandle, direction: isize,
    ) -> Option<common::theme::frame::FrameInputs> {
        use common::theme::pack::{Binding, ScreenId};
        if !self.root_tab_navigation_ready() || self.entries.len() < 2 { return None; }
        let (selected, first) = self.list.navigation_selection(direction)?;
        let count = crate::theme_adapter::capacity(handle, ScreenId::Recents, Binding::Entries);
        let mut input = self.recents_theme_model(ScreenId::Recents, handle, selected, first, count);
        if let Some(orbit) = &self.orbit {
            input.art = orbit.theme_art(&self.entries, selected, first, count);
        }
        Some(input)
    }

    fn base_theme_model(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
        selected: usize,
        first: usize,
        count: usize,
    ) -> common::theme::frame::FrameInputs {
        let _semantic = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::SemanticModel));
        self.build_theme_model(screen, handle, selected, first, count, None)
    }

    fn semantic_row(res: &Resources, entry: &Entry, names: &DisplayNames) -> common::theme::frame::Row {
        #[cfg(test)]
        SEMANTIC_ROW_BUILDS.with(|count| count.set(count.get() + 1));
        common::theme::frame::Row {
            subtitle: res.get::<ConsoleMapper>().get_console(entry.path()).map(|c| c.name.clone()).unwrap_or_default(),
            id: crate::theme_adapter::entry_id(entry.path()),
            label: names.name(entry).into(),
            value: if crate::network_runtime::download_id(entry.path()).is_some() {
                "Cloud".into()
            } else {
                String::new()
            },
            enabled: true,
        }
    }

    fn build_theme_model(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
        selected: usize,
        first: usize,
        count: usize,
        rows: Option<Vec<common::theme::frame::Row>>,
    ) -> common::theme::frame::FrameInputs {
        use common::theme::{frame::{Focus, FrameInputs, Hint}, pack::{Binding, ScreenId}};
        #[cfg(test)]
        if screen == ScreenId::Recents {
            RECENTS_SEMANTIC_BUILDS.with(|count| count.set(count.get() + 1));
        }
        let mut input = FrameInputs::new(screen);
        input.pagination = Some(common::theme::frame::Pagination {page: selected / count.max(1) + 1, total: self.entries.len().max(1).div_ceil(count.max(1))});
        let locale = self.res.get::<Locale>();
        let title = match screen {
            ScreenId::Apps => locale.t("tab-apps"),
            ScreenId::Recents => locale.t("tab-recents"),
            ScreenId::SearchResults => locale.t("sort-search"),
            _ => self.sort.directory_title().map(str::to_owned).unwrap_or_else(|| locale.t("tab-games")),
        };
        input.text.insert(Binding::Title, title);
        input.rows.insert(
            Binding::Entries,
            rows.unwrap_or_else(|| self.entries.iter().skip(first).take(count)
                .map(|entry| Self::semantic_row(&self.res, entry, &self.display_names)).collect()),
        );
        if let Some(entry) = self.entries.get(selected) {
            let selected_row = selected.checked_sub(first)
                .and_then(|index| input.rows[&Binding::Entries].get(index));
            let id = selected_row.map(|row| row.id.clone())
                .unwrap_or_else(|| crate::theme_adapter::entry_id(entry.path()));
            let platform = selected_row.map(|row| row.subtitle.clone()).unwrap_or_else(|| {
                self.res.get::<ConsoleMapper>().get_console(entry.path())
                    .map(|c| c.name.clone()).unwrap_or_default()
            });
            input.focus = Focus {
                component: crate::theme_adapter::component_id(handle, screen, Binding::Entries),
                item: Some(id.clone()),
            };
            input.selected.insert(Binding::Entries, id);
            input
                .text
                .insert(Binding::SelectedTitle, self.display_names.name(entry).into());
            if matches!(entry, Entry::Game(_)) {
                input.hints = vec![
                    Hint {
                        button: "A".into(),
                        label: "Play".into(),
                    },
                    Hint {
                        button: "START".into(),
                        label: "Details".into(),
                    },
                    Hint {
                        button: "X".into(),
                        label: "Search".into(),
                    },
                    Hint {
                        button: "SELECT".into(),
                        label: "Context".into(),
                    },
                    Hint {
                        button: "B".into(),
                        label: "Back".into(),
                    },
                    Hint {
                        button: "L/R".into(),
                        label: "Mode".into(),
                    },
                ];
                if S::HAS_BUTTON_HINTS {
                    input.hints.push(Hint {
                        button: "Y".into(),
                        label: "Sort".into(),
                    });
                }
            }
            let needs_info = handle.pack().screens[&screen].components.iter().any(|c| {
                matches!(
                    c.binding,
                    Some(Binding::SelectedMetadata | Binding::SelectedBlurb)
                )
            });
            if let Entry::Game(game) = entry
                && needs_info
            {
                let info = SelectedInfo::get(&mut self.semantic_info, game, &platform);
                if handle.pack().ink() {
                    input.metadata = info.metadata_runs.clone();
                }
                input
                    .text
                    .insert(Binding::SelectedMetadata, info.metadata.clone());
                input
                    .text
                    .insert(Binding::SelectedBlurb, info.blurb.clone());
            } else {
                let metadata = if let Entry::Directory(directory) = entry {
                    directory.game_count.map(|count| {
                        let mut args = std::collections::HashMap::new();
                        args.insert("count".into(), common::locale::LocaleFluentValue::from(count.to_string()));
                        self.res.get::<Locale>().ta("library-game-count", &args)
                    }).unwrap_or_default()
                } else { String::new() };
                if !metadata.is_empty() { input.metadata = vec![(String::new(), metadata.clone())]; }
                input.text.insert(Binding::SelectedMetadata, metadata);
                input.text.insert(Binding::SelectedBlurb, String::new());
            }
        }
        if !matches!(self.entries.get(selected), Some(Entry::Game(_))) {
            self.semantic_info = None;
        }
        if let Some(focus) = &self.image_focus {
            if Self::image_actions(handle, screen).contains(focus) { input.focus = focus.clone(); }
        }
        input
    }

    fn matching_media_config(&self, handle: &common::theme::ThemeHandle,
        screen: common::theme::pack::ScreenId,
    ) -> Option<&MediaConfig> {
        self.media_config.as_ref().filter(|config| config.screen == screen
            && config.generation == handle.generation() && config.digest.as_ref() == handle.digest())
    }

    fn recents_semantic_key(&self, screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle, selected: usize, first: usize, count: usize,
        path: Option<std::path::PathBuf>,
    ) -> RecentsSemanticKey {
        let revisions = RecentsRevisions {
            list_revision: self.list_revision,
            media_generation: common::media::generation(),
            name_generation: common::media::name_generation(),
            metadata_generation: crate::view::details::metadata_generation(),
            adoption_generation: super::orbit::metadata_completion_generation(),
            locale_revision: self.res.revision::<Locale>(),
            console_revision: self.res.revision::<ConsoleMapper>(),
            language: self.res.get::<Locale>().language(),
            theme_generation: handle.generation(),
            theme_digest: self.matching_media_config(handle, screen)
                .map_or_else(|| handle.digest().into(), |config| config.digest.clone()),
            screen,
        };
        RecentsSemanticKey {
            selected, path,
            first, count, image_focus: self.image_focus.clone(),
            cloud_rows: self.entries.iter().skip(first).take(count)
                .map(|entry| crate::network_runtime::has_download(entry.path())).collect(),
            revisions,
        }
    }

    pub(crate) fn normalized_recents_key(&self, handle: &common::theme::ThemeHandle,
        direction: Option<isize>, status: &common::theme::frame::Status,
    ) -> Option<NormalizedRecentsKey> {
        let _stage = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::FrameModelKey));
        use common::theme::pack::{Binding, ScreenId};
        if !self.root_tab_navigation_ready() || self.core.is_some()
            || self.theme_screen != Some(ScreenId::Recents) { return None; }
        let (selected, first) = match direction {
            Some(direction) => self.list.navigation_selection(direction)?,
            None => (self.list.selected(), self.list.window_start()),
        };
        // Entry replacement and reordering bump list_revision, so the normalized
        // cache can identify the selected path by revision and index.
        Some(NormalizedRecentsKey {
            semantic: self.recents_semantic_key(ScreenId::Recents, handle, selected, first,
                self.matching_media_config(handle, ScreenId::Recents).map_or_else(
                    || crate::theme_adapter::capacity(handle, ScreenId::Recents, Binding::Entries), |config| config.count), None),
            artwork: super::orbit::art_cache_revision(), completion: super::orbit::completion_generation(),
            stylesheet: self.res.revision::<Stylesheet>(), theme: self.res.revision::<Option<common::theme::ThemeHandle>>(),
            size: self.res.revision::<common::geom::Size>(), rect: self.rect, status: status.clone(),
        })
    }

    pub(crate) fn take_normalized_recents(&mut self, key: &NormalizedRecentsKey,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        let (mut models, artwork) = {
            let _stage = common::perf_trace::recording()
                .then(|| common::perf_trace::stage(common::perf_trace::Stage::FrameModelLookup));
            let Some(index) = self.normalized_recents.0.iter().position(|(cached, _, _)| cached == key) else { return Ok(None); };
            let (_, models, artwork) = self.normalized_recents.0.remove(index).unwrap();
            (models, artwork)
        };
        let _stage = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::FrameModelArtwork));
        self.apply_theme_art_snapshot(handle, &mut models[0], artwork.as_ref())?;
        // Artwork refresh can invalidate the proof even without a queued worker result.
        if key.artwork != super::orbit::art_cache_revision()
            || key.completion != super::orbit::completion_generation() { return Ok(None); }
        Ok(Some(models))
    }

    pub(crate) fn remember_normalized_recents(&mut self, key: NormalizedRecentsKey,
        mut models: Vec<common::theme::frame::FrameInputs>, handle: &common::theme::ThemeHandle,
    ) {
        // Retain no source pixels: eviction, weak artwork proofs and the art budget stay unchanged.
        if models.len() != 1 { return; }
        let artwork = self.orbit.as_ref().map(|orbit| orbit.art_snapshot(handle, &models[0].art));
        models[0].art = Default::default();
        self.normalized_recents.0.retain(|(cached, _, _)| cached != &key);
        self.normalized_recents.0.push_back((key, models, artwork));
        while self.normalized_recents.0.len() > 2 { self.normalized_recents.0.pop_front(); }
    }

    fn recents_theme_model(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
        selected: usize,
        first: usize,
        count: usize,
    ) -> common::theme::frame::FrameInputs {
        if screen != common::theme::pack::ScreenId::Recents {
            self.recents_semantic.clear();
            self.recents_rows = None;
            return self.base_theme_model(screen, handle, selected, first, count);
        }
        // Modal underlays still bypass both caches. Cache only raw semantics;
        // artwork and normalization (including live status) run after this call.
        let key = self.recents_semantic_key(screen, handle, selected, first, count,
            self.entries.get(selected).map(|entry| entry.path().to_path_buf()));
        let revisions = key.revisions.clone();
        if self.menu.is_some() || self.disc_picker.is_some() {
            let mut input = self.base_theme_model(screen, handle, selected, first, count);
            input.raster_revision = Some(format!("{:?}", revisions));
            return input;
        }
        let _semantic = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::SemanticModel));
        if let Some(index) = self.recents_semantic.iter().position(|cached| cached.key == key) {
            let cached = self.recents_semantic.remove(index).unwrap();
            let input = cached.input.clone();
            self.recents_semantic.push_back(cached);
            if !matches!(self.entries.get(selected), Some(Entry::Game(_))) {
                self.semantic_info = None;
            }
            return input;
        }
        if self.recents_rows.as_ref().is_none_or(|cached| cached.revisions != revisions) {
            self.recents_rows = Some(RecentsRows { revisions, rows: VecDeque::new() });
        }
        let cache = &mut self.recents_rows.as_mut().unwrap().rows;
        let rows = self.entries.iter().skip(first).take(count).zip(&key.cloud_rows)
            .map(|(entry, &cloud)| {
                let row = if let Some(index) = cache.iter().position(|(path, row)| {
                    path == entry.path() && row.value == if cloud { "Cloud" } else { "" }
                }) {
                    cache.remove(index).unwrap().1
                } else {
                    Self::semantic_row(&self.res, entry, &self.display_names)
                };
                let output = row.clone();
                cache.push_back((entry.path().to_path_buf(), row));
                while cache.len() > RECENTS_ROW_LIMIT { cache.pop_front(); }
                output
            }).collect();
        let mut input = self.build_theme_model(screen, handle, selected, first, count, Some(rows));
        input.raster_revision = Some(format!("{:?}", key.revisions));
        self.recents_semantic.push_back(RecentsSemantic { key, input: input.clone() });
        while self.recents_semantic.len() > RECENTS_MODEL_LIMIT { self.recents_semantic.pop_front(); }
        input
    }

    pub(crate) fn theme_models(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
        self.theme_models_covered(screen, handle, false)
    }

    fn apply_theme_art(&mut self, handle: &common::theme::ThemeHandle,
        input: &mut common::theme::frame::FrameInputs,
    ) -> Result<()> {
        self.apply_theme_art_snapshot(handle, input, None)
    }

    fn apply_theme_art_snapshot(&mut self, handle: &common::theme::ThemeHandle,
        input: &mut common::theme::frame::FrameInputs, artwork: Option<&super::orbit::ArtSnapshot>,
    ) -> Result<()> {
        use common::theme::pack::ScreenId;
        let config_stage = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::FrameArtConfig));
        let screen = input.screen;
        let config = self.media_configuration(handle, screen);
        let (count, ink, tall, targets, lower_targets) = (config.count, config.ink, config.tall,
            config.targets, if config.stack[0] == Some(ScreenId::GameDetail) { config.lower_targets } else { config.targets });
        self.list.set_visible_count_override(Some(count))?;
        let first = self.list.window_start();
        let selected = self.list.selected();
        if self.theme_screen != Some(screen) {self.image_focus=None;}
        self.theme_screen=Some(screen);
        let needs_art = self.details.is_some()
            || self.menu.is_some()
            || handle.pack().screens[&screen].components.iter().any(|c| {
                c.kind == common::theme::pack::Kind::Image || c.logo || c.art_size.unwrap_or(0) > 0
            });
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        if orbit.set_art_theme(handle) {
            self.idle_media_key = None;
            self.idle_media_work.clear();
            self.idle_lower_work.clear();
        }
        orbit.set_ink_art(ink);
        orbit.set_cover_shape(tall);
        orbit.set_art_targets(targets);
        drop(config_stage);
        let protection_stage = common::perf_trace::recording()
            .then(|| common::perf_trace::stage(common::perf_trace::Stage::FrameArtProtection));
        orbit.protect_covered(self.entries.get(selected).filter(|_| self.details.is_some()), lower_targets);
        if let Some(entry) = self.entries.get(selected) {
            orbit.protect_selected(entry);
        }
        drop(protection_stage);
        if needs_art {
            orbit.prepare_window(&self.entries, selected, first, count);
            input.art = artwork.and_then(|snapshot| snapshot.restore(orbit, handle, lower_targets))
                .unwrap_or_else(|| orbit.theme_art_for_targets(&self.entries, selected, first, count, lower_targets));
        }
        Ok(())
    }

    pub(crate) fn theme_models_covered(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
        _covered: bool,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::Layout);
        use common::theme::{
            frame::{Focus, FrameInputs, Hint, Row},
            pack::{Binding, ScreenId},
        };
        if let Some(child) = self.child.as_mut() {
            self.semantic_info = None;
            if let Some(orbit) = self.orbit.as_mut() {
                orbit.release_art();
            }
            return child.theme_models(screen, handle);
        }
        let count = self.media_configuration(handle, screen).count;
        self.list.set_visible_count_override(Some(count))?;
        let first = self.list.window_start();
        let selected = self.list.selected();
        if self.theme_screen != Some(screen) { self.image_focus = None; }
        self.theme_screen = Some(screen);
        let mut input = self.recents_theme_model(screen, handle, selected, first, count);
        self.apply_theme_art(handle, &mut input)?;
        let orbit = self.orbit.as_mut().unwrap();
        let mut models = vec![input];
        if let Some(details) = &self.details {
            let focus = if handle.pack().ink() {
                let targets = Self::detail_targets(handle);
                targets[self.detail_action % targets.len()].clone()
            } else { Default::default() };
            let mut detail = details.frame_input(handle, self.entries.get(selected).map_or("", Entry::name), focus);
            detail.art = orbit.theme_art(&self.entries, selected, first, count);
            models.push(detail);
        }
        if let Some(menu) = self.menu.as_mut() {
            let count =
                crate::theme_adapter::capacity(handle, ScreenId::ContextMenu, Binding::Entries);
            menu.set_visible_count_override(Some(count))?;
            let locale = self.res.get::<Locale>();
            let mut modal = FrameInputs::new(ScreenId::ContextMenu);
            if let Some(model) = models.last() {
                modal.art.selected_logo = model.art.selected_logo.clone();
                modal.art.selected_cover = model.art.selected_cover.clone();
                modal.art.completed_misses.selected_logo = model.art.completed_misses.selected_logo;
                modal.art.completed_misses.selected_cover = model.art.completed_misses.selected_cover;
            }
            modal.text.insert(
                Binding::Title,
                self.entries
                    .get(selected)
                    .map(|e| e.name().into())
                    .unwrap_or_default(),
            );
            modal.rows.insert(
                Binding::Entries,
                self.menu_entries
                    .iter()
                    .enumerate()
                    .skip(menu.window_start())
                    .take(count)
                    .map(|(i, row)| Row {
                        subtitle: String::new(),
                        id: format!("menu-{i}"),
                        label: row.text(&locale),
                        value: String::new(),
                        enabled: true,
                    })
                    .collect(),
            );
            modal.focus = Focus {
                component: crate::theme_adapter::component_id(
                    handle,
                    ScreenId::ContextMenu,
                    Binding::Entries,
                ),
                item: Some(format!("menu-{}", menu.selected())),
            };
            if handle.pack().ink() {
                modal.rows.insert(
                    Binding::Actions,
                    vec![Row {
                        subtitle: String::new(),
                        id: "back".into(),
                        label: "Close".into(),
                        value: String::new(),
                        enabled: false,
                    }],
                );
            }
            modal.hints = vec![
                Hint {
                    button: "A".into(),
                    label: locale.t("button-select"),
                },
                Hint {
                    button: "B".into(),
                    label: locale.t("button-back"),
                },
            ];
            models.push(modal);
        }
        if let Some(picker) = &self.disc_picker {
            let count =
                crate::theme_adapter::capacity(handle, ScreenId::DiscPicker, Binding::Discs);
            let first = picker
                .selected
                .saturating_sub(count - 1)
                .min(picker.playlist.discs.len().saturating_sub(count));
            let mut modal = FrameInputs::new(ScreenId::DiscPicker);
            modal
                .text
                .insert(Binding::Title, "Choose starting disc".into());
            modal.rows.insert(
                Binding::Discs,
                picker
                    .playlist
                    .discs
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(count)
                    .map(|(i, _)| Row {
                        subtitle: String::new(),
                        id: format!("disc-{i}"),
                        label: format!("Disc {}", i + 1),
                        value: String::new(),
                        enabled: true,
                    })
                    .collect(),
            );
            modal.focus = Focus {
                component: crate::theme_adapter::component_id(
                    handle,
                    ScreenId::DiscPicker,
                    Binding::Discs,
                ),
                item: Some(format!("disc-{}", picker.selected)),
            };
            modal.text.insert(
                Binding::Subtitle,
                picker.error.clone().unwrap_or_else(|| {
                    picker.playlist.discs[picker.selected]
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }),
            );
            if handle.pack().ink() {
                modal.rows.insert(
                    Binding::Actions,
                    vec![Row {
                        subtitle: String::new(),
                        id: "back".into(),
                        label: "Cancel".into(),
                        value: String::new(),
                        enabled: false,
                    }],
                );
            }
            modal.hints = vec![
                Hint {
                    button: "A".into(),
                    label: "Play".into(),
                },
                Hint {
                    button: "B".into(),
                    label: "Cancel".into(),
                },
            ];
            models.push(modal);
        }
        Ok(models)
    }

    pub(crate) fn release_theme_art(&mut self) {
        self.idle_media_key = None;
        self.idle_media_work.clear();
        self.idle_lower_work.clear();
        if let Some(orbit) = self.orbit.as_mut() { orbit.release_art(); }
        if let Some(child) = self.child.as_mut() { child.release_theme_art(); }
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        crate::view::details::invalidate_metadata();
        self.details = None;
        self.semantic_info = None;
        self.recents_semantic.clear();
        self.normalized_recents.0.clear();
        self.recents_rows = None;
        self.image_focus = None;
        self.orbit = None;
        self.list.set_visible_count_override(None)?;
        if let Some(menu) = self.menu.as_mut() {
            menu.set_visible_count_override(None)?;
        }
        if let Some(child) = self.child.as_mut() {
            child.clear_theme()?;
        }
        Ok(())
    }
    pub fn save(&self) -> EntryListState<S> {
        EntryListState {
            sort: self.sort.clone(),
            selected: self.list.selected(),
            selected_path: self
                .entries
                .get(self.list.selected())
                .map(|entry| entry.path().to_path_buf()),
            child: self.child.as_ref().map(|c| Box::new(c.save())),
        }
    }

    pub fn load(rect: Rect, res: Resources, state: EntryListState<S>) -> Result<Self> {
        let mut this = Self::new(rect, res.clone(), state.sort)?;
        let selected = state
            .selected_path
            .as_ref()
            .and_then(|path| this.entries.iter().position(|entry| entry.path() == path))
            .unwrap_or(state.selected);
        this.select(selected);
        if let Some(child) = state.child {
            this.child = Some(Box::new(Self::load(rect, res, *child)?));
        }
        Ok(this)
    }

    pub fn select(&mut self, index: usize) {
        let _stage = common::perf_trace::stage(common::perf_trace::Stage::Selection);
        let first = self.list.window_start();
        self.list.select(index);
        common::perf_trace::selection(common::perf_trace::generation(), first != self.list.window_start());
        if !matches!(self.entries.get(self.list.selected()), Some(Entry::Game(_))) {
            self.semantic_info = None;
        }
        debug!("Selected entry: {:?}", self.entries.get(index));
        self.prepare_orbit();
    }

    async fn select_entry(&mut self, commands: Sender<Command>) -> Result<()> {
        self.list_revision = self.list_revision.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(self.list.selected()) {
            match entry {
                Entry::Directory(dir) => {
                    let mut child = EntryList::new(
                        self.rect,
                        self.res.clone(),
                        self.sort.with_directory(dir.clone()),
                    )?;
                    if let Some(orbit) = self.orbit.as_mut() {
                        orbit.release_art();
                        child.enable_orbit();
                    }
                    self.semantic_info = None;
                    self.child = Some(Box::new(child));
                }
                Entry::Game(game) => {
                    if crate::network_runtime::begin(game, false)? {
                        return Ok(());
                    }
                    if self.orbit.is_some() {
                        match crate::discs::discover(&game.path) {
                            Ok(Some(playlist)) if playlist.discs.len() > 1 => {
                                self.disc_picker = Some(DiscPicker::new(playlist));
                                self.orbit.as_mut().unwrap().redraw();
                                return Ok(());
                            }
                            Err(error) => {
                                commands
                                    .send(Command::Toast(
                                        error.to_string(),
                                        Some(std::time::Duration::from_secs(5)),
                                    ))
                                    .await?;
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    let command = self.res.get::<ConsoleMapper>().launch_game(
                        &self.res.get(),
                        game,
                        false,
                    )?;
                    if let Some(cmd) = command {
                        commands.send(cmd).await?;
                    }
                }
                Entry::App(app) => {
                    commands.send(app.command()).await?;
                }
            }
        }
        Ok(())
    }

    pub fn sort(&mut self, sort: S) -> Result<()> {
        self.sort = sort;
        self.load_entries()?;
        if S::HAS_BUTTON_HINTS {
            self.button_hints
                .right_mut()
                .get_mut(1)
                .unwrap()
                .set_text(self.sort.button_hint(&self.res.get::<Locale>()));
            self.button_hints.set_should_draw();
        }
        Ok(())
    }

    fn load_entries(&mut self) -> Result<()> {
        self.list_revision = self.list_revision.wrapping_add(1);
        self.recents_semantic.clear();
        self.normalized_recents.0.clear();
        self.recents_rows = None;
        self.idle_media_key = None;
        self.idle_media_work.clear();
        self.idle_lower_work.clear();
        crate::view::details::invalidate_metadata();
        self.semantic_info = None;
        self.entries = self
            .sort
            .entries(&self.res.get(), &self.res.get(), &self.res.get())?;
        self.display_names = DisplayNames::new(&self.entries);
        if let Some(orbit) = self.orbit.as_mut() {
            orbit.set_display_names(self.display_names.clone());
        }
        self.media_generation = common::media::generation();
        self.list.set_items(
            self.entries
                .iter()
                .map(|e| match e {
                    Entry::Game(game) => {
                        format!("{}{}", if game.favorite { "♥ " } else { "" }, self.display_names.name(e))
                    }
                    _ => self.display_names.name(e).to_string(),
                })
                .collect(),
            self.sort.preserve_selection(),
        );

        self.prepare_orbit();
        Ok(())
    }

    fn open_menu(&mut self) -> Result<()> {
        let Rect { x, y, w, h } = self.rect;
        let styles = self.res.get::<Stylesheet>();
        let locale = self.res.get::<Locale>();

        let Some(entry) = self.entries.get(self.list.selected()) else {
            return Ok(());
        };
        let entries = match entry {
            Entry::Game(game) => {
                let mut entries = vec![
                    MenuEntry::Favorite(game.favorite),
                    MenuEntry::Launch(None),
                    MenuEntry::Reset,
                    MenuEntry::RemoveFromRecents,
                    MenuEntry::RepopulateDatabase,
                ];
                if crate::network_runtime::registered(&game.path) {
                    if let Some(id) = crate::network_runtime::download_offer_id(&game.path) {
                        entries.push(MenuEntry::Download(id));
                    }
                    entries.push(MenuEntry::ManageStorage);
                }

                let cores = self
                    .res
                    .get::<ConsoleMapper>()
                    .get_console(&game.path)
                    .map(|c| c.cores.clone())
                    .unwrap_or_default();

                if !cores.is_empty() {
                    let core = game.core.to_owned().unwrap_or_else(|| cores[0].clone());
                    let i = cores.iter().position(|c| c == &core).unwrap_or_default();

                    if let MenuEntry::Launch(ref mut launch_core) = entries[1] {
                        let console_mapper = self.res.get::<ConsoleMapper>();
                        *launch_core = Some(console_mapper.get_core_name(&core));
                    }

                    self.core = Some(CoreSelection { core: i, cores });
                } else {
                    self.core = None;
                }

                entries
            }
            Entry::Directory(_) => vec![MenuEntry::Launch(None), MenuEntry::RepopulateDatabase],
            Entry::App(_) => {
                vec![
                    MenuEntry::Launch(None),
                    MenuEntry::Reset,
                    MenuEntry::RemoveFromRecents,
                    MenuEntry::RepopulateDatabase,
                ]
            }
        };

        let height = entries.len() as u32
            * (styles.ui.ui_font.size + styles.ui.list_margin as u32 + styles.ui.padding_y as u32);

        let mut menu = ScrollList::new(
            self.res.clone(),
            Rect::new(
                x + styles.ui.margin_x + (w as i32 - styles.ui.margin_x * 2) / 6,
                (y + h as i32 - height as i32) / 2,
                (w - styles.ui.margin_x as u32 * 2) * 2 / 3,
                height,
            ),
            entries.iter().map(|e| e.text(&locale)).collect(),
            Alignment::Left,
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        menu.set_background_color(Some(StylesheetColor::BackgroundHighlightBlend));
        self.menu = Some(menu);
        self.menu_entries = entries;

        Ok(())
    }
}

#[async_trait(?Send)]
impl<S> View for EntryList<S>
where
    S: Sort,
{
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(child) = &mut self.child {
            return child.draw(display, styles);
        }

        let mut drawn = false;

        if let Some(picker) = self.disc_picker.as_mut() {
            if picker.should_draw() || self.orbit.as_ref().is_some_and(Orbit::should_draw) {
                if let Some(orbit) = self.orbit.as_mut() {
                    orbit.draw(&self.entries, self.list.selected(), display, styles);
                }
                picker.draw(display, styles);
                return Ok(true);
            }
            return Ok(false);
        }

        if self.menu.is_some() {
            if let Some(orbit) = self.orbit.as_mut() {
                if orbit.should_draw() {
                    orbit.draw(&self.entries, self.list.selected(), display, styles);
                    self.menu.as_mut().unwrap().set_should_draw();
                    drawn = true;
                }
            }
        }
        if let Some(menu) = &mut self.menu {
            if menu.should_draw() {
                let mut rect = menu.bounding_box(styles);
                rect.y -= styles.ui.margin_x;
                rect.h += styles.ui.margin_x as u32 * 2;
                rect.x -= styles.ui.margin_x * 2;
                rect.w += styles.ui.margin_x as u32 * 4;
                rect = rect.intersection(&display.bounding_box());

                // Draw menu background
                let radius = (styles.ui.ui_font.size + styles.ui.margin_y as u32) / 2;
                common::display::fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    rect,
                    radius,
                    StylesheetColor::BackgroundHighlightBlend.to_color(styles),
                );

                menu.set_should_draw();
                menu.draw(display, styles)?;
                drawn = true;
            }
            return Ok(drawn);
        }

        if let Some(orbit) = self.orbit.as_mut() {
            if orbit.should_draw() {
                orbit.draw(&self.entries, self.list.selected(), display, styles);
                return Ok(true);
            }
            return Ok(false);
        }

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;

        let hints_will_draw = self.button_hints.should_draw();
        if hints_will_draw {
            self.image.set_should_draw();
        }

        let mut image_will_draw = false;
        if styles.games.boxart_width > 0 {
            self.image.set_border_radius(
                (styles.ui.margin_x as f32 * styles.games.boxart_border_radius) as u32,
            );
            if let Some(entry) = self.entries.get_mut(self.list.selected()) {
                if let Some(path) = entry.image() {
                    trace!("Loading image from {:?}", path);
                    self.image.set_path(Some(path.to_path_buf()));
                } else {
                    trace!("No image for entry {:?}", entry);
                    self.image.set_path(None);
                }
                image_will_draw = self.image.should_draw();
            } else {
                self.image.set_path(None);
            }
        }

        if hints_will_draw || image_will_draw {
            display.load(self.button_hints.bounding_box(styles))?;
        }

        if image_will_draw && self.image.draw(display, styles)? {
            drawn = true;
        }

        if hints_will_draw || drawn {
            if drawn {
                self.button_hints.set_should_draw();
            }
            drawn |= self.button_hints.draw(display, styles)?;
        }
        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        if let Some(child) = self.child.as_ref() {
            child.should_draw()
        } else {
            if let Some(orbit) = self.orbit.as_ref() {
                return orbit.should_draw()
                    || self
                        .disc_picker
                        .as_ref()
                        .is_some_and(DiscPicker::should_draw)
                    || self.menu.as_ref().is_some_and(View::should_draw);
            }
            self.menu
                .as_ref()
                .is_some_and(common::view::View::should_draw)
                || self.list.should_draw()
                || self.image.should_draw()
                || self.button_hints.should_draw()
        }
    }

    fn set_should_draw(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.set_should_draw();
        } else {
            if let Some(menu) = self.menu.as_mut() {
                menu.set_should_draw();
            }
            if let Some(orbit) = self.orbit.as_mut() {
                orbit.redraw();
            }
            if let Some(picker) = self.disc_picker.as_mut() {
                picker.redraw();
            }
            self.list.set_should_draw();
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
        if let Some(child) = self.child.as_mut() {
            match child.handle_key_event(event, commands, bubble).await? {
                true => {
                    bubble.retain_mut(|c| match c {
                        Command::CloseView => {
                            self.child = None;
                            self.prepare_orbit();
                            self.set_should_draw();
                            false
                        }
                        _ => true,
                    });
                    Ok(true)
                }
                false => Ok(false),
            }
        } else if self.details.is_some() && self.disc_picker.is_none() {
            match event {
                KeyEvent::Pressed(Key::Left | Key::Right)
                    if crate::theme_adapter::snapshot(&self.res)
                        .is_some_and(|h| h.pack().ink()) =>
                {
                    let handle=crate::theme_adapter::snapshot(&self.res).unwrap();
                    let count=Self::detail_targets(&handle).len();
                    self.detail_action=if matches!(event,KeyEvent::Pressed(Key::Right)){(self.detail_action+1)%count}else{(self.detail_action+count-1)%count};
                    self.set_should_draw();
                }
                KeyEvent::Pressed(Key::B) => {
                    self.details = None;
                    self.set_should_draw();
                }
                KeyEvent::Pressed(Key::A) => {
                    self.details = None;
                    let play=crate::theme_adapter::snapshot(&self.res).filter(|h|h.pack().ink()).is_none_or(|h| {
                        let targets=Self::detail_targets(&h);targets[self.detail_action % targets.len()].item.as_deref()==Some("play")
                    });
                    if play {
                        self.select_entry(commands).await?;
                    }
                    self.set_should_draw();
                }
                KeyEvent::Pressed(Key::Up | Key::Down)
                | KeyEvent::Autorepeat(Key::Up | Key::Down) => {
                    let forward = matches!(
                        event,
                        KeyEvent::Pressed(Key::Down) | KeyEvent::Autorepeat(Key::Down)
                    );
                    self.details.as_mut().unwrap().turn(forward);
                    self.set_should_draw();
                }
                _ => {}
            }
            Ok(true)
        } else if self.disc_picker.is_some() {
            match event {
                KeyEvent::Pressed(Key::Up | Key::Down)
                | KeyEvent::Autorepeat(Key::Up | Key::Down) => {
                    let forward = matches!(
                        event,
                        KeyEvent::Pressed(Key::Down) | KeyEvent::Autorepeat(Key::Down)
                    );
                    self.disc_picker.as_mut().unwrap().select(forward);
                    Ok(true)
                }
                KeyEvent::Pressed(Key::B | Key::Select) => {
                    self.disc_picker = None;
                    self.orbit.as_mut().unwrap().redraw();
                    Ok(true)
                }
                KeyEvent::Pressed(Key::A) => {
                    let picker = self.disc_picker.as_ref().unwrap();
                    let playlist = picker.playlist.clone();
                    let index = picker.selected;
                    let Some(Entry::Game(game)) = self.entries.get_mut(self.list.selected()) else {
                        return Ok(true);
                    };
                    self.list_revision = self.list_revision.wrapping_add(1);
                    let result = self.res.get::<ConsoleMapper>().launch_game_with_disc(
                        &self.res.get(),
                        game,
                        &playlist,
                        index,
                    );
                    match result {
                        Ok(Some(command)) => {
                            self.disc_picker = None;
                            commands.send(command).await?;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let picker = self.disc_picker.as_mut().unwrap();
                            picker.error = Some(error.to_string());
                            picker.redraw();
                        }
                    }
                    Ok(true)
                }
                _ => Ok(true),
            }
        } else if let Some(menu) = self.menu.as_mut() {
            match event {
                KeyEvent::Pressed(Key::Left) => {
                    if let Some(core) = self.core.as_mut() {
                        let selected = &mut self.menu_entries[menu.selected()];
                        if let MenuEntry::Launch(launch_core) = selected {
                            core.core = core.core.saturating_sub(1);
                            let console_mapper = self.res.get::<ConsoleMapper>();
                            *launch_core =
                                Some(console_mapper.get_core_name(&core.cores[core.core]));
                            menu.set_item(menu.selected(), selected.text(&self.res.get()));
                        }
                    }
                    Ok(true) // trap tab focus
                }
                KeyEvent::Pressed(Key::Right) => {
                    if let Some(core) = self.core.as_mut() {
                        let selected = &mut self.menu_entries[menu.selected()];
                        if let MenuEntry::Launch(launch_core) = selected {
                            core.core = (core.core + 1).min(core.cores.len() - 1);
                            let console_mapper = self.res.get::<ConsoleMapper>();
                            *launch_core =
                                Some(console_mapper.get_core_name(&core.cores[core.core]));
                            menu.set_item(menu.selected(), selected.text(&self.res.get()));
                        }
                    }
                    Ok(true) // trap tab focus
                }
                KeyEvent::Pressed(Key::Select | Key::B) => {
                    self.menu = None;
                    commands.send(Command::Redraw).await?;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::A) => {
                    let selected = &self.menu_entries[menu.selected()];
                    match selected {
                        MenuEntry::Favorite(_) => {
                            self.list_revision = self.list_revision.wrapping_add(1);
                            let entry = self.entries.get_mut(self.list.selected()).unwrap();
                            if let Entry::Game(game) = entry {
                                game.favorite = !game.favorite;
                                self.res
                                    .get::<Database>()
                                    .set_favorite(&game.clone().into())?;
                                self.list.set_item(
                                    self.list.selected(),
                                    format!(
                                        "{}{}",
                                        if game.favorite { "♥ " } else { "" },
                                        self.display_names.name(entry)
                                    ),
                                );
                            }
                            commands.send(Command::Redraw).await?;
                        }
                        MenuEntry::Launch(_) => {
                            self.list_revision = self.list_revision.wrapping_add(1);
                            let entry = self.entries.get_mut(self.list.selected()).unwrap();
                            if let (Some(core), Entry::Game(game)) = (self.core.as_ref(), entry) {
                                let db = self.res.get::<Database>();
                                let core = &core.cores[core.core];
                                db.set_core(&game.path, core)?;
                                game.core = Some(core.to_string());
                            }
                            self.core = None;
                            self.select_entry(commands).await?;
                        }
                        MenuEntry::Reset => {
                            self.list_revision = self.list_revision.wrapping_add(1);
                            let entry = self.entries.get_mut(self.list.selected()).unwrap();
                            match entry {
                                Entry::Directory(_) | Entry::App(_) => {}
                                Entry::Game(game) => {
                                    if crate::network_runtime::begin(game, true)? {
                                        self.menu = None;
                                        return Ok(true);
                                    }
                                    let command = self.res.get::<ConsoleMapper>().launch_game(
                                        &self.res.get(),
                                        game,
                                        true,
                                    )?;
                                    if let Some(cmd) = command {
                                        commands.send(cmd).await?;
                                    }
                                }
                            }
                            commands.send(Command::Redraw).await?;
                        }
                        MenuEntry::RemoveFromRecents => {
                            if let Some(Entry::Game(game)) = self.entries.get(self.list.selected())
                            {
                                if game.path.exists() {
                                    self.res.get::<Database>().reset_game(&game.path)?;
                                } else {
                                    self.res.get::<Database>().delete_game(&game.path)?;
                                }
                                self.load_entries()?;
                                commands.send(Command::Redraw).await?;
                            }
                        }
                        MenuEntry::ManageStorage => {
                            crate::network_runtime::storage()?;
                        }
                        MenuEntry::Download(id) => {
                            crate::network_runtime::prefill(id)?;
                        }
                        MenuEntry::RepopulateDatabase => {
                            commands.send(Command::Redraw).await?;
                            #[cfg(not(feature = "miyoo"))]
                            {
                                let message = self.res.get::<Locale>().t("populating-database");
                                commands.send(Command::Toast(message, None)).await?;
                            }
                            commands.send(Command::PopulateDb).await?;
                            #[cfg(not(feature = "miyoo"))]
                            {
                                commands
                                    .send(Command::Toast(
                                        String::new(),
                                        Some(std::time::Duration::ZERO),
                                    ))
                                    .await?;
                            }
                            commands.send(Command::Redraw).await?;
                        }
                    }
                    self.menu = None;
                    Ok(true)
                }
                _ => menu.handle_key_event(event, commands, bubble).await,
            }
        } else {
            if let Some(handle)=crate::theme_adapter::snapshot(&self.res).filter(|h|h.pack().ink() && self.theme_screen==Some(common::theme::pack::ScreenId::Library)) {
                let actions=Self::image_actions(&handle,common::theme::pack::ScreenId::Library);
                if self.image_focus.is_some() {
                    match event {
                        KeyEvent::Pressed(Key::Down|Key::B) => {self.image_focus=None;self.set_should_draw();return Ok(true);}
                        KeyEvent::Pressed(Key::Left|Key::Right) => {
                            if !actions.is_empty(){let current=actions.iter().position(|f|Some(f)==self.image_focus.as_ref()).unwrap_or(0);let next=if matches!(event,KeyEvent::Pressed(Key::Right)){(current+1)%actions.len()}else{(current+actions.len()-1)%actions.len()};self.image_focus=Some(actions[next].clone());self.set_should_draw();}
                            return Ok(true);
                        }
                        KeyEvent::Pressed(Key::A) => {self.image_focus=None;self.select_entry(commands).await?;self.set_should_draw();return Ok(true);}
                        KeyEvent::Pressed(Key::Up) => return Ok(false),
                        _ => {}
                    }
                } else if matches!(event,KeyEvent::Pressed(Key::Up)) && !actions.is_empty() && !self.entries.is_empty() {
                    self.image_focus=Some(actions[0].clone());self.set_should_draw();return Ok(true);
                }
            }
            if self.orbit.is_some() {
                match event {
                    KeyEvent::Pressed(Key::Left | Key::Right | Key::Up | Key::Down)
                    | KeyEvent::Autorepeat(Key::Left | Key::Right | Key::Up | Key::Down) => {
                        if !self.entries.is_empty() {
                            let forward = matches!(
                                event,
                                KeyEvent::Pressed(Key::Right | Key::Down)
                                    | KeyEvent::Autorepeat(Key::Right | Key::Down)
                            );
                            let count = self.entries.len();
                            let index = if forward {
                                (self.list.selected() + 1) % count
                            } else {
                                (self.list.selected() + count - 1) % count
                            };
                            self.select(index);
                        }
                        return Ok(true);
                    }
                    KeyEvent::Pressed(Key::L | Key::R) => return Ok(false),
                    _ if self.entries.is_empty()
                        && matches!(
                            event,
                            KeyEvent::Pressed(Key::L2 | Key::R2 | Key::Select | Key::A)
                        ) =>
                    {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
            match event {
                KeyEvent::Pressed(Key::L2) => {
                    let selected = self.list.selected();
                    let len = self.entries.len();
                    let mut entries = self
                        .entries
                        .iter()
                        .rev()
                        .skip(len - selected)
                        .map(|e| e.name().chars().next());

                    let Some(char) = entries.next() else {
                        self.select(0);
                        return Ok(true);
                    };

                    if let Some(i) = entries.position(|c| c != char) {
                        self.select(selected - i - 1);
                    } else {
                        self.select(0);
                    }
                    Ok(true)
                }
                KeyEvent::Pressed(Key::R2) => {
                    let selected = self.list.selected();
                    let mut entries = self
                        .entries
                        .iter()
                        .skip(selected)
                        .map(|e| e.name().chars().next());
                    let Some(char) = entries.next() else {
                        self.select(self.entries.len() - 1);
                        return Ok(true);
                    };

                    if let Some(i) = entries.position(|c| c != char) {
                        self.select(selected + 1 + i);
                    } else {
                        self.select(self.entries.len() - 1);
                    }
                    Ok(true)
                }
                KeyEvent::Pressed(Key::B) => {
                    bubble.push_back(Command::CloseView);
                    Ok(true)
                }
                KeyEvent::Pressed(Key::A) => {
                    self.select_entry(commands).await?;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::Start) if crate::theme_adapter::active(&self.res) => {
                    if let Some(Entry::Game(game)) = self.entries.get(self.list.selected()) {
                        let platform = self
                            .res
                            .get::<ConsoleMapper>()
                            .get_console(&game.path)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        let mut details = {
                            let _trace = common::perf_trace::recording()
                                .then(|| common::perf_trace::stage(common::perf_trace::Stage::DetailsMetadata));
                            crate::view::details::Details::new(game, &platform)
                        };
                        let record = {
                            let _trace = common::perf_trace::recording()
                                .then(|| common::perf_trace::stage(common::perf_trace::Stage::DetailsDatabase));
                            self.res.get::<Database>().select_game(&game.path)
                        };
                        if let Ok(Some(record)) = record {
                            details.add_history(
                                record.play_count,
                                record.play_time.num_minutes().max(0) as u64,
                            );
                        }
                        if let Some(handle) = crate::theme_adapter::snapshot(&self.res) {
                            let _trace = common::perf_trace::recording()
                                .then(|| common::perf_trace::stage(common::perf_trace::Stage::DetailsReflow));
                            details.reflow(&handle);
                        }
                        self.detail_action = 0;
                        self.details = Some(details);
                        self.set_should_draw();
                    }
                    Ok(true)
                }
                KeyEvent::Pressed(Key::Y) => {
                    self.sort(self.sort.next())?;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::Select) => {
                    self.open_menu()?;
                    Ok(true)
                }
                _ => {
                    let selected = self.list.selected();
                    let first = self.list.window_start();
                    let res = self.list.handle_key_event(event, commands, bubble).await?;
                    common::perf_trace::selection(common::perf_trace::generation(), first != self.list.window_start());
                    if self.list.selected() != selected {
                        self.prepare_orbit();
                    }
                    debug!(
                        "Selected entry: {:?}",
                        self.entries.get(self.list.selected())
                    );
                    Ok(res)
                }
            }
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        if let Some(child) = self.child.as_ref() {
            vec![child.as_ref() as &dyn View]
        } else {
            vec![&self.list, &self.image, &self.button_hints]
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        if let Some(child) = self.child.as_mut() {
            vec![child.as_mut() as &mut dyn View]
        } else {
            vec![&mut self.list, &mut self.image, &mut self.button_hints]
        }
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

#[derive(Debug, Clone)]
enum MenuEntry {
    Favorite(bool),
    Launch(Option<String>),
    Reset,
    RemoveFromRecents,
    RepopulateDatabase,
    ManageStorage,
    Download(String),
}

impl MenuEntry {
    fn text(&self, locale: &Locale) -> String {
        match self {
            MenuEntry::Favorite(is_favorite) => {
                if *is_favorite {
                    locale.t("menu-unset-as-favorite")
                } else {
                    locale.t("menu-set-as-favorite")
                }
            }
            MenuEntry::Launch(core) => {
                if let Some(core) = core.as_deref() {
                    locale.ta(
                        "menu-launch-with-core",
                        &[("core".into(), core.into())].into_iter().collect(),
                    )
                } else {
                    locale.t("menu-launch")
                }
            }
            MenuEntry::Reset => locale.t("menu-reset"),
            MenuEntry::RemoveFromRecents => locale.t("menu-remove-from-recents"),
            MenuEntry::RepopulateDatabase => locale.t("menu-repopulate-database"),
            MenuEntry::ManageStorage => "Manage storage".into(),
            MenuEntry::Download(_) => "Download".into(),
        }
    }
}

#[cfg(test)]
mod perf_fix_tests {
    use super::*;
    use crate::entry::{directory::Directory, game::Game};
    use std::path::PathBuf;

    #[test]
    fn metadata_generation_refreshes_selected_path_metadata_and_blurb() {
        let root =
            std::env::temp_dir().join(format!("allium-selected-info-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut game = Game::new(root.join("Game.chd"));
        let xml = root.join("gamelist.xml");
        std::fs::write(
            &xml,
            "<gameList><game><path>./Game.chd</path><desc>First</desc></game></gameList>",
        )
        .unwrap();
        let mut slot = None;
        crate::view::details::invalidate_metadata();
        common::media::check_directory(&root);
        let first = SelectedInfo::get(&mut slot, &game, "PlayStation");
        let generation = first.generation;
        assert_eq!(first.blurb, "First");
        assert!(!first.metadata.contains("Year unavailable"));
        std::fs::write(
            &xml,
            "<gameList><game><path>./Game.chd</path><desc>Replacement</desc></game></gameList>",
        )
        .unwrap();
        game.release_date = chrono::NaiveDate::from_ymd_opt(1999, 1, 1);
        assert_eq!(
            SelectedInfo::get(&mut slot, &game, "PlayStation").blurb,
            "First"
        );
        crate::view::details::invalidate_metadata();
        common::media::check_directory(&root);
        let updated = SelectedInfo::get(&mut slot, &game, "PlayStation");
        assert_ne!(updated.generation, generation);
        assert_eq!(updated.blurb, "Replacement");
        assert!(updated.metadata.contains("Released 1999"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[derive(Debug, Clone)]
    struct EmptySort;
    impl Sort for EmptySort {
        const HAS_BUTTON_HINTS: bool = false;
        fn button_hint(&self, _: &Locale) -> String {
            String::new()
        }
        fn next(&self) -> Self {
            self.clone()
        }
        fn with_directory(&self, _: Directory) -> Self {
            self.clone()
        }
        fn entries(&self, _: &Database, _: &ConsoleMapper, _: &Locale) -> Result<Vec<Entry>> {
            Ok(vec![])
        }
        fn preserve_selection(&self) -> bool {
            false
        }
    }
    fn list() -> EntryList<EmptySort> {
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(Database::in_memory().unwrap());
        EntryList::new(Rect::new(0, 54, 640, 376), Resources::new(map), EmptySort).unwrap()
    }
    #[test]
    fn system_preview_uses_cached_count_without_repeating_platform() {
        use common::theme::{fonts::FontBank, pack::{Binding, ScreenId}, runtime::ThemeSnapshot, ThemeHandle};
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 1).unwrap();
        let mut list = list();
        select_directory(&mut list);
        if let Entry::Directory(directory) = &mut list.entries[0] { directory.game_count = Some(47); }
        let models = list.theme_models(ScreenId::Library, &theme).unwrap();
        assert_eq!(models[0].text[&Binding::SelectedMetadata], "47 games");
        assert_eq!(models[0].metadata, [(String::new(), "47 games".into())]);
        assert_eq!(theme.pack().screens[&ScreenId::Library].components.iter().filter(|c| c.binding == Some(Binding::SelectedTitle)).count(), 1);
    }

    #[test]
    fn folders_only_offer_navigation_and_database_refresh() {
        let mut list = list();
        select_directory(&mut list);
        list.open_menu().unwrap();
        let entries = &list.menu_entries;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| matches!(entry, MenuEntry::Launch(_) | MenuEntry::RepopulateDatabase)));
    }

    fn select_directory(list: &mut EntryList<EmptySort>) {
        list.entries = vec![Entry::Directory(Directory::new(PathBuf::from("Child")))];
        list.list.set_items(vec!["Child".into()], false);
        list.semantic_info = Some(SelectedInfo {
            path: "Game.chd".into(),
            generation: 0,
            media_generation: common::media::generation(),
            adoption_generation: crate::view::orbit::metadata_completion_generation(),
            platform: String::new(),
            metadata: "metadata".into(),
            metadata_runs: vec![],
            blurb: "blurb".into(),
        });
    }
    #[tokio::test]
    #[cfg(feature = "perf-bench")]
    #[serial_test::serial(orbit_fixture)]
    async fn context_menu_inherits_cover_and_logo_misses_and_settles() {
        use common::theme::{fonts::FontBank, pack::ScreenId, runtime::ThemeSnapshot, ThemeHandle};
        let root = std::env::temp_dir().join(format!("allium-menu-art-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        common::perf_trace::start_probe(root.join("trace.log").to_str().unwrap());
        let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
        for name in ["compact-plus", "orbit-hero"] {
            let snapshot = ThemeSnapshot::load(
                &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../themes").join(name).canonicalize().unwrap(),
            ).unwrap();
            let handle = ThemeHandle::prepare(snapshot, &bank, 993).unwrap();
            for has_cover in [false, true] {
                let stem = format!("{name}-{has_cover}");
                if has_cover {
                    tiny_skia::Pixmap::new(16, 16).unwrap()
                        .save_png(root.join(format!("Imgs/{stem}.png"))).unwrap();
                }
                common::media::invalidate();
                let mut list = list();
                list.res.insert(Some(handle.clone()));
                list.entries = vec![Entry::Game(Game::new(root.join(format!("{stem}.sfc"))))];
                list.list.set_items(vec![stem], false);
                list.enable_orbit();
                list.open_menu().unwrap();
                let models = list.theme_models(ScreenId::Library, &handle).unwrap();
                let modal = models.last().unwrap();
                assert_eq!(modal.screen, ScreenId::ContextMenu);
                assert!(!super::super::app::theme_content_complete(modal, &handle));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while list.idle_media_step(&handle, ScreenId::Library) {
                    assert!(std::time::Instant::now() < deadline, "{name} art timeout");
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                let models = list.theme_models(ScreenId::Library, &handle).unwrap();
                let source = &models[0].art;
                let mut modal = models.last().unwrap().clone();
                assert!(source.completed_misses.selected_logo, "{name} logo miss");
                assert!(modal.art.selected_logo.is_none());
                assert!(modal.art.completed_misses.selected_logo);
                assert_eq!(modal.art.completed_misses.selected_cover, source.completed_misses.selected_cover);
                assert_eq!(modal.art.completed_misses.selected_cover, !has_cover);
                assert_eq!(modal.art.selected_cover.is_some(), has_cover);
                if has_cover {
                    assert!(std::sync::Arc::ptr_eq(modal.art.selected_cover.as_ref().unwrap(), source.selected_cover.as_ref().unwrap()));
                }
                assert!(super::super::app::theme_content_complete(&modal, &handle), "{name} completed menu");
                modal.art.completed_misses.selected_logo = false;
                assert_eq!(super::super::app::theme_content_complete(&modal, &handle), name == "orbit-hero" && has_cover);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    fn review_selected_info_tracks_media_generation() {
        let root =
            std::env::temp_dir().join(format!("allium-review-selected-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let xml = root.join("gamelist.xml");
        std::fs::write(
            &xml,
            "<gameList><game><path>./G.sfc</path><desc>Before</desc></game></gameList>",
        )
        .unwrap();
        common::media::invalidate();
        let game = crate::entry::game::Game::new(root.join("G.sfc"));
        let _ = common::media::metadata(&game.path);
        let mut info = None;
        assert_eq!(SelectedInfo::get(&mut info, &game, "Test").blurb, "Before");
        std::fs::write(
            &xml,
            "<gameList><game><path>./G.sfc</path><desc>After</desc></game></gameList>",
        )
        .unwrap();
        common::media::check_directory(&root);
        let after = SelectedInfo::get(&mut info, &game, "Test").blurb.clone();
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
        assert_eq!(after, "After");
    }
    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn metadata_prewarm_two_games_refreshes_warm_and_selected_text() {
        use common::theme::{
            ThemeHandle,
            fonts::FontBank,
            frame::FrameInputs,
            pack::{Binding, ScreenId},
            runtime::ThemeSnapshot,
        };
        let root =
            std::env::temp_dir().join(format!("allium-prewarm-metadata-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        std::fs::create_dir_all(root.join("media/logos")).unwrap();
        let pixels = tiny_skia::Pixmap::new(32, 32)
            .unwrap()
            .encode_png()
            .unwrap();
        for name in ["First", "Second"] {
            for suffix in [".png", ".hero.png"] {
                std::fs::write(root.join(format!("Imgs/{name}{suffix}")), &pixels).unwrap();
            }
            std::fs::write(root.join(format!("media/logos/{name}.png")), &pixels).unwrap();
        }
        std::fs::write(root.join("gamelist.xml"), "<gameList><game><path>./First.sfc</path><desc>First synopsis</desc><developer>First studio</developer></game><game><path>./Second.sfc</path><desc>Second synopsis</desc><developer>Second studio</developer><releasedate>19990101T000000</releasedate><players>1-2</players></game></gameList>").unwrap();
        common::media::invalidate();
        let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
        let snapshot = ThemeSnapshot::load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../themes/compact-plus")
                .canonicalize()
                .unwrap(),
        )
        .unwrap();
        let handle = ThemeHandle::prepare(snapshot, &bank, 992).unwrap();
        let mut list = list();
        list.res.insert(Some(handle.clone()));
        list.entries = ["First", "Second"]
            .into_iter()
            .map(|name| Entry::Game(Game::new(root.join(format!("{name}.sfc")))))
            .collect();
        list.list
            .set_items(vec!["First".into(), "Second".into()], false);
        list.enable_orbit();
        list.theme_models(ScreenId::Library, &handle).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while list.idle_media_step(&handle, ScreenId::Library) {
            assert!(std::time::Instant::now() < deadline, "idle timeout");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let mut warmed = Vec::new();
        for direction in [1, -1] {
            let mut work = list
                .navigation_warm_model(&handle, direction, FrameInputs::new(ScreenId::Library))
                .unwrap()
                .unwrap();
            while !list.navigation_warm_art_step(&mut work) {
                assert!(std::time::Instant::now() < deadline, "prewarm timeout");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            warmed.push(work.input);
        }
        let (send, _recv) = tokio::sync::mpsc::channel(4);
        list.handle_key_event(KeyEvent::Pressed(Key::Right), send, &mut VecDeque::new())
            .await
            .unwrap();
        let mut models = list.theme_models(ScreenId::Library, &handle).unwrap();
        while list.idle_media_step(&handle, ScreenId::Library) {
            assert!(std::time::Instant::now() < deadline, "selection timeout");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        models.extend(list.theme_models(ScreenId::Library, &handle).unwrap());
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
        for model in models.iter().chain(&warmed) {
            assert_eq!(model.text[&Binding::SelectedBlurb], "Second synopsis");
            assert!(model.text[&Binding::SelectedMetadata].contains("1999"));
            assert!(
                model
                    .metadata
                    .iter()
                    .any(|(label, value)| label == "Released" && value == "1999")
            );
        }
    }
    #[tokio::test]
    #[cfg(feature = "perf-bench")]
    async fn review_right_over_cached_games_performs_zero_media_io() {
        use common::theme::{
            ThemeHandle, fonts::FontBank, frame::FrameInputs, pack::ScreenId,
            runtime::ThemeSnapshot,
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../common/tests/fixtures/media-sd/Roms/TEST");
        common::media::invalidate();
        common::media::check_directory(&root);
        let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
        for id in [
            "orbit-hero",
            "orbit-hero-tall",
            "orbit-row",
            "orbit-row-tall",
            "big-picture",
            "big-picture-tall",
            "compact-plus",
            "compact-plus-tall",
        ] {
            let snapshot = ThemeSnapshot::load_with_settings(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../themes")
                    .join(id.strip_suffix("-tall").unwrap_or(id))
                    .canonicalize()
                    .unwrap(),
                &std::collections::BTreeMap::from([("cover_shape".into(), if id.ends_with("-tall") { "tall" } else { "square" }.into())]),
            )
            .unwrap();
            let handle = ThemeHandle::prepare(snapshot, &bank, 991).unwrap();
            let mut list = list();
            list.res.insert(Some(handle.clone()));
            list.entries = ["Complete.sfc", "Legacy.sfc"]
                .into_iter()
                .map(|p| Entry::Game(crate::entry::game::Game::new(root.join(p))))
                .collect();
            list.list
                .set_items(vec!["Complete".into(), "Legacy".into()], false);
            list.enable_orbit();
            list.theme_models(ScreenId::Library, &handle).unwrap();
            for direction in [1, -1] {
                let mut plan = list
                    .navigation_warm_model(&handle, direction, FrameInputs::new(ScreenId::Library))
                    .unwrap()
                    .expect("metadata themes must support prefetch");
                while !list.navigation_warm_art_step(&mut plan) { tokio::time::sleep(std::time::Duration::from_millis(1)).await; }
            }
            common::perf::take();
            let before = common::perf_trace::frontend_io_ops();
            let (send, _recv) = tokio::sync::mpsc::channel(4);
            list.handle_key_event(KeyEvent::Pressed(Key::Right), send, &mut VecDeque::new())
                .await
                .unwrap();
            list.theme_models(ScreenId::Library, &handle).unwrap();
            list.theme_models(ScreenId::Library, &handle).unwrap();
            let (send, _recv) = tokio::sync::mpsc::channel(4);
            list.handle_key_event(KeyEvent::Pressed(Key::Right), send, &mut VecDeque::new())
                .await
                .unwrap();
            let models = list.theme_models(ScreenId::Library, &handle).unwrap();
            assert!(
                models[0].art.selected_logo.is_some(),
                "{id}: prefetched logo missing on returning Right"
            );
            let counts = common::perf::take();
            assert_eq!(
                (counts.media_io, counts.read_attempts, counts.decodes),
                (0, 0, 0),
                "{id}: {counts:?}"
            );
            assert_eq!(
                common::perf_trace::frontend_io_ops(),
                before,
                "{id}: frontend filesystem operation"
            );
        }
    }
    #[tokio::test]
    async fn semantic_info_is_cleared_when_entering_child_directory() {
        let mut list = list();
        select_directory(&mut list);
        list.enable_orbit();
        select_directory(&mut list);
        let (send, _) = tokio::sync::mpsc::channel(1);
        list.select_entry(send).await.unwrap();
        assert!(list.child.is_some());
        assert!(list.semantic_info.is_none());
    }
    #[test]
    fn selecting_non_game_clears_semantic_info() {
        let mut list = list();
        select_directory(&mut list);
        list.select(0);
        assert!(list.semantic_info.is_none());
    }
    include!("entry_latency_bench.rs");

}

#[cfg(test)]
pub(crate) mod ux4_tests {
    use super::*;
    use crate::entry::{directory::Directory, game::Game};
    use std::path::{Path, PathBuf};
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct PathsSort {
        paths: Vec<PathBuf>,
    }
    impl Sort for PathsSort {
        const HAS_BUTTON_HINTS: bool = false;
        fn button_hint(&self, _: &Locale) -> String {
            String::new()
        }
        fn next(&self) -> Self {
            self.clone()
        }
        fn with_directory(&self, _: Directory) -> Self {
            self.clone()
        }
        fn entries(&self, _: &Database, _: &ConsoleMapper, _: &Locale) -> Result<Vec<Entry>> {
            Ok(self
                .paths
                .iter()
                .map(|p| {
                    let mut g = Game::new(p.clone());
                    g.name = "Same title".into();
                    Entry::Game(g)
                })
                .collect())
        }
        fn preserve_selection(&self) -> bool {
            false
        }
    }
    fn resources() -> Resources {
        let mut map = type_map::TypeMap::new();
        let mut styles = Stylesheet::new();
        styles.load_fonts().unwrap();
        map.insert(styles);
        map.insert(Locale::new("en-US"));
        map.insert(ConsoleMapper::new());
        map.insert(common::geom::Size::new(640, 480));
        map.insert(Database::in_memory().unwrap());
        Resources::new(map)
    }
    #[test]
    fn collision_labels_reach_library_recents_and_search_without_reordering() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::{Binding, ScreenId}};
        let paths: Vec<PathBuf> = ["Game (USA).zip", "Game (Europe).nes", "Game (USA).nes"]
            .into_iter().map(PathBuf::from).collect();
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), resources(),
            PathsSort { paths: paths.clone() }).unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 701).unwrap();
        let expected = ["Same title (USA) (ZIP)", "Same title (Europe)", "Same title (USA) (NES)"];
        for screen in [ScreenId::Library, ScreenId::Recents, ScreenId::SearchResults] {
            for _ in 0..2 {
                let model = list.recents_theme_model(screen, &theme, 0, 0, 3);
                let labels: Vec<_> = model.rows[&Binding::Entries].iter().map(|row| row.label.as_str()).collect();
                assert_eq!(labels, expected);
                assert_eq!(model.text[&Binding::SelectedTitle], expected[0]);
            }
        }
        assert_eq!(list.entries.iter().map(|entry| entry.path().to_path_buf()).collect::<Vec<_>>(), paths);
        assert!(list.entries.iter().all(|entry| entry.name() == "Same title"));
        list.sort(PathsSort { paths: vec![paths[0].clone()] }).unwrap();
        let model = list.base_theme_model(ScreenId::Library, &theme, 0, 0, 3);
        assert_eq!(model.rows[&Binding::Entries][0].label, "Same title");
    }

    pub(crate) fn offers_download(game: Game) -> bool {
        let mut list = EntryList::new(
            Rect::new(0, 54, 640, 376),
            resources(),
            PathsSort {
                paths: vec![game.path.clone()],
            },
        )
        .unwrap();
        list.entries = vec![Entry::Game(game)];
        list.open_menu().unwrap();
        list.menu_entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Download(_)))
    }
    fn idle_queue_fixture(next_cover: bool) -> (EntryList<PathsSort>, common::theme::ThemeHandle) {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let source = repo.join("themes/compact-plus");
        let root = std::env::temp_dir().join(format!("allium-idle-queue-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut pack: serde_json::Value = serde_json::from_slice(
            &std::fs::read(source.join("theme.json")).unwrap()).unwrap();
        let details = pack["screens"]["game_detail"]["components"].as_array().unwrap().clone();
        let recents = pack["screens"]["recents"]["components"].as_array_mut().unwrap();
        for image in recents.iter_mut().filter(|c| c["kind"] == "image") {
            let detail = details.iter().find(|c| c["binding"] == image["binding"]).unwrap();
            image["rect"] = detail["rect"].clone();
        }
        recents.iter_mut().find(|c| c["binding"] == "entries").unwrap()["visible_count"] = 2.into();
        if next_cover {
            let mut next = details.iter().find(|c| c["binding"] == "selected_cover").unwrap().clone();
            next["id"] = "next-cover".into();
            next["binding"] = "next_cover".into();
            pack["screens"]["game_detail"]["components"].as_array_mut().unwrap().push(next);
        }
        for asset in pack["assets"].as_object().unwrap().values() {
            let path = asset["path"].as_str().unwrap();
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::copy(source.join(path), root.join(path)).unwrap();
        }
        std::fs::write(root.join("theme.json"), serde_json::to_vec(&pack).unwrap()).unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&root).unwrap(), &bank, 604).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        let res = resources();
        res.insert(Some(theme.clone()));
        let paths = (0..4).map(|i| root.join(format!("Game {i}.chd"))).collect();
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res, PathsSort { paths }).unwrap();
        list.select(1);
        (list, theme)
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "perf-bench")]
    #[serial_test::serial(orbit_fixture)]
    async fn medianoop_configuration_reuse_racing_completion_misses_and_eviction() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle,
            pack::ScreenId, render::PreparedFrame};
        use super::super::orbit::{art_result_queued, completion_generation, proof_art_gate};
        let (mut list, theme) = idle_queue_fixture(false);
        let root = list.entries[0].path().parent().unwrap().to_owned();
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        common::perf_trace::start_probe(root.join("medianoop.jsonl").to_str().unwrap());
        for i in 0..4 {
            std::fs::write(root.join(format!("Game {i}.chd")), []).unwrap();
            let mut pixels = tiny_skia::Pixmap::new(32, 32).unwrap();
            pixels.fill(tiny_skia::Color::from_rgba8(i * 50, 90, 120, 255));
            pixels.save_png(root.join(format!("Imgs/Game {i}.png"))).unwrap();
        }
        common::media::invalidate();
        let builds = MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get);
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 1);
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        proof_art_gate(Some(gate.clone()));
        list.idle_media_step(&theme, ScreenId::Recents);
        assert!(!art_result_queued());
        let generation = completion_generation();
        gate.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !art_result_queued() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(completion_generation(), generation, "queue signal must precede visible adoption");
        assert!(art_result_queued(), "probing must not consume the racing result");
        list.idle_media_step(&theme, ScreenId::Recents);
        assert_ne!(completion_generation(), generation, "racing missing hero must settle before render");
        let drain = |list: &mut EntryList<PathsSort>, theme: &ThemeHandle| {
            while list.idle_media_step(theme, ScreenId::Recents) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        drain(&mut list, &theme);
        let settled = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
        assert!(settled.art.selected_cover.is_some());
        assert!(settled.art.completed_misses.selected_hero && settled.art.completed_misses.selected_logo);
        assert!(super::super::app::theme_content_complete(&settled, &theme));
        for selected in [2, 1] {
            list.select(selected);
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            drain(&mut list, &theme);
        }
        let requests = super::super::orbit::test_art_requests();
        for selected in [2, 1, 2, 1] {
            list.select(selected);
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            drain(&mut list, &theme);
        }
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 1,
            "selection and completion must reuse immutable configuration");
        assert_eq!(super::super::orbit::test_art_requests(), requests, "completed misses were resubmitted");
        let before_eviction = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
        list.release_theme_art();
        let evicted = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
        assert!(evicted.art.selected_cover.is_none());
        assert!(!super::super::app::theme_content_complete(&evicted, &theme));
        drain(&mut list, &theme);
        let restored = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
        assert!(super::super::app::theme_content_complete(&restored, &theme));
        assert!(PreparedFrame::prepare(theme.clone(), &before_eviction).unwrap().pixels().data()
            == PreparedFrame::prepare(theme.clone(), &restored).unwrap().pixels().data());
        common::media::invalidate();
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        drain(&mut list, &theme);
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 1,
            "media library change must replan without rebuilding theme configuration");
        list.details = match &list.entries[1] {
            Entry::Game(game) => Some(crate::view::details::Details::cached(game, "")), _ => unreachable!(),
        };
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 2);
        list.open_menu().unwrap();
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 3);
        list.menu = None;
        list.disc_picker = Some(DiscPicker::new(crate::discs::Playlist {
            path: root.join("Game.m3u"), discs: vec![crate::discs::Disc { path: root.join("Game 1.chd"), available: true }], core_compatible: true,
        }));
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), builds + 4);
        list.disc_picker = None;
        list.details = None;
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        for name in ["compact-plus", "big-picture"] {
            let changed = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes").join(name)).unwrap(), &bank, 605).unwrap();
            let before = MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get);
            list.theme_models(ScreenId::Recents, &changed).unwrap();
            drain(&mut list, &changed);
            assert_eq!(MEDIA_CONFIG_BUILDS.with(std::cell::Cell::get), before + 1);
            let cached = list.theme_models(ScreenId::Recents, &changed).unwrap().remove(0);
            list.media_config = None;
            let fresh = list.theme_models(ScreenId::Recents, &changed).unwrap().remove(0);
            assert!(cached == fresh, "cached configuration changed readiness or inputs: {name}");
            assert!(PreparedFrame::prepare(changed.clone(), &cached).unwrap().pixels().data()
                == PreparedFrame::prepare(changed, &fresh).unwrap().pixels().data());
        }
        common::perf_trace::stop_probe();
        list.release_theme_art();
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn idle_details_preserves_demanded_next_cover_without_speculative_neighbors() {
        use common::theme::pack::ScreenId;
        use super::super::orbit::WarmArt;
        let (mut list, theme) = idle_queue_fixture(true);
        let Entry::Game(game) = &list.entries[1] else { unreachable!() };
        list.details = Some(crate::view::details::Details::cached(game, ""));
        list.idle_media_step(&theme, ScreenId::Recents);
        assert!(list.idle_media_work.iter().any(|task| matches!(task, WarmArt::Cover(_))
            && task.entry().path() == list.entries[2].path()), "explicit next cover was dropped");
        assert!(!list.idle_media_work.iter().any(|task|
            matches!(task, WarmArt::Hero(_) | WarmArt::Logo(_))
                && task.entry().path() != list.entries[1].path()), "queued speculative neighbour art");
        assert!(!list.idle_media_work.iter().any(|task|
            task.entry().path() == list.entries[0].path()), "queued speculative neighbour cover");
        list.release_theme_art();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn idle_details_close_rebuilds_equal_demand_and_targets_for_neighbors() {
        use common::theme::{art::Targets, pack::ScreenId};
        use super::super::{art_queue::Demand, orbit::WarmArt};
        let (mut list, theme) = idle_queue_fixture(false);
        let demand = |screen| Demand::from_components(&theme.pack().screens[&screen].components, true);
        assert_eq!(demand(ScreenId::Recents), demand(ScreenId::GameDetail));
        assert_eq!(Targets::for_screens(theme.pack(), &[ScreenId::Recents]),
            Targets::for_screens(theme.pack(), &[ScreenId::GameDetail]));
        let Entry::Game(game) = &list.entries[1] else { unreachable!() };
        list.details = Some(crate::view::details::Details::cached(game, ""));
        list.idle_media_step(&theme, ScreenId::Recents);
        assert!(list.idle_media_work.iter().all(|task|
            task.entry().path() == list.entries[1].path()));
        let detail_key = list.idle_media_key;
        list.details = None;
        list.idle_media_step(&theme, ScreenId::Recents);
        assert!(list.idle_media_work.iter().any(|task| matches!(task, WarmArt::Hero(_))
            && task.entry().path() == list.entries[0].path()), "closing Details did not resume neighbour prefetch");
        assert_ne!(list.idle_media_key, detail_key);
        list.release_theme_art();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn idle_details_only_selected_work_keeps_recents_neighbors_resident() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::ScreenId};
        use super::super::orbit::{test_art_requests, WarmArt};
        let root = common::constants::ALLIUM_GAMES_DIR.join("idle-details-selected");
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        std::fs::create_dir_all(root.join("media/logos")).unwrap();
        let paths = (0..3).map(|i| {
            let path = root.join(format!("Game {i}.chd"));
            std::fs::write(&path, []).unwrap();
            for file in [format!("Imgs/Game {i}.png"), format!("Imgs/Game {i}.hero.png"),
                format!("media/logos/Game {i}.png")] {
                let mut image = tiny_skia::Pixmap::new(64, 64).unwrap();
                image.fill(tiny_skia::Color::from_rgba8(i * 100, 90, 120, 255));
                image.save_png(root.join(file)).unwrap();
            }
            path
        }).collect();
        common::media::invalidate();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(
            ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 603).unwrap();
        let res = resources();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res, PathsSort { paths }).unwrap();
        list.select(1);
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        let drain = |list: &mut EntryList<PathsSort>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while list.idle_media_step(&theme, ScreenId::Recents) {
                if list.details.is_some() {
                    assert!(list.idle_media_work.iter().all(|task|
                        task.entry().path() == list.entries[1].path()), "queued a Details neighbour");
                }
                assert!(std::time::Instant::now() < deadline, "idle Details artwork timeout");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        drain(&mut list);
        assert_eq!(list.orbit_cache_counts(), (9, 0));
        let recents_targets = common::theme::art::Targets::for_screens(theme.pack(), &[ScreenId::Recents]);
        let recents: Vec<_> = (0..3).map(|i| list.orbit.as_ref().unwrap()
            .theme_art_for_targets(&list.entries, i, 0, 3, recents_targets)).collect();
        assert!(recents.iter().all(|art| art.selected_hero.is_some()
            && art.selected_logo.is_some() && art.selected_cover.is_some()));
        let Entry::Game(game) = &list.entries[1] else { unreachable!() };
        list.details = Some(crate::view::details::Details::cached(game, ""));
        let requests = test_art_requests();
        assert!(list.idle_media_step(&theme, ScreenId::Recents));
        assert!(list.idle_media_work.iter().all(|task| task.entry().path() == list.entries[1].path()));
        assert_eq!(list.idle_media_work.len(), 3);
        assert!(matches!(&list.idle_media_work[0], WarmArt::Logo(_)));
        assert!(matches!(&list.idle_media_work[1], WarmArt::Cover(_)));
        assert!(matches!(&list.idle_media_work[2], WarmArt::Metadata(_)));
        assert!(list.idle_lower_work.iter().all(|task| task.entry().path() == list.entries[1].path()));
        assert_eq!(list.idle_lower_work.len(), 4);
        drain(&mut list);
        assert_eq!(test_art_requests() - requests, 3, "only selected Details images need loading");
        let detail = list.theme_models(ScreenId::Recents, &theme).unwrap().pop().unwrap().art;
        assert!(detail.selected_hero.is_some() && detail.selected_logo.is_some() && detail.selected_cover.is_some());
        assert_eq!(list.orbit_cache_counts(), (12, 0));
        list.details = None;
        let requests = test_art_requests();
        drain(&mut list);
        assert_eq!(test_art_requests(), requests, "closing Details submitted a disk reload");
        for (i, before) in recents.iter().enumerate() {
            let after = list.orbit.as_ref().unwrap()
                .theme_art_for_targets(&list.entries, i, 0, 3, recents_targets);
            for (before, after) in [(&before.selected_hero, &after.selected_hero),
                (&before.selected_logo, &after.selected_logo), (&before.selected_cover, &after.selected_cover)] {
                assert!(std::sync::Arc::ptr_eq(before.as_ref().unwrap(), after.as_ref().unwrap()),
                    "Recents artwork for game {i} left RAM");
            }
        }
        list.release_theme_art();
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn artkeep_selection_while_details_open_and_generations_invalidate_both_screens() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::ScreenId};
        let root = common::constants::ALLIUM_GAMES_DIR.join("artkeep-selection");
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        std::fs::create_dir_all(root.join("media/logos")).unwrap();
        let paths: Vec<_> = (0..2).map(|i| {
            let path = root.join(format!("Game {i}.chd"));
            std::fs::write(&path, []).unwrap();
            for (file, w, h) in [(format!("Imgs/Game {i}.png"), 101, 101),
                (format!("Imgs/Game {i}.hero.png"), 360, 210),
                (format!("media/logos/Game {i}.png"), 220, 96)] {
                let mut image = tiny_skia::Pixmap::new(w, h).unwrap();
                image.fill(tiny_skia::Color::from_rgba8(i * 127, 90, 120, 255));
                image.save_png(root.join(file)).unwrap();
            }
            path
        }).collect();
        let res = resources();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let snapshot = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(snapshot(), &bank, 601).unwrap();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res, PathsSort { paths }).unwrap();
        let warm = |list: &mut EntryList<PathsSort>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                list.theme_models(ScreenId::Recents, &theme).unwrap();
                list.idle_media_step(&theme, ScreenId::Recents);
                let models = list.theme_models(ScreenId::Recents, &theme).unwrap();
                if models.iter().all(|input| input.art.selected_hero.is_some()
                    && input.art.selected_logo.is_some() && input.art.selected_cover.is_some()) {
                    return models;
                }
                assert!(std::time::Instant::now() < deadline, "artkeep selection artwork timeout");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let original = warm(&mut list).remove(0).art;
        let Entry::Game(game) = &list.entries[0] else { unreachable!() };
        list.details = Some(crate::view::details::Details::cached(game, ""));
        let old_detail = warm(&mut list).pop().unwrap().art;
        list.select(1);
        let models = list.theme_models(ScreenId::Recents, &theme).unwrap();
        for (before, after) in [
            (&original.selected_hero, &models[0].art.selected_hero),
            (&original.selected_logo, &models[0].art.selected_logo),
            (&original.selected_cover, &models[0].art.selected_cover),
            (&old_detail.selected_hero, &models[1].art.selected_hero),
            (&old_detail.selected_logo, &models[1].art.selected_logo),
            (&old_detail.selected_cover, &models[1].art.selected_cover),
        ] {
            assert!(after.as_ref().is_none_or(|after| !std::sync::Arc::ptr_eq(before.as_ref().unwrap(), after)));
        }
        let changed = warm(&mut list);
        assert_eq!(changed[1].art.selected_hero.as_ref().unwrap().pixel(0, 0).unwrap().red(), 127);
        common::media::invalidate();
        let invalidated = list.theme_models(ScreenId::Recents, &theme).unwrap();
        assert!(invalidated.iter().all(|input| input.art.selected_hero.is_none()
            && input.art.selected_logo.is_none() && input.art.selected_cover.is_none()));
        warm(&mut list);
        let new_theme = ThemeHandle::prepare(snapshot(), &bank, 602).unwrap();
        let invalidated = list.theme_models(ScreenId::Recents, &new_theme).unwrap();
        assert!(invalidated.iter().all(|input| input.art.selected_hero.is_none()
            && input.art.selected_logo.is_none() && input.art.selected_cover.is_none()));
        list.release_theme_art();
        assert_eq!(list.orbit_cache_counts(), (0, 0));
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(orbit_fixture)]
    async fn frameprep_prediction_uses_real_wrap_window_and_all_semantics_for_small_lists() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::ScreenId};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 992).unwrap();
        for len in [0, 1, 2, 12] {
            let paths = (0..len).map(|i| PathBuf::from(format!("frameprep-model/Game {i}.gba"))).collect();
            let mut list = EntryList::new(Rect::new(0, 54, 640, 376), resources(), PathsSort { paths }).unwrap();
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            if len < 2 {
                assert!(list.navigation_frame_model(&theme, 1).is_none());
                assert!(list.navigation_frame_model(&theme, -1).is_none());
                continue;
            }
            let (tx, _rx) = tokio::sync::mpsc::channel(8);
            for selected in 0..len {
                for direction in [1, -1] {
                    list.list.select(selected);
                    list.theme_models(ScreenId::Recents, &theme).unwrap();
                    let predicted_window = list.list.navigation_selection(direction).unwrap();
                    let mut predicted = list.navigation_frame_model(&theme, direction).unwrap();
                    list.list.handle_key_event(KeyEvent::Pressed(if direction == 1 { Key::Down } else { Key::Up }),
                        tx.clone(), &mut VecDeque::new()).await.unwrap();
                    assert_eq!((list.list.selected(), list.list.window_start()), predicted_window);
                    let mut actual = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
                    predicted.art = Default::default(); actual.art = Default::default();
                    assert!(predicted == actual, "length {len}, selected {selected}, direction {direction}");
                }
            }
            list.release_theme_art();
        }
        common::media::invalidate();
    }

    fn semcache_assert_uncached(
        list: &mut EntryList<PathsSort>,
        theme: &common::theme::ThemeHandle,
    ) -> common::theme::frame::FrameInputs {
        use common::theme::{frame::Art, pack::{Binding, ScreenId}};
        let mut actual = list.theme_models(ScreenId::Recents, theme).unwrap().remove(0);
        actual.art = Art::default();
        let revision = actual.raster_revision.take();
        // Also retire the one-entry metadata cache so the oracle recomputes it.
        list.semantic_info = None;
        let expected = list.base_theme_model(ScreenId::Recents, theme, list.list.selected(),
            list.list.window_start(), crate::theme_adapter::capacity(theme, ScreenId::Recents, Binding::Entries));
        assert!(actual == expected, "cached semantics differ at selection {}", list.list.selected());
        actual.raster_revision = revision;
        actual
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn semcache_navigation_reuses_rows_and_recent_selections_across_scrolls() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::{Binding, ScreenId}};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        for name in ["compact-plus", "big-picture", "orbit-hero"] {
            common::media::invalidate();
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes").join(name)).unwrap(), &bank, 621).unwrap();
            let count = crate::theme_adapter::capacity(&theme, ScreenId::Recents, Binding::Entries);
            let paths = (0..RECENTS_ROW_LIMIT + 2).map(|i| PathBuf::from(format!("semcache-rows/Game {i}.gba"))).collect();
            let mut list = EntryList::new(Rect::new(0, 54, 640, 376), resources(), PathsSort { paths }).unwrap();
            let mut seen = std::collections::HashSet::new();
            // Fill a window, scroll past it, then revisit in reverse. Continue
            // beyond both budgets to check eviction without changing semantics.
            let steps = (0..count + 2).chain((0..count + 2).rev()).chain(0..list.entries.len());
            for selected in steps {
                list.select(selected);
                list.list.set_visible_count_override(Some(count)).unwrap();
                let first = list.list.window_start();
                let unseen = (first..(first + count).min(list.entries.len()))
                    .filter(|index| seen.insert(*index)).count();
                let before = SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get);
                let actual = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
                let work = SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get) - before;
                assert_eq!(work, unseen, "{name}: repeated console matching / ID hashing");
                list.semantic_info = None;
                let expected = list.base_theme_model(ScreenId::Recents, &theme, selected, first, count);
                let mut raw = actual;
                raw.art = Default::default();
                raw.raster_revision = None;
                assert!(raw == expected, "{name}: selection {selected}, window {first}");
                assert!(list.recents_semantic.len() <= RECENTS_MODEL_LIMIT);
                assert!(list.recents_rows.as_ref().unwrap().rows.len() <= RECENTS_ROW_LIMIT);
            }
            // Revisit both selections in the final window, then Up is a full
            // semantic hit, including metadata.
            list.select(list.entries.len() - 2);
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            list.select(list.entries.len() - 1);
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            let before = RECENTS_SEMANTIC_BUILDS.with(std::cell::Cell::get);
            list.select(list.entries.len() - 2);
            list.theme_models(ScreenId::Recents, &theme).unwrap();
            assert_eq!(RECENTS_SEMANTIC_BUILDS.with(std::cell::Cell::get), before);
            list.release_theme_art();
        }
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn normalized_recents_reuses_allocations_and_invalidates_every_key_field() {
        use common::theme::{fonts::FontBank, frame::Status, pack::{Binding, ScreenId},
            render::PreparedFrame, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 861).unwrap();
        let res = resources();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res.clone(),
            PathsSort { paths: (0..12).map(|i| repo.join(format!("rank6-missing-{i}.gba"))).collect() }).unwrap();
        let status = Status { clock: Some("12:34".into()), battery: Some(80), ..Default::default() };
        let fresh = |list: &mut EntryList<PathsSort>, status: &Status| {
            let mut models = list.theme_models(ScreenId::Recents, &theme).unwrap();
            super::super::app::normalize_theme_models(&theme, &mut models, &res, 0, status);
            models
        };
        let mut models = fresh(&mut list, &status);
        let rows = models[0].rows[&Binding::Entries].as_ptr();
        let stale_art = std::sync::Arc::new(tiny_skia::Pixmap::new(1, 1).unwrap());
        let weak_art = std::sync::Arc::downgrade(&stale_art);
        models[0].art.selected_logo = Some(stale_art);
        let key = list.normalized_recents_key(&theme, None, &status).unwrap();
        list.remember_normalized_recents(key.clone(), models, &theme);
        assert!(weak_art.upgrade().is_none(), "remembered models must not retain source pixels");
        let again = list.normalized_recents_key(&theme, None, &status).unwrap();
        assert!(std::sync::Arc::ptr_eq(&key.semantic.revisions.theme_digest,
            &again.semantic.revisions.theme_digest), "warm key must share digest bytes");
        let models = list.take_normalized_recents(&key, &theme).unwrap().unwrap();
        assert!(models[0].art.selected_logo.is_none(), "reuse must refresh stale artwork");
        assert_eq!(rows, models[0].rows[&Binding::Entries].as_ptr(), "repeat must move, not clone, normalized rows");
        let rebuilt = fresh(&mut list, &status);
        assert!(models == rebuilt);
        assert_eq!(PreparedFrame::prepare(theme.clone(), &models[0]).unwrap().pixels().data(),
            PreparedFrame::prepare(theme.clone(), &rebuilt[0]).unwrap().pixels().data());
        list.remember_normalized_recents(key.clone(), models, &theme);
        let mut changes = Vec::new();
        macro_rules! changed {
            ($name:literal, $($field:ident).+, $value:expr) => {{
                let mut other = key.clone(); other.$($field).+ = $value;
                changes.push(($name, other));
            }};
        }
        changed!("selection", semantic.selected, 1);
        changed!("selected path", semantic.path, Some(repo.join("other.gba")));
        changed!("window", semantic.first, 1);
        changed!("capacity", semantic.count, key.semantic.count + 1);
        changed!("image focus", semantic.image_focus, Some(Default::default()));
        let mut cloud = key.semantic.cloud_rows.clone(); cloud[0] = !cloud[0];
        changed!("cloud availability", semantic.cloud_rows, cloud);
        changed!("list", semantic.revisions.list_revision, key.semantic.revisions.list_revision + 1);
        changed!("media", semantic.revisions.media_generation, key.semantic.revisions.media_generation + 1);
        changed!("names", semantic.revisions.name_generation, key.semantic.revisions.name_generation + 1);
        changed!("metadata", semantic.revisions.metadata_generation, key.semantic.revisions.metadata_generation + 1);
        changed!("metadata adoption", semantic.revisions.adoption_generation, key.semantic.revisions.adoption_generation + 1);
        changed!("locale", semantic.revisions.locale_revision, key.semantic.revisions.locale_revision + 1);
        changed!("consoles", semantic.revisions.console_revision, key.semantic.revisions.console_revision + 1);
        changed!("language", semantic.revisions.language, "fr-FR".into());
        changed!("theme generation", semantic.revisions.theme_generation, key.semantic.revisions.theme_generation + 1);
        changed!("theme digest", semantic.revisions.theme_digest, "different".into());
        changed!("screen", semantic.revisions.screen, ScreenId::Library);
        changed!("artwork", artwork, key.artwork + 1);
        changed!("art completion", completion, key.completion + 1);
        changed!("stylesheet", stylesheet, key.stylesheet + 1);
        changed!("theme resource", theme, key.theme + 1);
        changed!("size resource", size, key.size + 1);
        changed!("geometry", rect, Rect::new(1, 54, 640, 376));
        changed!("clock", status.clock, Some("12:35".into()));
        changed!("battery", status.battery, Some(79));
        changed!("charging", status.charging, true);
        changed!("network", status.network, Some("Wi-Fi".into()));
        for (cause, changed) in changes {
            assert!(list.take_normalized_recents(&changed, &theme).unwrap().is_none(), "{cause}");
            let rebuilt = fresh(&mut list, &changed.status);
            assert!(rebuilt[0].status == changed.status, "{cause} fallback normalization");
        }
        let other_theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/big-picture")).unwrap(), &bank, theme.generation()).unwrap();
        let other = list.normalized_recents_key(&other_theme, None, &status).unwrap();
        assert_ne!(other.semantic.revisions.theme_digest, key.semantic.revisions.theme_digest,
            "same generation must still compare the digest");
        assert_eq!(other.semantic.count, crate::theme_adapter::capacity(&other_theme, ScreenId::Recents, Binding::Entries));
        assert_ne!(other.semantic.count, key.semantic.count, "fixture must change capacity");
        assert!(list.take_normalized_recents(&other, &other_theme).unwrap().is_none());
        // Exercise actual producer changes as well as every field of the exact key.
        for cause in ["status", "selection", "list", "media", "metadata", "locale", "consoles", "style", "size", "artwork", "completion"] {
            let models = fresh(&mut list, &status);
            let old = list.normalized_recents_key(&theme, None, &status).unwrap();
            list.remember_normalized_recents(old.clone(), models, &theme);
            let mut live_status = status.clone();
            match cause {
                "status" => live_status.clock = Some("23:59".into()),
                "selection" => list.select(1),
                "list" => list.list_revision += 1,
                "media" => common::media::invalidate(),
                "metadata" => crate::view::details::invalidate_metadata(),
                "locale" => res.insert(Locale::new("fr-FR")),
                "consoles" => res.insert(ConsoleMapper::new()),
                "style" => { let style = res.get::<Stylesheet>().clone(); res.insert(style); },
                "size" => res.insert(common::geom::Size::new(640, 480)),
                "artwork" => super::super::orbit::invalidate_art(),
                "completion" => super::super::orbit::visible_completion(),
                _ => unreachable!(),
            }
            let current = list.normalized_recents_key(&theme, None, &live_status).unwrap();
            assert!(current != old, "{cause} producer did not invalidate");
            assert!(list.take_normalized_recents(&current, &theme).unwrap().is_none(), "{cause} reused");
            let rebuilt = fresh(&mut list, &live_status);
            assert!(rebuilt[0].status == live_status);
        }
        list.select(0);
        let models = fresh(&mut list, &status);
        let old = list.normalized_recents_key(&theme, None, &status).unwrap();
        list.remember_normalized_recents(old.clone(), models, &theme);
        let path = list.entries[0].path().to_owned();
        list.sort(PathsSort { paths: vec![repo.join("replacement.gba")] }).unwrap();
        let current = list.normalized_recents_key(&theme, None, &status).unwrap();
        assert_ne!(list.entries[0].path(), path);
        assert_eq!(old.semantic.selected, current.semantic.selected);
        assert_ne!(old.semantic.revisions.list_revision, current.semantic.revisions.list_revision);
        assert!(list.take_normalized_recents(&current, &theme).unwrap().is_none(), "path replacement");
        list.open_menu().unwrap();
        assert!(list.normalized_recents_key(&theme, None, &status).is_none(), "overlay");
        list.menu = None;
        list.core = Some(CoreSelection { core: 0, cores: Vec::new() });
        assert!(list.normalized_recents_key(&theme, None, &status).is_none(), "core overlay");
        list.release_theme_art();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn normalized_recents_art_snapshot_hits_and_falls_back_to_identical_pixels() {
        use common::theme::{fonts::FontBank, frame::Status, pack::ScreenId,
            render::PreparedFrame, runtime::ThemeSnapshot, ThemeHandle};
        let root = std::env::temp_dir().join(format!("allium-normalized-snapshot-{}", std::process::id()));
        assert!(!root.exists());
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        let mut pixels = tiny_skia::Pixmap::new(8, 8).unwrap();
        pixels.fill(tiny_skia::Color::from_rgba8(90, 140, 30, 255));
        for i in 0..12 { pixels.save_png(root.join(format!("Imgs/Game{i}.png"))).unwrap(); }
        common::media::invalidate();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let load = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(load(), &bank, 906).unwrap();
        let other = ThemeHandle::prepare(load(), &bank, theme.generation()).unwrap();
        let res = resources();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res.clone(),
            PathsSort { paths: (0..12).map(|i| root.join(format!("Game{i}.gba"))).collect() }).unwrap();
        let status = Status::default();
        list.theme_models(ScreenId::Recents, &theme).unwrap();
        list.orbit.as_mut().unwrap().prepare_loaded_window(&list.entries, 0, 0, 12);
        let mut predicted = list.navigation_frame_model(&theme, 1).unwrap();
        super::super::app::normalize_theme_models(&theme, std::slice::from_mut(&mut predicted), &res, 0, &status);
        assert!(predicted.art.selected_cover.is_some() && !predicted.art.entries.is_empty());
        let key = list.normalized_recents_key(&theme, Some(1), &status).unwrap();
        list.remember_normalized_recents(key.clone(), vec![predicted.clone()], &theme);
        list.select(1);
        let assert_protection = super::super::orbit::refresh_tests::expect_selection_protection(
            list.orbit.as_mut().unwrap(), &list.entries[0], &list.entries[1]);
        let hits = || super::super::orbit::ART_SNAPSHOT_HITS.with(std::cell::Cell::get);
        let before = hits();
        let models = list.take_normalized_recents(&key, &theme).unwrap().unwrap();
        assert_eq!(hits(), before + 1, "snapshot must be reused");
        assert_protection(list.orbit.as_ref().unwrap());
        assert!(models[0] == predicted);
        for (cause, handle) in [("hit", &theme), ("paths", &theme), ("theme handle", &other)] {
            let mut expected = list.theme_models(ScreenId::Recents, &theme).unwrap();
            super::super::app::normalize_theme_models(&theme, &mut expected, &res, 0, &status);
            let key = list.normalized_recents_key(&theme, None, &status).unwrap();
            list.remember_normalized_recents(key.clone(), expected, &theme);
            if cause == "paths" { common::media::clear_prepared_paths(); }
            let before = hits();
            let actual = list.take_normalized_recents(&key, handle).unwrap().unwrap();
            assert_eq!(hits() - before, usize::from(cause == "hit"), "{cause}");
            let mut expected = list.theme_models(ScreenId::Recents, handle).unwrap();
            super::super::app::normalize_theme_models(handle, &mut expected, &res, 0, &status);
            assert!(actual == expected, "{cause}: artwork map and source identity");
            assert!(actual[0].art.completed_misses == expected[0].art.completed_misses, "{cause}: completed misses");
            assert_eq!(PreparedFrame::prepare(handle.clone(), &actual[0]).unwrap().pixels().data(),
                PreparedFrame::prepare(handle.clone(), &expected[0]).unwrap().pixels().data(), "{cause}: pixels");
        }
        list.release_theme_art();
        common::media::invalidate();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(feature = "perf-bench")]
    #[ignore = "explicit normalized Recents host microbenchmark"]
    #[serial_test::serial(orbit_fixture)]
    fn normalized_recents_model_bench() {
        use common::theme::{fonts::FontBank, frame::Status, pack::ScreenId, runtime::ThemeSnapshot, ThemeHandle};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 862).unwrap();
        let res = resources();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res.clone(),
            PathsSort { paths: (0..12).map(|i| repo.join(format!("rank6-bench-missing-{i}.gba"))).collect() }).unwrap();
        let status = Status { clock: Some("12:34".into()), battery: Some(80), ..Default::default() };
        let mut times = [Vec::new(), Vec::new()];
        for _ in 0..101 {
            let start = std::time::Instant::now();
            let mut models = list.theme_models(ScreenId::Recents, &theme).unwrap();
            super::super::app::normalize_theme_models(&theme, &mut models, &res, 0, &status);
            times[0].push(start.elapsed().as_nanos());
            let key = list.normalized_recents_key(&theme, None, &status).unwrap();
            list.remember_normalized_recents(key, models, &theme);
            let start = std::time::Instant::now();
            let key = list.normalized_recents_key(&theme, None, &status).unwrap();
            let models = list.take_normalized_recents(&key, &theme).unwrap().unwrap();
            times[1].push(start.elapsed().as_nanos());
            std::hint::black_box(&models);
            list.remember_normalized_recents(key, models, &theme);
        }
        for values in &mut times { values.sort_unstable(); }
        println!("NORMALIZED_RECENTS host_debug_samples=101 rebuild_median_ns={} reuse_median_ns={}", times[0][50], times[1][50]);
        list.release_theme_art();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn semcache_navigation_invalidations_and_modal_bypass_match_uncached() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::Binding};
        let root = std::env::temp_dir().join(format!("allium-semcache-inputs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        common::media::invalidate();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let snapshot = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(snapshot(), &bank, 631).unwrap();
        let res = resources();
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res.clone(),
            PathsSort { paths: (0..12).map(|i| root.join(format!("Game {i}.gba"))).collect() }).unwrap();
        semcache_assert_uncached(&mut list, &theme);
        // Exercise the real asynchronous name generation, without a list reload.
        std::fs::write(root.join("gamelist.xml"),
            "<gameList><game><path>./Game 0.gba</path><name>Renamed</name><desc>New synopsis</desc></game></gameList>").unwrap();
        let generation = common::media::name_generation();
        let Entry::Game(game) = &mut list.entries[0] else { unreachable!() };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !game.hydrate_name() {
            common::media::poll_name_indexes();
            assert!(std::time::Instant::now() < deadline, "name adoption timeout");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_ne!(common::media::name_generation(), generation);
        let renamed = semcache_assert_uncached(&mut list, &theme);
        assert_eq!(renamed.rows[&Binding::Entries][0].label, "Renamed");
        let name_revision = renamed.raster_revision.clone();
        assert!(name_revision.is_some());
        common::media::invalidate();
        let media = semcache_assert_uncached(&mut list, &theme);
        assert_ne!(media.raster_revision, name_revision);
        crate::view::details::invalidate_metadata();
        let revision_only = semcache_assert_uncached(&mut list, &theme);
        assert_ne!(revision_only.raster_revision, media.raster_revision);
        assert_eq!(revision_only.text, media.text);
        let Entry::Game(game) = &mut list.entries[0] else { unreachable!() };
        game.release_date = chrono::NaiveDate::from_ymd_opt(1999, 1, 1);
        crate::view::details::invalidate_metadata();
        let metadata = semcache_assert_uncached(&mut list, &theme);
        assert!(metadata.text[&Binding::SelectedMetadata].contains("1999"));
        assert_ne!(metadata.raster_revision, revision_only.raster_revision);
        list.entries.swap(0, 1);
        list.list_revision = list.list_revision.wrapping_add(1);
        let before_locale = semcache_assert_uncached(&mut list, &theme);
        res.insert(Locale::new("fr-FR"));
        let localized = semcache_assert_uncached(&mut list, &theme);
        assert_ne!(localized.raster_revision, before_locale.raster_revision);
        let mut consoles = ConsoleMapper::new();
        consoles.load_config().unwrap();
        res.insert(consoles);
        let consoles = semcache_assert_uncached(&mut list, &theme);
        assert!(!consoles.rows[&Binding::Entries][0].subtitle.is_empty());
        let new_theme = ThemeHandle::prepare(snapshot(), &bank, 632).unwrap();
        semcache_assert_uncached(&mut list, &new_theme);
        let other_theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/big-picture")).unwrap(), &bank, 632).unwrap();
        semcache_assert_uncached(&mut list, &other_theme);
        list.open_menu().unwrap();
        for _ in 0..2 {
            let before = SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get);
            semcache_assert_uncached(&mut list, &theme);
            let count = crate::theme_adapter::capacity(&theme, common::theme::pack::ScreenId::Recents, Binding::Entries);
            assert_eq!(SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get) - before, 2 * count);
        }
        list.menu = None;
        list.disc_picker = Some(DiscPicker::new(crate::discs::Playlist {
            path: root.join("Game.m3u"),
            discs: vec![crate::discs::Disc { path: root.join("Game 0.gba"), available: true }],
            core_compatible: true,
        }));
        let cache_len = list.recents_semantic.len();
        for _ in 0..2 {
            let before = SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get);
            semcache_assert_uncached(&mut list, &theme);
            let count = crate::theme_adapter::capacity(&theme, common::theme::pack::ScreenId::Recents, Binding::Entries);
            assert_eq!(SEMANTIC_ROW_BUILDS.with(std::cell::Cell::get) - before, 2 * count);
            assert_eq!(list.recents_semantic.len(), cache_len, "bypass must not clone/store raw models");
        }
        list.release_theme_art();
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    #[serial_test::serial(orbit_fixture)]
    fn semcache_cloud_state_matches_uncached() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle, pack::Binding};
        let (_fixture, game) = crate::network_runtime::tests::details_slot_fixture();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(), &bank, 641).unwrap();
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), resources(),
            PathsSort { paths: vec![game.path.clone()] }).unwrap();
        let status = common::theme::frame::Status::default();
        let mut previous = None;
        for cached in [true, false, true] {
            crate::network_runtime::tests::set_semcache_cached(&game.path, cached);
            let model = semcache_assert_uncached(&mut list, &theme);
            assert_eq!(model.rows[&Binding::Entries][0].value, if cached { "" } else { "Cloud" });
            let key = list.normalized_recents_key(&theme, None, &status).unwrap();
            if let Some(old) = previous {
                assert_ne!(key, old, "cloud transition must invalidate normalized models");
                assert!(list.take_normalized_recents(&key, &theme).unwrap().is_none());
            }
            list.normalized_recents.0.clear();
            list.remember_normalized_recents(key.clone(), vec![model], &theme);
            previous = Some(key);
        }
        list.release_theme_art();
    }

    #[tokio::test]
    #[serial_test::serial(orbit_fixture)]
    async fn semcache_covered_recents_matches_uncached_and_invalidates() {
        use common::theme::{fonts::FontBank, runtime::ThemeSnapshot, ThemeHandle,
            frame::Status, pack::{Binding, ScreenId}, render::PreparedFrame};
        let root = common::constants::ALLIUM_GAMES_DIR.join("semcache");
        std::fs::create_dir_all(root.join("Imgs")).unwrap();
        tiny_skia::Pixmap::new(32, 32).unwrap().save_png(root.join("Imgs/Game 0.png")).unwrap();
        std::fs::write(root.join("gamelist.xml"),
            "<gameList><game><path>./Game 0.chd</path><desc>Adopted synopsis</desc><developer>Adopted studio</developer></game></gameList>").unwrap();
        common::media::invalidate();
        let paths = (0..12).map(|i| root.join(format!("Game {i}.chd"))).collect();
        let res = resources();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let snapshot = || ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap();
        let theme = ThemeHandle::prepare(snapshot(), &bank, 611).unwrap();
        res.insert(Some(theme.clone()));
        let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res.clone(), PathsSort { paths }).unwrap();
        let builds = || RECENTS_SEMANTIC_BUILDS.with(std::cell::Cell::get);
        let base = list.theme_models(ScreenId::Recents, &theme).unwrap().remove(0);
        let Entry::Game(game) = &list.entries[0] else { unreachable!() };
        list.details = Some(crate::view::details::Details::cached(game, ""));
        let detail = list.theme_models(ScreenId::Recents, &theme).unwrap().pop().unwrap();
        assert!(PreparedFrame::prepare(theme.clone(), &detail).unwrap().is_opaque());
        let before = builds();
        let mut cached = list.theme_models_covered(ScreenId::Recents, &theme, true).unwrap();
        assert_eq!(builds(), before, "opaque Details rebuilt Recents");
        assert_eq!(cached[0].rows, base.rows);
        assert!(!list.recents_semantic.back().unwrap().input.hints.is_empty());
        assert!(list.recents_semantic.back().unwrap().input.art.selected_cover.is_none());
        let mut fresh = list.theme_models(ScreenId::Recents, &theme).unwrap();
        let art = fresh[0].art.clone();
        fresh[0] = list.base_theme_model(ScreenId::Recents, &theme, list.list.selected(),
            list.list.window_start(), crate::theme_adapter::capacity(&theme, ScreenId::Recents, Binding::Entries));
        fresh[0].art = art;
        assert_eq!(builds(), before + 1, "uncached control must rebuild");
        let status = Status { clock: Some("12:34".into()), battery: Some(80), ..Default::default() };
        super::super::app::normalize_theme_models(&theme, &mut cached, &res, 0, &status);
        super::super::app::normalize_theme_models(&theme, &mut fresh, &res, 0, &status);
        assert!(cached[0].hints.is_empty() && cached[0].suppress_hints);
        assert!(!list.recents_semantic.back().unwrap().input.hints.is_empty());
        for (actual, expected) in cached.iter().zip(&fresh) {
            assert_eq!(PreparedFrame::prepare(theme.clone(), actual).unwrap().pixels().data(),
                PreparedFrame::prepare(theme.clone(), expected).unwrap().pixels().data());
        }
        let old = PreparedFrame::prepare(theme.clone(), cached.last().unwrap()).unwrap();
        let before = builds();
        let mut changed = list.theme_models_covered(ScreenId::Recents, &theme, true).unwrap();
        let status = Status { clock: Some("23:59".into()), battery: Some(12), charging: true,
            network: Some("Wi-Fi".into()) };
        super::super::app::normalize_theme_models(&theme, &mut changed, &res, 0, &status);
        assert_eq!(builds(), before, "status change rebuilt rows");
        assert!(changed.iter().all(|model| model.status == status));
        let new = PreparedFrame::prepare(theme.clone(), changed.last().unwrap()).unwrap();
        assert_ne!(old.pixels().data(), new.pixels().data(), "new status must change visible pixels");
        for field in ["clock", "battery"] {
            let mut only = cached.last().unwrap().clone();
            if field == "clock" { only.status.clock = status.clock.clone(); }
            else { only.status.battery = status.battery; }
            assert_ne!(old.pixels().data(), PreparedFrame::prepare(theme.clone(), &only).unwrap().pixels().data(),
                "{field} must change visible pixels");
        }
        let rebuild = |list: &mut EntryList<PathsSort>, theme: &ThemeHandle, cause: &str| {
            let before = builds();
            list.theme_models_covered(ScreenId::Recents, theme, true).unwrap();
            assert_eq!(builds(), before + 1, "{cause} did not rebuild");
            let mut cached = list.theme_models_covered(ScreenId::Recents, theme, true).unwrap().remove(0);
            assert_eq!(builds(), before + 1, "{cause} did not settle");
            cached.art = Default::default();
            cached.raster_revision = None;
            list.semantic_info = None;
            let fresh = list.base_theme_model(ScreenId::Recents, theme, list.list.selected(),
                list.list.window_start(), crate::theme_adapter::capacity(theme, ScreenId::Recents, Binding::Entries));
            assert!(cached == fresh, "{cause} differs from uncached semantics");
        };
        // Metadata is adopted through the real artwork worker, independently of invalidation.
        let adoption = super::super::orbit::metadata_completion_generation();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while super::super::orbit::metadata_completion_generation() == adoption {
            list.idle_media_step(&theme, ScreenId::Recents);
            assert!(std::time::Instant::now() < deadline, "metadata adoption timeout");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        rebuild(&mut list, &theme, "metadata adoption");
        assert_eq!(list.recents_semantic.back().unwrap().input.text[&Binding::SelectedBlurb], "Adopted synopsis");
        // Settle worker completions before testing independent key changes.
        while list.idle_media_step(&theme, ScreenId::Recents) {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let settled = list.theme_models_covered(ScreenId::Recents, &theme, true).unwrap();
        assert!(settled[0].art.selected_cover.is_some(), "arriving artwork must refresh the cached raw model");
        let before = builds();
        list.orbit.as_mut().unwrap().release_art();
        let evicted = list.theme_models_covered(ScreenId::Recents, &theme, true).unwrap();
        assert!(evicted[0].art.selected_cover.is_none());
        assert_eq!(builds(), before, "artwork eviction must only refresh artwork");
        list.select(1);
        rebuild(&mut list, &theme, "selection");
        let first = list.list.window_start();
        list.select(10);
        list.select(1);
        assert_ne!(list.list.window_start(), first);
        rebuild(&mut list, &theme, "window start with unchanged selection");
        list.entries[1] = Entry::Game(Game::new(root.join("Replacement.chd")));
        rebuild(&mut list, &theme, "selected path");
        list.list_revision = list.list_revision.wrapping_add(1);
        rebuild(&mut list, &theme, "list revision");
        list.load_entries().unwrap();
        rebuild(&mut list, &theme, "reload");
        list.sort(PathsSort { paths: vec![root.join("Sorted.chd")] }).unwrap();
        rebuild(&mut list, &theme, "sort");
        common::media::invalidate();
        rebuild(&mut list, &theme, "media generation");
        crate::view::details::invalidate_metadata();
        rebuild(&mut list, &theme, "metadata generation");
        res.insert(Locale::new("fr-FR"));
        rebuild(&mut list, &theme, "locale");
        assert_eq!(list.recents_semantic.back().unwrap().input.text[&Binding::Title], res.get::<Locale>().t("tab-recents"));
        res.insert(ConsoleMapper::new());
        rebuild(&mut list, &theme, "console configuration");
        list.image_focus = Some(common::theme::frame::Focus { component: "cover".into(), item: Some("play".into()) });
        rebuild(&mut list, &theme, "image focus");
        let new_theme = ThemeHandle::prepare(snapshot(), &bank, 612).unwrap();
        rebuild(&mut list, &new_theme, "theme generation");
        let other_theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes/big-picture")).unwrap(), &bank, 612).unwrap();
        rebuild(&mut list, &other_theme, "theme identity and capacity");
        let before = builds();
        list.theme_models_covered(ScreenId::Library, &other_theme, true).unwrap();
        assert!(list.recents_semantic.is_empty(), "other screens must retire the Recents slot");
        list.theme_models_covered(ScreenId::Recents, &other_theme, true).unwrap();
        assert_eq!(builds(), before + 1, "screen change must rebuild");
        list.details = None;
        let revealed = list.theme_models_covered(ScreenId::Recents, &other_theme, true).unwrap();
        assert!(!revealed[0].suppress_hints && !revealed[0].hints.is_empty());
        list.open_menu().unwrap();
        let before = builds();
        list.theme_models_covered(ScreenId::Recents, &other_theme, true).unwrap();
        assert_eq!(builds(), before + 1, "transparent menu must rebuild");
        list.release_theme_art();
        std::fs::remove_dir_all(root).unwrap();
        common::media::invalidate();
    }

    #[test]
    fn ux4_focus_restores_identity_after_list_shift_and_duplicate_names() {
        let res = resources();
        let rect = Rect::new(0, 54, 640, 376);
        let paths: Vec<PathBuf> = (0..21)
            .map(|i| PathBuf::from(format!("Game-{i}.chd")))
            .collect();
        let mut list = EntryList::new(rect, res.clone(), PathsSort { paths }).unwrap();
        list.select(18);
        let mut saved = serde_json::to_value(list.save()).unwrap();
        // The persisted identity must survive rebuilding the sorted list.
        saved["sort"]["paths"].as_array_mut().unwrap().drain(0..2);
        let restored =
            EntryList::<PathsSort>::load(rect, res, serde_json::from_value(saved).unwrap())
                .unwrap();
        assert_eq!(restored.list.selected(), 16);
        assert_eq!(
            restored.entries[restored.list.selected()].path(),
            Path::new("Game-18.chd")
        );
        assert_eq!(restored.save().selected, 16);
    }
    #[test]
    fn ux4_recursive_selection_survives_parent_and_child_reordering() {
        let res = resources();
        let rect = Rect::new(0, 54, 640, 376);
        let mut parent = EntryList::new(
            rect,
            res.clone(),
            PathsSort {
                paths: vec!["First.chd".into(), "Second.chd".into()],
            },
        )
        .unwrap();
        parent.select(1);
        let mut child = EntryList::new(
            rect,
            res.clone(),
            PathsSort {
                paths: (0..21)
                    .map(|i| PathBuf::from(format!("Child-{i}.chd")))
                    .collect(),
            },
        )
        .unwrap();
        child.select(18);
        parent.child = Some(Box::new(child));
        let mut saved = serde_json::to_value(parent.save()).unwrap();
        saved["sort"]["paths"].as_array_mut().unwrap().reverse();
        saved["child"]["sort"]["paths"]
            .as_array_mut()
            .unwrap()
            .drain(0..2);
        let restored =
            EntryList::<PathsSort>::load(rect, res, serde_json::from_value(saved).unwrap())
                .unwrap();
        assert_eq!(restored.list.selected(), 0);
        let child = restored.child.unwrap();
        assert_eq!(child.list.selected(), 16);
        assert_eq!(child.entries[16].path(), Path::new("Child-18.chd"));
    }
    #[test]
    fn ux4_legacy_json_and_missing_path_use_clamped_index() {
        let res = resources();
        let rect = Rect::new(0, 54, 640, 376);
        for path in [None, Some("Missing.chd")] {
            let mut json = serde_json::json!({"sort":{"paths":["One.chd","Two.chd"]},"selected":99,"child":null});
            if let Some(path) = path {
                json["selected_path"] = path.into();
            }
            let restored = EntryList::<PathsSort>::load(
                rect,
                res.clone(),
                serde_json::from_value(json).unwrap(),
            )
            .unwrap();
            assert_eq!(restored.list.selected(), 1);
        }
    }

}

impl<S: Sort> EntryList<S> {
    pub fn refresh_names(&mut self) -> bool {
        let mut changed = self
            .child
            .as_mut()
            .is_some_and(|child| child.refresh_names());
        let media_generation = common::media::generation();
        let media_changed = self.media_generation != media_generation;
        if media_changed {
            for entry in &mut self.entries {
                if let Entry::Game(game) = entry
                    && matches!(game.image, crate::entry::lazy_image::LazyImage::NotFound)
                {
                    game.image = crate::entry::lazy_image::LazyImage::Unknown(game.path.clone());
                }
            }
            self.media_generation = media_generation;
        }
        let selected_path = self
            .entries
            .get(self.list.selected())
            .map(|entry| entry.path().to_path_buf());
        let names_changed = self.sort.refresh_names(&mut self.entries);
        if names_changed {
            self.display_names = DisplayNames::new(&self.entries);
            if let Some(orbit) = self.orbit.as_mut() {
                orbit.set_display_names(self.display_names.clone());
            }
            self.list_revision = self.list_revision.wrapping_add(1);
            let selected = selected_path
                .as_ref()
                .and_then(|path| self.entries.iter().position(|entry| entry.path() == path))
                .unwrap_or(self.list.selected());
            self.list.set_items(
                self.entries
                    .iter()
                    .map(|entry| match entry {
                        Entry::Game(game) => {
                            format!("{}{}", if game.favorite { "♥ " } else { "" }, self.display_names.name(entry))
                        }
                        _ => self.display_names.name(entry).to_string(),
                    })
                    .collect(),
                false,
            );
            self.list.select(selected);
        }
        if names_changed || media_changed {
            if let Some(orbit) = self.orbit.as_mut() {
                orbit.refresh_names();
            }
            self.semantic_info = None;
            self.idle_media_key = None;
            self.idle_media_work.clear();
            self.idle_lower_work.clear();
            changed = true;
        }
        if changed {
            self.set_should_draw();
        }
        changed
    }
}
