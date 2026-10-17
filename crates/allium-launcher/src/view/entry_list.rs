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
                metadata: details.metadata,
                metadata_runs: details.metadata_runs,
                blurb: details.blurb,
            });
        }
        slot.as_ref().unwrap()
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
    media_generation: u64,
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
    idle_media_key: Option<(usize, usize, usize, u64, bool, bool, super::art_queue::Demand)>,
    idle_media_work: VecDeque<super::orbit::WarmArt>,
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
            media_generation: common::media::generation(),
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
            idle_media_key: None,
            idle_media_work: VecDeque::new(),
            detail_action: 0,
            image_focus: None,
            theme_screen: None,
        };

        this.load_entries()?;

        Ok(this)
    }

    pub fn enable_orbit(&mut self) {
        self.orbit = Some(Orbit::new());
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
        let count =
            crate::theme_adapter::capacity(handle, screen, common::theme::pack::Binding::Entries);
        let ink = handle.pack().ink();
        let tall = handle
            .pack()
            .alignment
            .as_ref()
            .is_some_and(|a| a.cover_shape == "tall");
        let mut composed_screens = vec![screen];
        if self.details.is_some() {
            composed_screens.push(common::theme::pack::ScreenId::GameDetail);
        }
        if self.menu.is_some() {
            composed_screens.push(common::theme::pack::ScreenId::ContextMenu);
        }
        if self.disc_picker.is_some() {
            composed_screens.push(common::theme::pack::ScreenId::DiscPicker);
        }
        let components: Vec<_> = composed_screens
            .iter()
            .flat_map(|screen| &handle.pack().screens[screen].components)
            .collect();
        let demand = super::art_queue::Demand::from_components(components.iter().copied(), ink);
        let key = (
            selected,
            first,
            count,
            common::media::generation(),
            ink,
            tall,
            demand,
        );
        if self.idle_media_key != Some(key) {
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
                super::art_queue::plan(&self.entries, selected, first, count, direction, demand);
            self.orbit.get_or_insert_with(Orbit::new).cancel_selection();
        }
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        orbit.set_ink_art(ink);
        orbit.set_cover_shape(tall);
        orbit.protect_window(
            &self.entries,
            selected,
            first,
            count,
            demand.rows,
            demand.next,
        );
        let visible_art = |mut art: common::theme::frame::Art| {
            use common::theme::pack::{Arrangement, Binding, Kind};
            let image = |binding| {
                components
                    .iter()
                    .any(|c| c.kind == Kind::Image && c.binding == Some(binding))
            };
            if !image(Binding::SelectedHero) {
                art.selected_hero = None;
                art.completed_misses.selected_hero = false;
            }
            if !image(Binding::SelectedCover) {
                art.selected_cover = None;
                art.completed_misses.selected_cover = false;
            }
            if !image(Binding::SelectedLogo) && !components.iter().any(|c| c.logo) {
                art.selected_logo = None;
                art.completed_misses.selected_logo = false;
            }
            if !image(Binding::NextCover)
                && !components.iter().any(|c| {
                    c.layout == Some(Arrangement::Shelf)
                        && (!ink || c.paint.as_ref().is_some_and(|p| p.peek.is_some()))
                })
            {
                art.next_cover = None;
                art.completed_misses.next_cover = false;
            }
            if !components.iter().any(|c| {
                c.kind == Kind::Entries
                    && (c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0)
            }) {
                art.entries.clear();
                art.completed_misses.entries.clear();
            }
            art
        };
        let before = visible_art(orbit.theme_art(&self.entries, selected, first, count));
        let completed = orbit.warm_art_step(&mut self.idle_media_work);
        let pending = !self.idle_media_work.is_empty() || orbit.art_pending();
        if !completed {
            return pending;
        }
        let after = visible_art(orbit.theme_art(&self.entries, selected, first, count));
        let art_changed = before != after;
        let readiness_changed = before.completed_misses != after.completed_misses;
        let mut metadata_changed = false;
        if let Some(Entry::Game(game)) = self.entries.get(selected) {
            use common::theme::pack::Binding;
            let metadata = components
                .iter()
                .any(|c| c.binding == Some(Binding::SelectedMetadata));
            let blurb = components
                .iter()
                .any(|c| c.binding == Some(Binding::SelectedBlurb));
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
        orbit.warm_art_step(&mut work.artwork);
        if !work.artwork.is_empty() || orbit.art_pending() {
            return false;
        }
        work.input.art = orbit.theme_art(&self.entries, work.selected, work.first, work.count);
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
        let selected = (self.list.selected() as isize + direction)
            .rem_euclid(self.entries.len() as isize) as usize;
        let first = self.list.window_start();
        let first = if selected < first {
            selected
        } else if selected >= first + count {
            selected + 1 - count
        } else {
            first
        };
        let rows = self
            .entries
            .iter()
            .skip(first)
            .take(count)
            .map(|entry| common::theme::frame::Row {
                subtitle: self.res.get::<ConsoleMapper>().get_console(entry.path()).map(|c| c.name.clone()).unwrap_or_default(),
                id: crate::theme_adapter::entry_id(entry.path()),
                label: entry.name().into(),
                value: if crate::network_runtime::download_id(entry.path()).is_some() {
                    "Cloud".into()
                } else {
                    String::new()
                },
                enabled: true,
            })
            .collect();
        input.rows.insert(Binding::Entries, rows);
        input.pagination = Some(common::theme::frame::Pagination {
            page: selected / count.max(1) + 1,
            total: self.entries.len().max(1).div_ceil(count.max(1)),
        });
        let entry = &self.entries[selected];
        let id = crate::theme_adapter::entry_id(entry.path());
        input.focus.item = Some(id.clone());
        input.selected.insert(Binding::Entries, id);
        input
            .text
            .insert(Binding::SelectedTitle, entry.name().into());
        let metadata = handle.pack().screens[&input.screen].components.iter().any(|c| {
            matches!(c.binding, Some(Binding::SelectedMetadata | Binding::SelectedBlurb))
        });
        let ink = handle.pack().ink();
        input.metadata.clear();
        if let Entry::Game(game) = entry
            && metadata
        {
            let platform = self
                .res
                .get::<ConsoleMapper>()
                .get_console(entry.path())
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let info = SelectedInfo::get(&mut self.semantic_info, game, &platform);
            input
                .text
                .insert(Binding::SelectedMetadata, info.metadata.clone());
            input
                .text
                .insert(Binding::SelectedBlurb, info.blurb.clone());
            if ink {
                input.metadata = info.metadata_runs.clone();
            }
        } else {
            let platform = self.res.get::<ConsoleMapper>().get_console(entry.path())
                .map(|c| c.name.clone()).unwrap_or_default();
            input.text.insert(Binding::SelectedMetadata, platform.chars().take(480).collect());
            input.text.insert(Binding::SelectedBlurb, String::new());
        }
        let demand = super::art_queue::Demand::from_components(
            &handle.pack().screens[&input.screen].components,
            handle.pack().ink(),
        );
        let mut artwork =
            super::art_queue::plan(&self.entries, selected, first, count, direction, demand);
        // Warm only art used by the predicted frame; App prepares one raster neighbor.
        artwork.retain(|task| {
            let path = task.entry().path();
            path == self.entries[selected].path()
                || (demand.rows && self.entries[first..(first + count).min(self.entries.len())]
                    .iter().any(|entry| entry.path() == path))
                || (demand.next && self.entries.get(first + count)
                    .is_some_and(|entry| entry.path() == path))
        });
        Ok(Some(NavigationWarm {
            input,
            artwork,
            selected,
            first,
            count,
            metadata,
            ink,
        }))
    }

    pub(crate) fn theme_models(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
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
        let count = crate::theme_adapter::capacity(handle, screen, Binding::Entries);
        self.list.set_visible_count_override(Some(count))?;
        let first = self.list.window_start();
        let selected = self.list.selected();
        if self.theme_screen != Some(screen) {self.image_focus=None;}
        self.theme_screen=Some(screen);
        let mut input = FrameInputs::new(screen);
        input.pagination = Some(common::theme::frame::Pagination {page: selected / count.max(1) + 1, total: self.entries.len().max(1).div_ceil(count.max(1))});
        let locale = self.res.get::<Locale>();
        let title = match screen {
            ScreenId::Apps => locale.t("tab-apps"),
            ScreenId::Recents => locale.t("tab-recents"),
            ScreenId::SearchResults => locale.t("sort-search"),
            _ => locale.t("tab-games"),
        };
        input.text.insert(Binding::Title, title);
        input.rows.insert(
            Binding::Entries,
            self.entries
                .iter()
                .skip(first)
                .take(count)
                .map(|entry| Row {
                    subtitle: self.res.get::<ConsoleMapper>().get_console(entry.path()).map(|c|c.name.clone()).unwrap_or_default(),
                    id: crate::theme_adapter::entry_id(entry.path()),
                    label: entry.name().into(),
                    value: if crate::network_runtime::download_id(entry.path()).is_some() {
                        "Cloud".into()
                    } else {
                        String::new()
                    },
                    enabled: true,
                })
                .collect(),
        );
        if let Some(entry) = self.entries.get(selected) {
            let id = crate::theme_adapter::entry_id(entry.path());
            input.focus = Focus {
                component: crate::theme_adapter::component_id(handle, screen, Binding::Entries),
                item: Some(id.clone()),
            };
            input.selected.insert(Binding::Entries, id);
            input
                .text
                .insert(Binding::SelectedTitle, entry.name().into());
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
            let platform = self
                .res
                .get::<ConsoleMapper>()
                .get_console(entry.path())
                .map(|c| c.name.clone())
                .unwrap_or_default();
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
                input.text.insert(
                    Binding::SelectedMetadata,
                    platform.chars().take(480).collect(),
                );
                input.text.insert(Binding::SelectedBlurb, String::new());
            }
        }
        if !matches!(self.entries.get(selected), Some(Entry::Game(_))) {
            self.semantic_info = None;
        }
        drop(locale);
        let needs_art = self.details.is_some()
            || self.menu.is_some()
            || handle.pack().screens[&screen].components.iter().any(|c| {
                c.kind == common::theme::pack::Kind::Image || c.logo || c.art_size.unwrap_or(0) > 0
            });
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        orbit.set_ink_art(handle.pack().ink());
        orbit.set_cover_shape(
            handle
                .pack()
                .alignment
                .as_ref()
                .is_some_and(|a| a.cover_shape == "tall"),
        );
        if let Some(entry) = self.entries.get(selected) {
            orbit.protect_selected(entry);
        }
        if needs_art {
            orbit.prepare_window(&self.entries, selected, first, count);
            input.art = orbit.theme_art(&self.entries, selected, first, count);
        }
        if let Some(focus)=&self.image_focus {
            if Self::image_actions(handle,screen).contains(focus) {input.focus=focus.clone();}
        }
        let mut models = vec![input];
        if let Some(details) = &self.details {
            let mut detail = FrameInputs::new(ScreenId::GameDetail);
            detail.pagination = Some(common::theme::frame::Pagination { page: details.page + 1, total: details.pages.len() });
            detail.detail_lines = details.section_pages.get(details.page).cloned().unwrap_or_default();
            if let Some(entry) = self.entries.get(selected) {
                detail
                    .text
                    .insert(Binding::SelectedTitle, entry.name().into());
            }
            detail
                .text
                .insert(Binding::SelectedMetadata, details.metadata.clone());
            if handle.pack().ink() {
                detail.metadata = details.metadata_runs.clone();
                detail.facts = details.facts.clone();
                detail
                    .text
                    .insert(Binding::SelectedSource, details.source.clone());
                detail.text.insert(
                    Binding::Pagination,
                    format!("{} / {}", details.page + 1, details.pages.len()),
                );
                detail
                    .text
                    .insert(Binding::SelectedBlurb, details.pages[details.page].clone());
            } else {
                detail.text.insert(Binding::SelectedBlurb, details.text());
            }
            detail.art = models[0].art.clone();
            detail.rows.insert(Binding::Entries, vec![]);
            detail.rows.insert(
                Binding::Actions,
                if handle.pack().ink() {
                    vec![
                        Row {
                            subtitle: String::new(),
                            id: "play".into(),
                            label: "Play".into(),
                            value: String::new(),
                            enabled: true,
                        },
                        Row {
                            subtitle: String::new(),
                            id: "back".into(),
                            label: "Back".into(),
                            value: String::new(),
                            enabled: true,
                        },
                    ]
                } else {
                    vec![]
                },
            );
            if handle.pack().ink() {
                let targets=Self::detail_targets(handle);
                detail.focus=targets[self.detail_action % targets.len()].clone();
            }
            detail.hints = vec![
                Hint {
                    button: "A".into(),
                    label: "Play".into(),
                },
                Hint {
                    button: "B".into(),
                    label: "Back".into(),
                },
                Hint {
                    button: "Dpad".into(),
                    label: "More information".into(),
                },
            ];
            models.push(detail);
        }
        if let Some(menu) = self.menu.as_mut() {
            let count =
                crate::theme_adapter::capacity(handle, ScreenId::ContextMenu, Binding::Entries);
            menu.set_visible_count_override(Some(count))?;
            let locale = self.res.get::<Locale>();
            let mut modal = FrameInputs::new(ScreenId::ContextMenu);
            if let Some(model) = models.first() {
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

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        crate::view::details::invalidate_metadata();
        self.details = None;
        self.semantic_info = None;
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
        self.idle_media_key = None;
        self.idle_media_work.clear();
        crate::view::details::invalidate_metadata();
        self.semantic_info = None;
        self.entries = self
            .sort
            .entries(&self.res.get(), &self.res.get(), &self.res.get())?;
        self.media_generation = common::media::generation();
        self.list.set_items(
            self.entries
                .iter()
                .map(|e| match e {
                    Entry::Game(game) => {
                        format!("{}{}", if game.favorite { "♥ " } else { "" }, e.name())
                    }
                    _ => e.name().to_string(),
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
            Entry::App(_) | Entry::Directory(_) => {
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
                                        entry.name()
                                    ),
                                );
                            }
                            commands.send(Command::Redraw).await?;
                        }
                        MenuEntry::Launch(_) => {
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
        assert!(first.metadata.contains("Year unavailable"));
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
    fn select_directory(list: &mut EntryList<EmptySort>) {
        list.entries = vec![Entry::Directory(Directory::new(PathBuf::from("Child")))];
        list.list.set_items(vec!["Child".into()], false);
        list.semantic_info = Some(SelectedInfo {
            path: "Game.chd".into(),
            generation: 0,
            media_generation: common::media::generation(),
            adoption_generation: crate::view::orbit::metadata_completion_generation(),
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
            let selected = selected_path
                .as_ref()
                .and_then(|path| self.entries.iter().position(|entry| entry.path() == path))
                .unwrap_or(self.list.selected());
            self.list.set_items(
                self.entries
                    .iter()
                    .map(|entry| match entry {
                        Entry::Game(game) => {
                            format!("{}{}", if game.favorite { "♥ " } else { "" }, entry.name())
                        }
                        _ => entry.name().to_string(),
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
            changed = true;
        }
        if changed {
            self.set_should_draw();
        }
        changed
    }
}
