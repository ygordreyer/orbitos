use super::*;
use crate::perf_trace::{self, Count};
use crate::theme::render_navigation::tests::{arts, model, theme};

fn probe(run: impl FnOnce()) {
    let path = std::env::temp_dir().join(format!("panelcache-{:?}.jsonl", std::thread::current().id()));
    perf_trace::start_probe(path.to_str().unwrap());
    run();
    perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}
fn count(kind: Count) -> u64 {
    perf_trace::probe_counts()["measurement"]["frame"]["counters"][kind as usize].as_u64().unwrap()
}
fn draw(theme: &ThemeHandle, input: &FrameInputs, frame: Option<PreparedFrame>) -> PreparedFrame {
    perf_trace::begin_render();
    PreparedFrame::prepare_reusing(theme.clone(), input, frame).unwrap()
}
fn identical(frame: &PreparedFrame, input: &FrameInputs) {
    let cold = crate::theme::render::navigation_reference(frame.theme.clone(), input).unwrap();
    assert!(frame.pixels.data() == cold.pixels.data(), "full frame differs from immediate paint");
    assert_eq!(frame.targets, cold.targets);
    assert_eq!(frame.text_boxes, cold.text_boxes);
}
fn input(theme: &ThemeHandle, selected: usize, arts: &[Arc<Pixmap>]) -> FrameInputs {
    let mut input = model(theme, ScreenId::Recents, selected, arts);
    input.art.selected_logo = None;
    input.metadata = vec![("Platform".into(), "PlayStation".into()), ("Release".into(), "1997".into())];
    input
}

#[test]
fn panelcache_down_up_matches_full_cold_frame_and_skips_layers() {
    probe(|| {
        let arts = arts();
        for id in ["compact-plus", "big-picture", "orbit-hero", "orbit-row", "compact"] {
            for shape in ["square", "tall"] {
                if matches!(id, "compact" | "orbit-row") && shape == "tall" { continue; }
                let theme = theme(id, shape);
                let original = input(&theme, 0, &arts);
                let mut frame = draw(&theme, &original, None);
                let grouped = frame.ink_cache.plan.as_ref().is_some_and(|plan|
                    plan.operations.iter().any(|op| matches!(op.paint, Paint::Panel { .. })));
                assert_eq!(grouped, matches!(id, "compact-plus" | "big-picture"), "{id} {shape}");
                identical(&frame, &original);
                frame = draw(&theme, &input(&theme, 1, &arts), Some(frame));
                assert_eq!(count(Count::PanelMiss), u64::from(grouped));
                identical(&frame, &input(&theme, 1, &arts));
                frame = draw(&theme, &original, Some(frame));
                assert_eq!(count(Count::PanelHit), u64::from(grouped), "{id} {shape}");
                assert_eq!(count(Count::PanelMiss), 0);
                if grouped {
                    assert_eq!(count(Count::ImageHit) + count(Count::ImageMiss), 0);
                    assert_eq!(count(Count::HeroHit) + count(Count::HeroMiss), 0);
                }
                if id == "compact-plus" {
                    println!("PANELCACHE {id} {shape} hit counters={}", perf_trace::probe_counts()["measurement"]["frame"]["counters"]);
                }
                identical(&frame, &original);
                frame = draw(&theme, &original, Some(frame));
                assert!(frame.damage.is_empty());
                assert_eq!(count(Count::PanelHit) + count(Count::PanelMiss), 0);
            }
        }
    });
}

#[test]
fn panelcache_input_invalidations_miss_and_match_cold_pixels() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let mut cases = Vec::new();
        for revision in ["media:1", "name:1", "metadata:1", "adoption:1", "locale:1"] {
            let mut changed = base.clone();
            changed.raster_revision = Some(revision.into());
            cases.push(("revision without visible value change", changed));
        }
        let mut changed = base.clone();
        changed.text.insert(Binding::SelectedTitle, "Renamed game".into());
        cases.push(("name", changed));
        let mut changed = base.clone();
        changed.metadata[1].1 = "2001".into();
        cases.push(("metadata generation", changed));
        for binding in [Binding::SelectedHero, Binding::SelectedCover, Binding::SelectedLogo] {
            let mut changed = base.clone();
            let source = Some(Arc::new((*arts[3]).clone()));
            match binding {
                Binding::SelectedHero => changed.art.selected_hero = source,
                Binding::SelectedCover => changed.art.selected_cover = source,
                _ => changed.art.selected_logo = source,
            }
            cases.push(("media generation", changed));
        }
        for value in ["Cloud only", "Downloading 43%", "Downloaded"] {
            let mut changed = base.clone();
            changed.rows.get_mut(&Binding::Entries).unwrap()[0].value = value.into();
            cases.push(("cloud/download status", changed));
        }
        let mut changed = base.clone();
        changed.text.insert(Binding::Title, "Recentes".into());
        changed.metadata[1].0 = "Lançamento".into();
        cases.push(("locale", changed));
        let mut changed = base.clone();
        changed.focus.component = "navigation".into();
        changed.focus.item = None;
        cases.push(("focus", changed));
        let mut changed = base.clone();
        changed.art.selected_cover = None;
        changed.art.entries.clear();
        cases.push(("cover removed", changed));
        for (name, changed) in cases {
            let frame = draw(&theme, &base, None);
            let frame = draw(&theme, &changed, Some(frame));
            assert_eq!(count(Count::PanelHit), 0, "{name}");
            assert_eq!(count(Count::PanelMiss), 1, "{name}");
            identical(&frame, &changed);
        }
        let frame = draw(&theme, &base, None);
        for new_theme in [theme.generation_copy(38), self::theme("compact-plus", "tall"),
            self::theme("big-picture", "square")] {
            let changed = input(&new_theme, 0, &arts);
            let switched = draw(&new_theme, &changed, Some(draw(&theme, &base, None)));
            assert_eq!(count(Count::PanelHit), 0);
            assert_eq!(count(Count::PanelMiss), 1);
            assert_eq!(switched.ink_cache.panels.0.len(), 1, "theme change did not evict");
            identical(&switched, &changed);
        }
        // Corrupt only the saved underlay. Equal child inputs still cannot reuse those pixels.
        let mut frame = frame;
        frame.ink_cache.panels.0.front_mut().unwrap().before[0] ^= 1;
        let frame = draw(&theme, &input(&theme, 1, &arts), Some(frame));
        let frame = draw(&theme, &base, Some(frame));
        assert_eq!(count(Count::PanelHit), 0);
        assert_eq!(count(Count::PanelMiss), 1);
        identical(&frame, &base);
    });
}

