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
fn panelcache_artwork_damage_covers_every_changed_pixel_and_replays_on_display() {
    let arts = arts();
    for id in ["compact-plus", "big-picture"] {
        for shape in ["square", "tall"] {
            let theme = theme(id, shape);
            for entry_cover in [false, true] {
                let mut input = input(&theme, 0, &arts);
                input.art.selected_cover = None;
                input.art.selected_logo = Some(arts[1].clone());
                if !entry_cover { input.art.entries.clear(); }
                let mut frame = draw(&theme, &input, None);
                let mut display = crate::theme::render::tests::TestDisplay(frame.pixels.as_ref().to_owned());
                for binding in [Binding::SelectedCover, Binding::SelectedCover, Binding::SelectedCover,
                    Binding::SelectedLogo, Binding::SelectedHero] {
                    let before = frame.pixels.data().to_vec();
                    match binding {
                        Binding::SelectedCover => input.art.selected_cover = match &input.art.selected_cover {
                            None => Some(arts[0].clone()),
                            Some(cover) if Arc::ptr_eq(cover, &arts[0]) => Some(arts[2].clone()),
                            _ => None,
                        },
                        Binding::SelectedLogo => input.art.selected_logo = Some(arts[2].clone()),
                        _ => input.art.selected_hero = Some(arts[2].clone()),
                    }
                    frame = draw(&theme, &input, Some(frame));
                    identical(&frame, &input);
                    for (index, (old, new)) in before.chunks_exact(4).zip(frame.pixels.data().chunks_exact(4)).enumerate() {
                        let (x, y) = (index as u32 % 640, index as u32 / 640);
                        assert!(old == new || frame.damage.iter().any(|r|
                            x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3),
                            "{id} {shape} entry_cover={entry_cover} {binding:?}: uncovered change at {x},{y}: {:?}", frame.damage);
                    }
                    frame.paint_damage(&mut display);
                    assert_eq!(display.0.data(), frame.pixels.data(), "{id} {shape} {binding:?}");
                    if binding == Binding::SelectedCover && entry_cover {
                        let cover = theme.pack().screens[&ScreenId::Recents].components.iter()
                            .find(|c| c.binding == Some(Binding::SelectedCover)).unwrap();
                        assert_eq!(frame.damage, vec![effects::paint_bounds(cover)]);
                    }
                }
                input.art.selected_cover = Some(arts[0].clone());
                input.art.selected_logo = Some(arts[1].clone());
                input.text.insert(Binding::SelectedTitle, "Simultaneous title and artwork change".into());
                frame = draw(&theme, &input, Some(frame));
                identical(&frame, &input);
                frame.paint_damage(&mut display);
                assert_eq!(display.0.data(), frame.pixels.data());
            }
        }
    }
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
            assert_eq!(switched.ink_cache.panels.borrow_mut().0.len(), 1, "theme change did not evict");
            identical(&switched, &changed);
        }
        // Corrupt only the saved underlay. Equal child inputs still cannot reuse those pixels.
        let frame = frame;
        frame.ink_cache.panels.borrow_mut().0.front_mut().unwrap().before[0] ^= 1;
        frame.ink_cache.panels.borrow_mut().0.front_mut().unwrap().underlay = Rc::new(());
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
fn panelcache_cover_damage_reconstructs_later_static_decoration() {
    probe(|| {
        let decoration = Rect(448, 300, 20, 20);
        let theme = altered_theme(|cs| {
            let mut panel = cs.iter().find(|c| c.kind == Kind::Panel).unwrap().clone();
            panel.id = "later-decoration".into();
            panel.binding = None;
            panel.rect = decoration;
            panel.radius = Some(0);
            cs.push(panel);
        });
        let arts = arts();
        let mut input = input(&theme, 0, &arts);
        let cover = theme.pack().screens[&ScreenId::Recents].components.iter()
            .find(|c| c.binding == Some(Binding::SelectedCover)).unwrap();
        let cover_bounds = effects::paint_bounds(cover);
        assert!(!overlap(cover_bounds, decoration));
        let mut frame = draw(&theme, &input, None);
        let operations = &frame.ink_cache.plan.as_ref().unwrap().operations;
        let group = operations.iter().position(|op| matches!(op.paint, Paint::Panel { .. })).unwrap();
        let later = operations.iter().position(|op| op.component.id == "later-decoration").unwrap();
        assert!(later > group && inside(operations[group].bounds, decoration));
        identical(&frame, &input);
        let mut display = crate::theme::render::tests::TestDisplay(frame.pixels.as_ref().to_owned());
        for (art, hit) in [(2, false), (0, true), (2, true)] {
            let before = frame.pixels.data().to_vec();
            input.art.selected_cover = Some(arts[art].clone());
            frame = draw(&theme, &input, Some(frame));
            assert_eq!(count(Count::PanelHit), u64::from(hit));
            identical(&frame, &input);
            assert_eq!(frame.damage, vec![cover_bounds]);
            assert_ne!(before, frame.pixels.data(), "replacement must change visible pixels");
            for (index, (old, new)) in before.chunks_exact(4).zip(frame.pixels.data().chunks_exact(4)).enumerate() {
                let (x, y) = (index as u32 % 640, index as u32 / 640);
                assert!(old == new || frame.damage.iter().any(|r|
                    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3),
                    "uncovered change at {x},{y}: {:?}", frame.damage);
            }
            frame.paint_damage(&mut display);
            assert_eq!(display.0.data(), frame.pixels.data());
        }
    });
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
            assert!(frame.ink_cache.panels.borrow_mut().0.is_empty());
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
            assert!(frame.ink_cache.panels.borrow_mut().0.iter().map(Panel::bytes).sum::<usize>() <= PANEL_LIMIT);
        }
        assert!(frame.ink_cache.panels.borrow_mut().0.len() < 8, "test must hit byte budget before entry limit");
        frame = draw(&theme, &input(&theme, 8, &arts), Some(frame));
        assert_eq!(count(Count::PanelHit), 1, "recent neighbour was evicted");
        frame = draw(&theme, &base, Some(frame));
        assert_eq!(count(Count::PanelMiss), 1, "oldest selection was not evicted");
        identical(&frame, &base);
        let before = frame.ink_cache.panels.borrow_mut().0.len();
        assert!(!frame.ink_cache.panels.borrow_mut().reserve(PANEL_LIMIT + 1));
        assert_eq!(frame.ink_cache.panels.borrow_mut().0.len(), before);
    });
}

