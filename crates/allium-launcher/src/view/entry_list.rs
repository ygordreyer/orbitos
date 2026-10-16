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
        let generation = crate::view::details::metadata_generation();
        if slot.as_ref().is_none_or(|info| {
            info.path != game.path
                || info.generation != generation
                || info.media_generation != common::media::generation()
        }) {
            let details = crate::view::details::Details::cached(game, platform);
            *slot = Some(Self {
                path: game.path.clone(),
                generation,
                media_generation: common::media::generation(),
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
    idle_media_key: Option<(usize, usize, usize, u64, bool, bool)>,
    idle_media_work: VecDeque<super::orbit::WarmArt>,
    detail_action: usize,
}

impl<S> EntryList<S>
where
    S: Sort,
{
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
        use super::orbit::WarmArt;
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
        let key = (
            selected,
            first,
            count,
            common::media::generation(),
            ink,
            tall,
        );
        if self.idle_media_key != Some(key) {
            self.idle_media_key = Some(key);
            self.idle_media_work =
                Orbit::navigation_art_work(&self.entries, selected, first, count);
            for i in [
                (selected + self.entries.len() - 1) % self.entries.len(),
                (selected + 1) % self.entries.len(),
                selected,
            ] {
                let e = self.entries[i].clone();
                self.idle_media_work.extend([
                    WarmArt::Metadata(e.clone()),
                    WarmArt::Cover(e.clone()),
                    WarmArt::Hero(e.clone()),
                    WarmArt::Logo(e.clone()),
                    WarmArt::OtherCover(e),
                ]);
            }
        }
        if self.idle_media_work.is_empty() {
            return false;
        }
        let orbit = self.orbit.get_or_insert_with(Orbit::new);
        orbit.set_ink_art(ink);
        orbit.set_cover_shape(tall);
        orbit.warm_art_step(&mut self.idle_media_work);
        if self.idle_media_work.is_empty() {
            self.semantic_info = None;
            self.set_should_draw();
        }
        true
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
        if !work.artwork.is_empty() {
            orbit.warm_art_step(&mut work.artwork);
            return false;
        }
        work.input.art = orbit.theme_art(&self.entries, work.selected, work.first, work.count);
        true
    }

    pub(crate) fn navigation_warm_model(
        &mut self,
        handle: &common::theme::ThemeHandle,
        direction: isize,
        mut input: common::theme::frame::FrameInputs,
    ) -> Result<Option<NavigationWarm>> {
        use common::theme::pack::{Binding, ScreenId};
        if let Some(child) = self.child.as_mut() {
            return child.navigation_warm_model(handle, direction, input);
        }
        if self.entries.len() < 2 || !self.root_tab_navigation_ready() {
            return Ok(None);
        }
        let count = crate::theme_adapter::capacity(handle, ScreenId::Library, Binding::Entries);
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
                id: crate::theme_adapter::entry_id(entry.path()),
                label: entry.name().into(),
                value: String::new(),
                enabled: true,
            })
            .collect();
        input.rows.insert(Binding::Entries, rows);
        let entry = &self.entries[selected];
        let id = crate::theme_adapter::entry_id(entry.path());
        input.focus.item = Some(id.clone());
        input.selected.insert(Binding::Entries, id);
        input
            .text
            .insert(Binding::SelectedTitle, entry.name().into());
        if let Entry::Game(game) = entry {
            let platform = self
                .res
                .get::<ConsoleMapper>()
                .get_console(entry.path())
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let _ = crate::view::details::Details::new(game, &platform);
            let info = SelectedInfo::get(&mut self.semantic_info, game, &platform);
            input
                .text
                .insert(Binding::SelectedMetadata, info.metadata.clone());
            input
                .text
                .insert(Binding::SelectedBlurb, info.blurb.clone());
            input.metadata = info.metadata_runs.clone();
        }
        let mut artwork = Orbit::navigation_art_work(&self.entries, selected, first, count);
        // Prime the current selection and both neighbours before the next input.
        let current = self.list.selected();
        for i in [
            current,
            (current + 1) % self.entries.len(),
            (current + self.entries.len() - 1) % self.entries.len(),
        ] {
            let e = self.entries[i].clone();
            artwork.extend([
                super::orbit::WarmArt::Cover(e.clone()),
                super::orbit::WarmArt::OtherCover(e.clone()),
                super::orbit::WarmArt::Hero(e.clone()),
                super::orbit::WarmArt::Logo(e),
            ]);
        }
        Ok(Some(NavigationWarm {
            input,
            artwork,
            selected,
            first,
            count,
        }))
    }

    pub(crate) fn theme_models(
        &mut self,
        screen: common::theme::pack::ScreenId,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Vec<common::theme::frame::FrameInputs>> {
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
        let mut input = FrameInputs::new(screen);
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
        if needs_art {
            orbit.prepare_window(&self.entries, selected, first, count);
            input.art = orbit.theme_art(&self.entries, selected, first, count);
        }
        let mut models = vec![input];
        if let Some(details) = &self.details {
            let mut detail = FrameInputs::new(ScreenId::GameDetail);
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
                            id: "play".into(),
                            label: "Play".into(),
                            value: String::new(),
                            enabled: true,
                        },
                        Row {
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
                detail.focus = Focus {
                    component: crate::theme_adapter::component_id(
                        handle,
                        ScreenId::GameDetail,
                        Binding::Actions,
                    ),
                    item: Some(
                        if self.detail_action == 0 {
                            "play"
                        } else {
                            "back"
                        }
                        .into(),
                    ),
                };
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
            modal.art.selected_logo = models
                .first()
                .and_then(|model| model.art.selected_logo.clone());
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
        self.list.select(index);
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
                    self.detail_action = 1 - self.detail_action;
                    self.set_should_draw();
                }
                KeyEvent::Pressed(Key::B) => {
                    self.details = None;
                    self.set_should_draw();
                }
                KeyEvent::Pressed(Key::A) => {
                    self.details = None;
                    if self.detail_action == 0
                        || !crate::theme_adapter::snapshot(&self.res)
                            .is_some_and(|h| h.pack().ink())
                    {
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
                        let mut details = crate::view::details::Details::new(game, &platform);
                        if let Ok(Some(record)) = self.res.get::<Database>().select_game(&game.path)
                        {
                            details.add_history(
                                record.play_count,
                                record.play_time.num_minutes().max(0) as u64,
                            );
                        }
                        if let Some(handle) = crate::theme_adapter::snapshot(&self.res) {
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
                    let res = self.list.handle_key_event(event, commands, bubble).await?;
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
            metadata: "metadata".into(),
            metadata_runs: vec![],
            blurb: "blurb".into(),
        });
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
            let snapshot = ThemeSnapshot::load(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../themes")
                    .join(id)
                    .canonicalize()
                    .unwrap(),
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
                while !list.navigation_warm_art_step(&mut plan) {}
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
