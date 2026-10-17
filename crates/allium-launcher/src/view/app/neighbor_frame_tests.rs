use super::*;
use common::theme::{frame::{FrameInputs, Row}, pack::{Binding, ScreenId}, render::PreparedFrame};

fn theme() -> common::theme::ThemeHandle { theme_named("compact-plus") }
fn theme_named(name: &str) -> common::theme::ThemeHandle {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let snapshot = if name == "compact" { common::theme::runtime::ThemeSnapshot::builtin(name).unwrap() }
        else { common::theme::runtime::ThemeSnapshot::load(&root.join("themes").join(name)).unwrap() };
    common::theme::ThemeHandle::prepare(snapshot, &bank, 991).unwrap()
}
fn input(theme: &common::theme::ThemeHandle, selected: usize) -> FrameInputs {
    let mut input = FrameInputs::new(ScreenId::Recents);
    input.text.insert(Binding::SelectedTitle, format!("Game {selected}"));
    input.rows.insert(Binding::Entries, (0..6).map(|i| Row { id: i.to_string(),
        label: format!("Game {i}"), subtitle: String::new(), value: String::new(), enabled: true }).collect());
    input.focus.component = crate::theme_adapter::component_id(theme, ScreenId::Recents, Binding::Entries);
    input.focus.item = Some(selected.to_string());
    input.selected.insert(Binding::Entries, selected.to_string());
    input
}
fn neighbor(base: &PreparedFrame, input: &FrameInputs) -> PreparedNeighbor {
    let (frame, charge) = base.prepare_neighbor(input, NEIGHBOR_LIMIT).unwrap().unwrap();
    PreparedNeighbor { base: base.navigation_stamp(), frame, charge, packed: None, pack_diagnostic: Default::default(), generation: NeighborGeneration {
        media: 0, names: 0, metadata: 0, adoption: 0, artwork: 0, locale: 0, consoles: 0, language: "en-US".parse().unwrap(),
    } }
}
fn identical(frame: &PreparedFrame, theme: &common::theme::ThemeHandle, input: &FrameInputs) {
    let reference = PreparedFrame::prepare(theme.clone(), input).unwrap();
    assert_eq!(frame.pixels().data().len(), 1_228_800);
    assert!(frame.pixels().data() == reference.pixels().data(), "screen {:?}", frame.screen());
    assert_eq!(frame.targets, reference.targets);
    assert_eq!(frame.text_boxes, reference.text_boxes);
    assert_eq!(frame.is_opaque(), reference.is_opaque());
    assert_eq!(frame.screen(), reference.screen());
}

#[test]
fn frameprep_adopts_only_the_clean_displayed_base_and_discards_sibling_damage() {
    let theme = theme();
    let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
    let displayed = base.navigation_stamp();
    let next = input(&theme, 1);
    let candidate = neighbor(&base, &next);
    let adopted_stamp = candidate.frame.navigation_stamp();
    let sibling = neighbor(&base, &input(&theme, 5));
    let mut slots = vec![candidate, sibling];
    let mut adopted = false;
    let mut graveyard = NeighborGraveyard::default();
    let frames = prepare_theme_frames(&theme, std::slice::from_ref(&next), vec![base], &mut None,
        &mut slots, &mut graveyard, Some(&displayed), &mut adopted).unwrap();
    assert!(adopted);
    assert!(frames[0].navigation_is_current(&adopted_stamp));
    assert!(slots.is_empty());
    assert!(graveyard.base.is_some());
    assert_eq!(graveyard.siblings.len(), 1);
    graveyard.clear();
    assert!(graveyard.base.is_none() && graveyard.siblings.is_empty());
    identical(&frames[0], &theme, &next);
    assert!(!frames[0].navigation_is_current(&displayed));
}

