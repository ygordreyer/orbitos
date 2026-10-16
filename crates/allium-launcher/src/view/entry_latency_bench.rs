#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "perf-bench")]
#[ignore = "explicit disposable 200-game Right latency fixture"]
async fn right_latency_200_games() {
    use common::theme::{
        ThemeHandle, fonts::FontBank, pack::ScreenId, render::PreparedFrame, runtime::ThemeSnapshot,
    };
    use std::{sync::Arc, time::Instant};
    let root = std::env::temp_dir().join(format!("allium-right-latency-{}", std::process::id()));
    assert!(!root.exists());
    std::fs::create_dir_all(&root).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(root.clone());
    let dir = root.join("Roms/TEST");
    let mut xml = String::from("<gameList>");
    for (kind, w, h) in [
        ("covers", 480, 480),
        ("covers-tall", 320, 480),
        ("logos", 480, 200),
        ("backgrounds", 640, 480),
    ] {
        std::fs::create_dir_all(dir.join("media").join(kind)).unwrap();
        let mut image = tiny_skia::Pixmap::new(w, h).unwrap();
        for (i, pixel) in image.pixels_mut().iter_mut().enumerate() {
            *pixel = tiny_skia::ColorU8::from_rgba(
                (i % 251) as u8,
                ((i / w as usize) % 251) as u8,
                89,
                255,
            )
            .premultiply();
        }
        let bytes = image.encode_png().unwrap();
        for i in 0..200 {
            std::fs::write(
                dir.join("media")
                    .join(kind)
                    .join(format!("Game {i:03}.png")),
                &bytes,
            )
            .unwrap();
        }
    }
    for i in 0..200 {
        std::fs::write(dir.join(format!("Game {i:03}.sfc")), []).unwrap();
        xml.push_str(&format!("<game><path>./Game {i:03}.sfc</path><name>Game {i:03}</name><desc>A long library description, with facts and words for fitting. {}</desc><releasedate>19980101T000000</releasedate><developer>Studio</developer></game>", "Explore the world and find the next challenge. ".repeat(10)));
    }
    xml.push_str("</gameList>");
    std::fs::write(dir.join("gamelist.xml"), xml).unwrap();
    common::media::invalidate();
    common::media::check_directory(&dir);
    for i in 0..200 {
        for kind in [
            common::media::Kind::Cover,
            common::media::Kind::TallCover,
            common::media::Kind::Logo,
            common::media::Kind::Background,
            common::media::Kind::PreparedCover,
        ] {
            common::media::resolve(&dir.join(format!("Game {i:03}.sfc")), kind);
        }
    }
    let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(100);
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            let handle = ThemeHandle::prepare(
                ThemeSnapshot::load_with_settings(
                    &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../../themes")
                        .join(base)
                        .canonicalize()
                        .unwrap(),
                    &std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]),
                )
                .unwrap(),
                &bank,
                1001,
            )
            .unwrap();
            let mut list = list();
            list.res.insert(Some(handle.clone()));
            list.entries = (0..200)
                .map(|i| Entry::Game(Game::new(dir.join(format!("Game {i:03}.sfc")))))
                .collect();
            list.list.set_items(
                list.entries.iter().map(|e| e.name().into()).collect(),
                false,
            );
            list.enable_orbit();
            let mut display = DefaultPlatform::new().unwrap().display().unwrap();
            let model = list
                .theme_models(ScreenId::Library, &handle)
                .unwrap()
                .remove(0);
            let mut frame = Some(PreparedFrame::prepare(handle.clone(), &model).unwrap());
            common::perf_trace::start_probe(
                root.join(format!("{base}-{shape}.trace")).to_str().unwrap(),
            );
            for scenario in ["cold_right", "warm_right", "rapid_right"] {
                if scenario == "rapid_right" {
                    list.select(50);
                    list.orbit.as_mut().unwrap().release_art();
                }
                if scenario == "warm_right" {
                    let start = Instant::now();
                    let mut ticks = 0;
                    let mut max_tick_us = 0;
                    loop {
                        common::perf::take();
                        let io = common::perf_trace::frontend_io_ops();
                        let tick = Instant::now();
                        let worked = list.idle_media_step(&handle, ScreenId::Library);
                        max_tick_us = max_tick_us.max(tick.elapsed().as_micros());
                        let counts = common::perf::take();
                        assert_eq!(
                            common::perf_trace::frontend_io_ops() - io,
                            0,
                            "idle storage on UI"
                        );
                        assert_eq!(
                            counts.read_attempts + counts.decodes + counts.media_io,
                            0,
                            "idle decode on UI: {counts:?}"
                        );
                        for (counts, stages) in list.orbit.as_mut().unwrap().take_worker_samples() {
                            println!(
                                "RIGHT_WORKER {}",
                                serde_json::json!({"theme":base,"shape":shape,"counts":counts,"trace":stages})
                            );
                            assert_eq!(counts.art_resizes, 0, "pre-sized Ink sources were resized");
                            assert_eq!(
                                counts.disk_hits, 0,
                                "pre-sized Ink sources used transformed disk cache"
                            );
                        }
                        if !worked {
                            break;
                        }
                        ticks += 1;
                        assert!(
                            start.elapsed().as_secs() < 30,
                            "artwork warm did not complete"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    println!(
                        "RIGHT_IDLE {}",
                        serde_json::json!({"theme":base,"shape":shape,"wall_us":start.elapsed().as_micros(),"max_ui_tick_us":max_tick_us,"ticks":ticks})
                    );
                }
                for i in 0..if scenario == "cold_right" {
                    1
                } else if scenario == "rapid_right" {
                    10
                } else {
                    20
                } {
                    common::perf::take();
                    let io = common::perf_trace::frontend_io_ops();
                    let start = Instant::now();
                    let key = if scenario == "warm_right" && i % 2 == 1 {
                        Key::Left
                    } else {
                        Key::Right
                    };
                    {
                        let _phase = common::perf_trace::phase(common::perf_trace::Phase::Input);
                        list.handle_key_event(
                            KeyEvent::Pressed(key),
                            tx.clone(),
                            &mut VecDeque::new(),
                        )
                        .await
                        .unwrap();
                    }
                    let input_us = start.elapsed().as_micros();
                    common::perf_trace::begin_render();
                    let model = list
                        .theme_models(ScreenId::Library, &handle)
                        .unwrap()
                        .remove(0);
                    frame = Some(
                        PreparedFrame::prepare_reusing(handle.clone(), &model, frame.take())
                            .unwrap(),
                    );
                    {
                        let _stage = common::perf_trace::stage(common::perf_trace::Stage::Present);
                        frame.as_ref().unwrap().paint(&mut display);
                        display.flush().unwrap();
                    }
                    let counts = common::perf::take();
                    let mut row = common::perf_trace::probe_counts();
                    row["theme"] = base.into();
                    row["shape"] = shape.into();
                    row["scenario"] = scenario.into();
                    row["index"] = i.into();
                    row["wall_us"] = (start.elapsed().as_micros() as u64).into();
                    row["input_us"] = (input_us as u64).into();
                    row["io_ops"] = (common::perf_trace::frontend_io_ops() - io).into();
                    row["counts"] = serde_json::to_value(counts).unwrap();
                    row["selected_cover_present"] = model.art.selected_cover.is_some().into();
                    println!("RIGHT_LATENCY {row}");
                    if scenario == "warm_right" {
                        assert!(
                            model.art.selected_cover.is_some(),
                            "warm selection lost its cover: {row}"
                        );
                    }
                    assert_eq!(row["io_ops"], 0, "storage on Right: {row}");
                    assert_eq!(counts.decodes, 0, "synchronous cold decode: {row}");
                    assert_eq!(
                        counts.read_attempts + counts.media_io,
                        0,
                        "storage on input thread: {row}"
                    );
                    std::hint::black_box(display.pixmap().data());
                }
            }
            drop(frame);
            drop(list);
            std::hint::black_box(Arc::strong_count(&bank));
        }
    }
    common::media::invalidate();
}