#[test]
fn panelcache_underlay_identity_fallback_and_restore_work() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let next = input(&theme, 1, &arts);
        for fallback in [false, true] {
            let frame = draw(&theme, &base, None);
            let mut frame = draw(&theme, &next, Some(frame));
            let mut store = frame.ink_cache.panels.borrow_mut();
            let panel = store.0.back_mut().unwrap();
            if fallback { panel.underlay = Rc::new(()); }
            let mut display = crate::theme::render::tests::TestDisplay(frame.pixels.as_ref().to_owned());
            let entry = frame.ink_cache.entries.iter().find(|e| e.bounds == Rect(425, 16, 206, 408)).unwrap();
            assert_eq!(Rc::ptr_eq(&panel.underlay, &entry.underlay), !fallback);
            drop(store);
            frame = draw(&theme, &base, Some(frame));
            assert_eq!(count(Count::PanelHit), 1);
            assert_eq!(count(Count::RestoreBytes), 106496 + 336192,
                "a proven hit must copy only the row underlays and final panel");
            frame.paint_damage(&mut display);
            assert_eq!(display.0.data(), frame.pixels.data());
            println!("HITPATH warm compared={} copied={} restore={}", count(Count::ComparedPixels),
                count(Count::CopiedPixels), count(Count::RestoreBytes));
            assert!(count(Count::ComparedPixels) < 110672, "dense rows were scanned");
            identical(&frame, &base);
        }
        let token = Rc::new(());
        let other = Rc::new(());
        assert!(same_underlay(&token, &token, &[1; 17], &[2; 17]), "identity must bypass the scan");
        assert!(same_underlay(&token, &other, &[1; 17], &[1; 17]));
        assert!(!same_underlay(&token, &other, &[1; 17], &[2; 17]));
        let frame = draw(&theme, &base, None);
        let frame = draw(&theme, &next, Some(frame));
        assert_eq!(count(Count::PanelMiss), 1);
        assert!(count(Count::RestoreBytes) >= 106496 + 336192);
        identical(&frame, &next);
    });
}

