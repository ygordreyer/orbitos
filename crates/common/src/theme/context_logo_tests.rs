use super::*;
use crate::theme::{FontBank, art, frame::Focus};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn theme(id: &str, shape: &str, expanded: bool) -> ThemeHandle {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let mut snapshot = ThemeSnapshot::load_with_settings(&root.join("themes").join(id),
        &BTreeMap::from([("cover_shape".into(), shape.into())])).unwrap();
    if !expanded {
        for c in &mut Arc::get_mut(&mut snapshot.0).unwrap().pack.screens
            .get_mut(&ScreenId::ContextMenu).unwrap().components {
            c.logo_rect = None;
        }
    }
    ThemeHandle::prepare(snapshot, &bank, 15).unwrap()
}
fn input() -> FrameInputs {
    let mut input = FrameInputs::new(ScreenId::ContextMenu);
    input.text.insert(Binding::Title, "Clock Tower: The First Fear".into());
    input.rows.insert(Binding::Entries, ["Reset", "Remove from Recents", "Repopulate Database"]
        .map(|label| Row { id: label.into(), label: label.into(), subtitle: String::new(),
            value: String::new(), enabled: true }).into());
    input.rows.insert(Binding::Actions, vec![Row { id: "close".into(), label: "Close".into(),
        subtitle: String::new(), value: String::new(), enabled: true }]);
    input.focus = Focus { component: "rows".into(), item: Some("Remove from Recents".into()) };
    input
}
fn title(theme: &ThemeHandle) -> &Component {
    theme.pack().screens[&ScreenId::ContextMenu].components.iter()
        .find(|c| c.logo).unwrap()
}
fn assert_reference(theme: &ThemeHandle, input: &FrameInputs, frame: &PreparedFrame) {
    let reference = navigation_reference(theme.clone(), input).unwrap();
    let mismatch = frame.pixels.data().chunks_exact(4).zip(reference.pixels.data().chunks_exact(4))
        .position(|(a, b)| a != b);
    assert!(mismatch.is_none(), "{} subtitle={:?} first mismatched pixel {:?}",
        theme.pack().id, input.text.get(&Binding::Subtitle), mismatch.map(|i| (i % 640, i / 640)));
    assert_eq!(frame.text_boxes, reference.text_boxes);
}

#[test]
fn context_logo_headers_all_packs_shapes_aspect_fallback_and_retained_damage() {
    for id in ["compact-plus", "orbit-hero", "orbit-row", "big-picture"] {
        for shape in ["square", "tall"] {
            let theme = theme(id, shape, true);
            let title = title(&theme);
            let band = title.logo_rect.unwrap();
            let components = &theme.pack().screens[&ScreenId::ContextMenu].components;
            let subtitle = components.iter().find(|c| c.binding == Some(Binding::Subtitle)).unwrap();
            assert_eq!(band.1 + band.3, subtitle.rect.1 + subtitle.rect.3);
            assert!(band.1 + band.3 <= components.iter().find(|c| c.binding == Some(Binding::Entries)).unwrap().rect.1);
            if id == "compact-plus" { assert_eq!(band, Rect(260, 80, 348, 76)); }
            assert_eq!(art::Targets::for_screens(theme.pack(), &[ScreenId::ContextMenu]).logo
                .map(|t| (t.width, t.height)), Some((band.2, band.3)));
            let mut input = input();
            let mut frame = Some(PreparedFrame::prepare(theme.clone(), &input).unwrap());
            let old = self::theme(id, shape, false);
            let old_text = PreparedFrame::prepare(old, &input).unwrap();
            assert!(frame.as_ref().unwrap().pixels.data() == old_text.pixels.data(), "text fallback changed");
            assert!(frame.as_ref().unwrap().text_boxes.iter().any(|(id, r)| id == &title.id && *r == title.rect));
            for (w, h) in [(640, 240), (120, 480)] {
                let mut source = Pixmap::new(w, h).unwrap();
                source.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
                let rect = PreparedFrame::logo_rect(title, &source);
                let mut old = title.clone(); old.logo_rect = None;
                assert!(rect.3 > PreparedFrame::logo_rect(&old, &source).3);
                assert_eq!(rect.0, band.0, "left aligned");
                assert!(rect.1 >= band.1 && rect.0 + rect.2 <= band.0 + band.2
                    && rect.1 + rect.3 <= band.1 + band.3);
                assert!((rect.2 as f64 - rect.3 as f64 * w as f64 / h as f64).abs() <= 1.5);
                input.art.selected_logo = Some(Arc::new(source));
                frame = Some(PreparedFrame::prepare_reusing(theme.clone(), &input, frame).unwrap());
                let rendered = frame.as_ref().unwrap();
                assert_reference(&theme, &input, rendered);
                if !art::logo_readable(w, h, band.2, band.3) {
                    assert!(rendered.pixels.data() == old_text.pixels.data());
                    continue;
                }
                let mut count = 0;
                for (index, pixel) in rendered.pixels.data().chunks_exact(4).enumerate() {
                    if pixel == [255, 0, 0, 255] {
                        let (x, y) = (index as u32 % 640, index as u32 / 640);
                        assert!(x >= rect.0 && x < rect.0 + rect.2 && y >= rect.1 && y < rect.1 + rect.3);
                        count += 1;
                    }
                }
                assert!(count > (rect.2 * rect.3 * 9 / 10) as usize, "logo clipped by retained bounds");
                assert!(!rendered.text_boxes.iter().any(|(id, _)| id == &title.id));
            }
            input.text.insert(Binding::Subtitle, "PlayStation".into());
            frame = Some(PreparedFrame::prepare_reusing(theme.clone(), &input, frame).unwrap());
            assert_reference(&theme, &input, frame.as_ref().unwrap());
            input.art.selected_logo = None;
            frame = Some(PreparedFrame::prepare_reusing(theme.clone(), &input, frame).unwrap());
            assert_reference(&theme, &input, frame.as_ref().unwrap());
            assert!(frame.unwrap().text_boxes.iter().any(|(id, r)| id == &title.id && *r == title.rect));
        }
    }
}

