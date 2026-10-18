use super::{
    fonts::{FontBank, ThemeHandle},
    frame::{Focus, FrameInputs, Hint, Row},
    pack::{Binding, NavGroup, ScreenId},
    render::PreparedFrame,
    runtime::ThemeSnapshot,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
fn font_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../static/.allium/fonts")
        .canonicalize()
        .unwrap()
}
fn frame(screen: ScreenId) -> FrameInputs {
    let mut f = FrameInputs::new(screen);
    for (b, value) in [
        (Binding::Title, "Library"),
        (Binding::SelectedTitle, "R4 - Ridge Racer Type 4"),
        (Binding::SelectedMetadata, "PlayStation · 1998"),
        (
            Binding::SelectedBlurb,
            "Race the golden hour. Find your line.",
        ),
        (Binding::Subtitle, "Disc 2 · game-disc-2.chd"),
        (
            Binding::Message,
            "Library unavailable\nInspect settings or go back",
        ),
        (Binding::KeyboardValue, "Ridge"),
        (
            Binding::GuideText,
            "Readable guide page\n日本語の見出し\nKeep the existing page cursor",
        ),
    ] {
        f.text.insert(b, value.into());
    }
    let games = vec![
        Row {
            subtitle: String::new(),
            id: "wipeout".into(),
            label: "WipEout 3".into(),
            value: "".into(),
            enabled: true,
        },
        Row {
            subtitle: String::new(),
            id: "r4".into(),
            label: "R4 - Ridge Racer Type 4".into(),
            value: "".into(),
            enabled: true,
        },
        Row {
            subtitle: String::new(),
            id: "ctr".into(),
            label: "Crash Team Racing".into(),
            value: "".into(),
            enabled: true,
        },
    ];
    f.rows.insert(Binding::Entries, games);
    f.rows.insert(
        Binding::SettingsRows,
        vec![Row {
            subtitle: String::new(),
            id: "theme".into(),
            label: "Theme".into(),
            value: "Preset".into(),
            enabled: true,
        }],
    );
    f.rows.insert(
        Binding::Discs,
        (1..=4)
            .map(|i| Row {
                subtitle: String::new(),
                id: format!("disc-{i}"),
                label: format!("Disc {i}"),
                value: format!("game-disc-{i}.chd"),
                enabled: true,
            })
            .collect(),
    );
    f.rows.insert(
        Binding::StorageEntries,
        vec![Row {
            subtitle: String::new(),
            id: "cached".into(),
            label: "Large cached game".into(),
            value: "1.4 GiB".into(),
            enabled: true,
        }],
    );
    f.rows.insert(
        Binding::Guides,
        vec![Row {
            subtitle: String::new(),
            id: "guide".into(),
            label: "Guidebook".into(),
            value: "".into(),
            enabled: true,
        }],
    );
    f.rows.insert(
        Binding::Actions,
        vec![
            Row {
                subtitle: String::new(),
                id: "confirm".into(),
                label: "Confirm".into(),
                value: "".into(),
                enabled: true,
            },
            Row {
                subtitle: String::new(),
                id: "cancel".into(),
                label: "Back".into(),
                value: "".into(),
                enabled: true,
            },
        ],
    );
    f.navigation.insert(
        NavGroup::Primary,
        ["Recents", "Library", "Apps", "Settings"]
            .into_iter()
            .map(|s| Row {
                subtitle: String::new(),
                id: s.to_lowercase(),
                label: s.into(),
                value: "".into(),
                enabled: true,
            })
            .collect(),
    );
    f.navigation.insert(NavGroup::LibraryActions, vec![]);
    f.focus = Focus {
        component: if screen == ScreenId::Library {
            "entries".into()
        } else {
            String::new()
        },
        item: if screen == ScreenId::Library {
            Some("r4".into())
        } else {
            None
        },
    };
    f.selected.insert(Binding::Entries, "r4".into());
    f.hints = vec![
        Hint {
            button: "A".into(),
            label: "Select".into(),
        },
        Hint {
            button: "B".into(),
            label: "Back".into(),
        },
    ];
    f.status.clock = Some("12:34".into());
    f.status.battery = Some(82);
    f.status.network = Some("online".into());
    f.progress = Some(0.4);
    f.keyboard = (0..44)
        .map(|i| char::from(b'a' + (i % 26) as u8).to_string())
        .chain(std::iter::once("Space".into()))
        .collect();
    f.keyboard_cursor = (5, 2);
    if let Ok(art) = std::env::var("ALLIUM_THEME_REAL_ART") {
        let cover = tiny_skia::Pixmap::load_png(Path::new(&art).join("r4-cover.png")).unwrap();
        let hero = tiny_skia::Pixmap::load_png(Path::new(&art).join("r4-hero.png")).unwrap();
        f.art.selected_cover = Some(Arc::new(
            super::runtime::fit_image(&cover, 164, 164, super::pack::Fit::Contain).unwrap(),
        ));
        f.art.selected_hero = Some(Arc::new(hero));
        for (id, file) in [
            ("wipeout", "wipeout-cover.png"),
            ("r4", "r4-cover.png"),
            ("ctr", "ctr-cover.png"),
        ] {
            let source = tiny_skia::Pixmap::load_png(Path::new(&art).join(file)).unwrap();
            f.art.entries.insert(
                id.into(),
                Arc::new(
                    super::runtime::fit_image(&source, 132, 132, super::pack::Fit::Contain)
                        .unwrap(),
                ),
            );
        }
    }
    if screen == ScreenId::Library {
        f.text.insert(Binding::SelectedMetadata, String::new());
        f.text.insert(Binding::SelectedBlurb, String::new());
    }
    if screen == ScreenId::GameDetail {
        f.rows.get_mut(&Binding::Entries).unwrap().clear();
    }
    f
}
#[test]
fn theme_foundation_real_fonts_all_screens_caption_and_owned_resources() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for id in ["orbit", "compact"] {
        let ready = ThemeHandle::prepare(ThemeSnapshot::builtin(id).unwrap(), &bank, 1).unwrap();
        for screen in ScreenId::ALL {
            let mut f = frame(screen);
            if matches!(screen, ScreenId::SettingsRoot | ScreenId::SettingsPage) {
                f.rows.get_mut(&Binding::SettingsRows).unwrap()[0].value =
                    ready.pack().name.clone();
            }
            let prepared = PreparedFrame::prepare(ready.clone(), &f).unwrap();
            assert_eq!(prepared.pixel_bytes(), 640 * 480 * 4);
            if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
                let screen = serde_json::to_value(screen)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned();
                prepared
                    .pixels()
                    .save_png(Path::new(&root).join(format!("{id}-{screen}.png")))
                    .unwrap();
            }
            if screen == ScreenId::Library {
                assert_eq!(
                    prepared
                        .targets
                        .iter()
                        .filter(|t| t.component == "entries")
                        .map(|t| t.item.as_str())
                        .collect::<Vec<_>>(),
                    vec!["wipeout", "r4", "ctr"]
                );
            }
            if screen == ScreenId::Library && id == "orbit" {
                for (_, r) in &prepared.text_boxes {
                    assert!(r.0 + r.2 <= 640 && r.1 + r.3 <= 480);
                }
                let captions: Vec<_> = prepared
                    .text_boxes
                    .iter()
                    .filter(|(name, _)| name == "entries")
                    .collect();
                assert_eq!(captions.len(), 3);
                assert!(
                    captions
                        .iter()
                        .all(|(_, r)| r.1 == 386 && r.3 == 36 && r.1 + r.3 == 422)
                );
                assert!(prepared.targets.iter().any(|t| t.item == "r4"));
            }
        }
    }
    assert_eq!(bank.accounting().0, 3);
    assert!(bank.accounting().1 < 32 * 1024 * 1024);
}

#[test]
fn theme_library_cases_clip_art_corners_and_bound_selected_halo() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    let mut art = tiny_skia::Pixmap::new(132, 132).unwrap();
    for pixel in art.data_mut().chunks_exact_mut(4) {
        pixel.copy_from_slice(&[255, 0, 255, 255]);
    }
    for id in ["wipeout", "r4", "ctr"] {
        input.art.entries.insert(id.into(), Arc::new(art.clone()));
    }

    input.focus = Focus::default();
    let unfocused = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    input.focus = Focus {
        component: "entries".into(),
        item: Some("r4".into()),
    };
    let focused = PreparedFrame::prepare(theme, &input).unwrap();
    let pixel = |frame: &PreparedFrame, x: u32, y: u32| -> [u8; 4] {
        let at = ((y * 640 + x) * 4) as usize;
        frame.pixels().data()[at..at + 4].try_into().unwrap()
    };

    assert_ne!(pixel(&focused, 208, 242), [255, 0, 255, 255]);
    assert_eq!(pixel(&focused, 216, 250), [255, 0, 255, 255]);
    assert_ne!(pixel(&focused, 28, 242), [255, 0, 255, 255]);
    assert_eq!(pixel(&focused, 36, 250), [255, 0, 255, 255]);
    assert_ne!(pixel(&focused, 202, 308), pixel(&unfocused, 202, 308));
    assert_eq!(pixel(&focused, 198, 308), pixel(&unfocused, 198, 308));
    assert_ne!(pixel(&focused, 210, 385), pixel(&unfocused, 210, 385));
    assert_eq!(pixel(&focused, 210, 390), pixel(&unfocused, 210, 390));
    assert_eq!(focused.pixel_bytes(), 640 * 480 * 4);
}

