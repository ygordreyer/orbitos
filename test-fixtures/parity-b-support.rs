fn parity_variants() -> Vec<(String, common::theme::ThemeHandle)> {
    use common::theme::{FontBank, ThemeHandle, runtime::ThemeSnapshot};
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
    let mut result = Vec::new();
    for theme in ["big-picture", "compact-plus", "orbit-hero", "orbit-row"] {
        for shape in ["square", "tall"] {
            for accent in if theme == "compact-plus" {
                vec!["orange", "big-picture", "white", "black"]
            } else {
                vec!["default"]
            } {
                let mut settings =
                    std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]);
                if theme == "compact-plus" {
                    settings.insert("color_scheme".into(), accent.into());
                }
                let snapshot =
                    ThemeSnapshot::load_with_settings(&repo.join("themes").join(theme), &settings)
                        .unwrap();
                result.push((
                    format!("{theme}-{shape}-{accent}"),
                    ThemeHandle::prepare(snapshot, &bank, 801).unwrap(),
                ));
            }
        }
    }
    result
}
fn parity_save(variant: &str, name: &str, pixels: &tiny_skia::Pixmap) {
    if let Ok(out) = std::env::var("ALLIUM_PARITY_B_FRAMES") {
        let dir = std::path::Path::new(&out).join(variant);
        std::fs::create_dir_all(&dir).unwrap();
        pixels.save_png(dir.join(format!("{name}.png"))).unwrap();
    }
}
fn parity_save_overlay(variant: &str, name: &str, frame: &common::theme::render::PreparedFrame) {
    use common::theme::{
        frame::{FrameInputs, Row, Status},
        pack::{Binding, NavGroup, ScreenId},
        render::PreparedFrame,
    };
    if std::env::var_os("ALLIUM_PARITY_B_FRAMES").is_none() {
        return;
    }
    let mut input = FrameInputs::new(ScreenId::Library);
    input.text.insert(Binding::Title, "Library".into());
    input
        .text
        .insert(Binding::SelectedTitle, "R4 - Ridge Racer Type 4".into());
    input
        .text
        .insert(Binding::SelectedMetadata, "PlayStation · 1998".into());
    input.rows.insert(
        Binding::Entries,
        ["WipEout 3", "R4 - Ridge Racer Type 4", "Crash Team Racing"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| Row {
                id: format!("game-{i}"),
                label: label.into(),
                subtitle: String::new(),
                value: String::new(),
                enabled: true,
            })
            .collect(),
    );
    input.navigation.insert(
        NavGroup::Primary,
        ["Recents", "Library", "Apps", "Settings"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| Row {
                id: format!("tab-{i}"),
                label: label.into(),
                subtitle: String::new(),
                value: String::new(),
                enabled: true,
            })
            .collect(),
    );
    input.selected.insert(Binding::Navigation, "tab-1".into());
    input.status = Status {
        clock: Some("12:34".into()),
        battery: Some(82),
        network: Some("online".into()),
        ..Default::default()
    };
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    for (index, stem) in ["wipeout", "r4", "ctr"].into_iter().enumerate() {
        let source = tiny_skia::Pixmap::load_png(repo.join(format!("test-fixtures/real-art/{stem}-cover.png"))).unwrap();
        let scale = (320.0 / source.width() as f32).min(320.0 / source.height() as f32).min(1.0);
        let mut cover = tiny_skia::Pixmap::new((source.width() as f32 * scale).round() as u32, (source.height() as f32 * scale).round() as u32).unwrap();
        cover.draw_pixmap(0, 0, source.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::from_scale(scale, scale), None);
        let cover = std::sync::Arc::new(cover);
        if index == 1 {
            input.art.selected_cover = Some(cover.clone());
        }
        input.art.entries.insert(format!("game-{index}"), cover);
    }
    let mut pixels = PreparedFrame::prepare(frame.snapshot().clone(), &input)
        .unwrap()
        .pixels()
        .clone();
    pixels.draw_pixmap(
        0,
        0,
        frame.pixels().as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    parity_save(variant, name, &pixels);
}