#[test]
fn frameprep_wrong_base_repairs_repeat_batches_and_overlay_stacks_fall_back() {
    let theme = theme();
    for case in ["wrong base", "repair", "repeat batch", "menu", "details", "status"] {
        let base_input = input(&theme, 0);
        let mut base = PreparedFrame::prepare(theme.clone(), &base_input).unwrap();
        let stamp = base.navigation_stamp();
        let next = input(&theme, 1);
        let candidate = neighbor(&base, &next);
        let candidate_stamp = candidate.frame.navigation_stamp();
        let mut slots = vec![candidate];
        let mut models = vec![next];
        let mut displayed = Some(&stamp);
        match case {
            "wrong base" => base = PreparedFrame::prepare_reusing(theme.clone(), &input(&theme, 3), Some(base)).unwrap(),
            "repair" => displayed = None,
            "repeat batch" => models[0] = input(&theme, 2),
            "menu" => models.push(FrameInputs::new(ScreenId::ContextMenu)),
            "details" => models.push(FrameInputs::new(ScreenId::GameDetail)),
            "status" => models[0].status.battery = Some(17),
            _ => unreachable!(),
        }
        let mut adopted = true;
        let frames = prepare_theme_frames(&theme, &models, vec![base], &mut None, &mut slots, &mut NeighborGraveyard::default(), displayed, &mut adopted).unwrap();
        assert!(!adopted, "{case} retained adoption flag");
        assert!(!frames[0].navigation_is_current(&candidate_stamp), "{case}");
        assert!(slots.is_empty(), "{case}");
        let first = if frames.last().unwrap().is_opaque() { frames.len() - 1 } else { 0 };
        for (frame, model) in frames[first..].iter().zip(&models[first..]) { identical(frame, &theme, model); }
    }
}