#[test]
fn orbit_footer_bottom_aligns_one_and_two_row_hint_blocks() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut baseline_input = frame(ScreenId::Library);
    baseline_input.hints.clear();
    let baseline = PreparedFrame::prepare(theme.clone(), &baseline_input).unwrap();

    for (name, hints) in [
        (
            "one-row",
            vec![Hint {
                button: "A".into(),
                label: "Select".into(),
            }],
        ),
        (
            "four-actions",
            vec![
                Hint {
                    button: "A".into(),
                    label: "Play".into(),
                },
                Hint {
                    button: "B".into(),
                    label: "Back".into(),
                },
                Hint {
                    button: "START".into(),
                    label: "Details".into(),
                },
                Hint {
                    button: "Dpad".into(),
                    label: "Browse".into(),
                },
            ],
        ),
    ] {
        let mut input = baseline_input.clone();
        input.hints = hints;
        let rendered = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let mut changed_rows = Vec::new();
        for y in 434..480 {
            let start = (y * 640 * 4) as usize;
            let end = start + 640 * 4;
            if rendered.pixels().data()[start..end] != baseline.pixels().data()[start..end] {
                changed_rows.push(y);
            }
        }
        assert_eq!(changed_rows.first().copied(), Some(448));
        assert!(changed_rows.last().copied().unwrap() < 468);
        if let Ok(root) = std::env::var("ALLIUM_HINTS_CORRECTION_PROOF_DIR") {
            rendered
                .pixels()
                .save_png(Path::new(&root).join(format!("orbit-footer-{name}.png")))
                .unwrap();
        }
    }
}

#[test]
fn keycap_two_row_height_fits_or_fails_before_unsigned_subtraction() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut input = frame(ScreenId::Library);
    input.hints = vec![
        Hint {
            button: "A".into(),
            label: "Play selected game".into(),
        },
        Hint {
            button: "B".into(),
            label: "Back to collection".into(),
        },
        Hint {
            button: "START".into(),
            label: "Game details and information".into(),
        },
        Hint {
            button: "Dpad".into(),
            label: "Browse games".into(),
        },
    ];

    let theme_with_height = |height| {
        let mut snapshot = ThemeSnapshot::builtin("orbit").unwrap();
        let resolved = Arc::get_mut(&mut snapshot.0).unwrap();
        let hint_style = resolved.pack.tokens.typography.get_mut(&super::pack::Role::Hint).unwrap();
        hint_style.size = 18;
        hint_style.line_height = 18;
        let hints = resolved
            .pack
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .iter_mut()
            .find(|component| component.binding == Some(Binding::Hints))
            .unwrap();
        hints.rect.3 = height;
        hints.max_lines = None;
        ThemeHandle::prepare(snapshot, &bank, 1).unwrap()
    };

    let fitted = PreparedFrame::prepare(theme_with_height(40), &input).unwrap();
    let boxes = fitted
        .text_boxes
        .iter()
        .filter(|(id, _)| id == "hints")
        .collect::<Vec<_>>();
    assert_eq!(boxes.len(), input.hints.len() * 2);
    assert!(
        boxes
            .iter()
            .all(|(_, rect)| rect.1 >= 436 && rect.1 + rect.3 <= 476)
    );
    assert!(PreparedFrame::prepare(theme_with_height(39), &input).is_err());
    input.hints.clear();
    assert!(PreparedFrame::prepare(theme_with_height(1), &input).is_ok());
}
#[test]
fn theme_foundation_runtime_100_switches_preserve_protected_bytes_and_failure_state() {
    use super::session::{Boundary, ThemeRuntime};
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-theme-runtime-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir(&root).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(root.clone());
    let protected = [
        "stylesheet.json",
        "RetroArch.cfg",
        "allium.db",
        "history",
        "Saves-sentinel",
    ];
    for name in protected {
        std::fs::write(root.join(name), format!("protected {name}")).unwrap();
    }
    let mut runtime = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    assert!(runtime.activate(Boundary::Launcher).snapshot.is_none());
    assert_eq!(runtime.resource_accounting(), (0, 0, 0));
    for i in 0..100 {
        let active = runtime
            .choose(if i % 2 == 0 { "orbit" } else { "compact" })
            .unwrap();
        let handle = active.snapshot.unwrap();
        assert_eq!(handle.generation(), i + 1);
        assert_eq!(active.status.effective.unwrap().digest, handle.digest());
        let prepared = PreparedFrame::prepare(handle.clone(), &frame(ScreenId::Library)).unwrap();
        assert_eq!(prepared.snapshot().digest(), handle.digest());
        assert!(
            runtime.resource_accounting().0 <= 3
                && runtime.resource_accounting().2 <= 4 * 1024 * 1024
        );
    }
    assert_eq!(runtime.resource_accounting().0, 3);
    let record = std::fs::read(root.join("ui-theme.json")).unwrap();
    let before = runtime.status().effective.clone();
    assert!(runtime.choose("missing-custom").is_err());
    assert_eq!(std::fs::read(root.join("ui-theme.json")).unwrap(), record);
    assert_eq!(runtime.status().effective, before);
    std::fs::write(
        root.join(format!(".ui-theme.json.new-{}", std::process::id())),
        b"unexpected partial worker bytes",
    )
    .unwrap();
    assert!(runtime.choose("orbit").is_err());
    assert_eq!(runtime.status().effective, before);
    assert_eq!(
        std::fs::read(root.join(format!(".ui-theme.json.new-{}", std::process::id()))).unwrap(),
        b"unexpected partial worker bytes"
    );
    std::fs::write(root.join("ui-theme.json"), b"torn record").unwrap();
    let a = runtime.activate(Boundary::MenuOpening);
    assert_eq!(a.snapshot.unwrap().pack().id, "orbit");
    assert!(a.diagnostic_once.is_some());
    assert!(
        runtime
            .activate(Boundary::OsdOpening)
            .diagnostic_once
            .is_none()
    );
    assert_eq!(
        std::fs::read(root.join("ui-theme.json")).unwrap(),
        b"torn record"
    );
    for name in protected {
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            format!("protected {name}").as_bytes()
        );
    }
}
#[test]
fn theme_foundation_hold_ack_before_restoration_and_new_capture() {
    use crate::display::RectHold;
    use std::sync::{Mutex, mpsc};
    let pixel = Arc::new(Mutex::new(tiny_skia::Pixmap::new(2, 2).unwrap()));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer_pixel = pixel.clone();
    let hold = RectHold::spawn(move |_stop| {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        writer_pixel.lock().unwrap().fill(tiny_skia::Color::WHITE);
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let restored = pixel.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let joiner = std::thread::spawn(move || {
        hold.stop_and_join().unwrap();
        let mut p = restored.lock().unwrap();
        assert_eq!(p.data(), &[255; 16]);
        p.fill(tiny_skia::Color::TRANSPARENT);
        let next_background = p.clone();
        done_tx.send(next_background.data().to_vec()).unwrap();
    });
    assert!(done_rx.try_recv().is_err());
    release_tx.send(()).unwrap();
    assert_eq!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        vec![0; 16]
    );
    joiner.join().unwrap();
    assert_eq!(pixel.lock().unwrap().data(), &[0; 16]);
    let broken = RectHold::spawn(|_| panic!("fixture writer failure"));
    assert!(broken.stop_and_join().is_err());
}
#[test]
fn theme_foundation_invalid_font_model_names_nav_and_caption_metrics() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-bad-font-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Nunito.ttf"), b"not a font").unwrap();
    let bank = FontBank::bind(&root).unwrap();
    assert!(ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).is_err());
    std::fs::remove_dir_all(&root).unwrap();
    let bank = FontBank::bind(&font_root()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("compact").unwrap(), &bank, 1).unwrap();
    let mut f = frame(ScreenId::Library);
    f.text.insert(Binding::ThemeName, "Orbit light".into());
    assert!(f.validate(&theme).is_err());
    f.text.remove(&Binding::ThemeName);
    f.navigation.get_mut(&NavGroup::Primary).unwrap().push(Row {
        subtitle: String::new(),
        id: "unavailable".into(),
        label: "Unexpected".into(),
        value: "".into(),
        enabled: true,
    });
    assert!(f.validate(&theme).is_err());
    let style = theme.text_style(
        super::pack::Role::Caption,
        crate::display::color::Color::new(30, 40, 45),
    );
    for text in [
        "R4 - Ridge Racer Type 4",
        "Crash Team Racing",
        "日本語の長いゲームタイトル",
    ] {
        let lines = super::render::wrap(&style, text, 132, 2);
        assert!(lines.len() <= 2 && lines.iter().all(|v| style.measure(v).w <= 132));
    }
}

