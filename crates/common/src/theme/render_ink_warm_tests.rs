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
