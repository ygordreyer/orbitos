fn dl15_library() -> (ThemeHandle, FrameInputs) {
    use super::super::frame::Hint;
    let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 15).unwrap();
    let mut input = FrameInputs::new(ScreenId::Library);
    input.status.clock = Some("12:34".into());
    input.status.battery = Some(73);
    input.hints = [
        ("A", "Launch"),
        ("B", "Back"),
        ("X", "Details"),
        ("Y", "Favorite"),
        ("L/R", "Page"),
        ("START", "Sort"),
        ("SELECT", "Search"),
    ]
    .into_iter()
    .map(|(button, label)| Hint {
        button: button.into(),
        label: label.into(),
    })
    .collect();
    let mut art = Pixmap::new(390, 189).unwrap();
    for (i, p) in art.data_mut().chunks_exact_mut(4).enumerate() {
        p.copy_from_slice(&[(i % 251) as u8, (i % 157) as u8, (i % 79) as u8, 255]);
    }
    let art = std::sync::Arc::new(art);
    input.art.selected_hero = Some(art.clone());
    input.art.next_cover = Some(art.clone());
    let rows: Vec<_> = ["Clock Tower: The First Fear", "WipEout 3", "Ridge Racer"]
        .iter()
        .enumerate()
        .map(|(i, label)| Row {
            subtitle: String::new(),
            id: i.to_string(),
            label: (*label).into(),
            value: String::new(),
            enabled: true,
        })
        .collect();
    for row in &rows {
        input.art.entries.insert(row.id.clone(), art.clone());
    }
    input
        .text
        .insert(Binding::SelectedTitle, rows[0].label.clone());
    input.rows.insert(Binding::Entries, rows);
    input.focus.component = "entries".into();
    input.focus.item = Some("0".into());
    (theme, input)
}

#[test]
fn dl15_host_warm_library_move_microbench() {
    if std::env::var_os("DL15_BENCH").is_none() {
        return;
    }
    let (theme, mut input) = dl15_library();
    let mut frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
    frame.paint(&mut display);
    let mut samples = Vec::new();
    for step in 0..120 {
        let row = &input.rows[&Binding::Entries][step % 2];
        input.focus.item = Some(row.id.clone());
        input.text.insert(Binding::SelectedTitle, row.label.clone());
        let start = std::time::Instant::now();
        frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
        frame.paint_damage(&mut display);
        std::hint::black_box(display.0.data());
        if step >= 20 {
            samples.push(start.elapsed().as_nanos());
        }
    }
    samples.sort_unstable();
    println!(
        "DL15_HOST_MOVE {}",
        serde_json::json!({"median_ns": samples[49], "p95_ns": samples[94], "samples":100, "release": !cfg!(debug_assertions), "seven_hints":true})
    );
}

fn dl15_reference(theme: ThemeHandle, input: &FrameInputs) -> PreparedFrame {
    let mut frame = PreparedFrame::prepare(theme, input).unwrap();
    frame.retained_layout = false;
    frame.render_components(input, false).unwrap();
    frame
}

