//! Bounded read-only model adapter. No command, ROM path or filesystem authority.
use super::pack::{Binding, NavGroup, ScreenId};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tiny_skia::Pixmap;

// Shared source pixels, not the number of component references to those pixels.
pub const ART_INPUT_BYTES: usize = 7 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    pub component: String,
    pub item: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub label: String,
    pub subtitle: String,
    pub value: String,
    pub enabled: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub clock: Option<String>,
    pub battery: Option<u8>,
    pub charging: bool,
    pub network: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Hint {
    pub button: String,
    pub label: String,
}
#[derive(Clone, Default)]
pub struct Art {
    pub selected_logo: Option<Arc<Pixmap>>,
    pub selected_cover: Option<Arc<Pixmap>>,
    pub next_cover: Option<Arc<Pixmap>>,
    pub selected_hero: Option<Arc<Pixmap>>,
    pub entries: BTreeMap<String, Arc<Pixmap>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pagination {
    pub page: usize,
    pub total: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailLine {
    pub text: String,
    pub heading: bool,
    pub gap_before: u32,
}
#[derive(Clone, PartialEq)]
pub struct FrameInputs {
    pub screen: ScreenId,
    pub text: BTreeMap<Binding, String>,
    pub rows: BTreeMap<Binding, Vec<Row>>,
    pub selected: BTreeMap<Binding, String>,
    pub pagination: Option<Pagination>,
    pub detail_lines: Vec<DetailLine>,
    pub metadata: Vec<(String, String)>,
    pub facts: Vec<(String, String)>,
    pub navigation: BTreeMap<NavGroup, Vec<Row>>,
    pub focus: Focus,
    pub hints: Vec<Hint>,
    pub suppress_hints: bool,
    pub status: Status,
    pub progress: Option<f32>,
    pub keyboard: Vec<String>,
    pub keyboard_cursor: (usize, usize),
    pub art: Art,
}
impl PartialEq for Art {
    fn eq(&self, other: &Self) -> bool {
        fn same(a: &Option<Arc<Pixmap>>, b: &Option<Arc<Pixmap>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        same(&self.selected_logo, &other.selected_logo)
            && same(&self.selected_cover, &other.selected_cover)
            && same(&self.next_cover, &other.next_cover)
            && same(&self.selected_hero, &other.selected_hero)
            && self.entries.len() == other.entries.len()
            && self.entries.iter().all(|(id, pixels)| {
                other
                    .entries
                    .get(id)
                    .is_some_and(|other| Arc::ptr_eq(pixels, other))
            })
    }
}
/// Controller-owned capture: callers rebuild from this without View::save or domain writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewMemory {
    pub screen: ScreenId,
    pub selected_game: Option<String>,
    pub focus: Focus,
    pub window_start: usize,
}
impl FrameInputs {
    pub fn new(screen: ScreenId) -> Self {
        Self {
            screen,
            text: BTreeMap::new(),
            rows: BTreeMap::new(),
            selected: BTreeMap::new(),
            pagination: None,
            detail_lines: vec![],
            metadata: vec![],
            facts: vec![],
            navigation: BTreeMap::new(),
            focus: Focus::default(),
            hints: vec![],
            suppress_hints: false,
            status: Status::default(),
            progress: None,
            keyboard: vec![],
            keyboard_cursor: (0, 0),
            art: Art::default(),
        }
    }
    pub fn validate(&self, theme: &super::fonts::ThemeHandle) -> Result<()> {
        ensure!(
            self.pagination
                .is_none_or(|p| p.page > 0 && p.total >= p.page),
            "Invalid pagination"
        );
        ensure!(
            self.detail_lines.len() <= 32
                && self
                    .detail_lines
                    .iter()
                    .map(|l| l.text.chars().count() + 1)
                    .sum::<usize>()
                    <= 512
                && self.detail_lines.iter().all(|l| l.gap_before <= 56),
            "Invalid detail runs"
        );
        for records in [&self.metadata, &self.facts] {
            ensure!(
                records.len() <= 8
                    && records
                        .iter()
                        .all(|(label, value)| label.chars().count() <= 64
                            && value.chars().count() <= 512),
                "Metadata run budget exceeded"
            );
        }
        for (binding, value) in &self.text {
            ensure!(
                matches!(
                    binding,
                    Binding::SelectedFacts
                        | Binding::SelectedSource
                        | Binding::SelectedEyebrow
                        | Binding::Pagination
                        | Binding::EmptyTitle
                        | Binding::EmptyHint
                        | Binding::OsdValue
                        | Binding::Title
                        | Binding::Subtitle
                        | Binding::Breadcrumb
                        | Binding::SelectedTitle
                        | Binding::SelectedMetadata
                        | Binding::SelectedBlurb
                        | Binding::ThemeName
                        | Binding::Message
                        | Binding::KeyboardValue
                        | Binding::GuideText
                ),
                "Invalid text model binding"
            );
            ensure!(
                value.chars().count()
                    <= if *binding == Binding::GuideText {
                        8192
                    } else {
                        512
                    },
                "Frame text exceeds limit"
            );
            if *binding == Binding::ThemeName {
                ensure!(
                    value == &theme.pack().name,
                    "Model active theme name differs from snapshot"
                );
            }
        }
        for (binding, rows) in &self.rows {
            ensure!(
                matches!(
                    binding,
                    Binding::Entries
                        | Binding::SettingsRows
                        | Binding::Discs
                        | Binding::StorageEntries
                        | Binding::Guides
                        | Binding::Actions
                ),
                "Invalid row binding"
            );
            validate_rows(rows, 16)?;
        }
        for (group, rows) in &self.navigation {
            validate_rows(rows, if *group == NavGroup::Primary { 4 } else { 3 })?;
        }
        ensure!(self.hints.len() <= 8, "Too many hints");
        for hint in &self.hints {
            ensure!(
                semantic_button(&hint.button),
                "Unknown semantic button"
            );
            ensure!(hint.label.chars().count() <= 64, "Hint exceeds limit");
        }
        ensure!(
            self.status.battery.is_none_or(|n| n <= 100)
                && self
                    .status
                    .clock
                    .as_ref()
                    .is_none_or(|s| s.chars().count() <= 16)
                && self
                    .status
                    .network
                    .as_ref()
                    .is_none_or(|s| s.chars().count() <= 24),
            "Invalid status"
        );
        ensure!(
            self.progress
                .is_none_or(|p| p.is_finite() && (0.0..=1.0).contains(&p)),
            "Invalid progress"
        );
        if matches!(self.screen, ScreenId::Keyboard | ScreenId::Search) {
            ensure!(
                self.keyboard.len() == 45
                    && self.keyboard_cursor.0 < 11
                    && self.keyboard_cursor.1 < 5,
                "Fixed keyboard model required"
            );
        }
        if matches!(self.screen, ScreenId::Keyboard | ScreenId::Search) && theme.pack().screens[&self.screen].components.iter().any(|c| c.paint.as_ref().is_some_and(|p| p.keyboard_layout.as_deref() == Some("qwerty"))) {
            ensure!(self.keyboard_cursor.0 < [10, 9, 7, 3, 1][self.keyboard_cursor.1], "Keyboard cursor points to inactive cell");
        }
        for key in &self.keyboard {
            ensure!(key.chars().count() <= 12, "Keyboard label too long");
        }
        let mut art_bytes = 0usize;
        let mut art_sources = BTreeSet::new();
        for image in self
            .art
            .selected_cover
            .iter()
            .chain(self.art.selected_hero.iter())
            .chain(self.art.selected_logo.iter())
            .chain(self.art.next_cover.iter())
            .chain(self.art.entries.values())
        {
            ensure!(
                image.width() <= 640 && image.height() <= 480,
                "Game art dimensions exceed limit"
            );
            if art_sources.insert(Arc::as_ptr(image)) {
                art_bytes += image.data().len();
            }
        }
        ensure!(
            self.art.entries.len() <= 16 && art_bytes <= ART_INPUT_BYTES,
            "Game art input budget exceeded"
        );
        if !self.focus.component.is_empty() {
            let c = theme
                .pack()
                .component(self.screen, &self.focus.component)
                .ok_or_else(|| anyhow::anyhow!("Unknown focused component"))?;
            let image_action = matches!(
                c.kind,
                super::pack::Kind::Image | super::pack::Kind::AssetImage
            ) && c.paint.as_ref().is_some_and(|p| p.action.is_some());
            ensure!(
                image_action
                    || matches!(
                        c.kind,
                        super::pack::Kind::Entries
                            | super::pack::Kind::SettingsRows
                            | super::pack::Kind::DialogActions
                            | super::pack::Kind::Navigation
                            | super::pack::Kind::Keyboard
                    )
                    || (theme.pack().ink() && c.binding == Some(Binding::KeyboardValue)),
                "Focused component is not interactive"
            );
            let rows = if let Some(group) = c.group {
                self.navigation.get(&group)
            } else {
                c.binding.and_then(|b| self.rows.get(&b))
            };
            if let Some(id) = &self.focus.item {
                ensure!(
                    image_action && c.paint.as_ref().and_then(|p| p.action.as_ref()) == Some(id)
                        || rows.is_some_and(|r| r
                            .iter()
                            .take(c.visible_count.unwrap_or(16) as usize)
                            .any(|v| &v.id == id && v.enabled)),
                    "Focused item absent from visible model"
                );
            }
        }
        Ok(())
    }
}
fn validate_rows(rows: &[Row], limit: usize) -> Result<()> {
    ensure!(rows.len() <= limit, "Visible row model exceeds limit");
    let mut ids = BTreeSet::new();
    for row in rows {
        ensure!(
            !row.id.is_empty()
                && row.id.len() <= 128
                && ids.insert(&row.id)
                && row.label.chars().count() <= 512
                && row.subtitle.chars().count() <= 512
                && row.value.chars().count() <= 512,
            "Invalid/duplicate row model"
        );
    }
    Ok(())
}

pub(crate) fn semantic_button(button: &str) -> bool {
    matches!(button, "A" | "B" | "X" | "Y" | "L" | "R" | "L/R" | "START" | "SELECT" | "MENU" | "Start" | "Select" | "Menu" | "Dpad" | "Up/Down" | "Left/Right" | "↑↓" | "←→" | "↑ ↓" | "← →")
}
