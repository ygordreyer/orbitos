use super::super::pack::NavGroup;
use super::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

#[derive(Clone, Default)]
pub(super) struct Metadata {
    pub targets: Vec<Target>,
    pub text_boxes: Vec<(String, Rect)>,
    pub rows: Vec<Metadata>,
}

pub(super) fn metadata_bytes(metadata: &Metadata) -> usize {
    metadata.targets.capacity() * std::mem::size_of::<Target>()
        + metadata.targets.iter().map(|t| t.component.capacity() + t.item.capacity()).sum::<usize>()
        + metadata.text_boxes.capacity() * std::mem::size_of::<(String, Rect)>()
        + metadata.text_boxes.iter().map(|(s, _)| s.capacity()).sum::<usize>()
        + metadata.rows.capacity() * std::mem::size_of::<Metadata>()
        + metadata.rows.iter().map(metadata_bytes).sum::<usize>()
}

pub(super) fn supported_screen(theme: &ThemeHandle, screen: ScreenId) -> bool {
    static COMPACT: std::sync::OnceLock<BTreeMap<ScreenId, Vec<Component>>> =
        std::sync::OnceLock::new();
    let compact = COMPACT.get_or_init(|| {
        let pack: super::super::pack::Pack =
            serde_json::from_str(include_str!("packs/compact.json")).unwrap();
        pack.screens
            .into_iter()
            .filter(|(screen, _)| eligible_screen(*screen))
            .map(|(screen, layout)| (screen, layout.components))
            .collect()
    });
    if compact.get(&screen).is_some_and(|original| {
        theme
            .pack()
            .screens
            .get(&screen)
            .is_some_and(|actual| &actual.components == original)
    }) && theme.pack().tokens.colors[&ColorRole::Background].ends_with("ff")
    {
        return true;
    }
    static LAYOUTS: std::sync::OnceLock<BTreeMap<ScreenId, Vec<Component>>> =
        std::sync::OnceLock::new();
    let layouts = LAYOUTS.get_or_init(|| {
        let pack: super::super::pack::Pack =
            serde_json::from_str(include_str!("packs/orbit.json")).unwrap();
        pack.screens
            .into_iter()
            .filter(|(screen, _)| eligible_screen(*screen))
            .map(|(screen, layout)| (screen, layout.components))
            .collect()
    });
    let Some(original) = layouts.get(&screen) else {
        return false;
    };
    let Some(actual) = theme.pack().screens.get(&screen) else {
        return false;
    };
    // Logo capability does not alter component geometry. Retain the same layouts
    // for older external packs whose title slots still use text only.
    let same_layout = |a: &[Component], b: &[Component]| {
        let normalize = |items: &[Component]| {
            items
                .iter()
                .cloned()
                .map(|mut c| {
                    c.logo = false;
                    c.logo_when_no_cover = false;
                    c
                })
                .collect::<Vec<_>>()
        };
        normalize(a) == normalize(b)
    };
    let matches = if screen == ScreenId::Library {
        let mut preview = original.clone();
        preview.insert(
            1,
            serde_json::from_str(
                r#"{"id":"next-disc","kind":"image",
            "rect":[576,296,64,128],"binding":"next_cover","fit":"cover","radius":0,
            "fade_edge":"none","fade_width":0,"blur_radius":4,"clip_shape":"left_semicircle"}"#,
            )
            .unwrap(),
        );
        same_layout(&actual.components, original) || same_layout(&actual.components, &preview)
    } else {
        let mut icon = original.clone();
        for c in &mut icon {
            if c.kind == Kind::Status {
                c.battery_style = Some(BatteryStyle::Icon);
            }
        }
        same_layout(&actual.components, original) || same_layout(&actual.components, &icon)
    };
    matches && theme.pack().tokens.colors[&ColorRole::Background].ends_with("ff")
}
#[derive(Clone, Default)]
struct ArtworkIdentity {
    logo: Option<Weak<Pixmap>>,
    hero: Option<Weak<Pixmap>>,
    cover: Option<Weak<Pixmap>>,
    next_cover: Option<Weak<Pixmap>>,
    entries: BTreeMap<String, Weak<Pixmap>>,
}
#[derive(Clone)]
pub(super) struct Inputs {
    pub model: FrameInputs,
    art: ArtworkIdentity,
}
pub(super) fn restore_model(saved: &Inputs) -> Option<FrameInputs> {
    let upgrade = |art: &Option<Weak<Pixmap>>| -> Option<Option<Arc<Pixmap>>> {
        Some(match art { Some(art) => Some(art.upgrade()?), None => None })
    };
    let mut model = saved.model.clone();
    model.art.selected_logo = upgrade(&saved.art.logo)?;
    model.art.selected_hero = upgrade(&saved.art.hero)?;
    model.art.selected_cover = upgrade(&saved.art.cover)?;
    model.art.next_cover = upgrade(&saved.art.next_cover)?;
    model.art.entries = saved
        .art
        .entries
        .iter()
        .map(|(id, pixels)| pixels.upgrade().map(|pixels| (id.clone(), pixels)))
        .collect::<Option<_>>()?;
    Some(model)
}

