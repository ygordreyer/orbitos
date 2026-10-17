use super::*;
use crate::perf_trace::{self, Count};

fn fixture() -> (PreparedFrame, Component) {
    let theme = crate::theme::render_navigation::tests::theme("compact-plus", "square");
    let c = theme.pack().screens[&ScreenId::Recents].components.iter()
        .find(|c| c.binding == Some(Binding::Entries)).unwrap().clone();
    (PreparedFrame::unpainted(theme, ScreenId::Recents), c)
}

fn probe(name: &str, run: impl FnOnce()) {
    let path = std::env::temp_dir().join(format!("allium-baseline-{name}-{}", std::process::id()));
    perf_trace::start_probe(path.to_str().unwrap());
    run();
    perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}

fn count(kind: Count) -> u64 {
    perf_trace::probe_counts()["measurement"]["frame"]["counters"][kind as usize].as_u64().unwrap()
}

fn draw(frame: &mut PreparedFrame, ty: &Type, text: &str, rect: Rect, align: Align, baseline: f32) -> Vec<u8> {
    perf_trace::begin_render();
    frame.pixels.fill(tiny_skia::Color::from_rgba8(23, 34, 45, 255));
    frame.text_boxes.clear();
    frame.ink_baseline("baseline-proof", rect, ty, text, align, baseline).unwrap();
    assert_eq!(frame.text_boxes, vec![("baseline-proof".into(), rect)]);
    frame.pixels.data().to_vec()
}

#[test]
fn baseline_text_cache_hit_preserves_pixels_and_skips_measurement() {
    let (mut frame, c) = fixture();
    let ty = frame.ink_type(&c, false);
    let rect = Rect(17, 21, 143, 47);
    let text = "Pokémon 日本語\nA long ellipsized title :x=123";
    clear_text_cache();
    probe("hit", || {
        LAYOUT_CALLS.set(0);
        let cold = draw(&mut frame, &ty, text, rect, Align::Center, 53.25);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 1));
        assert_eq!(LAYOUT_CALLS.get(), 1);
        GUIDE_GLYPH_VISITS.set(0);
        let hit = draw(&mut frame, &ty, text, rect, Align::Center, 53.25);
        assert_eq!(cold, hit);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (1, 0));
        assert_eq!(count(Count::Allocations), 0);
        assert_eq!(LAYOUT_CALLS.get(), 1);
        assert_eq!(GUIDE_GLYPH_VISITS.get(), 0);

        // Compare against the original immediate raster/blit contract as well as a cold cache.
        let value = ty.lines(&ty.value(text), rect.2, 1, Overflow::Ellipsis).pop().unwrap();
        let (_, left, right) = ty.metrics(&value);
        let x = (rect.2 as f32 - left - right) / 2.0;
        let mut pixels = ink_pixmap(rect.2, rect.3).unwrap();
        raster(&mut pixels, &ty, &value, x, 53.25 - rect.1 as f32, frame.color(ty.color));
        frame.pixels.fill(tiny_skia::Color::from_rgba8(23, 34, 45, 255));
        crate::theme::render::ink_blit::bitmap(&mut frame.pixels, rect.0 as i32, rect.1 as i32, pixels.as_ref());
        assert_eq!(hit, frame.pixels.data());
        TEXT.with(|cache| assert!(cache.borrow()[0].key.ends_with(&format!(":x={}", x.to_bits()))));
    });
}

