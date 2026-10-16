use super::*;
fn audit_value(v: &serde_json::Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").chars().take(512).collect()
}
thread_local! {static AUDIT_ART_CACHE:std::cell::RefCell<std::collections::BTreeMap<String,Arc<tiny_skia::Pixmap>>>=const{std::cell::RefCell::new(std::collections::BTreeMap::new())};}
fn audit_art(
    root: &Path,
    assets: &serde_json::Value,
    src: &str,
    w: u32,
    h: u32,
) -> Option<Arc<tiny_skia::Pixmap>> {
    let key = format!("{src}:{w}:{h}");
    if let Some(found) = AUDIT_ART_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some(found);
    }
    let path = assets[src].as_str()?;
    let source = tiny_skia::Pixmap::load_png(root.join(path)).unwrap();
    let ratio = (w as f64 / source.width() as f64)
        .min(h as f64 / source.height() as f64)
        .min(1.0);
    let (w, h) = (
        (source.width() as f64 * ratio).round() as u32,
        (source.height() as f64 * ratio).round() as u32,
    );
    let image = Arc::new(
        crate::theme::runtime::fit_image(&source, w, h, crate::theme::pack::Fit::Contain).unwrap(),
    );
    AUDIT_ART_CACHE.with(|cache| {
        cache.borrow_mut().insert(key, image.clone());
    });
    Some(image)
}
fn audit_input(
    record: &serde_json::Value,
    ready: &ThemeHandle,
    root: &Path,
    assets: &serde_json::Value,
) -> FrameInputs {
    let m = &record["model"];
    let screen: ScreenId = serde_json::from_value(record["screen"].clone()).unwrap();
    let mut f = FrameInputs::new(screen);
    for name in [
        "title",
        "subtitle",
        "breadcrumb",
        "selected_title",
        "selected_metadata",
        "selected_blurb",
        "selected_source",
        "empty_title",
        "empty_hint",
        "message",
        "keyboard_value",
        "guide_text",
    ] {
        let binding: Binding = serde_json::from_value(name.into()).unwrap();
        f.text.insert(binding, audit_value(m, name));
    }
    f.text.insert(Binding::ThemeName, ready.pack().name.clone());
    if let Some(p) = m["pagination"].as_object() {
        f.pagination = Some(super::super::frame::Pagination {
            page: p["page"].as_u64().unwrap_or(1) as usize,
            total: p["total"].as_u64().unwrap_or(1) as usize,
        });
    }
    if let Some(lines) = m["selected_blurb_runs"].as_array() {
        f.detail_lines = lines
            .iter()
            .map(|l| super::super::frame::DetailLine {
                text: audit_value(l, "text"),
                heading: l["heading"] == true,
                gap_before: l["gap_before"].as_u64().unwrap_or(0) as u32,
            })
            .collect();
    }
    if let Some(p) = m["pagination"].as_object() {
        let page = p["page"].as_u64().unwrap_or(1);
        let total = p["total"].as_u64().unwrap_or(1);
        f.text
            .insert(Binding::Pagination, format!("{page} / {total}"));
    }
    for (name, target) in [
        ("selected_metadata_runs", &mut f.metadata),
        ("selected_facts", &mut f.facts),
    ] {
        if let Some(rows) = m[name].as_array() {
            for row in rows {
                target.push((audit_value(row, "label"), audit_value(row, "value")));
            }
        }
    }
    for name in [
        "entries",
        "settings_rows",
        "discs",
        "storage_entries",
        "guides",
        "actions",
    ] {
        let binding: Binding = serde_json::from_value(name.into()).unwrap();
        let c = ready.pack().screens[&screen]
            .components
            .iter()
            .find(|c| c.binding == Some(binding));
        let count = c.and_then(|c| c.visible_count).unwrap_or(16) as usize;
        let mut rows = Vec::new();
        for (i, row) in m[name]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["empty"] != true)
            .take(count)
            .enumerate()
        {
            let id = row["actionId"]
                .as_str()
                .or(row["gameId"].as_str())
                .or(row["id"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{name}-{i}"));
            if let Some(source) = row["image"].as_str().filter(|_| c.is_some()) {
                if let Some(image) = audit_art(root, assets, source, 160, 180) {
                    f.art.entries.insert(id.clone(), image);
                }
            }
            let _val = if row["value"].as_str().is_some_and(|s| !s.is_empty()) {
                audit_value(row, "value")
            } else if c
                .and_then(|c| c.paint.as_ref())
                .is_some_and(|p| p.subtitle.is_some())
            {
                audit_value(row, "subtitle")
            } else {
                String::new()
            };
            rows.push(Row {
                subtitle: audit_value(row, "subtitle"),
                id,
                label: audit_value(row, "title"),
                value: audit_value(row, "value"),
                enabled: row["disabled"] != true,
            });
        }
        f.rows.insert(binding, rows);
    }
    for (name, group) in [
        ("primary", NavGroup::Primary),
        ("library_actions", NavGroup::LibraryActions),
    ] {
        let rows = m["navigation_groups"][name]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| Row {
                subtitle: String::new(),
                id: audit_value(row, "id"),
                label: audit_value(row, "title"),
                value: String::new(),
                enabled: true,
            })
            .collect();
        f.navigation.insert(group, rows);
    }
    let nav = m["activeNav"].as_u64().unwrap_or(1) as usize;
    if let Some(row) = f.navigation[&NavGroup::Primary].get(nav) {
        f.selected.insert(Binding::Navigation, row.id.clone());
    }
    let focused = m["focused"].as_u64().unwrap_or(0) as usize;
    let focus_binding = match screen {
        ScreenId::Library
        | ScreenId::SearchResults
        | ScreenId::Apps
        | ScreenId::ContextMenu
        | ScreenId::IngameMenu => Some(Binding::Entries),
        ScreenId::SettingsRoot | ScreenId::SettingsPage => Some(Binding::SettingsRows),
        ScreenId::DiscPicker => Some(Binding::Discs),
        ScreenId::GuideSelector => Some(Binding::Guides),
        ScreenId::GameDetail | ScreenId::Toast | ScreenId::NetworkDialog => Some(Binding::Actions),
        _ => None,
    };
    if let Some(binding) = focus_binding {
        if let Some(c) = ready.pack().screens[&screen]
            .components
            .iter()
            .find(|c| c.binding == Some(binding))
        {
            if let Some(row) =
                f.rows[&binding].get(focused.min(f.rows[&binding].len().saturating_sub(1)))
            {
                if row.enabled {
                    f.focus = Focus {
                        component: c.id.clone(),
                        item: Some(row.id.clone()),
                    };
                }
            }
        }
    }
    for hint in m["hints"].as_array().unwrap() {
        let mut button = audit_value(hint, "button");
        if button.contains('↑') || button.contains('←') {
            button = "Dpad".into();
        }
        f.hints.push(Hint {
            button,
            label: audit_value(hint, "label"),
        });
    }
    f.status.clock = Some(audit_value(&m["status"], "clock"));
    f.status.battery = Some(m["status"]["battery"].as_u64().unwrap_or(82) as u8);
    f.status.network = Some(audit_value(&m["status"], "network"));
    f.status.charging = m["status"]["charging"] == true;
    f.progress = m["progress"].as_f64().map(|p| (p / 100.0) as f32);
    f.keyboard = (0..45).map(|_| String::new()).collect();
    for (row, letters) in ["QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"].iter().enumerate() {
        for (col, ch) in letters.chars().enumerate() {
            f.keyboard[row * 11 + col] = ch.to_string();
        }
    }
    f.keyboard[33] = "⌫".into();
    f.keyboard[34] = "Clear".into();
    f.keyboard[35] = "Results".into();
    f.keyboard[44] = "Space".into();
    f.keyboard_cursor = (0, 0);
    if let Some(src) = m["selected_logo"].as_str() {
        let path = Path::new(&std::env::var("PARITY_WEB_SOURCE").unwrap())
            .join("dist")
            .join(src);
        if path.is_file() {
            let source = tiny_skia::Pixmap::load_png(path).unwrap();
            let scale = (640.0 / source.width() as f64)
                .min(480.0 / source.height() as f64)
                .min(1.0);
            f.art.selected_logo = Some(Arc::new(
                crate::theme::runtime::fit_image(
                    &source,
                    (source.width() as f64 * scale) as u32,
                    (source.height() as f64 * scale) as u32,
                    crate::theme::pack::Fit::Contain,
                )
                .unwrap(),
            ));
        }
    }
    f.art.selected_cover = audit_art(
        root,
        assets,
        m["selected_cover"].as_str().unwrap_or(""),
        320,
        320,
    );
    f.art.selected_hero = audit_art(
        root,
        assets,
        m["selected_hero"].as_str().unwrap_or(""),
        640,
        360,
    );
    f.art.next_cover = audit_art(
        root,
        assets,
        m["next_cover"].as_str().unwrap_or(""),
        160,
        180,
    );
    f
}
#[test]
#[ignore = "explicit parity capture with retained audit fixtures"]
fn parity_d_capture_web_models() {
    let root = PathBuf::from(std::env::var("PARITY_REFERENCE_ROOT").unwrap());
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("vm-models.json")).unwrap()).unwrap();
    let assets: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("model-assets.json")).unwrap()).unwrap();
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut count = 0;
    let selected_screens = std::env::var("PARITY_SCREENS").ok();
    let records: Vec<_> = data["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            selected_screens
                .as_ref()
                .is_none_or(|screens| screens.split(',').any(|s| Some(s) == r["screen"].as_str()))
        })
        .collect();
    let expected: usize = records
        .iter()
        .map(|r| if r["theme"] == "compact-plus" { 4 } else { 1 })
        .sum();
    for record in records {
        let id = record["pack"].as_str().unwrap();
        let name = record["screen"].as_str().unwrap();
        let base = record["theme"].as_str().unwrap();
        let accents: &[&str] = if base == "compact-plus" {
            &["orange", "big-picture", "white", "black"]
        } else {
            &["default"]
        };
        for accent in accents {
            let mut settings = std::collections::BTreeMap::from([(
                "cover_shape".into(),
                record["shape"].as_str().unwrap().into(),
            )]);
            if base == "compact-plus" {
                settings.insert("color_scheme".into(), accent.to_string());
            }
            let ready = ThemeHandle::prepare(
                ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings).unwrap(),
                &bank,
                500,
            )
            .unwrap();
            let input = audit_input(record, &ready, &root, &assets);
            let frame = PreparedFrame::prepare(ready.clone(), &input)
                .unwrap_or_else(|e| panic!("{id}/{name}: {e:#}"));
            let mut pixels = frame.pixels().clone();
            if input.screen.is_dialog() || input.screen.is_osd() {
                let library = data["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["pack"] == id && r["screen"] == "library")
                    .unwrap();
                let mut base = audit_input(library, &ready, &root, &assets);
                base.hints.clear();
                pixels = PreparedFrame::prepare(ready, &base)
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
            }
            let output = PathBuf::from(std::env::var("PARITY_OUTPUT_ROOT").unwrap());
            let dir = output
                .join("frames")
                .join(if *accent == "default" || *accent == "orange" {
                    id.to_string()
                } else {
                    format!("{id}-{accent}")
                });
            std::fs::create_dir_all(&dir).unwrap();
            pixels
                .save_png(dir.join(format!("{name}-native.png")))
                .unwrap();
            let historical = root
                .join("frames")
                .join(id)
                .join(format!("{name}-web-historical.png"));
            if historical.is_file() {
                let reference = image::open(&historical)
                    .unwrap()
                    .resize_exact(640, 480, image::imageops::FilterType::Triangle)
                    .to_rgba8();
                let native = image::RgbaImage::from_raw(640, 480, pixels.data().to_vec()).unwrap();
                let mut pair = image::RgbaImage::new(1280, 480);
                image::imageops::replace(&mut pair, &reference, 0, 0);
                image::imageops::replace(&mut pair, &native, 640, 0);
                let pair_dir = output.join("side-by-side").join(dir.file_name().unwrap());
                std::fs::create_dir_all(&pair_dir).unwrap();
                pair.save(pair_dir.join(format!("{name}-left-web-historical-right-native.png")))
                    .unwrap();
            }
            count += 1;
            println!("AUDIT_FRAME {id}/{name}");
        }
    }
    assert_eq!(count, expected);
    println!("AUDIT_FRAMES={count}");
}