#[test]
fn theme_foundation_named_ancestor_binding_latches_and_preserves_replacement() {
    let parent = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-theme-ancestor-{}", std::process::id()));
    assert!(!parent.exists());
    std::fs::create_dir(&parent).unwrap();
    let named = parent.join("named");
    std::fs::create_dir(&named).unwrap();
    std::fs::create_dir(named.join("state")).unwrap();
    let store = super::selection::SelectionStore::bind(&named.join("state")).unwrap();
    let held = super::runtime::anchored_root(&named.join("state")).unwrap();
    let binding = super::runtime::RootBinding::bind(&named.join("state")).unwrap();
    binding.verify_held(&held).unwrap();
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../static/.allium/fonts")
        .canonicalize()
        .unwrap();
    let mut menu =
        super::session::ThemeRuntime::bind(&named.join("state"), &parent, &fonts).unwrap();
    menu.choose("orbit").unwrap();
    assert!(menu.activate_menu("en").snapshot.is_some());
    assert!(!menu.activate_menu("en").changed);
    let selection = std::fs::read(named.join("state/ui-theme.json")).unwrap();
    std::fs::rename(&named, parent.join("old")).unwrap();
    std::fs::create_dir(&named).unwrap();
    std::fs::rename(parent.join("old/state"), named.join("state")).unwrap();
    let failed = menu.activate_menu("en");
    assert!(failed.status.requested.is_none());
    assert!(failed.status.diagnostic.is_some());
    assert_eq!(failed.snapshot.unwrap().generation(), 0);
    std::fs::write(
        named.join("state/ui-theme.json"),
        b"replacement unknown bytes",
    )
    .unwrap();
    assert!(store.read().is_err());
    assert!(store.choose_legacy().is_err());
    assert!(binding.check().is_err());
    std::fs::rename(named.join("state"), parent.join("old/state")).unwrap();
    std::fs::remove_dir(&named).unwrap();
    std::fs::rename(parent.join("old"), &named).unwrap();
    assert!(
        binding.check().is_err(),
        "failure must remain latched after restoring names"
    );
    assert_eq!(
        std::fs::read(named.join("state/ui-theme.json")).unwrap(),
        b"replacement unknown bytes"
    );
    std::fs::write(named.join("state/ui-theme.json"), selection).unwrap();
    let restored = menu.activate_menu("en");
    assert!(
        restored.status.requested.is_none(),
        "menu failure must remain latched after restoring names and selection"
    );
    assert!(restored.status.diagnostic.is_some());
    std::fs::remove_dir_all(&parent).unwrap();
}

#[test]
fn theme_foundation_third_external_layout_all_screens_and_failed_asset_preserved() {
    use super::session::{Boundary, ThemeRuntime};
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-third-theme-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir_all(root.join("rail/assets")).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(include_bytes!("packs/orbit.json")).unwrap();
    json["id"] = "rail".into();
    json["name"] = "Rail experiment".into();
    json["screens"]["library"]["components"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["binding"] != "next_cover");
    json["assets"]
        .as_object_mut()
        .unwrap()
        .remove("next-disc-mask");
    for c in json["screens"]["library"]["components"]
        .as_array_mut()
        .unwrap()
    {
        match c["kind"].as_str().unwrap() {
            "image" => {
                c["binding"] = "selected_cover".into();
                c["rect"] = serde_json::json!([24, 54, 164, 164]);
                c["fit"] = "contain".into();
                c["fade_edge"] = "none".into();
                c["fade_width"] = 0.into();
            }
            "text" => match c["binding"].as_str().unwrap() {
                "selected_title" => c["rect"] = serde_json::json!([212, 58, 400, 96]),
                "selected_metadata" => c["rect"] = serde_json::json!([212, 156, 400, 18]),
                "selected_blurb" => c["rect"] = serde_json::json!([212, 178, 400, 40]),
                _ => {}
            },
            "entries" => {
                c["layout"] = "list".into();
                c["visible_count"] = 4.into();
                c["item_extent"] = 44.into();
                c["gap"] = 2.into();
                c["art_size"] = 0.into();
                c["caption_height"] = 0.into();
                c["caption_lines"] = 0.into();
                c["focus_style"] = "outline".into();
                c["glow_radius"] = 0.into();
            }
            _ => {}
        }
    }
    std::fs::write(
        root.join("rail/theme.json"),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("rail/assets/accent-band.png"),
        include_bytes!("packs/accent-band.png"),
    )
    .unwrap();
    if let Ok(out) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
        std::fs::write(
            Path::new(&out).join("third-layout-theme.json"),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
    }
    let mut runtime = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    let activation = runtime.choose("rail").unwrap();
    let handle = activation.snapshot.unwrap();
    assert_eq!(handle.pack().screens.len(), 18);
    for screen in ScreenId::ALL {
        let mut input = frame(screen);
        input
            .text
            .insert(Binding::ThemeName, handle.pack().name.clone());
        let draw = PreparedFrame::prepare(handle.clone(), &input).unwrap();
        if let Ok(out) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
            draw.pixels()
                .save_png(Path::new(&out).join(format!(
                    "rail-{}.png",
                    serde_json::to_value(screen).unwrap().as_str().unwrap()
                )))
                .unwrap();
        }
    }
    let mut fresh = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    let reloaded = fresh.activate(Boundary::MenuOpening);
    assert_eq!(reloaded.status.effective.unwrap().id, "rail");
    let before = std::fs::read(root.join("ui-theme.json")).unwrap();
    let identity = runtime.status().effective.clone();
    std::fs::write(
        root.join("rail/assets/accent-band.png"),
        b"invalid hash/image",
    )
    .unwrap();
    assert!(runtime.choose("rail").is_err());
    assert_eq!(std::fs::read(root.join("ui-theme.json")).unwrap(), before);
    assert_eq!(runtime.status().effective, identity);
    let unavailable = runtime.activate(Boundary::OsdOpening);
    assert!(unavailable.diagnostic_once.is_some());
    assert_eq!(
        unavailable.snapshot.unwrap().pack().id,
        "rail",
        "existing validated snapshot retained"
    );
    let mut startup = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    let fallback = startup.activate(Boundary::Launcher);
    assert_eq!(fallback.status.requested.unwrap().id, "rail");
    assert_eq!(fallback.status.effective.unwrap().id, "orbit");
    assert_eq!(std::fs::read(root.join("ui-theme.json")).unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
    let _ = Boundary::Launcher;
}

#[test]
fn theme_foundation_shelf_caption_inner_width_shifted_resized_geometry() {
    use super::pack::Rect;
    let bank = FontBank::bind(&font_root()).unwrap();
    for shifted in [false, true] {
        let mut base = ThemeSnapshot::builtin("orbit").unwrap();
        let resolved = Arc::get_mut(&mut base.0).unwrap();
        let shelf = resolved
            .pack
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .iter_mut()
            .find(|c| c.id == "entries")
            .unwrap();
        shelf.art_size = Some(120);
        let (x, y, width, step) = if shifted {
            shelf.rect = Rect(40, 230, 568, 196);
            shelf.item_extent = Some(156);
            shelf.padding = Some(6);
            shelf.gap = Some(24);
            shelf.caption_height = Some(44);
            (46, 364, 144, 180)
        } else {
            (28, 366, 140, 180)
        };
        resolved.pack.validate().unwrap();
        let theme = ThemeHandle::prepare(base, &bank, 1).unwrap();
        let prepared = PreparedFrame::prepare(theme, &frame(ScreenId::Library)).unwrap();
        let captions: Vec<_> = prepared
            .text_boxes
            .iter()
            .filter(|(id, _)| id == "entries")
            .map(|(_, rect)| *rect)
            .collect();
        assert_eq!(
            captions,
            (0..3)
                .map(|i| Rect(x + i * step, y, width, 36))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            prepared
                .targets
                .iter()
                .filter(|t| t.component == "entries")
                .map(|t| t.item.as_str())
                .collect::<Vec<_>>(),
            vec!["wipeout", "r4", "ctr"]
        );
        if let Ok(out) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
            prepared
                .pixels()
                .save_png(Path::new(&out).join(if shifted {
                    "shelf-shifted-resized.png"
                } else {
                    "shelf-art120-inner132.png"
                }))
                .unwrap();
        }
    }
}

#[test]
fn theme_shelf_wide_item_stays_inside_accepted_component_and_screen() {
    use super::pack::Rect;

    let bank = FontBank::bind(&font_root()).unwrap();
    let mut snapshot = ThemeSnapshot::builtin("orbit").unwrap();
    let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
    let shelf = pack
        .screens
        .get_mut(&ScreenId::Library)
        .unwrap()
        .components
        .iter_mut()
        .find(|component| component.id == "entries")
        .unwrap();
    shelf.item_extent = Some(300);
    shelf.visible_count = Some(1);
    let bounds = shelf.rect;
    assert_eq!(bounds, Rect(24, 238, 592, 196));
    pack.validate().unwrap();

    let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    input.rows.get_mut(&Binding::Entries).unwrap().truncate(1);
    input.focus = Focus::default();
    let rendered = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    input.rows.get_mut(&Binding::Entries).unwrap().clear();
    let baseline = PreparedFrame::prepare(theme, &input).unwrap();

    let mut changed = 0;
    for y in 0..480 {
        for x in 0..640 {
            let offset = ((y * 640 + x) * 4) as usize;
            if rendered.pixels().data()[offset..offset + 4]
                != baseline.pixels().data()[offset..offset + 4]
            {
                assert!(
                    x >= bounds.0
                        && x < bounds.0 + bounds.2
                        && y >= bounds.1
                        && y < bounds.1 + bounds.3,
                    "shelf painted beyond accepted bounds at ({x}, {y})"
                );
                changed += 1;
            }
        }
    }
    assert!(changed > 0);
}

