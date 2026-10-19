use super::*;
use crate::theme::{art::Targets, pack::Fit};
use std::sync::Arc;

fn theme() -> ThemeHandle {
    super::neighbor_test_theme("compact-plus", "square")
}
fn source(w: u32, h: u32) -> Arc<Pixmap> {
    let mut image = Pixmap::new(w, h).unwrap();
    for y in 0..h {
        for x in 0..w {
            let color = if x < 12 { [255, 0, 0, 255] }
                else if x >= w - 12 { [0, 255, 0, 255] }
                else if y < 12 { [0, 0, 255, 255] }
                else if y >= h - 12 { [255, 255, 0, 255] }
                else { [80, 40, 20, 255] };
            image.data_mut()[((y * w + x) * 4) as usize..][..4].copy_from_slice(&color);
        }
    }
    Arc::new(image)
}
fn check_cover(w: u32, h: u32, expected: [(u32, u32); 2]) {
    let theme = theme();
    let source = source(w, h);
    for (screen, size) in [ScreenId::Library, ScreenId::GameDetail].into_iter().zip(expected) {
        let c = theme.pack().screens[&screen].components.iter()
            .find(|c| c.binding == Some(Binding::SelectedCover)).unwrap();
        assert_eq!(c.fit, Some(Fit::Contain));
        let target = Targets::from_components(theme.pack(), [c]).cover.unwrap();
        assert_eq!(target.fit, Fit::Contain);
        assert!(target.cover_box);
        let rect = PreparedFrame::ink_image_rect(c, &source);
        assert_eq!((rect.2, rect.3), size);
        assert_eq!(rect.0, c.rect.0 + (c.rect.2 - rect.2) / 2);
        assert_eq!(rect.1, c.rect.1 + (c.rect.3 - rect.3) / 2);
        let mut input = FrameInputs::new(screen);
        let empty = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        input.art.selected_cover = Some(source.clone());
        let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let midx = rect.0 + rect.2 / 2;
        let midy = rect.1 + rect.3 / 2;
        for (x, y, color) in [(rect.0 + 2, midy, [255, 0, 0, 255]),
            (rect.0 + rect.2 - 3, midy, [0, 255, 0, 255]),
            (midx, rect.1 + 2, [0, 0, 255, 255]),
            (midx, rect.1 + rect.3 - 3, [255, 255, 0, 255])] {
            let pixel = frame.pixels().pixel(x, y).unwrap();
            assert_eq!([pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()], color, "{screen:?} at {x},{y}");
        }
        // Empty slot space remains the panel, with no square frame across it.
        let (x, y) = if rect.3 < c.rect.3 { (midx, c.rect.1 + 2) } else { (c.rect.0 + 2, midy) };
        assert_eq!(frame.pixels().pixel(x, y), empty.pixels().pixel(x, y), "{screen:?} letterbox");
        let reference = super::navigation_reference(theme.clone(), &input).unwrap();
        assert!(frame.pixels().data() == reference.pixels().data());
    }
}
#[test]
fn fix25_landscape_cover_full_edges_library_and_game_detail() {
    check_cover(480, 341, [(160, 114), (176, 125)]);
}
#[test]
fn fix25_portrait_cover_fits_height_library_and_game_detail() {
    check_cover(320, 480, [(107, 160), (117, 176)]);
}
#[test]
fn fix25_square_cover_and_hero_pixels_unchanged() {
    let theme = theme();
    for (screen, binding, source) in [(ScreenId::Library, Binding::SelectedCover, source(480, 480)),
        (ScreenId::GameDetail, Binding::SelectedCover, source(480, 480)),
        (ScreenId::GameDetail, Binding::SelectedHero, source(480, 341))] {
        let frame = PreparedFrame::prepare(theme.clone(), &FrameInputs::new(screen)).unwrap();
        let c = theme.pack().screens[&screen].components.iter().find(|c| c.binding == Some(binding)).unwrap();
        let image_c = frame.ink_image_component(c);
        let rect = PreparedFrame::ink_image_rect(&image_c, &source);
        assert_eq!(rect, c.rect);
        assert_eq!(image_c.fit, Some(if binding == Binding::SelectedHero { c.fit.unwrap() } else { Fit::Contain }));
        let actual = frame.image_pixels(rect, &source, &image_c).unwrap();
        let mut before = image_c.clone();
        before.fit = Some(Fit::Cover);
        let expected = PreparedFrame::transform_image(&theme, &frame.image_key(screen, c.rect, &before, c.radius.unwrap_or(0)), &source).unwrap();
        assert_eq!(actual.data(), expected.data(), "{screen:?} {binding:?}");
    }
}
#[test]
fn fix25_explicit_cover_fit_keeps_crop_and_warm_frames_do_not_resize() {
    let theme = theme();
    let mut c = theme.pack().screens[&ScreenId::Library].components.iter()
        .find(|c| c.binding == Some(Binding::SelectedCover)).unwrap().clone();
    c.fit = Some(Fit::Cover);
    let target = Targets::from_components(theme.pack(), [&c]).cover.unwrap();
    assert_eq!(target.fit, Fit::Cover);
    assert!(!target.cover_box);
    assert_eq!(PreparedFrame::ink_image_rect(&c, &source(480, 341)), c.rect);
    let mut input = FrameInputs::new(ScreenId::Library);
    input.art.selected_cover = Some(source(480, 341));
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let path = std::env::temp_dir().join(format!("allium-fix25-probe-{:?}.jsonl", std::thread::current().id()));
    crate::perf_trace::start_probe(path.to_str().unwrap());
    let io = crate::perf_trace::frontend_io_ops();
    crate::perf_trace::begin_render();
    let frame = PreparedFrame::prepare_reusing(theme, &input, Some(frame)).unwrap();
    let counts = crate::perf_trace::probe_counts();
    for kind in [crate::perf_trace::Count::ResizePixels, crate::perf_trace::Count::ImageMiss, crate::perf_trace::Count::Allocations] {
        assert_eq!(counts["measurement"]["frame"]["counters"][kind as usize].as_u64().unwrap(), 0, "counter {}", kind as usize);
    }
    assert_eq!(crate::perf_trace::frontend_io_ops(), io);
    assert!(frame.pixels().pixel(450, 164).is_some());
    crate::perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}