pub(super) fn cover_source(saved: &Inputs) -> Option<Arc<Pixmap>> {
    saved.art.cover.as_ref()?.upgrade()
}

pub(super) fn capture(input: &FrameInputs) -> Inputs {
    let mut saved = input.clone();
    saved.art = Default::default();
    saved.art.next_cover_expected = input.art.next_cover_expected;
    saved.art.completed_misses = input.art.completed_misses.clone();
    let mut art = ArtworkIdentity {
        logo: input.art.selected_logo.as_ref().map(Arc::downgrade),
        hero: input.art.selected_hero.as_ref().map(Arc::downgrade),
        cover: input.art.selected_cover.as_ref().map(Arc::downgrade),
        next_cover: input.art.next_cover.as_ref().map(Arc::downgrade),
        ..Default::default()
    };
    for (id, pixels) in &input.art.entries {
        art.entries.insert(id.clone(), Arc::downgrade(pixels));
    }
    Inputs { model: saved, art }
}
#[cfg(test)]
pub(super) fn storage_bytes(input: &Inputs) -> usize {
    allocation_bytes(&input.model) + input.art.entries.keys().map(|id| 512 + id.capacity()).sum::<usize>()
}
pub(super) fn allocation_bytes(input: &FrameInputs) -> usize {
    let rows = |rows: &Vec<Row>| rows.capacity() * std::mem::size_of::<Row>()
        + rows.iter().map(|r| r.id.capacity() + r.label.capacity() + r.subtitle.capacity() + r.value.capacity()).sum::<usize>();
    std::mem::size_of::<FrameInputs>() + input.raster_revision.as_ref().map_or(0, String::capacity)
        + input.text.iter().chain(&input.selected).map(|(_, v)| 512 + v.capacity()).sum::<usize>()
        + input.suffix_lengths.keys().map(|s| 512 + s.capacity()).sum::<usize>()
        + input.rows.iter().map(|(_, r)| 512 + rows(r)).sum::<usize>()
        + input.navigation.iter().map(|(_, r)| 512 + rows(r)).sum::<usize>()
        + input.detail_lines.capacity() * std::mem::size_of::<super::super::frame::DetailLine>()
        + input.detail_lines.iter().map(|l| l.text.capacity()).sum::<usize>()
        + [&input.metadata, &input.facts].iter().map(|v| v.capacity() * std::mem::size_of::<(String, String)>()
            + v.iter().map(|(a, b)| a.capacity() + b.capacity()).sum::<usize>()).sum::<usize>()
        + input.focus.component.capacity() + input.focus.item.as_ref().map_or(0, String::capacity)
        + input.hints.capacity() * std::mem::size_of::<super::super::frame::Hint>()
        + input.hints.iter().map(|h| h.button.capacity() + h.label.capacity()).sum::<usize>()
        + input.keyboard.capacity() * std::mem::size_of::<String>()
        + input.keyboard.iter().map(String::capacity).sum::<usize>()
        + input.status.clock.as_ref().map_or(0, String::capacity)
        + input.status.network.as_ref().map_or(0, String::capacity)
        + input.art.entries.keys().map(|id| 512 + id.capacity()).sum::<usize>()
        + input.art.completed_misses.entries.iter().map(|id| 512 + id.capacity()).sum::<usize>()
}