#[test]
fn parity_d_pagination_and_image_focus_invalidate_retained_pixels() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let ready = ThemeHandle::prepare(
        ThemeSnapshot::load(&ink_pack_root().join("orbit-hero")).unwrap(),
        &bank,
        100,
    )
    .unwrap();
    let mut input = frame(ScreenId::GameDetail);
    input.pagination = Some(super::super::frame::Pagination { page: 1, total: 2 });
    let mut rendered = PreparedFrame::prepare(ready.clone(), &input).unwrap();
    assert!(rendered.text_boxes.iter().any(|(id, _)| id == "pages"));
    let first = rendered.pixels().data().to_vec();
    input.pagination = Some(super::super::frame::Pagination { page: 2, total: 2 });
    rendered = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(rendered)).unwrap();
    assert_ne!(rendered.pixels().data(), first);
    assert_eq!(
        rendered.pixels().data(),
        PreparedFrame::prepare(ready.clone(), &input)
            .unwrap()
            .pixels()
            .data()
    );
    input.pagination = Some(super::super::frame::Pagination { page: 1, total: 1 });
    rendered = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(rendered)).unwrap();
    assert!(!rendered.text_boxes.iter().any(|(id, _)| id == "pages"));
    let mut input = frame(ScreenId::Library);
    let c = ready.pack().screens[&ScreenId::Library]
        .components
        .iter()
        .find(|c| c.paint.as_ref().is_some_and(|p| p.action.is_some()))
        .unwrap();
    let mut rendered = PreparedFrame::prepare(ready.clone(), &input).unwrap();
    let idle = rendered.pixels().data().to_vec();
    let action = c.paint.as_ref().unwrap().action.clone().unwrap();
    assert!(
        rendered
            .targets
            .iter()
            .any(|t| t.component == c.id && t.item == action)
    );
    input.focus = Focus {
        component: c.id.clone(),
        item: Some(action),
    };
    rendered = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(rendered)).unwrap();
    assert_ne!(rendered.pixels().data(), idle);
    assert_eq!(
        rendered.pixels().data(),
        PreparedFrame::prepare(ready.clone(), &input)
            .unwrap()
            .pixels()
            .data()
    );
}