#[test]
fn theme_chooser_external_descriptor_only_and_guarded_legacy_roundtrip() {
    use super::{
        pack::Pack,
        session::{Boundary, ThemeRuntime},
    };
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-theme-chooser-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir_all(root.join("rail/assets")).unwrap();
    let mut pack = Pack::builtin("compact").unwrap();
    pack.id = "rail".into();
    pack.name = "Rail experiment".into();
    std::fs::write(
        root.join("rail/theme.json"),
        serde_json::to_vec(&pack).unwrap(),
    )
    .unwrap();
    let protected = [
        "stylesheet.json",
        "RetroArch.cfg",
        "allium.db",
        "history",
        "Saves-sentinel",
    ];
    for name in protected {
        std::fs::write(root.join(name), format!("protected {name}")).unwrap();
    }
    let mut runtime = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    for _ in 0..2 {
        let choices = runtime.choices().unwrap();
        assert_eq!(
            choices
                .choices
                .iter()
                .map(|c| c.id.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("orbit"), Some("compact"), Some("rail")]
        );
        assert_eq!(
            runtime.resource_accounting(),
            (0, 0, 0),
            "discovery must not decode assets or fonts"
        );
    }
    assert!(runtime.choose("rail").is_err());
    assert!(!root.join("ui-theme.json").exists());
    std::fs::write(
        root.join("rail/assets/accent-band.png"),
        include_bytes!("packs/accent-band.png"),
    )
    .unwrap();
    let selected = runtime.choose("rail").unwrap();
    assert_eq!(selected.status.effective.unwrap().id, "rail");
    assert_eq!(
        runtime
            .activate(Boundary::MenuOpening)
            .status
            .effective
            .unwrap()
            .id,
        "rail"
    );
    let legacy = runtime.choose_legacy().unwrap();
    assert!(legacy.changed && legacy.snapshot.is_none());
    assert!(!root.join("ui-theme.json").exists());
    assert!(!runtime.choose_legacy().unwrap().changed);
    assert!(runtime.activate(Boundary::Launcher).snapshot.is_none());
    let selected = runtime.choose("orbit").unwrap();
    assert_eq!(selected.snapshot.unwrap().generation(), 1);
    let record = std::fs::read(root.join("ui-theme.json")).unwrap();
    let before = runtime.status().effective.clone();
    let partial = root.join(format!(".ui-theme.json.new-{}", std::process::id()));
    std::fs::write(&partial, b"unexpected worker partial").unwrap();
    assert!(runtime.choose_legacy().is_err());
    assert_eq!(runtime.status().effective, before);
    assert_eq!(std::fs::read(root.join("ui-theme.json")).unwrap(), record);
    assert_eq!(
        std::fs::read(&partial).unwrap(),
        b"unexpected worker partial"
    );
    std::fs::remove_file(&partial).unwrap();
    std::fs::write(root.join("ui-theme.json"), b"unknown malformed record").unwrap();
    assert!(runtime.choose_legacy().is_err());
    assert_eq!(runtime.status().effective, before);
    assert_eq!(
        std::fs::read(root.join("ui-theme.json")).unwrap(),
        b"unknown malformed record"
    );
    std::fs::remove_file(root.join("ui-theme.json")).unwrap();
    std::os::unix::fs::symlink(root.join("Saves-sentinel"), root.join("ui-theme.json")).unwrap();
    assert!(runtime.choose_legacy().is_err());
    assert!(
        std::fs::symlink_metadata(root.join("ui-theme.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    for name in protected {
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            format!("protected {name}").as_bytes()
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn theme_chooser_missing_invalid_symlink_and_directory_limits() {
    use super::{pack::Pack, session::ThemeRuntime};
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("allium-theme-chooser-limit-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir(&root).unwrap();
    let missing = ThemeRuntime::bind(&root, &root.join("missing"), &font_root()).unwrap();
    assert_eq!(missing.choices().unwrap().choices.len(), 3);
    let mut pack = Pack::builtin("compact").unwrap();
    std::fs::create_dir(root.join("invalid")).unwrap();
    std::fs::write(root.join("invalid/theme.json"), vec![b' '; 65537]).unwrap();
    symlink(root.join("invalid"), root.join("alias")).unwrap();
    let runtime = ThemeRuntime::bind(&root, &root, &font_root()).unwrap();
    let choices = runtime.choices().unwrap();
    assert_eq!(choices.choices.len(), 3);
    assert_eq!(choices.rejected.len(), 1);
    std::fs::remove_file(root.join("alias")).unwrap();
    std::fs::remove_dir_all(root.join("invalid")).unwrap();
    for i in 0..32 {
        pack.id = format!("custom-{i:02}");
        pack.name = format!("Custom {i}");
        std::fs::create_dir(root.join(&pack.id)).unwrap();
        std::fs::write(
            root.join(&pack.id).join("theme.json"),
            serde_json::to_vec(&pack).unwrap(),
        )
        .unwrap();
    }
    assert_eq!(runtime.choices().unwrap().choices.len(), 35);
    std::fs::create_dir(root.join("extra")).unwrap();
    assert!(runtime.choices().is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn theme_menu_hint_and_actual_widget_window_readonly_capture() {
    use crate::{
        geom::{Alignment, Point, Rect},
        resources::Resources,
        stylesheet::Stylesheet,
        view::{Label, ScrollList, SettingsList, View},
    };
    let mut map = type_map::TypeMap::new();
    let mut styles = Stylesheet::default();
    styles.ui.list_margin = 0;
    map.insert(styles);
    let res = Resources::new(map);
    let labels = (0..10).map(|i| format!("Row {i}")).collect::<Vec<_>>();
    let mut scroll = ScrollList::new(
        res.clone(),
        Rect::new(0, 0, 200, 60),
        labels.clone(),
        Alignment::Left,
        20,
    );
    scroll.select(7);
    assert_eq!((scroll.selected(), scroll.window_start()), (7, 5));
    scroll.select(6);
    assert_eq!((scroll.selected(), scroll.window_start()), (6, 5));
    scroll.select(1);
    assert_eq!((scroll.selected(), scroll.window_start()), (1, 1));
    scroll.select(7);
    scroll.set_visible_count_override(Some(2)).unwrap();
    assert_eq!(
        (
            scroll.selected(),
            scroll.window_start(),
            scroll.visible_count()
        ),
        (7, 6, 2)
    );
    scroll.select(8);
    assert_eq!(scroll.window_start(), 7);
    scroll.select(7);
    assert_eq!(scroll.window_start(), 7);
    scroll.set_visible_count_override(Some(5)).unwrap();
    assert_eq!(
        (
            scroll.selected(),
            scroll.window_start(),
            scroll.visible_count()
        ),
        (7, 5, 5)
    );
    assert!(scroll.set_visible_count_override(Some(0)).is_err());
    assert!(scroll.set_visible_count_override(Some(65)).is_err());
    scroll.set_visible_count_override(None).unwrap();
    assert_eq!(
        (
            scroll.selected(),
            scroll.window_start(),
            scroll.visible_count()
        ),
        (7, 5, 3)
    );

    let right = labels
        .iter()
        .map(|s| {
            Box::new(Label::new(Point::zero(), s.clone(), Alignment::Left, None)) as Box<dyn View>
        })
        .collect();
    let mut settings = SettingsList::new(res, Rect::new(0, 0, 200, 60), labels, right, 20);
    settings.select(7);
    assert_eq!((settings.selected(), settings.window_start()), (7, 5));
    settings.select(6);
    assert_eq!((settings.selected(), settings.window_start()), (6, 5));
    settings.select(1);
    assert_eq!((settings.selected(), settings.window_start()), (1, 1));
    settings.select(7);
    settings.set_visible_count_override(Some(2)).unwrap();
    assert_eq!(
        (
            settings.selected(),
            settings.window_start(),
            settings.visible_count()
        ),
        (7, 6, 2)
    );
    settings.select(8);
    assert_eq!(settings.window_start(), 7);
    settings.select(7);
    assert_eq!(settings.window_start(), 7);
    settings.set_visible_count_override(Some(5)).unwrap();
    assert_eq!(
        (
            settings.selected(),
            settings.window_start(),
            settings.visible_count()
        ),
        (7, 5, 5)
    );
    assert!(settings.set_visible_count_override(Some(0)).is_err());
    assert!(settings.set_visible_count_override(Some(65)).is_err());
    settings.set_visible_count_override(None).unwrap();
    assert_eq!(
        (
            settings.selected(),
            settings.window_start(),
            settings.visible_count()
        ),
        (7, 5, 3)
    );

    let bank = FontBank::bind(&font_root()).unwrap();
    let handle = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut input = frame(ScreenId::IngameMenu);
    input.hints.push(Hint {
        button: "MENU".into(),
        label: "Continue".into(),
    });
    assert!(PreparedFrame::prepare(handle, &input).is_ok());
}

#[test]
fn theme_v2_next_cover_peek_cache_and_canvas_bounds() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    let mut art = tiny_skia::Pixmap::new(112, 160).unwrap();
    for y in 0..160 {
        for x in 0..112 {
            let i = ((y * 112 + x) * 4) as usize;
            art.data_mut()[i..i + 4].copy_from_slice(&[
                if x % 2 == 0 { 255 } else { 0 },
                0,
                0,
                255,
            ]);
        }
    }
    let art = Arc::new(art);
    input.art.entries.insert("ctr".into(), art.clone());
    let baseline = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    input.art.next_cover = Some(art);
    let first = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    #[cfg(debug_assertions)]
    let diagnostic_identity = theme.debug_prepared_preview_identity().unwrap();
    let cached = theme
        .base()
        .0
        .preview_cache
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .pixels
        .clone();
    let second = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    #[cfg(debug_assertions)]
    assert_eq!(
        theme.debug_prepared_preview_identity(),
        Some(diagnostic_identity)
    );
    assert!(Arc::ptr_eq(
        &cached,
        &theme
            .base()
            .0
            .preview_cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .pixels
    ));
    assert_eq!(first.pixels().data(), second.pixels().data());
    let shelf = theme.pack().component(ScreenId::Library, "entries").unwrap();
    let rect = super::pack::Rect(
        shelf.rect.0 + shelf.visible_count.unwrap() * (shelf.item_extent.unwrap() + shelf.gap.unwrap()),
        shelf.rect.1, shelf.item_extent.unwrap(), shelf.art_size.unwrap() + 2 * shelf.padding.unwrap(),
    );
    assert_eq!((rect.0, rect.1, rect.2, rect.3), (564, 238, 148, 148));
    assert!(rect.0 + rect.2 > 640);
    let mut changed = 0;
    for y in 0..480 {
        for x in 0..640 {
            let i = ((y * 640 + x) * 4) as usize;
            if first.pixels().data()[i..i + 4] != baseline.pixels().data()[i..i + 4] {
                changed += 1;
                assert!(x >= rect.0 && x < rect.0 + rect.2 && y >= rect.1 && y < rect.1 + rect.3);
            }
        }
    }
    assert!(changed > 1000);
    assert_eq!((cached.width(), cached.height()), (148, 148));
    for y in 0..148 {
        for x in 0..148 {
            let tile = cached.pixel(x, y).unwrap();
            let original = baseline.pixels().pixel(384 + x, 238 + y).unwrap();
            if tile.alpha() == 255 {
                assert_eq!(tile, original);
            }
        }
    }
    assert_eq!(cached.pixel(0, 0).unwrap().alpha(), 0);
    assert_eq!(cached.pixel(0, 74).unwrap().alpha(), 255);
    let center = first.pixels().pixel(580, 312).unwrap();
    let background = baseline.pixels().pixel(580, 312).unwrap();
    let foreground = cached.pixel(16, 74).unwrap();
    for (actual, under, over) in [
        (center.red(), background.red(), foreground.red()),
        (center.green(), background.green(), foreground.green()),
        (center.blue(), background.blue(), foreground.blue()),
    ] {
        let expected = (f32::from(under) * 0.38 + f32::from(over) * 0.62).round() as u8;
        assert!(actual.abs_diff(expected) <= 1);
    }
    assert_eq!(first.text_boxes, baseline.text_boxes);
    assert_eq!(first.targets.len(), baseline.targets.len());
    assert!(cached.data().len() <= 640 * 480 * 4);
    input.art.next_cover = Some(Arc::new(tiny_skia::Pixmap::new(64, 128).unwrap()));
    PreparedFrame::prepare(theme.clone(), &input).unwrap();
    assert!(!Arc::ptr_eq(
        &cached,
        &theme
            .base()
            .0
            .preview_cache
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .pixels
    ));
    input.art.next_cover = None;
    assert_eq!(
        PreparedFrame::prepare(theme.clone(), &input)
            .unwrap()
            .pixels()
            .data(),
        baseline.pixels().data()
    );
    assert!(theme.base().0.preview_cache.lock().unwrap().is_none());
    #[cfg(debug_assertions)]
    assert!(theme.debug_prepared_preview_identity().is_none());
    if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
        first
            .pixels()
            .save_png(Path::new(&root).join("orbit-library-next-cover-peek.png"))
            .unwrap();
    }
}

#[test]
fn orbit_legacy_next_cover_component_uses_shelf_peek() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let snapshot = ThemeSnapshot::builtin("orbit").unwrap();
    let current = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
    let mut legacy = ThemeSnapshot::builtin("orbit").unwrap();
    let preview = serde_json::from_value(serde_json::json!({
        "id": "next-disc", "kind": "image", "rect": [576, 296, 64, 128],
        "binding": "next_cover", "fit": "cover", "radius": 0,
        "fade_edge": "none", "fade_width": 0, "blur_radius": 4,
        "clip_shape": "left_semicircle"
    })).unwrap();
    Arc::get_mut(&mut legacy.0).unwrap().pack.screens
        .get_mut(&ScreenId::Library).unwrap().components.insert(1, preview);
    let legacy = ThemeHandle::prepare(legacy, &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    let mut cover = tiny_skia::Pixmap::new(140, 140).unwrap();
    cover.fill(tiny_skia::Color::from_rgba8(240, 60, 30, 255));
    input.art.next_cover = Some(Arc::new(cover));
    assert_eq!(
        PreparedFrame::prepare(current, &input).unwrap().pixels().data(),
        PreparedFrame::prepare(legacy, &input).unwrap().pixels().data(),
    );
}

#[test]
fn orbit_game_reference_polish_real_art_proof() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    input.hints = [
        ("A", "Play"), ("START", "Details"), ("X", "Search"),
        ("SELECT", "Context"), ("B", "Back"), ("L/R", "Recents / Apps"), ("Y", "Sort"),
    ].into_iter().map(|(button, label)| Hint { button: button.into(), label: label.into() }).collect();
    let art = std::env::var("ALLIUM_THEME_REAL_ART").expect("Real art directory required for game proof");
    let next_art = tiny_skia::Pixmap::load_png(Path::new(&art).join("ctr-cover.png")).unwrap();
    input.art.next_cover = Some(Arc::new(
        super::runtime::fit_image(&next_art, 140, 140, super::pack::Fit::Cover).unwrap(),
    ));
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    assert_eq!(frame.pixel_bytes(), 640 * 480 * 4);
    let hint_boxes: Vec<_> = frame.text_boxes.iter().filter(|(id, _)| id == "hints").collect();
    assert_eq!(hint_boxes.len(), 14, "all seven game actions must fit in one row");
    assert!(hint_boxes.iter().all(|(_, rect)| rect.1 >= 448 && rect.1 + rect.3 <= 468 && rect.0 + rect.2 <= 616));
    assert_eq!(theme.pack().component(ScreenId::Library, "navigation").unwrap().layout,
        Some(super::pack::Arrangement::Hidden));
    assert!(theme.pack().component(ScreenId::Library, "accent-band").is_none());
    assert!(theme.pack().component(ScreenId::Library, "next-disc").is_none());
    assert!(!theme.pack().assets.contains_key("next-disc-mask"));
    if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
        frame.pixels().save_png(Path::new(&root).join("orbit-game-reference-polish.png")).unwrap();
        input.hints.pop();
        let six = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        assert_eq!(six.text_boxes.iter().filter(|(id, _)| id == "hints").count(), 12);
        six.pixels().save_png(Path::new(&root).join("orbit-game-six-hints.png")).unwrap();
    }
}

#[test]
fn orbit_one_row_uses_supplied_folder_and_custom_pack_hints() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut custom = ThemeSnapshot::builtin("orbit").unwrap();
    Arc::get_mut(&mut custom.0).unwrap().pack.id = "custom-orbit".into();
    let theme = ThemeHandle::prepare(custom, &bank, 1).unwrap();
    let mut input = frame(ScreenId::Library);
    input.text.insert(Binding::SelectedTitle, "Games folder".into());
    input.art = Default::default();
    input.hints = [("A", "Select"), ("B", "Back"), ("L/R", "Recents / Apps")]
        .into_iter().map(|(button, label)| Hint { button: button.into(), label: label.into() }).collect();
    let folder = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let positions: Vec<_> = folder.text_boxes.iter().filter(|(id, _)| id == "hints").map(|(_, rect)| rect.0).collect();
    assert_eq!(positions.len(), 6);
    assert_eq!(positions[0], 24);
    assert!(positions[2] > positions[0] && positions[4] > positions[2]);
    let original = folder.pixels().data().to_vec();
    input.hints.swap(0, 1);
    assert_ne!(PreparedFrame::prepare(theme.clone(), &input).unwrap().pixels().data(), original);
    input.hints.swap(0, 1);
    input.hints[0].label = "Play".into();
    assert_ne!(PreparedFrame::prepare(theme.clone(), &input).unwrap().pixels().data(), original);
    input.hints = [("Dpad", "Inspect"), ("A", "Select")]
        .into_iter().map(|(button, label)| Hint { button: button.into(), label: label.into() }).collect();
    let arbitrary = PreparedFrame::prepare(theme, &input).unwrap();
    assert_eq!(arbitrary.text_boxes.iter().filter(|(id, _)| id == "hints").count(), 4);
    if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
        folder.pixels().save_png(Path::new(&root).join("orbit-folder-select.png")).unwrap();
        arbitrary.pixels().save_png(Path::new(&root).join("custom-pack-arbitrary-hints.png")).unwrap();
    }
}

#[test]
fn theme_v2_extension_rejection_and_v1_compatibility() {
    use super::pack::Pack;
    assert_eq!(Pack::builtin("compact").unwrap().schema_version, 1);
    let original: serde_json::Value =
        serde_json::from_slice(Pack::builtin_bytes("orbit").unwrap()).unwrap();
    for (field, value) in [
        ("blur_radius", serde_json::json!(5)),
        ("mask_asset_id", serde_json::json!("missing")),
        ("binding", serde_json::json!("actions")),
    ] {
        let mut changed = original.clone();
        changed["screens"]["library"]["components"][1][field] = value;
        assert!(Pack::parse(&serde_json::to_vec(&changed).unwrap()).is_err());
    }
    let mut v1 = original.clone();
    v1["schema_version"] = 1.into();
    assert!(Pack::parse(&serde_json::to_vec(&v1).unwrap()).is_err());
    let mut compact: serde_json::Value =
        serde_json::from_slice(Pack::builtin_bytes("compact").unwrap()).unwrap();
    compact["screens"]["library"]["components"][0]["blur_radius"] = 1.into();
    assert!(Pack::parse(&serde_json::to_vec(&compact).unwrap()).is_err());
}

#[test]
fn theme_v2_clean_library_preserves_semantic_metadata_for_v1_external_packs() {
    use super::pack::{Pack, ScreenId};
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut populated = frame(ScreenId::Library);
    populated
        .text
        .insert(Binding::SelectedMetadata, "PlayStation / 1998".into());
    populated.text.insert(
        Binding::SelectedBlurb,
        "Existing catalog synopsis remains available to every theme.".into(),
    );
    let mut empty = populated.clone();
    empty.text.insert(Binding::SelectedMetadata, String::new());
    empty.text.insert(Binding::SelectedBlurb, String::new());
    for id in ["orbit", "compact", "external-metadata"] {
        let mut base =
            ThemeSnapshot::builtin(if id == "orbit" { "orbit" } else { "compact" }).unwrap();
        if id == "external-metadata" {
            let resolved = Arc::get_mut(&mut base.0).unwrap();
            resolved.pack.id = id.into();
            resolved.pack.name = "External metadata".into();
            resolved.pack.validate().unwrap();
        }
        let handle = ThemeHandle::prepare(base, &bank, 1).unwrap();
        let visible = PreparedFrame::prepare(handle.clone(), &populated).unwrap();
        let hidden = PreparedFrame::prepare(handle.clone(), &empty).unwrap();
        if id == "orbit" {
            assert_eq!(visible.pixels().data(), hidden.pixels().data());
            assert!(
                !handle.pack().screens[&ScreenId::Library]
                    .components
                    .iter()
                    .any(|c| matches!(
                        c.binding,
                        Some(Binding::SelectedMetadata | Binding::SelectedBlurb)
                    ))
            );
        } else {
            assert_ne!(visible.pixels().data(), hidden.pixels().data());
            assert!(visible.text_boxes.iter().any(|(id, _)| id == "metadata"));
        }
        if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
            visible
                .pixels()
                .save_png(Path::new(&root).join(format!("{id}-semantic-library.png")))
                .unwrap();
        }
    }
    for binding in [Binding::SelectedMetadata, Binding::SelectedBlurb] {
        let mut compact = Pack::builtin("compact").unwrap();
        compact
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .retain(|c| c.binding != Some(binding));
        assert!(compact.validate().is_err());
        compact.schema_version = 2;
        assert!(compact.validate().is_ok());
    }
    for binding in [
        Binding::SelectedTitle,
        Binding::Entries,
        Binding::Navigation,
        Binding::Status,
        Binding::Hints,
    ] {
        let mut orbit = Pack::builtin("orbit").unwrap();
        orbit
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .retain(|c| c.binding != Some(binding));
        assert!(orbit.validate().is_err());
    }
}


