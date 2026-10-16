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
        super::super::runtime::fit_image(&source, w, h, super::super::pack::Fit::Contain).unwrap(),
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
            rows.push(Row {
                id,
                label: audit_value(row, "title"),
                value: audit_value(row, "value"),
                subtitle: audit_value(row, "subtitle"),
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
                id: audit_value(row, "id"),
                label: audit_value(row, "title"),
                value: String::new(),
                subtitle: String::new(),
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
    f.art.selected_logo = audit_art(
        root,
        assets,
        m["selected_logo"].as_str().unwrap_or(""),
        320,
        320,
    );
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
#[ignore = "Explicit evidence capture requires PARITY_AUDIT_ROOT and PARITY_A_OUTPUT"]
fn parity_a_capture_all_options() {
    let root = PathBuf::from(std::env::var("PARITY_AUDIT_ROOT").unwrap());
    let out = PathBuf::from(std::env::var("PARITY_A_OUTPUT").unwrap());
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("vm-models.json")).unwrap()).unwrap();
    let mut assets: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("model-assets.json")).unwrap()).unwrap();
    let web = Path::new("../orbit-web-sim/dist");
    for record in data["records"].as_array().unwrap() {
        if let Some(src) = record["model"]["selected_logo"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            let path = Path::new(src);
            assert!(
                src.starts_with("assets/")
                    && path
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
            );
            assets[src] = web.join(path).to_string_lossy().into_owned().into();
        }
    }
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut count = 0;
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            let accents: &[&str] = if base == "compact-plus" {
                &["orange", "big-picture", "white", "black"]
            } else {
                &["default"]
            };
            for accent in accents {
                let settings = std::collections::BTreeMap::from([
                    ("cover_shape".into(), shape.into()),
                    ("color_scheme".into(), (*accent).into()),
                ]);
                let snapshot =
                    ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings)
                        .unwrap();
                let ready = ThemeHandle::prepare(snapshot, &bank, 500).unwrap();
                let id = format!("{base}-{shape}-{accent}");
                let historical = format!("{base}{}", if shape == "tall" { "-tall" } else { "" });
                let records: Vec<_> = data["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|r| r["pack"] == historical)
                    .collect();
                for record in &records {
                    let name = record["screen"].as_str().unwrap();
                    let input = audit_input(record, &ready, &root, &assets);
                    let rendered = PreparedFrame::prepare(ready.clone(), &input)
                        .unwrap_or_else(|e| panic!("{id}/{name}: {e:#}"));
                    let mut pixels = rendered.pixels().clone();
                    if input.screen.is_dialog() || input.screen.is_osd() {
                        let library = records.iter().find(|r| r["screen"] == "library").unwrap();
                        let mut model = audit_input(library, &ready, &root, &assets);
                        model.hints.clear();
                        pixels = PreparedFrame::prepare(ready.clone(), &model)
                            .unwrap()
                            .pixels()
                            .clone();
                        pixels.draw_pixmap(
                            0,
                            0,
                            rendered.pixels().as_ref(),
                            &tiny_skia::PixmapPaint::default(),
                            tiny_skia::Transform::identity(),
                            None,
                        );
                    }
                    let dir = out.join("frames").join(&id);
                    std::fs::create_dir_all(&dir).unwrap();
                    pixels
                        .save_png(dir.join(format!("{name}-native.png")))
                        .unwrap();
                    println!(
                        "PARITY_FRAME {id}/{name} {}",
                        super::super::runtime::digest(pixels.data())
                    );
                    if name == "library" {
                        let states = out.join("states").join(&id);
                        std::fs::create_dir_all(&states).unwrap();
                        for state in ["offline-empty", "charging-full", "unavailable"] {
                            let mut model = input.clone();
                            match state {
                                "offline-empty" => {
                                    model.status.network = Some("offline".into());
                                    model.status.battery = Some(0);
                                }
                                "charging-full" => {
                                    model.status.battery = Some(100);
                                    model.status.charging = true;
                                }
                                _ => {
                                    model.status.network = None;
                                    model.status.battery = None;
                                    model.status.clock = None;
                                }
                            }
                            let frame = PreparedFrame::prepare(ready.clone(), &model).unwrap();
                            frame
                                .pixels()
                                .save_png(states.join(format!("{state}.png")))
                                .unwrap();
                            println!(
                                "PARITY_STATUS {id}/{state} {}",
                                super::super::runtime::digest(frame.pixels().data())
                            );
                        }
                    }
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 252);
}