#[test]
fn logo_area_downscale_preserves_thin_edges_and_premultiplied_alpha() {
    let mut source = Pixmap::new(12, 8).unwrap();
    for y in 0..8 {
        for x in 0..12 {
            let at = ((y * 12 + x) * 4) as usize;
            let pixel = if x % 4 == 0 { [255, 0, 0, 255] } else { [0, 0, 0, 0] };
            source.data_mut()[at..at + 4].copy_from_slice(&pixel);
        }
    }
    let scaled = art::fit_logo(&source, 3, 2).unwrap();
    assert_eq!((scaled.width(), scaled.height()), (3, 2));
    for pixel in scaled.data().chunks_exact(4) { assert_eq!(pixel, [64, 0, 0, 64]); }
    // Two-point bilinear sampling misses these one-pixel strokes entirely.
    let old = crate::theme::runtime::fit_image(&source, 3, 2, crate::theme::pack::Fit::Contain).unwrap();
    assert!(old.data().chunks_exact(4).all(|p| p == [0, 0, 0, 0]));
}

#[test]
fn context_logo_real_asset_capture() {
    let Some(path) = std::env::var_os("ALLIUM_FIX15_LOGO") else { return; };
    let source = Arc::new(Pixmap::load_png(path).unwrap());
    for expanded in [false, true] {
        let theme = theme("compact-plus", "square", expanded);
        let mut input = input();
        input.art.selected_logo = Some(if expanded { source.clone() } else {
            let r = PreparedFrame::logo_rect(title(&theme), &source);
            Arc::new(crate::theme::runtime::fit_image(&source, r.2, r.3, crate::theme::pack::Fit::Contain).unwrap())
        });
        let frame = PreparedFrame::prepare(theme, &input).unwrap();
        frame.pixels.save_png(format!("/private/tmp/allium-fix15-{}.png", if expanded { "after" } else { "before" })).unwrap();
    }
}