#[test]
fn ux4_seven_menu_rows_fit_builtin_and_external_pack() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let external = std::env::var("ALLIUM_UX4_PACK").unwrap();
    for base in [
        ThemeSnapshot::builtin("orbit").unwrap(),
        ThemeSnapshot::load(Path::new(&external)).unwrap(),
    ] {
        let theme = ThemeHandle::prepare(base, &bank, 1).unwrap();
        for screen in [ScreenId::ContextMenu, ScreenId::IngameMenu] {
            let mut input = frame(screen);
            input.text.insert(Binding::Subtitle, String::new());
            input.rows.insert(Binding::Actions, vec![]);
            input.rows.insert(
                Binding::Entries,
                (0..7)
                    .map(|i| Row {
                        subtitle: String::new(),
                        id: format!("action-{i}"),
                        label: if screen == ScreenId::ContextMenu {
                            [
                                "Set as Favorite",
                                "Launch with PCSX ReARMed",
                                "Reset",
                                "Remove from Recents",
                                "Repopulate Database",
                                "Download",
                                "Manage storage",
                            ][i]
                                .into()
                        } else {
                            [
                                "Continue", "Save", "Load", "Guide", "Settings", "Reset", "Quit",
                            ][i]
                                .into()
                        },
                        value: String::new(),
                        enabled: true,
                    })
                    .collect(),
            );
            input.focus = Focus {
                component: "rows".into(),
                item: Some("action-6".into()),
            };
            input.selected.insert(Binding::Entries, "action-6".into());
            let prepared = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let targets: Vec<_> = prepared
                .targets
                .iter()
                .filter(|t| t.component == "rows")
                .collect();
            assert_eq!(targets.len(), 7, "{screen:?}");
            if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
                std::fs::create_dir_all(&proof).unwrap();
                prepared
                    .pixels()
                    .save_png(
                        Path::new(&proof)
                            .join(format!("{}-{screen:?}-seven-rows.png", theme.pack().id)),
                    )
                    .unwrap();
            }
            assert!(
                targets
                    .iter()
                    .all(|t| if screen == ScreenId::IngameMenu {
                        t.rect.1 >= 16 && t.rect.1 + t.rect.3 <= 424
                    } else { t.rect.1 >= 130 && t.rect.1 + t.rect.3 <= 396 })
            );
            let rows = theme.pack().screens[&screen]
                .components
                .iter()
                .find(|c| c.id == "rows")
                .unwrap();
            let font_size = theme.pack().tokens.typography[&rows.role.unwrap()].size;
            assert!(rows.item_extent.unwrap() >= font_size + rows.padding.unwrap_or(0) * 2);
        }
    }
}