#[test]
fn frameprep_completed_misses_keep_actual_readiness_independent_of_pixels() {
    let theme = theme();
    let mut model = input(&theme, 0);
    let before = PreparedFrame::prepare(theme.clone(), &model).unwrap();
    assert!(!theme_content_complete(&model, &theme));
    model.art.completed_misses = common::theme::frame::ArtMisses {
        selected_cover: true, selected_hero: true, selected_logo: true, next_cover: true,
        entries: model.rows[&Binding::Entries].iter().map(|r| r.id.clone()).collect(),
    };
    let after = PreparedFrame::prepare(theme.clone(), &model).unwrap();
    assert!(before.pixels().data() == after.pixels().data());
    assert!(theme_content_complete(&model, &theme));
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn frameprep_eager_generation_checks_reject_equal_visible_value_changes() {
    let mut map = type_map::TypeMap::new();
    map.insert(Locale::new("en-US"));
    map.insert(crate::consoles::ConsoleMapper::new());
    let res = Resources::new(map);
    for change in ["media", "metadata", "locale revision", "console revision", "language"] {
        let before = NeighborGeneration::current(&res, res.get::<Locale>().lang.clone());
        assert_eq!(before.borrowed(), NeighborGeneration::current(&res, &res.get::<Locale>().lang));
        match change {
            "media" => common::media::invalidate(),
            "metadata" => super::super::details::invalidate_metadata(),
            "locale revision" => res.insert(Locale::new("en-US")),
            "console revision" => res.insert(crate::consoles::ConsoleMapper::new()),
            "language" => res.0.borrow_mut().get_mut::<Locale>().unwrap().lang = "pt-BR".parse().unwrap(),
            _ => unreachable!(),
        }
        assert_ne!(before, NeighborGeneration::current(&res, res.get::<Locale>().lang.clone()), "{change}");
    }
}

#[test]
fn frameptr_empty_preparation_resets_presentation_flag() {
    let mut adopted = true;
    let frames = prepare_theme_frames(&theme(), &[], vec![], &mut None, &mut vec![], &mut NeighborGraveyard::default(), None, &mut adopted).unwrap();
    assert!(frames.is_empty());
    assert!(!adopted);
}

#[cfg(feature = "perf-bench")]
#[test]
fn prepack_neighbor_budget_prioritizes_first_direction_and_retires_in_idle() {
    for name in ["compact", "compact-plus"] {
        let theme = theme_named(name);
        for constrained in [false, true] {
            let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
            let mut probe = common::platform::PresenterProbe::new(640, 480);
            base.paint(probe.display());
            let mut neighbors = vec![neighbor(&base, &input(&theme, 1)), neighbor(&base, &input(&theme, 5))];
            for candidate in &mut neighbors { candidate.charge += std::mem::size_of::<PreparedNeighbor>(); }
            let first_charge = neighbors[0].frame.pack_neighbor(probe.display(), NEIGHBOR_LIMIT, &mut Default::default()).unwrap().unwrap().charge();
            if constrained {
                neighbors[0].charge += NEIGHBOR_LIMIT - neighbors.iter().map(|n| n.charge).sum::<usize>() - first_charge;
            }
            while pack_neighbors(&mut neighbors, probe.display()).unwrap() {}
            assert!(neighbors[0].packed.is_some());
            if constrained { assert!(neighbors[1].packed.is_none()); }
            let total: usize = neighbors.iter().map(|n| n.charge).sum();
            assert!(total <= NEIGHBOR_LIMIT);
            println!("PREPACK theme={name} constrained={constrained} total={total} limit={NEIGHBOR_LIMIT} packs={} first_charge={first_charge}",
                neighbors.iter().filter(|n| n.packed.is_some()).count());
            let mut graveyard = NeighborGraveyard::default();
            retire_neighbor_packs(&mut neighbors, &mut graveyard, "pack_drop_view");
            assert!(neighbors.is_empty());
            assert!(graveyard.packed[0].is_some());
            graveyard.clear();
            assert!(graveyard.packed.iter().all(Option::is_none));
        }
    }
}

#[cfg(feature = "perf-bench")]
#[test]
fn prepack_status_hold_overlay_theme_and_immediate_miss_retire_payloads() {
    for case in ["hit_then_miss", "status_during_hold", "overlay", "theme"] {
        let theme = theme();
        let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
        let stamp = base.navigation_stamp();
        let mut probe = common::platform::PresenterProbe::new(640, 480);
        base.paint(probe.display());
        probe.display().flush().unwrap();
        let next = input(&theme, 1);
        let mut neighbors = vec![neighbor(&base, &next)];
        while pack_neighbors(&mut neighbors, probe.display()).unwrap() {}
        assert!(neighbors[0].packed.is_some());
        let mut models = vec![next.clone()];
        let handle = if case == "theme" { theme_named("compact") } else { theme.clone() };
        if case == "status_during_hold" { models[0].status.battery = Some(17); }
        if case == "overlay" { models.push(FrameInputs::new(ScreenId::ContextMenu)); }
        let mut graveyard = NeighborGraveyard::default();
        let mut adopted = false;
        let mut frames = prepare_theme_frames(&handle, &models, vec![base], &mut None,
            &mut neighbors, &mut graveyard, Some(&stamp), &mut adopted).unwrap();
        assert_eq!(adopted, case == "hit_then_miss", "{case}");
        assert!(graveyard.packed[0].is_some(), "press must retain packed storage for idle: {case}");
        if adopted {
            let packed = graveyard.packed[0].as_ref().unwrap();
            assert!(probe.display().packed_base_matches(packed));
            let damage = frames[0].adopt_neighbor_damage(probe.display()).unwrap();
            probe.display().flush_packed_rects(&damage, packed).unwrap();
            probe.assert_visible();
            let miss = input(&theme, 4);
            let frame = PreparedFrame::prepare_reusing(theme.clone(), &miss, Some(frames.remove(0))).unwrap();
            let damage = frame.paint_damage(probe.display()).unwrap();
            probe.display().flush_rects(&damage).unwrap();
            probe.assert_visible();
            identical(&frame, &theme, &miss);
            assert!(graveyard.packed[0].is_some());
        } else {
            for frame in &frames { frame.paint(probe.display()); }
            probe.display().flush().unwrap();
            probe.assert_visible();
        }
        graveyard.clear();
        assert!(graveyard.packed.iter().all(Option::is_none));
    }
}

#[cfg(feature = "perf-bench")]
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn prepack2_details_round_trip_and_early_neighbor_present() {
    use crate::consoles::ConsoleMapper;
    use common::{database::Database, platform::{DefaultPlatform, Platform}};
    use std::time::{Duration, Instant};

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
    }
    let root = common::constants::ALLIUM_GAMES_DIR.join("integration-prepack2");
    std::fs::create_dir_all(root.join("Imgs")).unwrap();
    std::fs::create_dir_all(root.join("media/logos")).unwrap();
    let _cleanup = Cleanup(root.clone());
    let database = Database::in_memory().unwrap();
    for index in 0..12 {
        let path = root.join(format!("Game {index:02}.chd"));
        std::fs::write(&path, []).unwrap();
        for (suffix, w, h) in [("", 240, 240), (".orbit", 101, 101), (".hero", 360, 210)] {
            let mut art = tiny_skia::Pixmap::new(w, h).unwrap();
            art.fill(tiny_skia::Color::from_rgba8(index * 17, 90, 120, 255));
            art.save_png(root.join(format!("Imgs/Game {index:02}{suffix}.png"))).unwrap();
        }
        let mut art = tiny_skia::Pixmap::new(192, 48).unwrap();
        art.fill(tiny_skia::Color::from_rgba8(index * 17, 90, 120, 255));
        art.save_png(root.join(format!("media/logos/Game {index:02}.png"))).unwrap();
        let mut game: common::database::NewGame = crate::entry::game::Game::new(path).into();
        game.image = Some(root.join(format!("Imgs/Game {index:02}.png")));
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
    let handle = theme();
    res.insert(Some(handle.clone()));
    let rect = Rect::new(0, 54, 640, 376);
    let views = (
        Recents::load_or_new(rect, res.clone(), None).unwrap(),
        Games::load_or_new(rect, res.clone(), None).unwrap(),
        Apps::load_or_new(rect, res.clone(), None).unwrap(),
        Settings::new(rect, res.clone(), Default::default()).unwrap(),
    );
    let mut app = App::new(Rect::new(0, 0, 640, 480), res, views, 0,
        DefaultPlatform::new().unwrap().battery().unwrap()).unwrap();
    let mut probe = common::platform::PresenterProbe::new(640, 480);
    let (tx, _rx) = tokio::sync::mpsc::channel(100);

    fn present(app: &mut App<<DefaultPlatform as Platform>::Battery>, display: &mut impl Display) {
        if app.should_draw() { app.prepare_theme().unwrap(); }
        if app.draw_prepared_theme(display).unwrap() == Some(true) {
            if let Some(damage) = app.take_present_damage() {
                app.flush_prepared_damage(display, &damage).unwrap();
            } else { display.flush().unwrap(); }
        }
    }
    async fn settle(app: &mut App<<DefaultPlatform as Platform>::Battery>, probe: &mut common::platform::PresenterProbe) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.idle_media_step() {
            present(app, probe.display());
            assert!(Instant::now() < deadline, "media did not settle");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        present(app, probe.display());
        probe.assert_visible();
    }
    settle(&mut app, &mut probe).await;
    for (cycle, case) in ["complete", "early", "slot_one", "unpacked", "base_mismatch", "status_drop", "prepare_drop", "declined", "not_adopted"].into_iter().enumerate() {
        probe.set_page((cycle % 2) as u32);
        for key in [Key::Start, Key::B] {
            app.handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut VecDeque::new()).await.unwrap();
            present(&mut app, probe.display());
            assert_eq!(app.details_active(), key == Key::Start);
            settle(&mut app, &mut probe).await;
        }
        let mut work = None;
        let mut warm_steps = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while warm_steps < 2 {
            let before = app.neighbors.iter().filter(|n| n.packed.is_some()).count();
            let kind = app.navigation_warm_kind(&work);
            let steps = warm_steps;
            app.idle_navigation_step(probe.display(), &mut work, &mut warm_steps, || true).unwrap();
            assert_eq!(app.navigation_warm_kind(&work), kind, "pending input advanced warming");
            assert_eq!(warm_steps, steps);
            assert_eq!(app.neighbors.iter().filter(|n| n.packed.is_some()).count(), before);
            if matches!(case, "unpacked" | "not_adopted") {
                app.prewarm_navigation_step_with_pending(&mut work, 1, || false).unwrap();
            } else {
                app.idle_navigation_step(probe.display(), &mut work, &mut warm_steps, || false).unwrap();
            }
            assert!(app.neighbors.iter().filter(|n| n.packed.is_some()).count() <= before + 1,
                "an idle turn packed more than one frame");
            if matches!(case, "early" | "unpacked" | "not_adopted") && app.neighbors.len() == 2 { break; }
            if case == "declined" && app.neighbors.len() == 1 && app.neighbors[0].packed.is_none() {
                app.neighbors[0].charge = NEIGHBOR_LIMIT;
            }
            assert!(Instant::now() < deadline, "warm did not finish");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        println!("PREPACK2 case={case} warm_steps={warm_steps} kind={} candidates={} packs={}",
            app.navigation_warm_kind(&work), app.neighbors.len(), app.neighbors.iter().filter(|n| n.packed.is_some()).count());
        if case == "slot_one" {
            // Isolate retirement routing with a rejected packet ahead of the adopted one.
            // Only this injected packet bypasses production admission; budget tests run separately.
            app.neighbor_graveyard.packed[0] = app.neighbors[1].frame.pack_neighbor(probe.display(), NEIGHBOR_LIMIT, &mut Default::default()).unwrap();
            assert!(app.neighbor_graveyard.packed[0].is_some());
        }
        if case == "base_mismatch" {
            let same_pixels = std::sync::Arc::new(probe.display().pixmap().to_owned());
            assert!(probe.display().set_shared_pixmap(&same_pixels));
        }
        if case == "status_drop" { app.invalidate_neighbors_for("invalidated:status"); }
        let declined = (case == "declined").then(|| {
            let diagnostic = app.neighbors[0].pack_diagnostic;
            assert_eq!(diagnostic.reason, "pack_budget");
            assert!(diagnostic.needed_bytes > diagnostic.available_bytes);
            diagnostic
        });
        let trace = root.join("trace.jsonl");
        common::perf_trace::start_probe(trace.to_str().unwrap());
        app.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
        if matches!(case, "prepare_drop" | "not_adopted") {
            app.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
        }
        app.defer_media_step();
        present(&mut app, probe.display());
        let counts = common::perf_trace::probe_counts();
        common::perf_trace::stop_probe();
        println!("PREPACK2 case={case} adopted={} hit={} reason={} adopt_reason={}", counts["neighbor_pointer_adopts"], counts["packed_span_hit"], counts["packed_span_reason"], counts["measurement"]["neighbor_adopt"]);
        let hit = matches!(case, "complete" | "early" | "slot_one");
        let adopted = !matches!(case, "status_drop" | "prepare_drop" | "not_adopted");
        let reason = match case {
            "unpacked" => "no_pack_built",
            "base_mismatch" => "base_mismatch",
            "status_drop" => "pack_drop_status",
            "prepare_drop" => "pack_drop_prepare",
            "declined" => "pack_budget",
            "not_adopted" => "not_adopted",
            _ => "hit",
        };
        assert_eq!(counts["neighbor_pointer_adopts"], u64::from(adopted), "{case}");
        assert_eq!(counts["packed_span_hit"], u64::from(hit), "{case}");
        assert_eq!(counts["packed_span_reason"], reason, "{case}");
        if let Some(diagnostic) = declined {
            assert_eq!(counts["packed_span_needed_bytes"], diagnostic.needed_bytes);
            assert_eq!(counts["packed_span_available_bytes"], diagnostic.available_bytes);
            println!("PREPACK3 adopted_decline={} needed={} available={}", diagnostic.reason, diagnostic.needed_bytes, diagnostic.available_bytes);
        } else {
            assert_eq!(counts["packed_span_needed_bytes"], 0, "{case}");
            assert_eq!(counts["packed_span_available_bytes"], 0, "{case}");
        }
        if case == "slot_one" { assert_eq!(app.present_packed, Some(1)); }
        probe.assert_visible();
        assert_eq!(app.proof_navigation_scene().0, app.proof_fresh_navigation_pixels());
        settle(&mut app, &mut probe).await;
    }
}

#[cfg(feature = "perf-bench")]
#[test]
fn nbr_reuses_eligibility_in_both_slots_and_directions_with_identical_presentation() {
    for name in ["compact", "compact-plus"] {
        let theme = theme_named(name);
        for selected in [1, 5] {
            for slot in [0, 1] {
                let mut results = vec![];
                for reuse in [false, true] {
                    let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
                    let displayed = base.navigation_stamp();
                    let models = vec![input(&theme, selected)];
                    let other = if selected == 1 { 5 } else { 1 };
                    let mut neighbors = vec![neighbor(&base, &models[0]), neighbor(&base, &input(&theme, other))];
                    neighbors.swap(0, slot);
                    let mut probe = common::platform::PresenterProbe::new(640, 480);
                    base.paint(probe.display());
                    probe.display().flush().unwrap();
                    while pack_neighbors(&mut neighbors, probe.display()).unwrap() {}
                    let previous = vec![base];
                    common::perf::take();
                    let matched = MatchedNeighbor::find(&theme, &models, &previous, &neighbors, Some(&displayed)).unwrap();
                    assert_eq!(matched.index, slot);
                    let mut graveyard = NeighborGraveyard::default();
                    let mut adopted = false;
                    let mut frames = prepare_theme_frames_matched(&theme, &models, previous, &mut None,
                        &mut neighbors, &mut graveyard, Some(&displayed), &mut adopted, reuse.then_some(matched)).unwrap();
                    let comparisons = common::perf::take().neighbor_matches;
                    assert_eq!(comparisons, (slot as u64 + 1) * if reuse { 1 } else { 2 });
                    assert!(adopted);
                    identical(&frames[0], &theme, &models[0]);
                    let packed = graveyard.packed[graveyard.adopted_pack.unwrap()].as_ref().unwrap();
                    let packed_hit = probe.display().packed_base_matches(packed);
                    assert!(packed_hit);
                    let damage = frames[0].adopt_neighbor_damage(probe.display()).unwrap();
                    probe.display().flush_packed_rects(&damage, packed).unwrap();
                    probe.assert_visible();
                    results.push((probe.display().pixmap().data().to_vec(), frames[0].targets.clone(),
                        frames[0].text_boxes.clone(), theme_content_pending_reasons(&models, &theme),
                        graveyard.adopted_diagnostic.unwrap().reason, damage, packed_hit));
                }
                assert_eq!(results[0], results[1], "{name} selected={selected} slot={slot}");
            }
        }
    }
}

#[cfg(feature = "perf-bench")]
#[test]
fn nbr_changed_dependencies_drop_the_proof_and_preserve_slow_path_results() {
    let theme = theme();
    for case in ["miss", "status", "artwork", "miss readiness", "base", "displayed", "theme", "layout",
        "overlay", "candidate replaced", "candidate prepared again", "candidate moved", "equal model replacement"]
    {
        let mut results = vec![];
        for reuse in [false, true] {
            let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
            let displayed = base.navigation_stamp();
            let models = vec![input(&theme, 1)];
            let mut neighbors = vec![neighbor(&base, &models[0])];
            let mut previous = vec![base];
            let matched = MatchedNeighbor::find(&theme, &models, &previous, &neighbors, Some(&displayed));
            assert!(matched.is_some());
            let mut changed = models.clone();
            let mut handle = theme.clone();
            let mut stamp = Some(&displayed);
            match case {
                "miss" => changed[0] = input(&theme, 3),
                "status" => changed[0].status.battery = Some(17),
                "artwork" => changed[0].art.selected_cover = Some(std::sync::Arc::new(tiny_skia::Pixmap::new(8, 8).unwrap())),
                "miss readiness" => changed[0].art.completed_misses.selected_logo = true,
                "base" => previous[0] = PreparedFrame::prepare(theme.clone(), &input(&theme, 2)).unwrap(),
                "displayed" => stamp = None,
                "theme" => handle = theme_named("compact-plus"),
                "layout" => handle = theme_named("compact"),
                "overlay" => changed.push(FrameInputs::new(ScreenId::ContextMenu)),
                "candidate replaced" => neighbors[0] = neighbor(&previous[0], &input(&theme, 3)),
                "candidate prepared again" => {
                    let mut candidate = neighbors.remove(0);
                    candidate.frame = PreparedFrame::prepare_reusing(theme.clone(), &input(&theme, 3), Some(candidate.frame)).unwrap();
                    neighbors.push(candidate);
                }
                "candidate moved" => { neighbors.insert(0, neighbor(&previous[0], &input(&theme, 3))); }
                "equal model replacement" => {},
                _ => unreachable!(),
            }
            let original_models = matches!(case, "base" | "displayed" | "candidate replaced" | "candidate prepared again" | "candidate moved");
            let actual_models = if original_models { &models } else { &changed };
            let actual_handle = if matches!(case, "theme" | "layout") { &handle } else { &theme };
            assert_eq!(MatchedNeighbor::find(actual_handle, actual_models, &previous, &neighbors, stamp).is_some(),
                matches!(case, "candidate moved" | "equal model replacement"), "eligibility: {case}");
            common::perf::take();
            let mut adopted = false;
            let mut graveyard = NeighborGraveyard::default();
            let frames = prepare_theme_frames_matched(actual_handle, actual_models, previous, &mut None,
                &mut neighbors, &mut graveyard, stamp, &mut adopted, if reuse { matched } else { None }).unwrap();
            let comparisons = common::perf::take().neighbor_matches;
            assert_eq!(adopted, matches!(case, "candidate moved" | "equal model replacement"), "{case}");
            if !matches!(case, "base" | "displayed" | "overlay") { assert!(comparisons > 0, "{case} skipped slow path"); }
            let mut display = <common::platform::DefaultPlatform as common::platform::Platform>::Display::default();
            for frame in &frames { frame.paint(&mut display); }
            results.push((display.pixmap().data().to_vec(), adopted, comparisons,
                theme_content_pending_reasons(visible_theme_models(actual_models, &frames), actual_handle), graveyard.packed_reason));
        }
        assert_eq!(results[0], results[1], "{case}");
    }
}

#[cfg(all(feature = "perf-bench", not(any(feature = "miyoo", feature = "simulator"))))]
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn nbr_real_presses_reuse_once_and_queued_media_invalidates_before_matching() {
    use common::{database::Database, platform::{DefaultPlatform, Platform}};
    use std::time::{Duration, Instant};

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
    }
    let root = common::constants::ALLIUM_GAMES_DIR.join("integration-nbr");
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = Cleanup(root.clone());
    let database = Database::in_memory().unwrap();
    for index in 0..6 {
        let path = root.join(format!("Game {index:02}.chd"));
        std::fs::write(&path, []).unwrap();
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
    map.insert(crate::consoles::ConsoleMapper::new());
    map.insert(common::geom::Size::new(640, 480));
    map.insert(database);
    let res = Resources::new(map);
    let handle = theme();
    res.insert(Some(handle.clone()));
    let rect = Rect::new(0, 54, 640, 376);
    let views = (
        Recents::load_or_new(rect, res.clone(), None).unwrap(),
        Games::load_or_new(rect, res.clone(), None).unwrap(),
        Apps::load_or_new(rect, res.clone(), None).unwrap(),
        Settings::new(rect, res.clone(), Default::default()).unwrap(),
    );
    let mut app = App::new(Rect::new(0, 0, 640, 480), res, views, 0,
        DefaultPlatform::new().unwrap().battery().unwrap()).unwrap();
    let mut probe = common::platform::PresenterProbe::new(640, 480);
    let (tx, _rx) = tokio::sync::mpsc::channel(100);

    fn present(app: &mut App<<DefaultPlatform as Platform>::Battery>, probe: &mut common::platform::PresenterProbe) {
        if app.should_draw() { app.prepare_theme().unwrap(); }
        flush(app, probe);
    }
    fn flush(app: &mut App<<DefaultPlatform as Platform>::Battery>, probe: &mut common::platform::PresenterProbe) {
        if app.draw_prepared_theme(probe.display()).unwrap() == Some(true) {
            if let Some(damage) = app.take_present_damage() {
                app.flush_prepared_damage(probe.display(), &damage).unwrap();
            } else { probe.display().flush().unwrap(); }
        }
    }
    async fn warm(app: &mut App<<DefaultPlatform as Platform>::Battery>, probe: &mut common::platform::PresenterProbe) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.idle_media_step() {
            present(app, probe);
            assert!(Instant::now() < deadline, "media did not settle");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        present(app, probe);
        let mut work = None;
        let direction = app.navigation_direction;
        while !app.prewarm_navigation_step(&mut work, direction).unwrap() {
            assert!(Instant::now() < deadline, "neighbors did not warm");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(app.neighbors.len(), 2);
    }
    for key in [Key::Down, Key::Up] {
        warm(&mut app, &mut probe).await;
        app.handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut VecDeque::new()).await.unwrap();
        assert!(app.defer_media_step());
        let mut models = app.theme_models(&handle).unwrap().unwrap();
        app.normalize_theme_inputs(&handle, &mut models);
        let slot = app.neighbors.iter().position(|candidate| candidate.frame.input_matches(&models[0])).unwrap();
        common::perf::take();
        app.prepare_theme().unwrap();
        assert_eq!(common::perf::take().neighbor_matches, slot as u64 + 1, "{key:?}");
        assert!(app.neighbor_adopted);
        assert!(app.media_deferred);
        assert_eq!(app.proof_navigation_scene().0, app.proof_fresh_navigation_pixels());
        assert_eq!(app.content_complete(), theme_content_complete(&models[0], &handle));
        flush(&mut app, &mut probe);
        probe.assert_visible();
    }
    warm(&mut app, &mut probe).await;
    assert!(app.defer_media_step());
    app.views.0.release_theme_art();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    super::super::orbit::proof_art_gate(Some(gate.clone()));
    let requests = super::super::orbit::test_art_requests();
    assert!(app.idle_media_step());
    assert!(super::super::orbit::test_art_requests() > requests);
    assert!(!super::super::orbit::art_result_queued());
    assert!(!app.neighbors.is_empty());
    gate.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !super::super::orbit::art_result_queued() {
        assert!(Instant::now() < deadline, "worker did not queue its result");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    super::super::orbit::proof_art_completion();
    common::perf::take();
    app.prepare_theme().unwrap();
    assert!(super::super::orbit::proof_art_completion().is_some(), "queued result was not adopted");
    assert_eq!(common::perf::take().neighbor_matches, 0, "queued completion must retire candidates before matching");
    assert!(!app.neighbor_adopted);
    assert!(!app.media_deferred);
    assert!(app.neighbors.is_empty());
    let mut models = app.theme_models(&handle).unwrap().unwrap();
    app.normalize_theme_inputs(&handle, &mut models);
    assert_eq!(app.proof_navigation_scene().0, app.proof_fresh_navigation_pixels());
    assert_eq!(app.content_complete(), theme_content_complete(&models[0], &handle));
    flush(&mut app, &mut probe);
    probe.assert_visible();
    while app.idle_media_step() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