#[test]
fn panelcache_typed_key_covers_each_previous_debug_dependency() {
    let arts = arts();
    let theme = theme("compact-plus", "square");
    let base = input(&theme, 0, &arts);
    let descriptor: Rc<str> = "descriptors:bounds:generation:digest".into();
    let key = PanelKey::new(&descriptor, &base);
    let changes: Vec<Box<dyn Fn(&mut FrameInputs)>> = vec![
        Box::new(|i| i.raster_revision = Some("equal values, new revision".into())),
        Box::new(|i| { i.text.insert(Binding::Title, "changed".into()); }),
        Box::new(|i| i.rows.get_mut(&Binding::Entries).unwrap()[0].enabled = false),
        Box::new(|i| { i.selected.insert(Binding::Entries, "changed".into()); }),
        Box::new(|i| i.pagination = Some(crate::theme::frame::Pagination { page: 1, total: 9 })),
        Box::new(|i| i.detail_lines.push(crate::theme::frame::DetailLine { text: "line".into(), heading: true, gap_before: 3 })),
        Box::new(|i| i.metadata.push(("key".into(), "value".into()))),
        Box::new(|i| i.facts.push(("key".into(), "value".into()))),
        Box::new(|i| { i.navigation.insert(crate::theme::pack::NavGroup::Primary, vec![Row { id: "nav".into(), label: "new".into(), subtitle: String::new(), value: String::new(), enabled: true }]); }),
        Box::new(|i| i.focus.component = "changed".into()),
        Box::new(|i| i.hints.push(crate::theme::frame::Hint { button: "A".into(), label: "changed".into() })),
        Box::new(|i| i.suppress_hints = !i.suppress_hints),
        Box::new(|i| i.progress = Some(0.25)),
        Box::new(|i| i.keyboard.push("key".into())),
        Box::new(|i| i.keyboard_cursor = (9, 4)),
        Box::new(|i| i.art.next_cover_expected = !i.art.next_cover_expected),
        Box::new(|i| { i.art.entries.insert("new identity".into(), i.art.selected_cover.clone().unwrap()); }),
    ];
    for (index, change) in changes.iter().enumerate() {
        let mut changed = base.clone();
        change(&mut changed);
        assert!(PanelKey::new(&descriptor, &changed) != key, "dependency {index}");
    }
    for changed in ["other descriptor:bounds:generation:digest", "descriptors:other bounds:generation:digest",
        "descriptors:bounds:other generation:digest", "descriptors:bounds:generation:other digest"] {
        assert!(PanelKey::new(&Rc::from(changed), &base) != key);
    }
    let mut live = base.clone();
    live.status.clock = Some("23:59".into());
    live.art.completed_misses.selected_cover = true;
    assert!(PanelKey::new(&descriptor, &live) == key);
}