#[test]
#[ignore]
fn ux4_external_font_memory_measurement() {
    fn rss() -> u64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        #[cfg(target_os = "macos")]
        return usage.ru_maxrss as u64;
        #[cfg(not(target_os = "macos"))]
        return usage.ru_maxrss as u64 * 1024;
    }
    let before = rss();
    let started = std::time::Instant::now();
    let bank = FontBank::bind(&font_root()).unwrap();
    let snapshot =
        ThemeSnapshot::load(Path::new(&std::env::var("ALLIUM_UX4_PACK").unwrap())).unwrap();
    let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
    let mut input = frame(ScreenId::IngameMenu);
    input.text.insert(Binding::Subtitle, String::new());
    input.rows.insert(Binding::Actions, vec![]);
    let prepared = PreparedFrame::prepare(theme, &input).unwrap();
    println!(
        "UX4_FONT_MEMORY {{\"rss_before\":{before},\"rss_after\":{},\"font_payload\":{},\"font_heap\":{},\"open_us\":{},\"frame_bytes\":{}}}",
        rss(),
        bank.accounting().1,
        bank.heap_bytes(),
        started.elapsed().as_micros(),
        prepared.pixels().data().len()
    );
    assert!(
        bank.heap_bytes() < 1024 * 1024,
        "owned font payload exceeds menu heap budget"
    );
}