#[test]
fn parity_a_status_icons_and_effects_restore_with_retained_frames() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            let settings = std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]);
            let ready = ThemeHandle::prepare(
                ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings).unwrap(),
                &bank,
                500,
            )
            .unwrap();
            let mut input = frame(ScreenId::Library);
            let entries = ready.pack().screens[&input.screen]
                .components
                .iter()
                .find(|c| c.binding == Some(Binding::Entries))
                .unwrap();
            input.focus = Focus {
                component: entries.id.clone(),
                item: Some("wipeout".into()),
            };
            input.status.network = Some("online".into());
            input.status.battery = Some(82);
            let before = PreparedFrame::prepare(ready.clone(), &input).unwrap();
            input.status.network = Some("offline".into());
            input.status.battery = Some(0);
            input.status.charging = true;
            input.focus.item = Some("r4".into());
            let after =
                PreparedFrame::prepare_reusing(ready.clone(), &input, Some(before)).unwrap();
            let fresh = PreparedFrame::prepare(ready.clone(), &input).unwrap();
            assert!(
                after.pixels().data() == fresh.pixels().data(),
                "retained effects: {base}/{shape}"
            );
            input.status.network = Some("online".into());
            let online = PreparedFrame::prepare(ready.clone(), &input).unwrap();
            assert!(
                online.pixels().data() != fresh.pixels().data(),
                "offline slash absent: {base}/{shape}"
            );
            input.status.network = None;
            input.status.battery = None;
            input.status.clock = None;
            let removed =
                PreparedFrame::prepare_reusing(ready.clone(), &input, Some(after)).unwrap();
            let fresh = PreparedFrame::prepare(ready, &input).unwrap();
            assert!(
                removed.pixels().data() == fresh.pixels().data(),
                "removed status: {base}/{shape}"
            );
        }
    }
}

#[test]
fn parity_a_stacked_battery_icon_and_shadow_extend_outside_surface() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut snapshot = ThemeSnapshot::load(&ink_pack_root().join("compact-plus")).unwrap();
    let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
    let screen = pack.screens.get_mut(&ScreenId::GameDetail).unwrap();
    let mut status = screen
        .components
        .iter()
        .find(|c| c.kind == super::super::pack::Kind::Status)
        .unwrap()
        .clone();
    status.battery_style = Some(super::super::pack::BatteryStyle::Icon);
    screen.components = vec![status.clone()];
    let ready = ThemeHandle::prepare(snapshot, &bank, 500).unwrap();
    let mut input = FrameInputs::new(ScreenId::GameDetail);
    let absent = PreparedFrame::prepare(ready.clone(), &input).unwrap();
    input.status.battery = Some(100);
    let present = PreparedFrame::prepare(ready, &input).unwrap();
    assert!(
        present.pixels().data() != absent.pixels().data(),
        "icon-only stacked battery was omitted"
    );
    let mut snapshot = ThemeSnapshot::load(&ink_pack_root().join("orbit-hero")).unwrap();
    let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
    let screen = pack.screens.get_mut(&ScreenId::Library).unwrap();
    let mut panel = screen
        .components
        .iter()
        .find(|c| {
            c.kind == super::super::pack::Kind::Panel && c.paint.as_ref().unwrap().shadow.is_some()
        })
        .unwrap()
        .clone();
    panel.rect = super::super::pack::Rect(100, 100, 100, 60);
    let shadow = panel.paint.as_mut().unwrap().shadow.as_mut().unwrap();
    shadow.alpha = 1.0;
    shadow.blur = 8;
    shadow.y = 4;
    screen.components = vec![panel];
    let ready = ThemeHandle::prepare(snapshot, &bank, 501).unwrap();
    let frame = PreparedFrame::prepare(ready, &FrameInputs::new(ScreenId::Library)).unwrap();
    assert!(
        frame.pixels().pixel(150, 165).unwrap().alpha() > 0,
        "shadow clipped to the panel border"
    );
    assert_eq!(
        frame.pixels().pixel(150, 190).unwrap().alpha(),
        0,
        "shadow exceeded bounded blur support"
    );
}