#[test]
fn parity_d_styled_sections_keep_headings_with_body_and_fit_every_option() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            for accent in if base == "compact-plus" {
                vec!["orange", "big-picture", "white", "black"]
            } else {
                vec!["default"]
            } {
                let mut settings =
                    std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]);
                if base == "compact-plus" {
                    settings.insert("color_scheme".into(), accent.into());
                }
                let ready = ThemeHandle::prepare(
                    ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings)
                        .unwrap(),
                    &bank,
                    100,
                )
                .unwrap();
                let c = ready.pack().screens[&ScreenId::GameDetail]
                    .components
                    .iter()
                    .find(|c| c.binding == Some(Binding::SelectedBlurb))
                    .unwrap();
                let sections = vec![
                    (
                        Some("SYNOPSIS".into()),
                        "A long description with accents é and a game story. ".repeat(40),
                    ),
                    (
                        Some("PROVENANCE".into()),
                        "First release: 1998\nSource: Wikipedia\nCC BY-SA".into(),
                    ),
                ];
                let pages = super::super::render::ink_detail_pages(&ready, c, &sections, true);
                assert!(pages.len() > 1);
                for lines in pages {
                    assert!(!lines.last().unwrap().heading, "{base}/{shape}/{accent}");
                    let mut input = frame(ScreenId::GameDetail);
                    input.detail_lines = lines;
                    let rendered = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                    assert!(
                        rendered
                            .text_boxes
                            .iter()
                            .filter(|(id, _)| id == &c.id)
                            .all(|(_, r)| r.1 + r.3 <= c.rect.1 + c.rect.3)
                    );
                }
            }
        }
    }
}

