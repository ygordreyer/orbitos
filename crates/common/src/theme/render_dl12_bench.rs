#[test]
fn dl12_host_render_microbench() {
    if std::env::var_os("DL12_BENCH").is_none() {
        return;
    }
    use std::{hint::black_box, sync::Arc, time::Instant};
    let bank = super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut home = FrameInputs::new(ScreenId::Library);
    home.text
        .insert(Binding::SelectedTitle, "Clock Tower: The First Fear".into());
    home.text
        .insert(Binding::SelectedMetadata, "PlayStation · 1997".into());
    home.text.insert(
        Binding::SelectedBlurb,
        "A quiet mansion hides a terrible secret.".into(),
    );
    let mut art = Pixmap::new(390, 189).unwrap();
    for (i, p) in art.data_mut().chunks_exact_mut(4).enumerate() {
        p.copy_from_slice(&[(i % 251) as u8, (i % 157) as u8, (i % 79) as u8, 255]);
    }
    let art = Arc::new(art);
    home.art.selected_hero = Some(art.clone());
    home.art.selected_cover = Some(art.clone());
    home.art.next_cover = Some(art.clone());
    let rows: Vec<_> = ["Clock Tower", "WipEout 3", "Ridge Racer"]
        .iter()
        .enumerate()
        .map(|(i, name)| Row {
            id: i.to_string(),
            label: (*name).into(),
            value: "NAS".into(),
            enabled: true,
        })
        .collect();
    for row in &rows {
        home.art.entries.insert(row.id.clone(), art.clone());
    }
    home.selected.insert(Binding::Entries, "0".into());
    home.rows.insert(Binding::Entries, rows);
    home.focus.component = "entries".into();
    home.focus.item = Some("0".into());
    home.status.battery = Some(73);
    home.status.clock = Some("12:34".into());
    let mut frame = PreparedFrame::prepare(theme.clone(), &home).unwrap();
    let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        frame = PreparedFrame::prepare_reusing(theme.clone(), &home, Some(frame)).unwrap();
        frame.paint(&mut display);
        black_box(display.0.data());
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    println!(
        "DL12_HOST_MICRO {{\"operation\":\"home_compose\",\"median_ns\":{},\"release\":{}}}",
        samples[10],
        !cfg!(debug_assertions)
    );
    let mut menu = FrameInputs::new(ScreenId::IngameMenu);
    menu.text
        .insert(Binding::Title, "Clock Tower: The First Fear".into());
    menu.rows.insert(
        Binding::Entries,
        [
            "Resume",
            "Save state",
            "Load state",
            "Screenshot",
            "Change disc",
            "Exit game",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| Row {
            id: i.to_string(),
            label: (*name).into(),
            value: "".into(),
            enabled: true,
        })
        .collect(),
    );
    let prepared = PreparedFrame::prepare(theme, &menu).unwrap();
    let mut samples = Vec::new();
    for _ in 0..21 {
        display
            .0
            .fill(tiny_skia::Color::from_rgba8(31, 45, 91, 255));
        let start = Instant::now();
        for _ in 0..10 {
            prepared.paint(&mut display);
            black_box(display.0.data());
        }
        samples.push(start.elapsed().as_nanos() / 10);
    }
    samples.sort_unstable();
    println!(
        "DL12_HOST_MICRO {{\"operation\":\"menu_compose\",\"median_ns\":{},\"release\":{}}}",
        samples[10],
        !cfg!(debug_assertions)
    );
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..100 {
            frame
                .text_options(
                    "bench",
                    Rect(20, 20, 240, 48),
                    Role::Title,
                    ColorRole::Text,
                    "Clock Tower: The First Fear",
                    2,
                    Align::Left,
                    Overflow::Wrap,
                )
                .unwrap();
            frame.text_boxes.clear();
            black_box(frame.pixels.data());
        }
        samples.push(start.elapsed().as_nanos() / 100);
    }
    samples.sort_unstable();
    println!(
        "DL12_HOST_MICRO {{\"operation\":\"text_draw\",\"median_ns\":{},\"release\":{}}}",
        samples[10],
        !cfg!(debug_assertions)
    );
}
