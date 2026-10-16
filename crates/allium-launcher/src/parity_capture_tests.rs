#[tokio::test]
#[ignore = "explicit parity capture with host fixture"]
#[serial_test::serial(orbit_fixture)]
async fn parity_d_capture_actual_adapters() {
    let out =
        std::path::PathBuf::from(std::env::var("PARITY_OUTPUT_ROOT").unwrap()).join("integration");
    prepare_art();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        let target = common::constants::ALLIUM_SD_ROOT.join("Themes").join(base);
        std::fs::create_dir_all(target.join("assets")).unwrap();
        std::fs::copy(
            repo.join("themes").join(base).join("theme.json"),
            target.join("theme.json"),
        )
        .unwrap();
        for asset in std::fs::read_dir(repo.join("themes").join(base).join("assets")).unwrap() {
            let asset = asset.unwrap();
            std::fs::copy(asset.path(), target.join("assets").join(asset.file_name())).unwrap();
        }
    }
    let source =
        std::path::PathBuf::from(std::env::var("PARITY_WEB_SOURCE").unwrap()).join("dist/assets");
    for (stem, asset) in [
        ("R4 - Ridge Racer Type 4 (USA)", "r4"),
        ("Silent Hill (USA)", "silent-hill"),
    ] {
        for (folder, suffix) in [("covers-tall", "tall.jpg"), ("logos", "logo.png")] {
            let image = source.join(format!("{asset}-{suffix}"));
            if image.exists() {
                let dir = ALLIUM_GAMES_DIR.join("PS/media").join(folder);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::copy(
                    image,
                    dir.join(format!(
                        "{stem}.{}",
                        if folder == "logos" { "png" } else { "jpg" }
                    )),
                )
                .unwrap();
            }
        }
    }
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            for accent in if base == "compact-plus" {
                vec!["orange", "big-picture", "white", "black"]
            } else {
                vec!["default"]
            } {
                let id = format!("{base}-{shape}-{accent}");
                let dir = out.join(&id);
                std::fs::create_dir_all(&dir).unwrap();
                if ALLIUM_LAUNCHER_STATE.exists() {
                    std::fs::remove_file(ALLIUM_LAUNCHER_STATE.as_path()).unwrap();
                }
                let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
                crate::theme_adapter::choose(&app.res, Some(base)).unwrap();
                crate::theme_adapter::choose_setting(&app.res, "cover_shape", shape).unwrap();
                if base == "compact-plus" {
                    crate::theme_adapter::choose_setting(&app.res, "color_scheme", accent).unwrap();
                }
                app.view.proof_games_directory(ALLIUM_GAMES_DIR.join("PS"));
                app.view.proof_select_tab(1);
                let (tx, _rx) = tokio::sync::mpsc::channel(100);
                fn save(app: &mut AlliumLauncher<DefaultPlatform>, path: &std::path::Path) {
                    app.view.set_should_draw();
                    render_theme(app);
                    for _ in 0..256 {
                        if !app.view.idle_media_step() {
                            break;
                        }
                    }
                    app.view.set_should_draw();
                    render_theme(app);
                    app.display.pixmap().to_owned().save_png(path).unwrap();
                }
                save(&mut app, &dir.join("library.png"));
                let handle = crate::theme_adapter::snapshot(&app.res).unwrap();
                if let Some(action) = handle.pack().screens[&common::theme::pack::ScreenId::Library]
                    .components
                    .iter()
                    .find(|c| {
                        c.paint
                            .as_ref()
                            .is_some_and(|p| p.action.as_deref() == Some("play"))
                    })
                {
                    press(&mut app, Key::Up, &tx).await;
                    assert_eq!(
                        app.view.proof_game_theme_inputs()[0].focus.component,
                        action.id
                    );
                    save(&mut app, &dir.join("library-image-focus.png"));
                    press(&mut app, Key::Down, &tx).await;
                }
                press(&mut app, Key::Start, &tx).await;
                save(&mut app, &dir.join("game_detail.png"));
                let inputs = app.view.proof_game_theme_inputs();
                let detail = inputs.last().unwrap();
                assert_eq!(detail.screen, common::theme::pack::ScreenId::GameDetail);
                assert!(!detail.detail_lines.is_empty());
                assert!(detail.pagination.is_some());
                if let Some(action) = handle.pack().screens
                    [&common::theme::pack::ScreenId::GameDetail]
                    .components
                    .iter()
                    .find(|c| {
                        c.paint
                            .as_ref()
                            .is_some_and(|p| p.action.as_deref() == Some("play"))
                    })
                {
                    press(&mut app, Key::Left, &tx).await;
                    assert_eq!(
                        app.view
                            .proof_game_theme_inputs()
                            .last()
                            .unwrap()
                            .focus
                            .component,
                        action.id
                    );
                    save(&mut app, &dir.join("game_detail-image-focus.png"));
                    press(&mut app, Key::Right, &tx).await;
                }
                press(&mut app, Key::Down, &tx).await;
                save(&mut app, &dir.join("game_detail-next.png"));
                press(&mut app, Key::B, &tx).await;
                press(&mut app, Key::Select, &tx).await;
                save(&mut app, &dir.join("context_menu.png"));
                press(&mut app, Key::B, &tx).await;
                app.view.search("R4".into()).unwrap();
                save(&mut app, &dir.join("search_results.png"));
                app.view.close_search_results();
                for (tab, name) in [(0, "recents"), (2, "apps"), (3, "settings_root")] {
                    app.view.proof_select_tab(tab);
                    save(&mut app, &dir.join(format!("{name}.png")));
                }
                println!("PARITY_ADAPTER {id}");
            }
        }
    }
}
