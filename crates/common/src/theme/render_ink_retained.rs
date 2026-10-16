use super::super::retained;
use super::*;

const ACTIVE_LIMIT: usize = 8 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 12 * 1024 * 1024;

#[derive(Default)]
pub(in crate::theme::render) struct Cache {
    screen: Option<ScreenId>,
    entries: Vec<Entry>,
    pub(in crate::theme::render) opaque_base: bool,
    parked: VecDeque<(ScreenId, Pixmap, Vec<Entry>, bool)>,
    declined: Vec<ScreenId>,
    #[cfg(test)]
    peak_active_bytes: usize,
    #[cfg(test)]
    peak_snapshot_bytes: usize,
}
impl Cache {
    fn active_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.before.len()).sum()
    }
    fn snapshot_bytes(&self) -> usize {
        self.active_bytes()
            + self
                .parked
                .iter()
                .map(|(_, pixels, entries, _)| {
                    pixels.data().len()
                        + entries
                            .iter()
                            .map(|entry| entry.before.len())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
    // Reserve before allocating, including the temporary final pixels used for overlap repair.
    fn admit(&mut self, additional: usize) {
        while self.snapshot_bytes() + additional > SNAPSHOT_LIMIT {
            assert!(
                self.parked.pop_back().is_some(),
                "ink snapshot admission exceeds budget"
            );
        }
        #[cfg(test)]
        {
            self.peak_snapshot_bytes = self
                .peak_snapshot_bytes
                .max(self.snapshot_bytes() + additional);
        }
    }
    fn admit_active(&mut self, additional: usize) {
        assert!(self.active_bytes() + additional <= ACTIVE_LIMIT);
        self.admit(additional);
        #[cfg(test)]
        {
            self.peak_active_bytes = self.peak_active_bytes.max(self.active_bytes() + additional);
        }
    }
}
struct Entry {
    component: Component,
    bounds: Rect,
    dependencies: retained::Inputs,
    before: Vec<u8>,
    targets: Vec<Target>,
    text: Vec<(String, Rect)>,
}
enum Paint {
    Component,
    Hero(Vec<Component>),
    ShelfCard(usize),
    ShelfPeek,
}
struct Operation<'a> {
    component: Component,
    input: std::borrow::Cow<'a, FrameInputs>,
    bounds: Rect,
    paint: Paint,
}
struct Hero {
    theme: std::sync::Weak<crate::theme::fonts::PreparedTheme>,
    source: Option<crate::theme::image_cache::SourceIdentity>,
    key: String,
    before: Vec<u8>,
    pixels: Vec<u8>,
}
const HERO_LIMIT: usize = 4 * 1024 * 1024;
thread_local! {
    static HEROES: RefCell<VecDeque<Hero>> = const { RefCell::new(VecDeque::new()) };
}
pub(super) fn union(a: Rect, b: Rect) -> Rect {
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    Rect(
        x,
        y,
        (a.0 + a.2).max(b.0 + b.2) - x,
        (a.1 + a.3).max(b.1 + b.3) - y,
    )
}
fn dependencies(c: &Component, input: &FrameInputs) -> FrameInputs {
    let mut out = FrameInputs::new(input.screen);
    if c.logo || c.binding == Some(Binding::SelectedLogo) {
        out.art.selected_logo = input.art.selected_logo.clone();
    }
    match c.kind {
        Kind::Text => {
            let binding = match c.binding.unwrap() {
                Binding::SelectedEyebrow => Binding::SelectedMetadata,
                b => b,
            };
            out.text.insert(
                binding,
                input.text.get(&binding).cloned().unwrap_or_default(),
            );
            if binding == Binding::KeyboardValue && input.focus.component == c.id {
                out.focus = input.focus.clone();
            }
            if c.logo {
                out.art.selected_logo = input.art.selected_logo.clone();
            }
            if binding == Binding::SelectedMetadata {
                out.metadata = input.metadata.clone();
            }
            if binding == Binding::SelectedBlurb {
                out.detail_lines = input.detail_lines.clone();
            }
            if binding == Binding::SelectedFacts {
                out.facts = input.facts.clone();
            }
            if binding == Binding::Pagination {
                out.rows = input.rows.clone();
                out.pagination = input.pagination;
            }
            if binding == Binding::OsdValue {
                out.progress = input.progress;
            }
        }
        Kind::Image => match c.binding {
            Some(Binding::SelectedLogo) => {
                out.text.insert(
                    Binding::SelectedTitle,
                    input
                        .text
                        .get(&Binding::SelectedTitle)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            Some(Binding::SelectedHero) => out.art.selected_hero = input.art.selected_hero.clone(),
            Some(Binding::NextCover) => out.art.next_cover = input.art.next_cover.clone(),
            _ => out.art.selected_cover = input.art.selected_cover.clone(),
        },
        Kind::Entries
        | Kind::SettingsRows
        | Kind::DialogActions
        | Kind::Navigation
        | Kind::Keyboard => {
            if let Some(binding) = c.binding {
                if let Some(rows) = input.rows.get(&binding) {
                    out.rows.insert(binding, rows.clone());
                    for row in rows {
                        if let Some(art) = input.art.entries.get(&row.id) {
                            out.art.entries.insert(row.id.clone(), art.clone());
                        }
                    }
                }
                if let Some(selected) = input.selected.get(&binding) {
                    out.selected.insert(binding, selected.clone());
                }
            }
            if c.kind == Kind::Navigation {
                if let Some(group) = c.group {
                    if let Some(rows) = input.navigation.get(&group) {
                        out.navigation.insert(group, rows.clone());
                    }
                }
                if let Some(selected) = input.selected.get(&Binding::Navigation) {
                    out.selected.insert(Binding::Navigation, selected.clone());
                }
            }
            if input.focus.component == c.id {
                let local = out
                    .rows
                    .values()
                    .flatten()
                    .any(|row| input.focus.item.as_ref() == Some(&row.id));
                if c.kind == Kind::Navigation || local {
                    out.focus = input.focus.clone();
                }
            }
            for (binding, selected) in input
                .selected
                .iter()
                .filter(|(b, _)| c.kind != Kind::Navigation && c.binding == Some(**b))
            {
                if !out
                    .rows
                    .get(binding)
                    .is_some_and(|rows| rows.iter().any(|r| &r.id == selected))
                {
                    out.selected.remove(binding);
                }
            }
            if c.layout == Some(Arrangement::Shelf) {
                out.art.next_cover = input.art.next_cover.clone();
            }
            out.keyboard = input.keyboard.clone();
            out.keyboard_cursor = input.keyboard_cursor;
        }
        Kind::Status => out.status = input.status.clone(),
        Kind::Hints => {
            out.hints = input.hints.clone();
            out.suppress_hints = input.suppress_hints;
        }
        Kind::Progress => out.progress = input.progress,
        _ => {}
    }
    if matches!(c.kind, Kind::Image | Kind::AssetImage) {
        if c.binding == Some(Binding::SelectedLogo) {
            out.art.selected_logo = input.art.selected_logo.clone();
        }
        if c.paint.as_ref().is_some_and(|p| p.action.is_some()) && input.focus.component == c.id {
            out.focus.component = c.id.clone();
        }
    }
    out
}
fn capture(pixels: &Pixmap, r: Rect) -> Vec<u8> {
    #[cfg(test)]
    CAPTURED_BYTES.with(|bytes| bytes.set(bytes.get() + (r.2 * r.3 * 4) as usize));
    let mut bytes = Vec::with_capacity((r.2 * r.3 * 4) as usize);
    capture_into(pixels, r, &mut bytes);
    bytes
}
fn capture_into(pixels: &Pixmap, r: Rect, bytes: &mut Vec<u8>) {
    bytes.clear();
    let stride = pixels.width() as usize * 4;
    for y in r.1..r.1 + r.3 {
        let at = y as usize * stride + r.0 as usize * 4;
        bytes.extend_from_slice(&pixels.data()[at..at + r.2 as usize * 4]);
    }
}

#[cfg(test)]
thread_local! {
    static CAPTURED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::theme::fonts::FontBank;
    use std::path::Path;

    #[test]
    fn logo_only_selection_change_repaints_retained_titles() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let snapshots = [
            ThemeSnapshot::builtin("orbit").unwrap(),
            ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(),
        ];
        for snapshot in snapshots {
            let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
            let mut input = FrameInputs::new(ScreenId::Library);
            input
                .text
                .insert(Binding::SelectedTitle, "Same title".into());
            let logo = |color| {
                let mut p = Pixmap::new(80, 20).unwrap();
                p.fill(color);
                Arc::new(p)
            };
            input.art.selected_logo = Some(logo(tiny_skia::Color::from_rgba8(255, 0, 0, 255)));
            let before = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let saved = retained::capture(&input);
            assert!(retained::matches(&saved, &input));
            input.art.selected_logo = Some(logo(tiny_skia::Color::from_rgba8(0, 0, 255, 255)));
            assert!(!retained::matches(&saved, &input));
            let fresh = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            assert_ne!(before.pixels.data(), fresh.pixels.data());
            let after =
                PreparedFrame::prepare_reusing(theme.clone(), &input, Some(before)).unwrap();
            assert_eq!(after.pixels.data(), fresh.pixels.data());
            input.art.selected_logo = None;
            let removed =
                PreparedFrame::prepare_reusing(theme.clone(), &input, Some(after)).unwrap();
            assert_eq!(
                removed.pixels.data(),
                PreparedFrame::prepare(theme, &input).unwrap().pixels.data()
            );
        }
    }

    fn synthetic_theme(oversized: bool) -> ThemeHandle {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut snapshot = ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap();
        let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
        let components = &mut pack.screens.get_mut(&ScreenId::Library).unwrap().components;
        if oversized {
            let mut panel = components
                .iter()
                .find(|c| c.kind == Kind::Panel)
                .unwrap()
                .clone();
            panel.rect = Rect(0, 0, 640, 480);
            panel.radius = Some(0);
            panel.paint = Some(Default::default());
            while components.len() < 64 {
                panel.id = format!("budget-panel-{}", components.len());
                components.push(panel.clone());
            }
        } else {
            let index = components
                .iter()
                .position(|c| c.kind == Kind::Status)
                .unwrap();
            let mut status = components.remove(index);
            status.rect = Rect(100, 100, 40, 24);
            status.battery_style = Some(BatteryStyle::Icon);
            components.push(status);
        }
        pack.validate().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        ThemeHandle::prepare(snapshot, &bank, 1).unwrap()
    }

    #[test]
    fn ink_review_snapshot_budget_admits_before_capture() {
        let theme = synthetic_theme(true);
        let mut input = FrameInputs::new(ScreenId::Library);
        let recents =
            PreparedFrame::prepare(theme.clone(), &FrameInputs::new(ScreenId::Recents)).unwrap();
        let mut frame = Some(
            PreparedFrame::prepare_reusing(
                theme.clone(),
                &FrameInputs::new(ScreenId::GameDetail),
                Some(recents),
            )
            .unwrap(),
        );
        for tick in 0..3 {
            input.status.clock = Some(format!("12:{tick:02}"));
            CAPTURED_BYTES.with(|bytes| bytes.set(0));
            let next = PreparedFrame::prepare_reusing(theme.clone(), &input, frame).unwrap();
            let captured = CAPTURED_BYTES.with(|bytes| bytes.get());
            assert!(
                captured <= 8 * 1024 * 1024,
                "retained capture peak: {captured}"
            );
            assert_eq!(captured, 0, "oversized screens must use the uncached path");
            assert!(next.ink_cache.entries.is_empty());
            assert!(next.ink_cache.peak_active_bytes <= ACTIVE_LIMIT);
            assert!(next.ink_cache.peak_snapshot_bytes <= SNAPSHOT_LIMIT);
            assert!(next.ink_cache.declined.contains(&ScreenId::Library));
            assert!(
                next.ink_cache
                    .parked
                    .iter()
                    .map(|(_, p, entries, _)| {
                        p.data().len() + entries.iter().map(|e| e.before.len()).sum::<usize>()
                    })
                    .sum::<usize>()
                    <= 12 * 1024 * 1024
            );
            frame = Some(next);
        }
        let mut display =
            crate::theme::render::tests::TestDisplay(frame.as_ref().unwrap().pixels.clone());
        let input = FrameInputs::new(ScreenId::Recents);
        let restored = PreparedFrame::prepare_reusing(theme.clone(), &input, frame).unwrap();
        assert_eq!(restored.damage, vec![Rect(0, 0, 640, 480)]);
        restored.paint_damage(&mut display);
        assert!(display.0.data() == PreparedFrame::prepare(theme, &input).unwrap().pixels.data());
    }

    #[test]
    fn ink_review_status_battery_cap_is_clipped_and_removed() {
        let theme = synthetic_theme(false);
        let mut input = FrameInputs::new(ScreenId::Library);
        let baseline = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        input.status.battery = Some(73);
        input.status.network = Some("online".into());
        let before = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let mut display = crate::theme::render::tests::TestDisplay(before.pixels.clone());
        input.status.battery = None;
        input.status.network = None;
        let after = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(before)).unwrap();
        let cap = ((109 * 640 + 156) * 4) as usize;
        assert_eq!(
            &after.pixels.data()[cap..cap + 8],
            &baseline.pixels.data()[cap..cap + 8],
            "old battery cap remains outside status"
        );
        assert!(
            after.pixels.data() == baseline.pixels.data(),
            "status removal must match a fresh render"
        );
        after.paint_damage(&mut display);
        assert!(display.0.data() == baseline.pixels.data());
        input.status.battery = Some(73);
        input.status.network = Some("online".into());
        let populated = PreparedFrame::prepare(theme, &input).unwrap();
        for y in 0..480 {
            for x in 0..640 {
                if !(100..140).contains(&x) || !(100..124).contains(&y) {
                    let at = ((y * 640 + x) * 4) as usize;
                    assert_eq!(
                        &populated.pixels.data()[at..at + 4],
                        &baseline.pixels.data()[at..at + 4],
                        "status painted outside component at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn ink_review_snapshot_peak_includes_parked_frames_and_overlap_scratch() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut snapshot = ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap();
        let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
        for screen in [ScreenId::Library, ScreenId::Recents, ScreenId::GameDetail] {
            let components = &mut pack.screens.get_mut(&screen).unwrap().components;
            let mut panel = components
                .iter()
                .find(|c| c.kind == Kind::Panel)
                .unwrap()
                .clone();
            panel.rect = Rect(0, 0, 640, 480);
            panel.radius = Some(0);
            panel.paint = Some(Default::default());
            panel.paint.as_mut().unwrap().opacity = Some(0.5);
            for index in 0..3 {
                panel.id = format!("memory-panel-{index}");
                components.push(panel.clone());
            }
        }
        pack.validate().unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
        let mut previous = None;
        let mut display = crate::theme::render::tests::TestDisplay(Pixmap::new(640, 480).unwrap());
        for screen in [
            ScreenId::Library,
            ScreenId::Recents,
            ScreenId::GameDetail,
            ScreenId::Library,
        ] {
            for tick in 0..2 {
                let mut input = FrameInputs::new(screen);
                input.status.clock = Some(format!("12:{tick:02}"));
                let frame =
                    PreparedFrame::prepare_reusing(theme.clone(), &input, previous).unwrap();
                assert!(frame.ink_cache.declined.is_empty());
                assert!(frame.ink_cache.peak_active_bytes <= ACTIVE_LIMIT);
                assert!(frame.ink_cache.peak_snapshot_bytes <= SNAPSHOT_LIMIT);
                frame.paint_damage(&mut display);
                let fresh = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                assert!(display.0.data() == fresh.pixels.data());
                previous = Some(frame);
            }
        }
        println!(
            "retained peaks: active={} snapshot={}",
            previous.as_ref().unwrap().ink_cache.peak_active_bytes,
            previous.as_ref().unwrap().ink_cache.peak_snapshot_bytes
        );
    }
}
fn restore(pixels: &mut Pixmap, r: Rect, data: &[u8]) {
    let stride = pixels.width() as usize * 4;
    for (row, y) in (r.1..r.1 + r.3).enumerate() {
        let at = y as usize * stride + r.0 as usize * 4;
        let src = row * r.2 as usize * 4;
        pixels.data_mut()[at..at + r.2 as usize * 4]
            .copy_from_slice(&data[src..src + r.2 as usize * 4]);
    }
}
fn intersections(r: Rect, damage: &[Rect], y: u32) -> Vec<(usize, usize)> {
    let mut spans: Vec<_> = damage
        .iter()
        .filter(|d| y >= d.1 && y < d.1 + d.3)
        .filter_map(|d| {
            let left = r.0.max(d.0);
            let right = (r.0 + r.2).min(d.0 + d.2);
            (left < right).then(|| ((left - r.0) as usize * 4, (right - r.0) as usize * 4))
        })
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = vec![];
    for (a, b) in spans {
        if let Some(last) = merged.last_mut().filter(|last| a <= last.1) {
            last.1 = last.1.max(b);
        } else {
            merged.push((a, b));
        }
    }
    merged
}
fn replay_clip(r: Rect, damage: &[Rect]) -> Vec<Rect> {
    let mut clip: Vec<Rect> = vec![];
    for y in r.1..r.1 + r.3 {
        for (left, right) in intersections(r, damage, y) {
            let x = r.0 + left as u32 / 4;
            let width = (right - left) as u32 / 4;
            if let Some(previous) = clip
                .iter_mut()
                .rev()
                .find(|c| c.0 == x && c.2 == width && c.1 + c.3 == y)
            {
                previous.3 += 1;
            } else {
                clip.push(Rect(x, y, width, 1));
            }
        }
    }
    clip
}
fn overlap(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}
fn transfer(pixels: &mut Pixmap, r: Rect, data: &[u8], damage: &[Rect], inside: bool) {
    let stride = pixels.width() as usize * 4;
    for (row, y) in (r.1..r.1 + r.3).enumerate() {
        let at = y as usize * stride + r.0 as usize * 4;
        let src = row * r.2 as usize * 4;
        let spans = intersections(r, damage, y);
        let mut left = 0;
        for (a, b) in spans
            .into_iter()
            .chain(std::iter::once((r.2 as usize * 4, r.2 as usize * 4)))
        {
            let (start, end) = if inside { (a, b) } else { (left, a) };
            pixels.data_mut()[at + start..at + end].copy_from_slice(&data[src + start..src + end]);
            left = b;
        }
    }
}
impl PreparedFrame {
    pub(in crate::theme::render) fn prewarm_ink_hero(
        &mut self,
        input: &FrameInputs,
        hero: &Component,
    ) -> Result<()> {
        let components = &self.theme.pack().screens[&self.screen].components;
        let Some(index) = components.iter().position(|c| c.id == hero.id) else {
            return Ok(());
        };
        let mut group = vec![hero.clone()];
        let mut bounds = effects::paint_bounds(hero);
        for c in &components[index + 1..] {
            if c.kind != Kind::Overlay
                || c.binding.is_some()
                || c.paint.as_ref().unwrap().gradient.is_none()
            {
                break;
            }
            bounds = union(bounds, effects::paint_bounds(c));
            group.push(c.clone());
        }
        if group.len() == 1 {
            return Ok(());
        }
        let Some(entry) = self
            .ink_cache
            .entries
            .iter()
            .find(|entry| entry.component == *hero && entry.bounds == bounds)
        else {
            return Ok(());
        };
        let saved = capture(&self.pixels, bounds);
        restore(&mut self.pixels, bounds, &entry.before);
        let result = self.ink_hero(&group, bounds, input);
        restore(&mut self.pixels, bounds, &saved);
        result
    }
    fn ink_hero(&mut self, group: &[Component], r: Rect, input: &FrameInputs) -> Result<()> {
        let weak = self.theme.weak();
        let source = input
            .art
            .selected_hero
            .as_ref()
            .map(crate::theme::image_cache::source_identity);
        let key = format!("{:?}:{:?}", self.screen, group);
        let hit = HEROES.with(|heroes| {
            let mut heroes = heroes.borrow_mut();
            let index = heroes.iter().position(|entry| {
                entry.theme.ptr_eq(&weak)
                    && entry.theme.strong_count() > 0
                    && entry.source == source
                    && entry.key == key
                    && (r.1..r.1 + r.3).enumerate().all(|(row, y)| {
                        let at = ((y * 640 + r.0) * 4) as usize;
                        let src = row * r.2 as usize * 4;
                        self.pixels.data()[at..at + r.2 as usize * 4]
                            == entry.before[src..src + r.2 as usize * 4]
                    })
            });
            let Some(index) = index else { return false };
            let entry = heroes.remove(index).unwrap();
            restore(&mut self.pixels, r, &entry.pixels);
            heroes.push_front(entry);
            true
        });
        if hit {
            crate::perf_trace::count(crate::perf_trace::Count::ComposedPixels, (r.2 * r.3) as u64);
            crate::perf_trace::count(
                crate::perf_trace::Count::InkBitmapPixels,
                (r.2 * r.3) as u64,
            );
            return Ok(());
        }
        // Include the underlay: regrouping rounded source-over layers changes integer rounding.
        let bytes = (r.2 * r.3 * 8) as usize;
        HEROES.with(|heroes| {
            let mut heroes = heroes.borrow_mut();
            heroes.retain(|entry| entry.theme.strong_count() > 0);
            while !heroes.is_empty()
                && (heroes.len() >= 8
                    || heroes
                        .iter()
                        .map(|e| e.before.len() + e.pixels.len() + e.key.len())
                        .sum::<usize>()
                        + bytes
                        + key.len()
                        > HERO_LIMIT)
            {
                heroes.pop_back();
            }
        });
        let before = capture(&self.pixels, r);
        // Cache a complete hero even when its caller will replay only a damaged region.
        with_replay_clip(None, || -> Result<()> {
            for c in group {
                self.ink_paint_component(c, input)?;
            }
            Ok(())
        })?;
        if bytes + key.len() <= HERO_LIMIT {
            let pixels = capture(&self.pixels, r);
            HEROES.with(|heroes| {
                heroes.borrow_mut().push_front(Hero {
                    theme: weak,
                    source,
                    key,
                    before,
                    pixels,
                })
            });
        }
        Ok(())
    }
    pub(super) fn render_ink_retained(
        &mut self,
        input: &FrameInputs,
        components: &[Component],
    ) -> Result<()> {
        if self.ink_cache.declined.contains(&input.screen) {
            self.ink_cache.screen = None;
            self.ink_cache.entries.clear();
            self.ink_cache.opaque_base = false;
            return self.render_ink_uncached(input, components);
        }
        let mut operations = Vec::new();
        let mut pending = components.iter().peekable();
        while let Some(c) = pending.next() {
            let rows = c.binding.and_then(|binding| input.rows.get(&binding));
            if c.kind == Kind::Image
                && c.binding == Some(Binding::SelectedHero)
                && c.paint.as_ref().unwrap().action.is_none()
            {
                let mut group = vec![c.clone()];
                let mut bounds = effects::paint_bounds(c);
                while pending.peek().is_some_and(|next| {
                    next.kind == Kind::Overlay
                        && next.binding.is_none()
                        && next.paint.as_ref().unwrap().gradient.is_some()
                }) {
                    let next = pending.next().unwrap();
                    bounds = union(bounds, effects::paint_bounds(next));
                    group.push(next.clone());
                }
                let paint = if group.len() > 1 {
                    Paint::Hero(group)
                } else {
                    Paint::Component
                };
                operations.push(Operation {
                    component: c.clone(),
                    input: std::borrow::Cow::Borrowed(input),
                    bounds,
                    paint,
                });
            } else if matches!(c.kind, Kind::Entries | Kind::SettingsRows)
                && matches!(c.layout, Some(Arrangement::List | Arrangement::Shelf))
                && c.paint.as_ref().unwrap().opacity.unwrap_or(1.0) == 1.0
                && rows.is_some_and(|rows| !rows.is_empty())
            {
                let rows = rows.unwrap();
                for (index, row) in rows
                    .iter()
                    .take(c.visible_count.unwrap_or(16) as usize)
                    .enumerate()
                {
                    let mut part = c.clone();
                    let (bounds, paint) = if c.layout == Some(Arrangement::Shelf) {
                        (
                            self.ink_shelf_bounds(c, row, index),
                            Paint::ShelfCard(index),
                        )
                    } else {
                        part.rect = Rect(
                            c.rect.0,
                            c.rect.1 + index as u32 * (c.item_extent.unwrap() + c.gap.unwrap()),
                            c.rect.2,
                            c.item_extent.unwrap(),
                        );
                        part.visible_count = Some(1);
                        (effects::paint_bounds(&part), Paint::Component)
                    };
                    let mut model = input.clone();
                    model.rows.insert(c.binding.unwrap(), vec![row.clone()]);
                    model.art.next_cover = None;
                    operations.push(Operation {
                        component: part,
                        input: std::borrow::Cow::Owned(model),
                        bounds,
                        paint,
                    });
                }
                if c.layout == Some(Arrangement::Shelf) && c.paint.as_ref().unwrap().peek.is_some()
                {
                    let mut model = input.clone();
                    model.focus = Default::default();
                    model.selected.clear();
                    operations.push(Operation {
                        component: c.clone(),
                        input: std::borrow::Cow::Owned(model),
                        bounds: self.ink_shelf_bounds(c, rows.last().unwrap(), rows.len()),
                        paint: Paint::ShelfPeek,
                    });
                }
            } else {
                operations.push(Operation {
                    component: c.clone(),
                    input: std::borrow::Cow::Borrowed(input),
                    bounds: effects::paint_bounds(c),
                    paint: Paint::Component,
                });
            }
        }
        let underlay_bytes = operations
            .iter()
            .enumerate()
            .map(|(index, op)| {
                let c = &op.component;
                if index == 0 && c.kind == Kind::Background {
                    0
                } else {
                    {
                        let r = op.bounds;
                        (r.2 * r.3 * 4) as usize
                    }
                }
            })
            .sum::<usize>();
        let scratch_bytes = operations
            .iter()
            .map(|op| {
                let r = op.bounds;
                (r.2 * r.3 * 4) as usize
            })
            .max()
            .unwrap_or(0);
        if underlay_bytes + scratch_bytes > ACTIVE_LIMIT {
            self.ink_cache.declined.push(input.screen);
            self.ink_cache.screen = None;
            self.ink_cache.entries.clear();
            self.ink_cache.opaque_base = false;
            return self.render_ink_uncached(input, components);
        }
        let mut cache = std::mem::take(&mut self.ink_cache);
        let switched = cache.screen != Some(input.screen);
        if let Some(previous) = cache.screen.filter(|screen| *screen != input.screen) {
            cache.admit(self.pixels.data().len());
            cache.parked.push_front((
                previous,
                self.pixels.clone(),
                std::mem::take(&mut cache.entries),
                cache.opaque_base,
            ));
        }
        if cache.screen != Some(input.screen) {
            if let Some(index) = cache
                .parked
                .iter()
                .position(|(screen, _, _, _)| *screen == input.screen)
            {
                let (_, pixels, entries, opaque_base) = cache.parked.remove(index).unwrap();
                self.pixels = pixels;
                cache.entries = entries;
                cache.opaque_base = opaque_base;
                cache.screen = Some(input.screen);
            }
        }
        let incremental = cache.screen == Some(input.screen)
            && cache.entries.len() == operations.len()
            && cache
                .entries
                .iter()
                .zip(&operations)
                .all(|(a, op)| a.component == op.component && a.bounds == op.bounds);
        self.targets.clear();
        self.text_boxes.clear();
        self.damage.clear();
        if !incremental {
            self.pixels.data_mut().fill(0);
            cache.entries.clear();
            cache.opaque_base = false;
            self.damage.push(Rect(0, 0, 640, 480));
        }
        cache.admit(if incremental {
            scratch_bytes
        } else {
            underlay_bytes + scratch_bytes
        });
        crate::perf_trace::render_reason(if incremental {
            "ink_retained"
        } else {
            "ink_initial"
        });
        for (index, op) in operations.iter().enumerate() {
            let c = &op.component;
            let input = op.input.as_ref();
            let r = op.bounds;
            let mut deps = dependencies(c, input);
            if matches!(op.paint, Paint::ShelfCard(_)) {
                deps.art.next_cover = None;
            }
            if matches!(op.paint, Paint::ShelfPeek) {
                deps = FrameInputs::new(input.screen);
                deps.art.next_cover = input.art.next_cover.clone();
                if let Some(row) = input
                    .rows
                    .get(&c.binding.unwrap())
                    .and_then(|rows| rows.last())
                {
                    if let Some(source) = input.art.entries.get(&row.id) {
                        // The preview has no rows, so capture its fallback outside row artwork.
                        deps.art.selected_cover = Some(source.clone());
                    }
                }
            }
            let changed =
                !incremental || !retained::matches(&cache.entries[index].dependencies, &deps);
            let affected = changed || self.damage.iter().any(|d| overlap(*d, r));
            if !affected {
                self.targets
                    .extend(cache.entries[index].targets.iter().cloned());
                self.text_boxes
                    .extend(cache.entries[index].text.iter().cloned());
                continue;
            }
            let covered = self.damage.iter().any(|d| inside(*d, r));
            let has_overlap = self.damage.iter().any(|d| overlap(*d, r));
            let old_final = (incremental && !covered && has_overlap).then(|| {
                cache.admit_active((r.2 * r.3 * 4) as usize);
                capture(&self.pixels, r)
            });
            if incremental && !covered {
                restore(&mut self.pixels, r, &cache.entries[index].before);
                if let Some(old_final) = &old_final {
                    transfer(&mut self.pixels, r, old_final, &self.damage, true);
                }
            }
            let before = if index == 0 && c.kind == Kind::Background {
                Vec::new()
            } else if incremental {
                let mut before = std::mem::take(&mut cache.entries[index].before);
                if has_overlap {
                    capture_into(&self.pixels, r, &mut before);
                }
                before
            } else {
                cache.admit_active((r.2 * r.3 * 4) as usize);
                capture(&self.pixels, r)
            };
            let targets = self.targets.len();
            let text = self.text_boxes.len();
            let clip = (incremental && !changed && !covered).then(|| replay_clip(r, &self.damage));
            with_replay_clip(clip, || -> Result<()> {
                match &op.paint {
                    Paint::Component => self.ink_paint_component(c, input)?,
                    Paint::Hero(group) => self.ink_hero(group, r, input)?,
                    Paint::ShelfCard(card) => {
                        self.ink_shelf(c, &input.rows[&c.binding.unwrap()][0], *card, input, 1.0)?
                    }
                    Paint::ShelfPeek => {
                        self.ink_shelf_peek(c, &input.rows[&c.binding.unwrap()], input)?
                    }
                }
                Ok(())
            })?;
            if !incremental && index == 0 && c.kind == Kind::Background && r == Rect(0, 0, 640, 480)
            {
                cache.opaque_base = self
                    .pixels
                    .pixels()
                    .iter()
                    .all(|pixel| pixel.alpha() == 255);
            }
            if incremental && !changed && !covered {
                transfer(
                    &mut self.pixels,
                    r,
                    old_final.as_ref().unwrap(),
                    &self.damage,
                    false,
                );
            }
            let entry = Entry {
                component: c.clone(),
                bounds: r,
                dependencies: retained::capture(&deps),
                before,
                targets: self.targets[targets..].to_vec(),
                text: self.text_boxes[text..].to_vec(),
            };
            if incremental {
                cache.entries[index] = entry;
            } else {
                cache.entries.push(entry);
            }
            if incremental && changed && !self.damage.iter().any(|d| inside(*d, r)) {
                self.damage.push(r);
            }
        }
        cache.screen = Some(input.screen);
        if switched {
            self.damage = vec![Rect(0, 0, 640, 480)];
        }
        self.ink_cache = cache;
        Ok(())
    }
}