#[test]
fn context_logo_warm_frames_do_not_resize_or_access_files() {
    use crate::perf_trace::{self, Count};
    let theme = theme("compact-plus", "square", true);
    let mut input = input();
    let mut source = Pixmap::new(640, 240).unwrap();
    source.fill(tiny_skia::Color::from_rgba8(220, 30, 80, 255));
    input.art.selected_logo = Some(Arc::new(source));
    let path = std::env::temp_dir().join(format!("allium-fix15-probe-{:?}.jsonl", std::thread::current().id()));
    perf_trace::start_probe(path.to_str().unwrap());
    let io = perf_trace::frontend_io_ops();
    perf_trace::begin_render();
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let count = |kind: Count| perf_trace::probe_counts()["measurement"]["frame"]["counters"][kind as usize].as_u64().unwrap();
    assert!(count(Count::ResizePixels) > 0);
    perf_trace::begin_render();
    let frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
    assert_eq!(count(Count::ResizePixels), 0);
    assert_eq!(count(Count::ImageMiss), 0);
    assert_eq!(perf_trace::frontend_io_ops(), io);
    assert_reference(&theme, &input, &frame);
    perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fix19_tall_logo_falls_back_in_headers_and_retained_frames() {
    for id in ["compact-plus", "orbit-hero", "orbit-row", "big-picture"] {
        for screen in [ScreenId::ContextMenu, ScreenId::Library, ScreenId::Recents] {
            let theme = theme(id, "square", true);
            if !theme.pack().screens[&screen].components.iter().any(|c| c.logo || c.binding == Some(Binding::SelectedLogo)) { continue; }
            let mut input = if screen == ScreenId::ContextMenu { input() } else { FrameInputs::new(screen) };
            if screen != ScreenId::ContextMenu {
                input.rows.insert(Binding::Entries, vec![Row { id: "game".into(), label: "Kowai Shashin".into(), subtitle: String::new(), value: String::new(), enabled: true }]);
                input.selected.insert(Binding::Entries, "game".into());
            }
            input.text.insert(Binding::SelectedTitle, "Kowai Shashin".into());
            input.text.insert(Binding::Title, "Kowai Shashin".into());
            let fallback = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let mut source = Pixmap::new(120, 480).unwrap();
            source.fill(tiny_skia::Color::from_rgba8(230, 35, 80, 255));
            input.art.selected_logo = Some(Arc::new(source));
            let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            if let Ok(stage) = std::env::var("ALLIUM_FIX19_CAPTURE") {
                frame.pixels().save_png(format!("/private/tmp/allium-fix19-logo-{id}-{screen:?}-{stage}.png")).unwrap();
                continue;
            }
            assert!(frame.pixels().data() == fallback.pixels().data(), "{id}/{screen:?}: tall logo must use title");
            assert_eq!(frame.text_boxes, fallback.text_boxes);
            let mut wide = Pixmap::new(640, 120).unwrap(); wide.fill(tiny_skia::Color::from_rgba8(230, 35, 80, 255));
            input.art.selected_logo = Some(Arc::new(wide));
            let wide = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
            assert!(wide.pixels().data() != fallback.pixels().data(), "{id}/{screen:?}: wide logo must remain visible");
            input.art.selected_logo = Some(Arc::new(Pixmap::new(120, 480).unwrap()));
            let restored = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(wide)).unwrap();
            assert!(restored.pixels().data() == fallback.pixels().data());
            assert_reference(&theme, &input, &restored);
        }
    }
}

#[test]
fn fix19_logo_readability_boundary_and_warm_rejection() {
    assert!(!art::logo_readable(79, 100, 360, 40));
    assert!(art::logo_readable(80, 100, 360, 40));
    assert!(art::logo_readable(96, 24, 480, 68));
    assert!(art::logo_readable(360, 40, 360, 40));
    assert!(!art::logo_readable(120, 480, 360, 40));
    assert!(!art::logo_readable(0, 480, 360, 40));
    let theme = theme("big-picture", "square", true);
    let mut input = input();
    input.art.selected_logo = Some(Arc::new(Pixmap::new(120, 480).unwrap()));
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let path = std::env::temp_dir().join(format!("allium-fix19-probe-{:?}.jsonl", std::thread::current().id()));
    crate::perf_trace::start_probe(path.to_str().unwrap());
    let io = crate::perf_trace::frontend_io_ops();
    crate::perf_trace::begin_render();
    let frame = PreparedFrame::prepare_reusing(theme, &input, Some(frame)).unwrap();
    let counts = crate::perf_trace::probe_counts();
    for kind in [crate::perf_trace::Count::ResizePixels, crate::perf_trace::Count::ImageMiss] {
        assert_eq!(counts["measurement"]["frame"]["counters"][kind as usize].as_u64().unwrap(), 0);
    }
    assert_eq!(crate::perf_trace::frontend_io_ops(), io);
    assert!(frame.text_boxes.iter().any(|(id, _)| id == "title"));
    crate::perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fix19_builtin_hint_kind_invalidates_retained_pixels() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    for id in ["compact", "orbit"] {
        let theme = ThemeHandle::prepare(ThemeSnapshot::builtin(id).unwrap(), &bank, 19).unwrap();
        let mut input = FrameInputs::new(ScreenId::Library);
        input.hints = vec![crate::theme::frame::Hint { button: "A".into(), label: "Play".into() }];
        let mut frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let game = frame.pixels().data().to_vec();
        for folder in [true, false, true] {
            input.selected_is_folder = folder;
            frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
            assert_reference(&theme, &input, &frame);
            assert_eq!(frame.pixels().data() == game, !folder);
        }
    }
}