#[test]
fn baseline_text_cache_key_changes_miss_and_match_cold_pixels() {
    let (mut frame, c) = fixture();
    let ty = frame.ink_type(&c, false);
    let rect = Rect(17, 21, 143, 47);
    let text = "Pokémon 日本語 title :x=123";
    let baseline = 53.25_f32;
    let mut cases = vec![
        ("text", ty.clone(), "A different title", rect, Align::Left, baseline),
        ("center", ty.clone(), text, rect, Align::Center, baseline),
        ("right", ty.clone(), text, rect, Align::Right, baseline),
        ("width", ty.clone(), text, Rect(rect.0, rect.1, rect.2 + 1, rect.3), Align::Left, baseline),
        ("height", ty.clone(), text, Rect(rect.0, rect.1, rect.2, rect.3 + 1), Align::Left, baseline),
        ("baseline", ty.clone(), text, rect, Align::Left, baseline + 0.5),
        ("baseline bits", ty.clone(), text, rect, Align::Left, f32::from_bits(baseline.to_bits() + 1)),
    ];
    for field in ["color", "size", "line", "weight", "font", "tracking", "upper", "literal color"] {
        let mut changed = ty.clone();
        match field {
            "color" => changed.color = ColorRole::FocusText,
            "size" => changed.size += 0.5,
            "line" => changed.line += 1,
            "weight" => {
                changed.weight += 100;
                changed.font = frame.theme.ink_font(changed.family, changed.weight);
            }
            "font" => {
                changed.family = if ty.family == FontId::Roboto { FontId::Nunito } else { FontId::Roboto };
                changed.font = frame.theme.ink_font(changed.family, changed.weight);
            }
            "tracking" => changed.tracking += 0.25,
            "upper" => changed.upper = !changed.upper,
            "literal color" => changed.literal_color = Some(Color::rgba(45, 67, 89, 123)),
            _ => unreachable!(),
        }
        cases.push((field, changed, text, rect, Align::Left, baseline));
    }
    probe("keys", || {
        for (field, changed, text, changed_rect, align, changed_baseline) in cases {
            clear_text_cache();
            draw(&mut frame, &ty, "Pokémon 日本語 title :x=123", rect, Align::Left, baseline);
            let actual = draw(&mut frame, &changed, text, changed_rect, align, changed_baseline);
            assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 1), "{field}");
            assert_eq!(actual, draw(&mut frame, &changed, text, changed_rect, align, changed_baseline), "{field}");
            assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (1, 0), "{field}");
            clear_text_cache();
            assert_eq!(actual, draw(&mut frame, &changed, text, changed_rect, align, changed_baseline), "{field}");
            assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 1), "{field}");
        }
    });
}

#[test]
fn baseline_text_cache_focused_and_unfocused_rows_have_separate_entries() {
    let (mut frame, c) = fixture();
    let normal = frame.ink_type(&c, false);
    let focused = frame.ink_type(&c, true);
    assert_ne!(frame.color(normal.color), frame.color(focused.color));
    let row = Row { id: "row-proof".into(), label: "Pokémon title".into(), subtitle: "Platform".into(), value: String::new(), enabled: true };
    let mut input = FrameInputs::new(ScreenId::Recents);
    let mut draw_row = |input: &FrameInputs| {
        perf_trace::begin_render();
        frame.pixels.fill(tiny_skia::Color::from_rgba8(23, 34, 45, 255));
        frame.text_boxes.clear();
        frame.ink_row(&c, &row, Rect(20, 30, 240, 60), input, false).unwrap();
        (frame.pixels.data().to_vec(), frame.text_boxes.clone())
    };
    clear_text_cache();
    probe("focus", || {
        let normal = draw_row(&input);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 2));
        input.focus.component = c.id.clone();
        input.focus.item = Some(row.id.clone());
        let focused = draw_row(&input);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 2));
        assert_ne!(normal.0, focused.0);
        assert_eq!(focused, draw_row(&input));
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (2, 0));
        clear_text_cache();
        assert_eq!(focused, draw_row(&input));
        input.focus.item = None;
        assert_eq!(normal, draw_row(&input));
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 2));
    });
}

#[test]
fn baseline_text_collection_and_hero_scratch_preserve_immediate_behavior() {
    let (mut frame, c) = fixture();
    let ty = frame.ink_type(&c, false);
    let rect = Rect(17, 21, 143, 47);
    clear_text_cache();
    probe("collection", || {
        let expected = draw(&mut frame, &ty, "Baseline", rect, Align::Left, 53.25);
        let key = TEXT.with(|cache| cache.borrow()[0].key.clone());
        TEXT_PLAN.with(|plan| *plan.borrow_mut() = Some(Vec::new()));
        let actual = draw(&mut frame, &ty, "Baseline", rect, Align::Left, 53.25);
        let plan = TEXT_PLAN.with(|plan| plan.borrow_mut().take().unwrap());
        assert_eq!(actual, expected);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, key);
        let planned = plan[0].pixels().unwrap();
        TEXT.with(|cache| assert_eq!(planned.data(), cache.borrow()[0].pixels.data()));
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 0));
        TEXT.with(|cache| assert_eq!(cache.borrow()[0].key, key));

        clear_text_cache();
        assert!(admit_text(&frame.theme, plan[0].key.clone(), planned));
        assert_eq!(draw(&mut frame, &ty, "Baseline", rect, Align::Left, 53.25), expected);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (1, 0));

        clear_text_cache();
        HERO_SCRATCH.set(true);
        let scratch = draw(&mut frame, &ty, "Baseline", rect, Align::Left, 53.25);
        HERO_SCRATCH.set(false);
        assert_eq!(scratch, expected);
        assert_eq!((count(Count::TextHit), count(Count::TextMiss)), (0, 1));
        TEXT.with(|cache| assert!(cache.borrow().is_empty()));
    });
}