fn altered_theme(change: impl FnOnce(&mut Vec<Component>)) -> ThemeHandle {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let mut snapshot = ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap();
    change(&mut Arc::get_mut(&mut snapshot.0).unwrap().pack.screens.get_mut(&ScreenId::Recents).unwrap().components);
    ThemeHandle::prepare(snapshot, &bank, 92).unwrap()
}

#[test]
fn panelcache_clock_is_live_and_overlap_bypasses_grouping() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let frame = draw(&theme, &base, None);
        let frame = draw(&theme, &input(&theme, 1, &arts), Some(frame));
        let mut next = base.clone();
        next.status.clock = Some("12:59".into());
        next.status.battery = Some(41);
        let frame = draw(&theme, &next, Some(frame));
        assert_eq!(count(Count::PanelHit), 1);
        identical(&frame, &next);
        for after in [false, true] {
            let theme = altered_theme(|cs| {
                let index = cs.iter().position(|c| c.kind == Kind::Status).unwrap();
                let mut clock = cs.remove(index);
                clock.rect = Rect(470, 300, 96, 60);
                cs.insert(if after { cs.len() } else { index }, clock);
            });
            let mut next = input(&theme, 0, &arts);
            let frame = draw(&theme, &next, None);
            next.status.clock = Some("13:01".into());
            let frame = draw(&theme, &next, Some(frame));
            assert!(frame.ink_cache.panels.0.is_empty());
            assert_eq!(count(Count::PanelHit) + count(Count::PanelMiss), 0);
            identical(&frame, &next);
        }
    });
}

#[test]
fn panelcache_overlays_and_details_match_cold_pixels() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let frame = draw(&theme, &base, None);
        let mut frame = draw(&theme, &input(&theme, 1, &arts), Some(frame));
        for screen in [ScreenId::ContextMenu, ScreenId::NetworkDialog, ScreenId::GameDetail] {
            let mut overlay = base.clone();
            overlay.screen = screen;
            overlay.focus = Default::default();
            overlay.text.insert(Binding::Message, "Overlay message".into());
            overlay.rows.insert(Binding::Actions, vec![Row { id: "close".into(), label: "Close".into(),
                subtitle: String::new(), value: String::new(), enabled: true }]);
            frame = draw(&theme, &overlay, Some(frame));
            assert_eq!(count(Count::PanelHit) + count(Count::PanelMiss), 0);
            identical(&frame, &overlay);
            frame = draw(&theme, &base, Some(frame));
            identical(&frame, &base);
        }
    });
}

#[test]
fn panelcache_byte_budget_evicts_lru_and_refuses_oversized_entries() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let mut frame = draw(&theme, &base, None);
        for selected in 1..10 {
            let next = input(&theme, selected, &arts);
            frame = draw(&theme, &next, Some(frame));
            assert!(frame.ink_cache.panels.0.iter().map(Panel::bytes).sum::<usize>() <= PANEL_LIMIT);
        }
        assert!(frame.ink_cache.panels.0.len() < 8, "test must hit byte budget before entry limit");
        frame = draw(&theme, &input(&theme, 8, &arts), Some(frame));
        assert_eq!(count(Count::PanelHit), 1, "recent neighbour was evicted");
        frame = draw(&theme, &base, Some(frame));
        assert_eq!(count(Count::PanelMiss), 1, "oldest selection was not evicted");
        identical(&frame, &base);
        let before = frame.ink_cache.panels.0.len();
        assert!(!frame.ink_cache.panels.reserve(PANEL_LIMIT + 1));
        assert_eq!(frame.ink_cache.panels.0.len(), before);
    });
}
