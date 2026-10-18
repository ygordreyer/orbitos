use super::*;
use crate::theme::frame::{DetailLine, Hint, Pagination};
use crate::theme::render_navigation::tests::{arts, model, theme};

#[test]
fn compact_plus_details_reopen_text_hits_and_bounded_pressure() {
    let log = std::env::temp_dir().join(format!("allium-details-text-{}.log", std::process::id()));
    crate::perf_trace::start_probe(log.to_str().unwrap());
    for shape in ["square", "tall"] {
        let theme = theme("compact-plus", shape);
        let arts = arts();
        let mut recents = model(&theme, ScreenId::Recents, 0, &arts);
        recents.hints = [("A", "Play"), ("START", "Details"), ("Y", "Sort"), ("SELECT", "Menu")]
            .into_iter().map(|(button, label)| Hint { button: button.into(), label: label.into() }).collect();
        let mut details = FrameInputs::new(ScreenId::GameDetail);
        details.text = recents.text.clone();
        details.art = recents.art.clone();
        details.status = recents.status.clone();
        details.metadata = vec![("Developer".into(), "Namco é 日本語".into()),
            ("Released".into(), "1998".into()), ("Players".into(), "1 to 2".into())];
        details.facts = [("Platform", "PlayStation"), ("Genre", "Racing"), ("Region", "USA"),
            ("Played", "42 times"), ("Time", "12 hours"), ("Source", "Fixture")]
            .into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        details.detail_lines = (0..18).map(|i| DetailLine {
            text: format!("Line {i}: AV é e\u{301} 日本語"),
            heading: i == 0, gap_before: 0,
        }).collect();
        details.pagination = Some(Pagination { page: 1, total: 3 });
        details.hints = vec![Hint { button: "B".into(), label: "Back".into() }];
        TEXT.with(|cache| cache.borrow_mut().clear());
        let mut expected = None;
        for cycle in 0..4 {
            PreparedFrame::prepare(theme.clone(), &recents).unwrap();
            crate::perf_trace::begin_render();
            let frame = PreparedFrame::prepare(theme.clone(), &details).unwrap();
            let counts = crate::perf_trace::probe_counts();
            let misses = counts["text_misses"].as_u64().unwrap();
            if misses > 0 {
                assert!(counts["measurement"]["frame"]["cpu_ns"][5].as_u64().unwrap() > 0);
            }
            assert!(counts["stages"]["phases"]["snapshot_capture"]["wall_us"].as_u64().unwrap() > 0);
            if cycle == 0 {
                assert!(counts["stages"]["phases"]["shadow_gaussian"]["wall_us"].as_u64().unwrap() > 0);
            } else {
                assert!(counts["stages"]["phases"]["snapshot_restore"]["wall_us"].as_u64().unwrap() > 0);
            }

            if cycle == 0 {
                expected = Some((frame.pixels.data().to_vec(), frame.text_boxes.clone()));
            } else {
                assert_eq!(frame.pixels.data(), expected.as_ref().unwrap().0);
                assert_eq!(frame.text_boxes, expected.as_ref().unwrap().1);
            }
            TEXT.with(|cache| {
                let cache = cache.borrow();
                let bytes = cache.iter().map(|e| e.pixels.data().len() + e.key.len()).sum::<usize>();
                assert!(bytes <= TEXT_LIMIT);
                println!("DETAILS_TEXT shape={shape} cycle={cycle} misses={misses} entries={} bytes={bytes}", cache.len());
                if cycle > 0 { assert_eq!(misses, 0, "{shape} warm reopen {cycle}"); }
            });
        }
        // More than the former entry limit, followed by byte-pressure eviction.
        let mut frame = PreparedFrame::prepare(theme.clone(), &recents).unwrap();
        let c = theme.pack().screens[&ScreenId::Recents].components.iter()
            .find(|c| c.kind == Kind::Text).unwrap();
        let ty = frame.ink_type(c, false);
        for i in 0..200 {
            frame.ink_text("pressure", Rect(0, 0, 120, 32), &ty, &format!("Pressure {i}"),
                1, Align::Left, Overflow::Ellipsis, false).unwrap();
            TEXT.with(|cache| assert!(cache.borrow().iter()
                .map(|e| e.pixels.data().len() + e.key.len()).sum::<usize>() <= TEXT_LIMIT));
        }
        assert!(crate::perf_trace::probe_counts()["measurement"]["frame"]["counters"][2].as_u64().unwrap() > 0,
            "byte pressure must count evictions");
    }
    crate::perf_trace::stop_probe();
    std::fs::remove_file(log).unwrap();
}