#[test]
fn parity_d_retained_logos_follow_art_changes_on_text_slots() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let ready = ThemeHandle::prepare(
        ThemeSnapshot::load(&ink_pack_root().join("compact-plus")).unwrap(),
        &bank,
        100,
    )
    .unwrap();
    let mut input = frame(ScreenId::Library);
    let mut red = tiny_skia::Pixmap::new(96, 24).unwrap();
    red.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
    input.art.selected_logo = Some(Arc::new(red));
    let first = PreparedFrame::prepare(ready.clone(), &input).unwrap();
    let bytes = first.pixels().data().to_vec();
    let mut blue = tiny_skia::Pixmap::new(96, 24).unwrap();
    blue.fill(tiny_skia::Color::from_rgba8(0, 0, 255, 255));
    input.art.selected_logo = Some(Arc::new(blue));
    let next = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(first)).unwrap();
    assert_ne!(next.pixels().data(), bytes);
    assert_eq!(
        next.pixels().data(),
        PreparedFrame::prepare(ready.clone(), &input)
            .unwrap()
            .pixels()
            .data()
    );
    input.art.selected_logo = None;
    let fallback = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(next)).unwrap();
    assert_eq!(
        fallback.pixels().data(),
        PreparedFrame::prepare(ready, &input)
            .unwrap()
            .pixels()
            .data()
    );
}