fn ink_pack_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes").canonicalize().unwrap()
}
#[test]
fn theme_port_four_packs_options_parse_validate_and_discover() {
    use super::pack::Pack;
    let root=ink_pack_root();
    let state=std::env::temp_dir().join(format!("allium-ink-choice-{}",std::process::id()));std::fs::create_dir_all(&state).unwrap();let state=state.canonicalize().unwrap();
    let mut runtime=super::ThemeRuntime::bind(&state,&root,&font_root()).unwrap();
    let choices=runtime.choices().unwrap();
    assert!(choices.rejected.is_empty(),"{:?}",choices.rejected);
    assert_eq!(choices.choices.len(),7);
    for base in ["orbit-hero","orbit-row","big-picture","compact-plus"] {
        for suffix in ["","-tall"] {
            let id=format!("{base}{suffix}");let bytes=std::fs::read(root.join(base).join("theme.json")).unwrap();
            let p=Pack::parse(&bytes).unwrap_or_else(|e|panic!("{id}: {e:#}"));
            assert_eq!(p.id,base);assert!(p.ink());assert!(bytes.len()<=super::pack::MANIFEST_LIMIT);
            assert!(choices.choices.iter().any(|c|c.id.as_deref()==Some(base)));
            assert_eq!(p.settings[0].id, "cover_shape");
            ThemeSnapshot::load_with_settings(&root.join(base), &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty(){"square"}else{"tall"}.into())])).unwrap();
            let activation=runtime.choose(&id).unwrap();assert_eq!(activation.snapshot.unwrap().pack().id,base);
        }
    }
    std::fs::remove_dir_all(state).unwrap();
}
#[test]
fn theme_port_all_variants_all_screens_real_raster() {
    let root=ink_pack_root();let bank=FontBank::bind(&font_root()).unwrap();
    let update = std::env::var_os("ALLIUM_UPDATE_FRAME_HASHES").is_some();
    let mut hashes = std::collections::BTreeMap::new();
    for base in ["orbit-hero","orbit-row","big-picture","compact-plus"] {for suffix in ["","-tall"] {
        let id=format!("{base}{suffix}");let ready=ThemeHandle::prepare(ThemeSnapshot::load_with_settings(&root.join(base), &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty(){"square"}else{"tall"}.into())])).unwrap(),&bank,500).unwrap();
        for screen in ScreenId::ALL {
            let mut input=frame(screen);
            input.art = Default::default();
            let title=match screen{ScreenId::Apps=>"Apps",ScreenId::Recents=>"Recents",ScreenId::SettingsRoot=>"Settings",ScreenId::SettingsPage=>"Display",ScreenId::Search|ScreenId::Keyboard=>"Search",ScreenId::SearchResults=>"Search results",ScreenId::DiscPicker=>"Choose a disc",ScreenId::ContextMenu=>"Game actions",ScreenId::IngameMenu=>"In-game menu",ScreenId::NetworkDialog=>"Manage storage",ScreenId::Toast=>"Ready",ScreenId::OsdVolume=>"Volume",ScreenId::OsdBrightness=>"Brightness",ScreenId::GuideSelector=>"Guide Watch",ScreenId::TextReader=>"Real Racing Roots",_=>"Library"};
            input.text.insert(Binding::Title,title.into());
            if screen == ScreenId::IngameMenu {
                input.text.insert(Binding::Title, "Gleylancer".into());
                input.rows.insert(Binding::Actions, vec![]);
                input.rows.insert(Binding::Entries, ["Continue", "Save", "Load", "Guide", "Settings", "Reset", "Quit"]
                    .map(|label| Row { id: label.to_lowercase(), label: label.into(), subtitle: String::new(),
                        value: if matches!(label, "Save" | "Load") { "Slot 0".into() } else { String::new() }, enabled: true }).into());
            }
            input.selected.insert(Binding::Navigation,match screen{ScreenId::Apps|ScreenId::GuideSelector|ScreenId::TextReader=>"apps",ScreenId::SettingsRoot|ScreenId::SettingsPage=>"settings",ScreenId::Recents=>"recents",_=>"library"}.into());
            input.text.insert(Binding::SelectedMetadata,"PlayStation · Released 1998 (USA)".into());
            input.facts=vec![("Developer".into(),"Namco".into()),("Publisher".into(),"Namco".into()),("Genres".into(),"Racing".into()),("First release".into(),"1998".into())];
            if suffix=="-tall" {for image in input.art.entries.values_mut(){*image=Arc::new(super::runtime::fit_image(image,104,156,super::pack::Fit::Cover).unwrap());}}
            if screen==ScreenId::Recents{input.rows.get_mut(&Binding::Entries).unwrap().clear();input.art=Default::default();input.text.remove(&Binding::SelectedTitle);}
            if screen==ScreenId::NetworkDialog{input.rows.get_mut(&Binding::StorageEntries).unwrap().clear();}
            let focus_binding = match screen {
                ScreenId::Library | ScreenId::Apps | ScreenId::SearchResults
                | ScreenId::ContextMenu | ScreenId::IngameMenu => Some(Binding::Entries),
                ScreenId::SettingsRoot | ScreenId::SettingsPage => Some(Binding::SettingsRows),
                ScreenId::DiscPicker => Some(Binding::Discs),
                ScreenId::GuideSelector => Some(Binding::Guides),
                ScreenId::NetworkDialog | ScreenId::GameDetail | ScreenId::Toast => Some(Binding::Actions),
                _ => None,
            };
            if let Some(binding) = focus_binding {
                let component = ready.pack().screens[&screen].components.iter()
                    .find(|c| c.binding == Some(binding)).unwrap();
                let first = input.rows[&binding].first().unwrap();
                input.focus = Focus { component: component.id.clone(), item: Some(first.id.clone()) };
            }
            let rendered=PreparedFrame::prepare(ready.clone(),&input).unwrap_or_else(|e|panic!("{id}/{screen:?}: {e:#}"));
            if focus_binding.is_some() {
                let focused = rendered.targets.iter().find(|t| t.component == input.focus.component
                    && Some(&t.item) == input.focus.item.as_ref()).unwrap();
                let mut unfocused = input.clone();
                unfocused.focus = Focus::default();
                let other = PreparedFrame::prepare(ready.clone(), &unfocused).unwrap();
                let r = focused.rect;
                assert!((r.1..r.1+r.3).any(|y| (r.0..r.0+r.2).any(|x| {
                    let at = (y * 640 + x) as usize;
                    rendered.pixels().pixels()[at] != other.pixels().pixels()[at]
                })), "{id}/{screen:?}: focused row pixels did not change");
            }
            let hash = super::runtime::digest(rendered.pixels().data());
            println!("THEME_FRAME {id} {screen:?} {hash}");
            let expected: std::collections::BTreeMap<String, String> = serde_json::from_str(include_str!("settings-frame-hashes.json")).unwrap();
            if !update { assert_eq!(hash, expected[&format!("{id}/{screen:?}")], "{id}/{screen:?}"); }
            hashes.insert(format!("{id}/{screen:?}"), hash);
            assert!(rendered.pixels().pixels().iter().any(|p|p.alpha()>0));
            for t in &rendered.targets {assert!(t.rect.0+t.rect.2<=640&&t.rect.1+t.rect.3<=480,"{id}/{screen:?} {t:?}");}
            if let Ok(out)=std::env::var("ALLIUM_THEME_PORT_FRAMES") {
                let dir=Path::new(&out).join(&id);std::fs::create_dir_all(&dir).unwrap();
                let name=serde_json::to_value(screen).unwrap().as_str().unwrap().to_owned();
                let mut pixels=if screen.is_dialog()||screen.is_osd(){let mut underlay=frame(ScreenId::Library);underlay.hints.clear();underlay.suppress_hints=true;PreparedFrame::prepare(ready.clone(),&underlay).unwrap().pixels().clone()}else{rendered.pixels().clone()};
                if screen.is_dialog()||screen.is_osd(){pixels.draw_pixmap(0,0,rendered.pixels().as_ref(),&tiny_skia::PixmapPaint::default(),tiny_skia::Transform::identity(),None);}
                pixels.save_png(dir.join(format!("{name}.png"))).unwrap();
            }
        }
    }}
    if update {
        std::fs::write(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/theme/settings-frame-hashes.json"), serde_json::to_string_pretty(&hashes).unwrap() + "\n").unwrap();
    }
}

#[test]
fn theme_port_missing_fields_and_ungated_styles_reject() {
    let bytes=std::fs::read(ink_pack_root().join("orbit-hero/theme.json")).unwrap();
    let original:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
    let mut value=original.clone();value.as_object_mut().unwrap().remove("alignment");
    assert!(super::pack::Pack::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    for field in ["role","padding","gap"] {
        let mut value=original.clone();
        let keyboard=value["screens"]["search"]["components"].as_array_mut().unwrap().iter_mut().find(|c|c["kind"]=="keyboard").unwrap();
        keyboard.as_object_mut().unwrap().remove(field);
        assert!(super::pack::Pack::parse(&serde_json::to_vec(&value).unwrap()).is_err(),"{field}");
    }
    let mut value=original;
    value["screens"]["library"]["omit"]=serde_json::json!(["entries"]);
    assert!(super::pack::Pack::parse(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn theme_port_hostile_typography_rejects_without_panic() {
    let bytes = std::fs::read(ink_pack_root().join("orbit-hero/theme.json")).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (field, bad) in [("line_height", 10000.into()), ("size", 10000.into()),
        ("weight", 10000.into()), ("letter_spacing", 10000.into())] {
        let mut value = original.clone();
        value["ink_typography"]["hint"][field] = bad;
        assert!(super::pack::Pack::parse(&serde_json::to_vec(&value).unwrap()).is_err(), "{field}");
    }

}

#[test]
fn theme_port_rejects_wrong_screen_kind_fields_and_unbounded_lines() {
    use super::pack::{Kind, Pack};
    let original = Pack::parse(&std::fs::read(ink_pack_root().join("orbit-hero/theme.json")).unwrap()).unwrap();
    for kind in [Kind::Image, Kind::Entries, Kind::SettingsRows, Kind::Keyboard,
        Kind::Status, Kind::Navigation, Kind::DialogActions, Kind::Progress] {
        let c = original.screens.values().flat_map(|s| &s.components).find(|c| c.kind == kind).unwrap().clone();
        let screen = if matches!(kind, Kind::Hints | Kind::Status | Kind::Navigation) { ScreenId::OsdVolume } else { ScreenId::TextReader };
        let mut pack = original.clone();
        let mut c = c;
        c.id = "invalid-screen".into();
        pack.screens.get_mut(&screen).unwrap().components.push(c);
        let error = pack.validate().unwrap_err().to_string();
        assert!(error.contains("Component incompatible with screen"), "{kind:?}: {error}");
    }
    for kind in [Kind::Panel, Kind::Text] {
        let mut pack = original.clone();
        let c = pack.screens.get_mut(&ScreenId::TextReader).unwrap().components.iter_mut()
            .find(|c| c.binding == Some(Binding::GuideText)).unwrap();
        c.kind = kind;
        c.max_lines = Some(u32::MAX);
        assert!(pack.validate().is_err());
    }
    let mut pack = original.clone();
    let c = pack.screens.get_mut(&ScreenId::TextReader).unwrap().components.iter_mut()
        .find(|c| c.kind == Kind::Panel).unwrap();
    c.max_lines = Some(1);
    assert!(pack.validate().unwrap_err().to_string().contains("Incompatible ink component fields"));
    let bank = FontBank::bind(&font_root()).unwrap();
    let ready = ThemeHandle::prepare(ThemeSnapshot::load(&ink_pack_root().join("orbit-hero")).unwrap(), &bank, 1).unwrap();
    for screen in [ScreenId::Search, ScreenId::Keyboard] {
        let mut input = frame(screen);
        input.keyboard.pop();
        assert!(PreparedFrame::prepare(ready.clone(), &input).is_err());
    }
}

#[test]
fn theme_port_populated_recents_search_and_keyboard_focus_changes_both_targets() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for suffix in ["", "-tall"] {
            let id = format!("{base}{suffix}");
            let ready = ThemeHandle::prepare(ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty(){"square"}else{"tall"}.into())])).unwrap(), &bank, 1).unwrap();
            for screen in [ScreenId::Recents, ScreenId::Search, ScreenId::Keyboard] {
                let mut input = frame(screen);
                let binding = if screen == ScreenId::Recents { Binding::Entries } else { Binding::Keyboard };
                let c = ready.pack().screens[&screen].components.iter().find(|c| c.binding == Some(binding)).unwrap();
                let mut renders = Vec::new();
                let mut items = Vec::new();
                for index in 0..2 {
                    let item = if screen == ScreenId::Recents {
                        let item = input.rows[&Binding::Entries][index].id.clone();
                        input.focus = Focus { component: c.id.clone(), item: Some(item.clone()) };
                        item
                    } else {
                        input.keyboard_cursor = (index, 0);
                        format!("key-0-{index}")
                    };
                    items.push(item);
                    renders.push(PreparedFrame::prepare(ready.clone(), &input).unwrap());
                }
                for item in items {
                    let rect = renders[0].targets.iter().find(|t| t.component == c.id && t.item == item).unwrap().rect;
                    assert!((rect.1..rect.1 + rect.3).any(|y| (rect.0..rect.0 + rect.2).any(|x| {
                        let at = (y * 640 + x) as usize;
                        renders[0].pixels().pixels()[at] != renders[1].pixels().pixels()[at]
                    })), "{id}/{screen:?}/{item}: cursor did not change target pixels");
                }
            }
        }
    }
}