pub(super) fn matches(saved: &Inputs, input: &FrameInputs) -> bool {
    let _stage = crate::perf_trace::stage(crate::perf_trace::Stage::RetainedCompare);
    same_model(&saved.model, input)
        && same_art(saved.art.logo.as_ref(), input.art.selected_logo.as_ref())
        && same_art(saved.art.hero.as_ref(), input.art.selected_hero.as_ref())
        && same_art(saved.art.cover.as_ref(), input.art.selected_cover.as_ref())
        && same_art(saved.art.next_cover.as_ref(), input.art.next_cover.as_ref())
        && saved.art.entries.len() == input.art.entries.len()
        && input
            .art
            .entries
            .iter()
            .all(|(id, image)| same_art(saved.art.entries.get(id), Some(image)))
}
#[cfg(test)]
pub(super) fn matches_cloned(saved: &Inputs, input: &FrameInputs) -> bool {
    let mut model = input.clone();
    model.art = Default::default();
    model.art.next_cover_expected = input.art.next_cover_expected;
    saved.model == model
        && same_art(saved.art.logo.as_ref(), input.art.selected_logo.as_ref())
        && same_art(saved.art.hero.as_ref(), input.art.selected_hero.as_ref())
        && same_art(saved.art.cover.as_ref(), input.art.selected_cover.as_ref())
        && same_art(saved.art.next_cover.as_ref(), input.art.next_cover.as_ref())
        && saved.art.entries.len() == input.art.entries.len()
        && input
            .art
            .entries
            .iter()
            .all(|(id, image)| same_art(saved.art.entries.get(id), Some(image)))
}
fn same_text(a: &str, b: &str) -> bool {
    super::equal_words(a.as_bytes(), b.as_bytes())
}
fn same_optional_text(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_text(a, b),
        (None, None) => true,
        _ => false,
    }
}
fn same_slice<T>(a: &[T], b: &[T], same: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
}
fn same_map<K: PartialEq, V>(a: &BTreeMap<K, V>, b: &BTreeMap<K, V>, same: impl Fn(&V, &V) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((ka, a), (kb, b))| ka == kb && same(a, b))
}
fn same_rows(a: &Vec<Row>, b: &Vec<Row>) -> bool {
    same_slice(a, b, |a, b| {
        let Row { id, label, subtitle, value, enabled } = b;
        a.enabled == *enabled && same_text(&a.id, id) && same_text(&a.label, label)
            && same_text(&a.subtitle, subtitle) && same_text(&a.value, value)
    })
}
pub(super) fn same_model(a: &FrameInputs, b: &FrameInputs) -> bool {
    use super::super::frame::{DetailLine, Focus, Hint, Status};
    // Exhaustive patterns make every new model field an explicit proof decision.
    let FrameInputs { screen, raster_revision, text, suffix_lengths, rows, selected, selected_is_folder, pagination, detail_lines,
        metadata, facts, navigation, focus, hints, suppress_hints, status, progress,
        keyboard, keyboard_cursor, art } = b;
    let Focus { component, item } = focus;
    let Status { clock, battery, charging, network } = status;
    a.screen == *screen
        && a.suffix_lengths == *suffix_lengths
        && a.selected_is_folder == *selected_is_folder
        && same_map(&a.selected, selected, |a, b| same_text(a, b))
        && same_text(&a.focus.component, component) && same_optional_text(&a.focus.item, item)
        && a.pagination == *pagination && a.suppress_hints == *suppress_hints
        && a.progress == *progress && a.keyboard_cursor == *keyboard_cursor
        && a.status.battery == *battery && a.status.charging == *charging
        && a.art.next_cover_expected == art.next_cover_expected
        && same_optional_text(&a.raster_revision, raster_revision)
        && same_optional_text(&a.status.clock, clock) && same_optional_text(&a.status.network, network)
        && same_map(&a.text, text, |a, b| same_text(a, b))
        && same_map(&a.rows, rows, same_rows) && same_map(&a.navigation, navigation, same_rows)
        && same_slice(&a.detail_lines, detail_lines, |a, b| {
            let DetailLine { text, heading, gap_before } = b;
            a.heading == *heading && a.gap_before == *gap_before && same_text(&a.text, text)
        })
        && [(&a.metadata, metadata), (&a.facts, facts)].into_iter().all(|(a, b)|
            same_slice(a, b, |(ak, av), (bk, bv)| same_text(ak, bk) && same_text(av, bv)))
        && same_slice(&a.hints, hints, |a, b| {
            let Hint { button, label } = b;
            same_text(&a.button, button) && same_text(&a.label, label)
        })
        && same_slice(&a.keyboard, keyboard, |a, b| same_text(a, b))
}
fn eligible_screen(screen: ScreenId) -> bool {
    matches!(
        screen,
        ScreenId::Library
            | ScreenId::Recents
            | ScreenId::Apps
            | ScreenId::SearchResults
            | ScreenId::SettingsRoot
            | ScreenId::SettingsPage
            | ScreenId::GuideSelector
    )
}
pub(super) fn eligible(input: &FrameInputs) -> bool {
    eligible_screen(input.screen)
        && input
            .navigation
            .get(&NavGroup::LibraryActions)
            .is_none_or(Vec::is_empty)
}
fn same_art(a: Option<&Weak<Pixmap>>, b: Option<&Arc<Pixmap>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.upgrade().is_some_and(|a| Arc::ptr_eq(&a, b)),
        (None, None) => true,
        _ => false,
    }
}
pub(super) fn changed(c: &Component, previous: &Inputs, b: &FrameInputs) -> bool {
    let a = &previous.model;
    if a.suffix_lengths != b.suffix_lengths && matches!(c.kind, Kind::Text | Kind::Entries | Kind::Image) { return true; }
    if c.logo_when_no_cover
        && (previous.art.cover.is_some()
            || a.selected.get(&Binding::Entries)
                .is_some_and(|id| previous.art.entries.contains_key(id)))
            != (b.art.selected_cover.is_some()
                || b.selected.get(&Binding::Entries)
                    .is_some_and(|id| b.art.entries.contains_key(id)))
    {
        return true;
    }
    if c.logo_rect.is_some()
        && a.text.get(&Binding::Subtitle).is_some_and(|s| !s.is_empty())
            != b.text.get(&Binding::Subtitle).is_some_and(|s| !s.is_empty())
    {
        return true;
    }
    match c.kind {
        Kind::Text => {
            (c.logo && !same_art(previous.art.logo.as_ref(), b.art.selected_logo.as_ref()))
                || (c.binding != Some(Binding::ThemeName)
                    && a.text.get(&c.binding.unwrap()) != b.text.get(&c.binding.unwrap()))
        }
        Kind::Status => a.status != b.status,
        Kind::Hints => a.hints != b.hints || a.selected_is_folder != b.selected_is_folder,
        Kind::Image => match c.binding {
            Some(Binding::SelectedLogo) => {
                !same_art(previous.art.logo.as_ref(), b.art.selected_logo.as_ref())
            }
            Some(Binding::NextCover) => {
                !same_art(previous.art.next_cover.as_ref(), b.art.next_cover.as_ref())
            }
            Some(Binding::SelectedHero) => {
                !same_art(previous.art.hero.as_ref(), b.art.selected_hero.as_ref())
            }
            _ => !same_art(previous.art.cover.as_ref(), b.art.selected_cover.as_ref()),
        },
        Kind::Navigation => {
            a.navigation.get(&c.group.unwrap()) != b.navigation.get(&c.group.unwrap())
                || a.selected.get(&Binding::Navigation) != b.selected.get(&Binding::Navigation)
                || (a.focus != b.focus && (a.focus.component == c.id || b.focus.component == c.id))
        }
        Kind::Entries | Kind::SettingsRows => {
            a.rows.get(&c.binding.unwrap()) != b.rows.get(&c.binding.unwrap())
                || a.selected.get(&c.binding.unwrap()) != b.selected.get(&c.binding.unwrap())
                || a.focus != b.focus
                || !same_art(previous.art.next_cover.as_ref(), b.art.next_cover.as_ref())
                || b.rows
                    .get(&Binding::Entries)
                    .into_iter()
                    .flatten()
                    .any(|row| {
                        !same_art(
                            previous.art.entries.get(&row.id),
                            b.art.entries.get(&row.id),
                        )
                    })
        }
        _ => false,
    }
}
fn support(c: &Component) -> Option<Rect> {
    match c.kind {
        Kind::Background => None,
        Kind::Navigation
            if c.layout == Some(Arrangement::Hidden)
                || c.group == Some(NavGroup::LibraryActions) =>
        {
            None
        }
        Kind::Entries if c.layout == Some(Arrangement::Shelf) => Some(Rect(20, 234, 620, 200)),
        _ => Some(c.rect),
    }
}
fn intersects(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}
fn merge(rects: &mut Vec<Rect>, rect: Rect) {
    let mut rect = rect;
    let mut index = 0;
    while index < rects.len() {
        let other = rects[index];
        if intersects(rect, other) {
            rects.remove(index);
            let x = rect.0.min(other.0);
            let y = rect.1.min(other.1);
            rect = Rect(
                x,
                y,
                (rect.0 + rect.2).max(other.0 + other.2) - x,
                (rect.1 + rect.3).max(other.1 + other.3) - y,
            );
            index = 0;
        } else {
            index += 1;
        }
    }
    rects.push(rect);
}
fn list(c: &Component) -> bool {
    matches!(c.kind, Kind::Entries | Kind::SettingsRows) && c.layout == Some(Arrangement::List)
}
fn row_rect(c: &Component, index: usize) -> Rect {
    Rect(
        c.rect.0,
        c.rect.1 + index as u32 * (c.item_extent.unwrap() + c.gap.unwrap()),
        c.rect.2,
        c.item_extent.unwrap(),
    )
}
fn row_state(c: &Component, row: &Row, input: &FrameInputs) -> (bool, bool) {
    (
        input.focus.component == c.id && input.focus.item.as_ref() == Some(&row.id),
        input.selected.get(&c.binding.unwrap()) == Some(&row.id),
    )
}
fn list_damage(c: &Component, a: &Inputs, b: &FrameInputs, damage: &mut Vec<Rect>) {
    let old = a
        .model
        .rows
        .get(&c.binding.unwrap())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let new = b
        .rows
        .get(&c.binding.unwrap())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if old.len() != new.len() {
        merge(damage, c.rect);
        return;
    }
    for (index, (old, new)) in old.iter().zip(new).enumerate() {
        if old != new
            || a.model.suffix_len(&old.label) != b.suffix_len(&new.label)
            || row_state(c, old, &a.model) != row_state(c, new, b)
            || !same_art(a.art.entries.get(&new.id), b.art.entries.get(&new.id))
        {
            merge(damage, row_rect(c, index));
        }
    }
}
fn damage(theme: &ThemeHandle, components: &[Component], a: &Inputs, b: &FrameInputs) -> Vec<Rect> {
    let mut damage = Vec::new();
    let old = &theme.pack().screens[&a.model.screen].components;
    for c in old {
        if !components.contains(c) {
            if let Some(rect) = support(c) {
                merge(&mut damage, rect);
            }
        }
    }
    for c in components {
        if !old.contains(c) {
            if let Some(rect) = support(c) {
                merge(&mut damage, rect);
            }
        } else if list(c) {
            list_damage(c, a, b, &mut damage);
        } else if changed(c, a, b) {
            if let Some(rect) = support(c) {
                merge(&mut damage, rect);
            }
        }
    }
    loop {
        let before = damage.clone();
        for c in components {
            if list(c) {
                continue;
            }
            if let Some(rect) = support(c) {
                if damage.iter().any(|d| intersects(*d, rect)) {
                    merge(&mut damage, rect);
                }
            }
        }
        if before == damage {
            break;
        }
    }
    damage
}
impl PreparedFrame {
    pub(super) fn render_components(
        &mut self,
        input: &FrameInputs,
        incremental: bool,
    ) -> Result<()> {
        let theme = self.theme.clone();
        let components = &theme.pack().screens[&input.screen].components;
        let old_metadata = std::mem::take(&mut self.component_metadata);
        let old_components = self
            .previous_input
            .as_ref()
            .map(|saved| &theme.pack().screens[&saved.model.screen].components);
        self.damage = if incremental {
            damage(
                &theme,
                components,
                self.previous_input.as_ref().unwrap(),
                input,
            )
        } else {
            vec![Rect(0, 0, 640, 480)]
        };
        if incremental {
            let shelf_unchanged_size = self
                .previous_input
                .as_ref()
                .unwrap()
                .model
                .rows
                .get(&Binding::Entries)
                .map(Vec::len)
                == input.rows.get(&Binding::Entries).map(Vec::len);
            let color = self.color(ColorRole::Background);
            for rect in &self.damage {
                if shelf_unchanged_size && *rect == Rect(20, 234, 620, 200) {
                    continue;
                }
                crate::display::fill_rect(&mut self.pixels.as_mut(), draw_rect(*rect), color);
            }
        } else if !(self.retained_layout && eligible(input)) {
            self.pixels.clear();
            crate::perf_trace::count(crate::perf_trace::Count::ComposedPixels, 640 * 480);
        }
        self.targets.clear();
        self.text_boxes.clear();
        for c in components {
            let repaint = !incremental
                || old_components.is_none_or(|old| !old.contains(c))
                || support(c).is_some_and(|rect| self.damage.iter().any(|d| intersects(*d, rect)));
            let old = old_components
                .and_then(|components| components.iter().position(|old| old.id == c.id))
                .and_then(|index| old_metadata.get(index));
            let metadata = if self.retained_layout && list(c) {
                self.render_list(c, input, old, incremental)?
            } else if repaint {
                let targets = self.targets.len();
                let text = self.text_boxes.len();
                self.component(c, input)?;
                Metadata {
                    targets: self.targets[targets..].to_vec(),
                    text_boxes: self.text_boxes[text..].to_vec(),
                    rows: vec![],
                }
            } else {
                let metadata = old.cloned().unwrap_or_default();
                self.targets.extend(metadata.targets.iter().cloned());
                self.text_boxes.extend(metadata.text_boxes.iter().cloned());
                metadata
            };
            self.component_metadata.push(metadata);
        }
        Ok(())
    }
    fn render_list(
        &mut self,
        c: &Component,
        input: &FrameInputs,
        old: Option<&Metadata>,
        incremental: bool,
    ) -> Result<Metadata> {
        let targets = self.targets.len();
        let text = self.text_boxes.len();
        let mut metadata = Metadata::default();
        for (index, row) in input
            .rows
            .get(&c.binding.unwrap())
            .into_iter()
            .flatten()
            .enumerate()
        {
            let rect = row_rect(c, index);
            let repaint = !incremental || self.damage.iter().any(|d| intersects(*d, rect));
            let previous = old.and_then(|old| old.rows.get(index));
            let row_metadata = if repaint || previous.is_none() {
                let targets = self.targets.len();
                let text = self.text_boxes.len();
                self.row(c, row, rect, input)?;
                Metadata {
                    targets: self.targets[targets..].to_vec(),
                    text_boxes: self.text_boxes[text..].to_vec(),
                    rows: vec![],
                }
            } else {
                let saved = previous.unwrap().clone();
                self.targets.extend(saved.targets.iter().cloned());
                self.text_boxes.extend(saved.text_boxes.iter().cloned());
                saved
            };
            metadata.rows.push(row_metadata);
        }
        metadata.targets = self.targets[targets..].to_vec();
        metadata.text_boxes = self.text_boxes[text..].to_vec();
        Ok(metadata)
    }
}