#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "perf-bench")]
async fn right_during_slow_artwork_worker_remains_storage_free() {
    use common::theme::{
        ThemeHandle, fonts::FontBank, pack::ScreenId, render::PreparedFrame, runtime::ThemeSnapshot,
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../common/tests/fixtures/media-sd/Roms/TEST");
    common::media::check_directory(&root);
    let bank = FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let handle = ThemeHandle::prepare(
        ThemeSnapshot::load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../themes/big-picture")
                .canonicalize()
                .unwrap(),
        )
        .unwrap(),
        &bank,
        1201,
    )
    .unwrap();
    let mut list = list();
    list.res.insert(Some(handle.clone()));
    list.entries = ["Complete.sfc", "Legacy.sfc"]
        .into_iter()
        .map(|p| Entry::Game(Game::new(root.join(p))))
        .collect();
    list.list
        .set_items(vec!["Complete".into(), "Legacy".into()], false);
    list.enable_orbit();
    list.theme_models(ScreenId::Library, &handle).unwrap();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    super::super::orbit::proof_art_gate(Some(gate.clone()));
    list.idle_media_step(&handle, ScreenId::Library);
    super::super::orbit::proof_art_gate(None);
    assert!(list.orbit.as_ref().unwrap().art_pending());
    common::perf::take();
    let io = common::perf_trace::frontend_io_ops();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    list.handle_key_event(KeyEvent::Pressed(Key::Right), tx, &mut VecDeque::new())
        .await
        .unwrap();
    let input = list
        .theme_models(ScreenId::Library, &handle)
        .unwrap()
        .remove(0);
    let frame = PreparedFrame::prepare(handle, &input).unwrap();
    gate.wait();
    let counts = common::perf::take();
    assert_eq!(list.list.selected(), 1);
    assert!(
        frame
            .targets
            .iter()
            .any(|target| Some(&target.item) == input.focus.item.as_ref())
    );
    assert!(
        input.art.selected_cover.is_none(),
        "pending worker must present a placeholder"
    );
    assert_eq!(common::perf_trace::frontend_io_ops() - io, 0);
    assert_eq!(counts.read_attempts + counts.decodes + counts.media_io, 0);
    while list.orbit.as_ref().unwrap().art_pending() {
        list.orbit
            .as_mut()
            .unwrap()
            .warm_art_step(&mut VecDeque::new());
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}