#[test]
fn review_builtin_game_heading_slots_are_logo_capable() {
    for id in ["orbit","compact"] {
        let pack=crate::theme::pack::Pack::builtin(id).unwrap();
        for (screen, definition) in &pack.screens {
            for c in &definition.components {
                if c.binding==Some(Binding::SelectedTitle) || c.binding==Some(Binding::Title) && matches!(screen,ScreenId::GameDetail|ScreenId::ContextMenu|ScreenId::IngameMenu) {
                    assert!(c.logo,"{id}/{screen:?}/{}",c.id);
                }
            }
        }
    }
}

#[test]
fn media_logos_replace_selected_headings_and_fall_back_per_theme() {
    let bank=FontBank::bind(&font_root()).unwrap();
    let mut logo=tiny_skia::Pixmap::new(96,24).unwrap();logo.fill(tiny_skia::Color::from_rgba8(250,10,80,255));let logo=Arc::new(logo);
    let mut themes=Vec::new();
    for base in ["orbit-hero","orbit-row","big-picture","compact-plus"] {for suffix in ["","-tall"] {let id=format!("{base}{suffix}");themes.push((id.clone(),ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty() { "square" } else { "tall" }.into())])).unwrap()));}}
    for id in ["orbit","compact"] {themes.push((id.into(),ThemeSnapshot::builtin(id).unwrap()));}
    for (id,snapshot) in themes {
        let theme=ThemeHandle::prepare(snapshot,&bank,901).unwrap();
        for screen in ScreenId::ALL {
            let slots:Vec<_>=theme.pack().screens[&screen].components.iter().filter(|c|c.logo||c.binding==Some(Binding::SelectedLogo)).collect();
            if slots.is_empty(){continue;}
            let mut input=frame(screen);input.art=Default::default();input.focus=Focus::default();
            let text=PreparedFrame::prepare(theme.clone(),&input).unwrap();
            input.art.selected_logo=Some(logo.clone());let image=PreparedFrame::prepare(theme.clone(),&input).unwrap();
            for slot in slots {
                let r=slot.rect;
                assert!((r.1..r.1+r.3).any(|y|(r.0..r.0+r.2).any(|x|text.pixels().pixel(x,y)!=image.pixels().pixel(x,y))),"{id}/{screen:?}/{} logo did not replace text",slot.id);
                let title_binding=if slot.binding==Some(Binding::Title){Binding::Title}else{Binding::SelectedTitle};
                let mut changed=input.clone();changed.text.insert(title_binding,"Another game heading".into());
                let same=PreparedFrame::prepare(theme.clone(),&changed).unwrap();
                assert!((r.1..r.1+r.3).all(|y|(r.0..r.0+r.2).all(|x|same.pixels().pixel(x,y)==image.pixels().pixel(x,y))),"{id}/{screen:?}/{} title leaked over logo",slot.id);
            }
            input.art.selected_logo=None;let fallback=PreparedFrame::prepare(theme.clone(),&input).unwrap();assert_eq!(text.pixels().data(),fallback.pixels().data());
        }
    }
}

#[test]
fn media_big_picture_recents_renders_selected_game_art_on_right() {
    let bank=FontBank::bind(&font_root()).unwrap();
    for suffix in ["","-tall"] {
        let theme=ThemeHandle::prepare(ThemeSnapshot::load_with_settings(&ink_pack_root().join("big-picture"), &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty() { "square" } else { "tall" }.into())])).unwrap(),&bank,902).unwrap();
        let mut input=frame(ScreenId::Recents);input.focus=Focus::default();
        let fixture=Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/media-sd/Roms/TEST");
        let entry=crate::media::metadata(&fixture.join("Complete.sfc"));
        input.rows.insert(Binding::Entries,vec![Row{subtitle:String::new(),id:"recent-fixture".into(),label:entry.name.clone(),value:String::new(),enabled:true}]);
        input.selected.insert(Binding::Entries,"recent-fixture".into());
        input.text.insert(Binding::SelectedTitle,entry.name.clone());
        let kind=if suffix.is_empty(){crate::media::Kind::Cover}else{crate::media::Kind::TallCover};
        let cover=tiny_skia::Pixmap::load_png(crate::media::resolve(&fixture.join("Complete.sfc"),kind).unwrap()).unwrap();
        let no_art=PreparedFrame::prepare(theme.clone(),&input).unwrap();input.art.selected_cover=Some(Arc::new(cover));
        let rendered=PreparedFrame::prepare(theme.clone(),&input).unwrap();
        assert_ne!(rendered.pixels().pixel(500,250),no_art.pixels().pixel(500,250));
        input.text.insert(Binding::SelectedTitle,String::new());let no_title=PreparedFrame::prepare(theme.clone(),&input).unwrap();
        assert!((120..168).any(|y|(432..608).any(|x|rendered.pixels().pixel(x,y)!=no_title.pixels().pixel(x,y))));
        input.art.selected_logo=crate::media::load_logo(&fixture.join("Complete.sfc"));
        let logo=PreparedFrame::prepare(theme.clone(),&input).unwrap();assert_eq!(logo.pixels().data(),no_title.pixels().data());
        input.art.selected_cover=None;let fallback=PreparedFrame::prepare(theme,&input).unwrap();assert_ne!(fallback.pixels().pixel(500,144),logo.pixels().pixel(500,144));
    }
}

#[path = "parity_a_tests.rs"]
mod parity_a;
#[path = "logo_cover_tests.rs"]
mod logo_cover;
#[path = "parity_c_tests.rs"]
mod parity_c_tests;
#[path = "parity_tests.rs"]
mod parity_d;

#[test]
fn settings_builtin_values_use_remaining_row_width() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for id in ["orbit", "compact"] {
        let theme = ThemeHandle::prepare(ThemeSnapshot::builtin(id).unwrap(), &bank, 1).unwrap();
        let c = theme.pack().screens[&ScreenId::SettingsPage].components.iter().find(|c| c.binding == Some(Binding::SettingsRows)).unwrap();
        let style = theme.text_style(c.role.unwrap(), crate::display::color::Color::new(255, 255, 255));
        let label = "Date";
        let value = "2026-10-18 13:04:00";
        let mut input = FrameInputs::new(ScreenId::SettingsPage);
        input.rows.insert(Binding::SettingsRows, vec![Row { id: "clock".into(), label: label.into(), value: value.into(), subtitle: String::new(), enabled: true }]);
        let rendered = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let boxes: Vec<_> = rendered.text_boxes.iter().filter(|(id, _)| id == &c.id).map(|(_, r)| *r).collect();
        assert_eq!(boxes.len(), 2);
        assert!(boxes[1].2 >= style.measure(value).w, "{id}: value should fit after short label");
        assert!(boxes[0].0 + boxes[0].2 + 8 <= boxes[1].0);
    }
}