#[test]
fn panelcache_prior_overlap_restores_and_renews_underlay_before_hit() {
    probe(|| {
        let theme = altered_theme(|cs| {
            let mut panel = cs.iter().find(|c| c.kind == Kind::Panel).unwrap().clone();
            panel.id = "underlay-overlap".into();
            panel.rect = Rect(430, 20, 32, 32);
            panel.radius = Some(0);
            let first = cs.iter().position(|c| c.binding == Some(Binding::SelectedHero)).unwrap();
            cs.insert(first, panel);
        });
        let arts = arts();
        let base = input(&theme, 0, &arts);
        let mut frame = draw(&theme, &base, None);
        let panel_index = frame.ink_cache.plan.as_ref().unwrap().operations.iter()
            .position(|op| matches!(op.paint, Paint::Panel { .. })).unwrap();
        let token = frame.ink_cache.entries[panel_index].underlay.clone();
        // Replay an equal static underlay to force prior overlapping damage.
        let entry = frame.ink_cache.entries.iter_mut().find(|e| e.component.id == "underlay-overlap").unwrap();
        Rc::make_mut(&mut entry.dependencies).model.raster_revision = Some("force replay".into());
        frame = draw(&theme, &base, Some(frame));
        assert_eq!(count(Count::PanelHit), 1);
        assert!(!Rc::ptr_eq(&token, &frame.ink_cache.entries[panel_index].underlay));
        assert!(count(Count::RestoreBytes) > 2 * 336192, "overlap skipped required restoration");
        identical(&frame, &base);
    });
}

#[test]
fn panelcache_all_art_slots_and_expired_weak_sources_invalidate() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        for slot in 0..5 {
            let mut changed = base.clone();
            let replacement = Arc::new((*arts[0]).clone());
            match slot {
                0 => changed.art.selected_hero = Some(replacement),
                1 => changed.art.selected_logo = Some(replacement),
                2 => changed.art.selected_cover = Some(replacement),
                3 => changed.art.next_cover = Some(replacement),
                _ => { let id = changed.art.entries.keys().next().unwrap().clone(); changed.art.entries.insert(id, replacement); },
            }
            let frame = draw(&theme, &base, None);
            let frame = draw(&theme, &changed, Some(frame));
            assert_eq!(count(Count::PanelHit), 0, "art slot {slot}");
            assert_eq!(count(Count::PanelMiss), 1, "art slot {slot}");
            identical(&frame, &changed);
        }
        let mut frame = draw(&theme, &base, None);
        let descriptor = frame.ink_cache.plan.as_ref().unwrap().operations.iter().find_map(|op|
            if let Paint::Panel { key, .. } = &op.paint { Some(key.clone()) } else { None }).unwrap();
        let key = PanelKey::new(&descriptor, &base);
        let mut store = frame.ink_cache.panels.borrow_mut();
        let panel = store.0.front_mut().unwrap();
        let transient = Arc::new((*arts[0]).clone());
        panel.sources[0] = Some(Arc::downgrade(&transient));
        let sources = panel.sources.clone();
        let before = panel.before.clone();
        let token = panel.underlay.clone();
        drop(transient);
        drop(store);
        let panels = frame.ink_cache.panels.clone();
        assert!(!frame.ink_panel_hit(&key, &sources, Rect(425, 16, 206, 408), &before, &token, &mut panels.borrow_mut()));
    });
}

#[test]
fn panelcache_dense_damage_copies_only_the_bound_and_counts_real_work() {
    probe(|| {
        let arts = arts();
        let theme = theme("compact-plus", "square");
        let base = input(&theme, 0, &arts);
        let mut frame = draw(&theme, &base, None);
        let r = Rect(425, 16, 206, 408);
        frame.damage = vec![r];
        let mut display = crate::theme::render::tests::TestDisplay(frame.pixels.as_ref().to_owned());
        for y in r.1..r.1 + r.3 {
            for x in r.0..r.0 + r.2 { display.0.data_mut()[((y * 640 + x) * 4) as usize] ^= 1; }
        }
        perf_trace::begin_render();
        let damage = frame.paint_damage(&mut display).unwrap();
        assert_eq!(display.0.data(), frame.pixels.data());
        assert_eq!(damage, vec![crate::geom::Rect::new(r.0 as i32, r.1 as i32, r.2, r.3)]);
        assert_eq!(count(Count::ComparedPixels), 32);
        assert_eq!(count(Count::ChangedPixels), 32);
        assert_eq!(count(Count::CopiedPixels), r.2 as u64 * r.3 as u64);
    });
}