#[test]
fn dl15_incremental_matches_fresh_after_every_event() {
    let (mut theme, mut input) = dl15_library();
    let original_theme = theme.clone();
    let mut actual_display = TestDisplay(Pixmap::new(640, 480).unwrap());
    let mut expected_display = TestDisplay(Pixmap::new(640, 480).unwrap());
    let mut frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    frame.paint(&mut actual_display);
    frame.paint(&mut expected_display);
    for step in 0..96 {
        match step {
            12 => {
                input
                    .rows
                    .get_mut(&Binding::Entries)
                    .unwrap()
                    .rotate_left(1);
            }
            20 => {
                input.rows.get_mut(&Binding::Entries).unwrap().reverse();
            }
            27 => {
                input.art.entries.remove("1");
                input.art.selected_hero = None;
            }
            30 => {
                input.status.clock = Some("12:35".into());
                input.status.charging = true;
            }
            34 => {
                input.status.network = Some("Offline".into());
                input.status.battery = None;
            }
            38 => {
                input.hints[0].label = "Open".into();
            }
            42 => {
                theme = theme.generation_copy(16);
            }
            46 => {
                input.screen = ScreenId::ContextMenu;
                input.focus = Default::default();
            }
            47 => {
                input.screen = ScreenId::Library;
                input.focus.component = "entries".into();
            }
            52 => {
                input.screen = ScreenId::Toast;
                input.focus = Default::default();
            }
            53 => {
                input.screen = ScreenId::Library;
                input.focus.component = "entries".into();
            }
            56 => {
                let mut snapshot = ThemeSnapshot::builtin("orbit").unwrap();
                let resolved = std::sync::Arc::get_mut(&mut snapshot.0).unwrap();
                resolved
                    .pack
                    .tokens
                    .colors
                    .insert(ColorRole::Background, "#1b293dff".into());
                resolved
                    .pack
                    .tokens
                    .colors
                    .insert(ColorRole::Text, "#f1e5baff".into());
                let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR)
                    .unwrap();
                theme = ThemeHandle::prepare(snapshot, &bank, 17).unwrap();
            }
            58 => {
                theme = original_theme.clone();
            }
            64 => {
                let art = input.art.entries["0"].clone();
                input
                    .art
                    .entries
                    .insert("0".into(), std::sync::Arc::new((*art).clone()));
            }
            65 => {
                let mut art = Pixmap::new(140, 140).unwrap();
                art.fill(tiny_skia::Color::from_rgba8(50, 160, 20, 173));
                let art = std::sync::Arc::new(art);
                input.art.entries.insert("0".into(), art.clone());
                input.art.selected_hero = Some(art.clone());
                input.art.next_cover = Some(art);
            }
            70 => {
                input.art.next_cover = None;
            }
            74 => {
                input.art.next_cover = input.art.entries.get("0").cloned();
            }
            _ => {}
        }
        if input.screen == ScreenId::Library {
            let rows = input.rows.get_mut(&Binding::Entries).unwrap();
            if step % 7 == 0 {
                rows.rotate_right(1);
            }
            let row = &rows[step % 3];
            input.focus.item = Some(row.id.clone());
            input.text.insert(Binding::SelectedTitle, row.label.clone());
        }
        frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
        let reference = dl15_reference(theme.clone(), &input);
        frame.paint_damage(&mut actual_display);
        reference.paint(&mut expected_display);
        assert!(
            actual_display.0.data() == expected_display.0.data(),
            "partial copy after event {step}"
        );
        assert!(frame.tiles.bytes() <= 1024 * 1024);
        assert!(
            frame.pixels.data() == reference.pixels.data(),
            "pixels after event {step}"
        );
        assert_eq!(
            frame.targets, reference.targets,
            "targets after event {step}"
        );
        assert_eq!(
            frame.text_boxes, reference.text_boxes,
            "text metadata after event {step}"
        );
    }
}

#[test]
fn dl15_full_library_text_working_set_has_stable_warm_hits() {
    let (theme, mut input) = dl15_library();
    for step in 0..8 {
        input.text.insert(
            Binding::SelectedTitle,
            input.rows[&Binding::Entries][step % 2].label.clone(),
        );
        let _ = dl15_reference(theme.clone(), &input);
    }
    let before = super::super::text_cache::stats();
    for step in 0..12 {
        input.text.insert(
            Binding::SelectedTitle,
            input.rows[&Binding::Entries][step % 2].label.clone(),
        );
        let _ = dl15_reference(theme.clone(), &input);
    }
    let after = super::super::text_cache::stats();
    assert_eq!(before.1, after.1, "warm text misses");
    assert_eq!(before.2, after.2, "warm text evictions");
    assert!(after.0 > before.0 + 200);
    assert!(after.3 <= 512 * 1024);
}

#[test]
fn dl15_retained_dependencies_do_not_pin_source_artwork() {
    let (theme, input) = dl15_library();
    let source = input.art.selected_hero.as_ref().unwrap();
    let before = std::sync::Arc::strong_count(source);
    let frame = PreparedFrame::prepare(theme, &input).unwrap();
    // Only the pre-existing single preview cache may retain its original source.
    assert!(std::sync::Arc::strong_count(source) <= before + 1);
    let weak = std::sync::Arc::downgrade(source);
    drop(input);
    *frame.theme.base().0.preview_cache.lock().unwrap() = None;
    assert!(weak.upgrade().is_none());
    assert!(frame.tiles.bytes() > 0);
}