#[test]
fn details_shadow_cold_partial_full_and_cached_frames_are_identical() {
    for shape in ["square", "tall"] {
        let handle = theme("compact-plus", shape);
        let arts = arts();
        let recents = model(&handle, ScreenId::Recents, 0, &arts);
        let mut details = FrameInputs::new(ScreenId::GameDetail);
        details.text = recents.text.clone();
        details.art = recents.art.clone();
        details.status = recents.status.clone();
        details.metadata = vec![("Developer".into(), "Namco é 日本語".into())];
        details.detail_lines = vec![DetailLine { text: "Frozen shadow parity".into(), heading: false, gap_before: 0 }];
        effects::benchmark_clear_shadows();
        let cold = PreparedFrame::prepare(handle.clone(), &details).unwrap();
        let frame = PreparedFrame::prepare(handle.clone(), &recents).unwrap();
        let presented = (frame.pixels.data().to_vec(), frame.targets.clone(), frame.text_boxes.clone());
        for batches in [1, 10, 100, 240, usize::MAX] {
            effects::benchmark_clear_shadows();
            let mut work = frame.detail_shadow_warm();
            let mut steps = 0;
            while steps < batches {
                steps += 1;
                let done = frame.prewarm_detail_shadow_step(&mut work);
                assert_eq!(frame.pixels.data(), presented.0);
                assert_eq!(frame.targets, presented.1);
                assert_eq!(frame.text_boxes, presented.2);
                if done { break; }
                assert!(steps < 1000);
            }
            if batches == usize::MAX {
                assert_eq!(effects::cached_shadow_count(), 3, "fully warmed Details keys");
            }
            let actual = PreparedFrame::prepare(handle.clone(), &details).unwrap();
            assert_eq!(effects::cached_shadow_count(), 3, "real Details must use the warmed keys");
            assert_eq!(actual.pixels.data(), cold.pixels.data(), "{shape} batches={batches}");
            assert_eq!(actual.targets, cold.targets);
            assert_eq!(actual.text_boxes, cold.text_boxes);
            let cached = PreparedFrame::prepare(handle.clone(), &details).unwrap();
            assert_eq!(cached.pixels.data(), cold.pixels.data());
            assert_eq!(cached.targets, cold.targets);
        }
        let mut stale = frame.detail_shadow_warm();
        frame.prewarm_detail_shadow_step(&mut stale);
        let replacement = PreparedFrame::prepare(theme("compact-plus", shape), &recents).unwrap();
        assert!(replacement.prewarm_detail_shadow_step(&mut stale), "theme replacement must discard work");
    }
}

#[test]
fn explicit_theme_switch_releases_previous_theme_cached_pixels() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let state = std::env::temp_dir().canonicalize().unwrap().join(format!("allium-switch-cache-{}", std::process::id()));
    std::fs::create_dir_all(&state).unwrap();
    let mut runtime = crate::theme::ThemeRuntime::bind(&state, &repo.join("themes"), &repo.join("static/.allium/fonts")).unwrap();
    let handle = runtime.choose("compact-plus").unwrap().snapshot.unwrap();
    let input = model(&handle, ScreenId::Recents, 0, &arts());
    let frame = PreparedFrame::prepare(handle.clone(), &input).unwrap();
    let pixels: Vec<_> = TEXT.with(|cache| cache.borrow().iter().map(|e| Arc::downgrade(&e.pixels)).collect());
    let surfaces: Vec<_> = SURFACES.with(|cache| cache.borrow().iter().map(|e| Arc::downgrade(&e.pixels)).collect());
    assert!(!pixels.is_empty());
    assert!(!surfaces.is_empty());
    drop((frame, handle, input));
    runtime.choose("orbit-row").unwrap();
    std::fs::remove_dir_all(state).unwrap();
    assert!(pixels.iter().all(|p| p.upgrade().is_none()), "previous theme text pixels remain resident");
    assert!(surfaces.iter().all(|p| p.upgrade().is_none()), "previous theme surface pixels remain resident");
}