#[test]
fn dl16_external_library_retains_pixels_and_metadata() {
    let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
    let snapshot = ThemeSnapshot::load(std::path::Path::new(
        &std::env::var("ALLIUM_UX4_PACK").unwrap(),
    ))
    .unwrap();
    let theme = ThemeHandle::prepare(snapshot, &bank, 16).unwrap();
    let (_, mut input) = dl15_library();
    let mut frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    assert!(
        frame.retained_layout,
        "live external Orbit layout must use retained rendering"
    );
    let mut actual = TestDisplay(Pixmap::new(640, 480).unwrap());
    frame.paint(&mut actual);
    let original_rows = input.rows[&Binding::Entries].clone();
    for step in 0..96 {
        if step == 48 {
            input.rows.get_mut(&Binding::Entries).unwrap().truncate(2);
        }
        if step == 60 {
            input.rows.insert(Binding::Entries, original_rows.clone());
        }
        let rows = input.rows.get_mut(&Binding::Entries).unwrap();
        if step % 7 == 0 {
            rows.rotate_left(1);
        }
        let row = &rows[step % rows.len()];
        input.focus.item = Some(row.id.clone());
        input.text.insert(Binding::SelectedTitle, row.label.clone());
        if step % 5 == 0 {
            let mut art = Pixmap::new(140, 140).unwrap();
            art.fill(tiny_skia::Color::from_rgba8(step as u8, 125, 30, 173));
            input.art.next_cover = Some(std::sync::Arc::new(art));
        }
        if step == 20 {
            input.status.clock = Some("12:35".into());
        }
        if step == 30 {
            input.hints[0].label = "Open".into();
        }
        frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
        let reference = dl15_reference(theme.clone(), &input);
        let damage = frame.paint_damage(&mut actual).unwrap();
        assert!(damage.iter().map(|r| r.w as u64 * r.h as u64).sum::<u64>() < 640 * 480);
        assert!(
            frame.pixels.data() == reference.pixels.data(),
            "external composition {step}"
        );
        assert!(
            actual.0.data() == reference.pixels.data(),
            "external partial copy {step}"
        );
        assert_eq!(frame.targets, reference.targets, "external targets {step}");
        assert_eq!(
            frame.text_boxes, reference.text_boxes,
            "external text {step}"
        );
        assert!(frame.tiles.bytes() <= 1024 * 1024);
    }
}

#[test]
fn dl16_fragmented_copy_bounds_damage_memory_and_keeps_full_pixels() {
    let (theme, input) = dl15_library();
    let mut frame = PreparedFrame::prepare(theme, &input).unwrap();
    let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
    display.0.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
    for (i, pixel) in frame.pixels.data_mut().chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(if (i % 640 + i / 640) % 9 == 0 {
            &[100, 100, 100, 255]
        } else {
            &[0, 0, 0, 255]
        });
    }
    frame.damage = vec![Rect(0, 0, 640, 480)];
    frame.opaque = true;
    let damage = frame.paint_damage(&mut display).unwrap();
    assert_eq!(damage, vec![display.bounding_box()]);
    assert!(display.0.data() == frame.pixels.data());
}

fn dl17_list_input(theme: &ThemeHandle, screen: ScreenId, selected: usize) -> FrameInputs {
    use super::super::pack::NavGroup;
    let c = theme.pack().screens[&screen]
        .components
        .iter()
        .find(|c| matches!(c.kind, Kind::Entries | Kind::SettingsRows))
        .unwrap();
    let mut input = FrameInputs::new(screen);
    input.text.insert(Binding::Title, format!("{screen:?}"));
    input.status.clock = Some("12:34".into());
    let rows: Vec<_> = (0..6)
        .map(|i| Row {
            subtitle: String::new(),
            id: i.to_string(),
            label: format!("Row {i}"),
            value: if screen == ScreenId::SettingsRoot {
                "On".into()
            } else {
                String::new()
            },
            enabled: true,
        })
        .collect();
    input.focus.component = c.id.clone();
    input.focus.item = Some(rows[selected].id.clone());
    input
        .selected
        .insert(c.binding.unwrap(), rows[selected].id.clone());
    input.rows.insert(c.binding.unwrap(), rows);
    input.navigation.insert(
        NavGroup::Primary,
        (0..4)
            .map(|i| Row {
                subtitle: String::new(),
                id: i.to_string(),
                label: format!("Tab {i}"),
                value: String::new(),
                enabled: true,
            })
            .collect(),
    );
    input.selected.insert(Binding::Navigation, "0".into());
    input
}

#[test]
fn dl17_lists_and_tabs_match_full_render() {
    let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
    for snapshot in [
        ThemeSnapshot::builtin("orbit").unwrap(),
        ThemeSnapshot::load(std::path::Path::new(
            &std::env::var("ALLIUM_UX4_PACK").unwrap(),
        ))
        .unwrap(),
    ] {
        let theme = ThemeHandle::prepare(snapshot, &bank, 17).unwrap();
        let screens = [
            ScreenId::Recents,
            ScreenId::SettingsRoot,
            ScreenId::Apps,
            ScreenId::SettingsPage,
            ScreenId::SearchResults,
        ];
        let mut frame =
            PreparedFrame::prepare(theme.clone(), &dl17_list_input(&theme, screens[0], 0)).unwrap();
        assert!(frame.retained_layout, "lists must retain their composition");
        let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
        frame.paint(&mut display);
        #[cfg(feature = "perf-bench")]
        let mut presenter = crate::platform::PresenterProbe::new(640, 480);
        for step in 0..120 {
            let screen = screens[(step / 24) % screens.len()];
            let mut input = dl17_list_input(&theme, screen, step % 6);
            let binding = *input.rows.keys().next().unwrap();
            if step % 24 == 12 {
                input.rows.get_mut(&binding).unwrap().reverse();
            }
            if step % 24 == 15 {
                input.rows.get_mut(&binding).unwrap().truncate(3);
                input.focus.item = Some("0".into());
                input.selected.insert(binding, "0".into());
            }
            if step % 24 == 18 {
                input.status.clock = Some("12:35".into());
            }
            let same_screen = frame.screen == screen;
            frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
            let reference = dl15_reference(theme.clone(), &input);
            let presented_damage = frame.paint_damage(&mut display).unwrap();
            #[cfg(feature = "perf-bench")]
            presenter
                .present(display.0.as_ref(), &presented_damage, (step % 2) as u32)
                .unwrap();
            #[cfg(not(feature = "perf-bench"))]
            let _ = presented_damage;
            assert!(
                frame.pixels.data() == reference.pixels.data(),
                "composition {screen:?} {step}"
            );
            assert!(
                display.0.data() == reference.pixels.data(),
                "present {screen:?} {step}"
            );
            assert_eq!(frame.targets, reference.targets, "targets {step}");
            assert_eq!(frame.text_boxes, reference.text_boxes, "text {step}");
            if same_screen && step % 24 < 12 && step > 0 {
                assert!(
                    frame.damage.iter().map(|r| r.2 * r.3).sum::<u32>() <= 592 * 48 * 2,
                    "two rows only {step}: {:?}",
                    frame.damage
                );
            }
        }
    }
}

#[test]
fn dl17_library_tab_roundtrip_matches_full_render() {
    let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
    let theme = ThemeHandle::prepare(
        ThemeSnapshot::load(std::path::Path::new(
            &std::env::var("ALLIUM_UX4_PACK").unwrap(),
        ))
        .unwrap(),
        &bank,
        17,
    )
    .unwrap();
    let (_, library) = dl15_library();
    let mut frame = PreparedFrame::prepare(theme.clone(), &library).unwrap();
    let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
    frame.paint(&mut display);
    #[cfg(feature = "perf-bench")]
    let mut presenter = crate::platform::PresenterProbe::new(640, 480);
    for step in 0..32 {
        let input = if step % 2 == 0 {
            dl17_list_input(&theme, ScreenId::Recents, step % 6)
        } else {
            library.clone()
        };
        frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
        let reference = dl15_reference(theme.clone(), &input);
        let damage = frame.paint_damage(&mut display).unwrap();
        #[cfg(feature = "perf-bench")]
        presenter
            .present(display.0.as_ref(), &damage, (step % 2) as u32)
            .unwrap();
        #[cfg(not(feature = "perf-bench"))]
        let _ = damage;
        assert!(frame.damage.iter().map(|r| r.2 * r.3).sum::<u32>() < 640 * 480);
        assert!(
            frame.pixels.data() == reference.pixels.data(),
            "tab composition {step}"
        );
        assert!(
            display.0.data() == reference.pixels.data(),
            "tab presentation {step}"
        );
        assert_eq!(frame.targets, reference.targets);
        assert_eq!(frame.text_boxes, reference.text_boxes);
    }
}

#[test]
fn dl17_empty_library_actions_preserves_damage_budget() {
    let (theme, mut input) = dl15_library();
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    input.focus.item = Some("1".into());
    input
        .text
        .insert(Binding::SelectedTitle, "WipEout 3".into());
    input.art.selected_hero = input
        .art
        .selected_hero
        .map(|art| std::sync::Arc::new((*art).clone()));
    let frame = PreparedFrame::prepare_reusing(theme, &input, Some(frame)).unwrap();
    assert!(frame.damage.iter().map(|r| r.2 * r.3).sum::<u32>() <= 225_024);
}
