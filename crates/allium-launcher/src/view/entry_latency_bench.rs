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
        for i in 0..200 {
            for (at, pixel) in image.pixels_mut().iter_mut().enumerate() {
                *pixel = tiny_skia::ColorU8::from_rgba(
                    ((at + i * 13) % 251) as u8,
                    ((at / w as usize + i * 31) % 251) as u8,
                    (i % 251) as u8,
                    255,
                )
                .premultiply();
            }
            let bytes = image.encode_png().unwrap();
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
            let mut frame = None;
            common::perf_trace::start_probe(
                root.join(format!("{base}-{shape}.trace")).to_str().unwrap(),
            );
            for scenario in [
                "forward_with_completion",
                "idle_prepared_forward",
                "held_forward",
                "details_action_focus",
            ].into_iter().filter(|scenario| std::env::var("ALLIUM_BENCH_SCENARIO").map_or(true, |only| only == *scenario)) {
                drop(frame.take());
                list.details = None;
                list.select(0);
                reset_bench_art(&mut list);
                list.theme_models(ScreenId::Library, &handle).unwrap();
                if scenario == "idle_prepared_forward" || scenario == "details_action_focus" {
                    settle_bench_art(&mut list, &handle).await;
                }
                if scenario == "details_action_focus" {
                    list.handle_key_event(
                        KeyEvent::Pressed(Key::Start),
                        tx.clone(),
                        &mut VecDeque::new(),
                    )
                    .await
                    .unwrap();
                    assert!(list.details.is_some());
                }
                let initial = list
                    .theme_models(ScreenId::Library, &handle)
                    .unwrap()
                    .pop()
                    .unwrap();
                frame = Some(PreparedFrame::prepare(handle.clone(), &initial).unwrap());
                drop(initial);
                frame.as_ref().unwrap().paint(&mut display);
                display.flush().unwrap();
                let mut firsts = Vec::new();
                let mut finals = Vec::new();
                let mut settled = Vec::new();
                let mut incomplete = 0;
                let mut previous_viewport = list.list.window_start();
                for i in 0..32 {
                    // This warms the actual current window between presses, never alternates Left/Right.
                    if scenario == "idle_prepared_forward" {
                        settle_bench_art(&mut list, &handle).await;
                    }
                    common::perf::take();
                    let io = common::perf_trace::frontend_io_ops();
                    let before_selection = list.list.selected();
                    let start = Instant::now();
                    let event = if scenario == "held_forward" && i != 0 {
                        KeyEvent::Autorepeat(Key::Right)
                    } else {
                        KeyEvent::Pressed(Key::Right)
                    };
                    {
                        let _phase = common::perf_trace::phase(common::perf_trace::Phase::Input);
                        list.handle_key_event(event, tx.clone(), &mut VecDeque::new())
                            .await
                            .unwrap();
                    }
                    let generation = i + 1;
                    let viewport = list.list.window_start();
                    common::perf_trace::selection(generation, viewport != previous_viewport);
                    let input_us = start.elapsed().as_micros() as u64;
                    common::perf_trace::begin_render();
                    let model = list
                        .theme_models(ScreenId::Library, &handle)
                        .unwrap()
                        .pop()
                        .unwrap();
                    frame = Some(
                        PreparedFrame::prepare_reusing(handle.clone(), &model, frame.take())
                            .unwrap(),
                    );
                    {
                        let _stage = common::perf_trace::stage(common::perf_trace::Stage::Present);
                        frame.as_ref().unwrap().paint_damage(&mut display);
                        display.flush().unwrap();
                    }
                    let feedback = start.elapsed().as_micros() as u64;
                    let content_ready = bench_content_ready(&model, &handle);
                    common::perf_trace::presented(generation, content_ready);
                    firsts.push(feedback);
                    let counts = common::perf::take();
                    let input_io = common::perf_trace::frontend_io_ops() - io;
                    let mut row = common::perf_trace::probe_counts();
                    row["theme"] = base.into();
                    row["shape"] = shape.into();
                    row["scenario"] = scenario.into();
                    row["index"] = i.into();
                    row["selected"] = list.list.selected().into();
                    row["viewport_start"] = viewport.into();
                    row["first_feedback_us"] = feedback.into();
                    row["input_us"] = input_us.into();
                    row["io_ops"] = input_io.into();
                    row["counts"] = serde_json::to_value(counts).unwrap();
                    row["selected_cover_present"] = model.art.selected_cover.is_some().into();
                    assert_eq!(input_io, 0, "storage on input: {row}");
                    assert_eq!(
                        counts.read_attempts + counts.media_io + counts.decodes,
                        0,
                        "storage/decode on UI: {row}"
                    );
                    if scenario == "details_action_focus" {
                        assert_eq!(
                            list.list.selected(),
                            before_selection,
                            "details changed game"
                        );
                        assert_eq!(model.screen, ScreenId::GameDetail);
                        assert_eq!(
                            model.text[&common::theme::pack::Binding::SelectedTitle],
                            "Game 000"
                        );
                    } else {
                        assert_eq!(list.list.selected(), i as usize + 1);
                    }
                    common::perf_trace::drawn_frame();
                    if content_ready {
                        finals.push(feedback);
                        row["final_content_us"] = feedback.into();
                        row["content_complete"] = true.into();
                    } else if scenario != "held_forward" || i == 31 {
                        let wait_start = Instant::now();
                        let (model, complete) = await_bench_content(&mut list, &handle).await;
                        row["completion_wait_us"] = (wait_start.elapsed().as_micros() as u64).into();
                        let final_prepare_start = Instant::now();
                        common::perf_trace::begin_render();
                        frame = Some(
                            PreparedFrame::prepare_reusing(handle.clone(), &model, frame.take())
                                .unwrap(),
                        );
                        {
                            let _stage =
                                common::perf_trace::stage(common::perf_trace::Stage::Present);
                            frame.as_ref().unwrap().paint_damage(&mut display);
                            display.flush().unwrap();
                        }
                        let settled_us = start.elapsed().as_micros() as u64;
                        row["completion_prepare_us"] = (final_prepare_start.elapsed().as_micros() as u64).into();
                        settled.push(settled_us);
                        row["worker_settled_or_content_ready_us"] = settled_us.into();
                        row["content_complete"] = complete.into();
                        if complete {
                            finals.push(settled_us);
                            common::perf_trace::presented(generation, true);
                            row["final_content_us"] = settled_us.into();
                        } else {
                            incomplete += 1;
                            row["final_content_us"] = serde_json::Value::Null;
                            row["completion"] = "workers_settled_with_missing_visible_art".into();
                        }
                        row["input_totals_after_completion"] = common::perf_trace::probe_counts();
                        common::perf_trace::drawn_frame();
                    } else {
                        row["final_content_us"] = serde_json::Value::Null;
                        row["completion"] = "superseded_by_next_repeat".into();
                    }
                    println!("RIGHT_LATENCY {row}");
                    previous_viewport = viewport;
                    std::hint::black_box(display.pixmap().data());
                }
                list.handle_key_event(
                    KeyEvent::Released(Key::Right),
                    tx.clone(),
                    &mut VecDeque::new(),
                )
                .await
                .unwrap();
                assert_eq!(incomplete, 0, "visible art missing: {base} {shape} {scenario}");
                println!(
                    "RIGHT_SUMMARY {}",
                    serde_json::json!({"theme":base,"shape":shape,"scenario":scenario,"first_feedback":bench_latency_summary(&firsts),"final_content":bench_latency_summary(&finals),"completion_samples":finals.len(),"incomplete_after_worker_settlement":incomplete,"settled_or_ready":bench_latency_summary(&settled),"scope":"input to mock completed flush; held intermediates superseded; neighbor art may be prefetched; OS file cache uncontrolled"})
                );
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
    let trace = std::env::temp_dir().join(format!("allium-art-task-{}.trace", std::process::id()));
    common::perf_trace::start_probe(trace.to_str().unwrap());
    common::perf_trace::selection(701, false);
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
    let start = std::time::Instant::now();
    common::perf_trace::selection(702, false);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    list.handle_key_event(KeyEvent::Pressed(Key::Right), tx, &mut VecDeque::new())
        .await
        .unwrap();
    let input = list
        .theme_models(ScreenId::Library, &handle)
        .unwrap()
        .remove(0);
    let frame = PreparedFrame::prepare(handle.clone(), &input).unwrap();
    let mut display = DefaultPlatform::new().unwrap().display().unwrap();
    frame.paint(&mut display);
    display.flush().unwrap();
    let feedback_us = start.elapsed().as_micros() as u64;
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
    // Release on a worker thread after a controlled delay; UI feedback was already prepared.
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(25));
        gate.wait();
    });
    while list.orbit.as_ref().unwrap().art_pending() {
        list.orbit
            .as_mut()
            .unwrap()
            .warm_art_step(&mut VecDeque::new());
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    release.join().unwrap();
    settle_bench_art(&mut list, &handle).await;
    let input = list
        .theme_models(ScreenId::Library, &handle)
        .unwrap()
        .remove(0);
    assert!(input.art.selected_cover.is_some());
    let complete = PreparedFrame::prepare_reusing(handle, &input, Some(frame)).unwrap();
    complete.paint_damage(&mut display);
    display.flush().unwrap();
    let final_us = start.elapsed().as_micros() as u64;
    assert!(final_us >= feedback_us + 25_000);
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&trace).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    for generation in [701, 702] {
        assert!(events.iter().any(|event| event["event"] == "task_completed"
            && event["task"]["name"] == "artwork"
            && event["selection_generation"] == generation), "missing artwork task {generation}");
    }
    std::fs::remove_file(trace).unwrap();
    println!(
        "RIGHT_DELAYED {}",
        serde_json::json!({"delay_ms":25,"first_feedback_us":feedback_us,"final_content_us":final_us,"scope":"input to mock completed flush; gated real worker"})
    );
}

#[cfg(feature = "perf-bench")]
fn bench_latency_summary(samples: &[u64]) -> serde_json::Value {
    if samples.is_empty() {
        return serde_json::json!({"presses":0,"median_us":null,"p95_us":null,"max_us":null});
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    serde_json::json!({"presses":sorted.len(),"median_us":(sorted[(sorted.len()-1)/2] as f64 + sorted[sorted.len()/2] as f64) / 2.0,"p95_us":sorted[(sorted.len()*95).div_ceil(100)-1],"max_us":sorted.last().unwrap()})
}

#[cfg(feature = "perf-bench")]
fn reset_bench_art<S: Sort>(list: &mut EntryList<S>) {
    // A fresh scenario must reset the controller's completed plan as well as its art.
    list.idle_media_key = None;
    list.idle_media_work.clear();
    list.orbit.as_mut().unwrap().release_art();
}

#[cfg(feature = "perf-bench")]
async fn settle_bench_art<S: Sort>(list: &mut EntryList<S>, handle: &common::theme::ThemeHandle) {
    let start = std::time::Instant::now();
    loop {
        common::perf::take();
        let io = common::perf_trace::frontend_io_ops();
        let worked = list.idle_media_step(handle, common::theme::pack::ScreenId::Library);
        let counts = common::perf::take();
        assert_eq!(
            counts.read_attempts + counts.decodes + counts.media_io,
            0,
            "idle storage on UI"
        );
        assert_eq!(common::perf_trace::frontend_io_ops() - io, 0);
        for (counts, stages) in list.orbit.as_mut().unwrap().take_worker_samples() {
            println!(
                "RIGHT_WORKER {}",
                serde_json::json!({"counts":counts,"trace":stages})
            );
        }
        if !worked {
            break;
        }
        assert!(start.elapsed().as_secs() < 30, "artwork did not settle");
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

#[cfg(feature = "perf-bench")]
fn bench_content_ready(
    model: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
) -> bool {
    if std::env::var("ALLIUM_BENCH_READINESS").as_deref() == Ok("rows") {
        bench_rows_content_ready(model, handle)
    } else {
        super::super::app::theme_content_complete(model, handle)
    }
}

#[cfg(feature = "perf-bench")]
async fn await_bench_content<S: Sort>(
    list: &mut EntryList<S>,
    handle: &common::theme::ThemeHandle,
) -> (common::theme::frame::FrameInputs, bool) {
    let start = std::time::Instant::now();
    loop {
        let model = list
            .theme_models(common::theme::pack::ScreenId::Library, handle)
            .unwrap()
            .pop()
            .unwrap();
        if bench_content_ready(&model, handle) {
            return (model, true);
        }
        common::perf::take();
        let io = common::perf_trace::frontend_io_ops();
        let worked = list.idle_media_step(handle, common::theme::pack::ScreenId::Library);
        let counts = common::perf::take();
        assert_eq!(
            counts.read_attempts + counts.decodes + counts.media_io,
            0,
            "idle storage on UI"
        );
        assert_eq!(common::perf_trace::frontend_io_ops() - io, 0);
        if !worked {
            let model = list
                .theme_models(common::theme::pack::ScreenId::Library, handle)
                .unwrap()
                .pop()
                .unwrap();
            let complete = bench_content_ready(&model, handle);
            return (model, complete);
        }
        assert!(
            start.elapsed().as_secs() < 30,
            "selected artwork did not settle"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

#[test]
fn refresh_names_preserves_nested_selection_and_images_without_storage() {
    use crate::entry::lazy_image::LazyImage;
    use crate::view::games::GamesSort;
    common::media::invalidate();
    let root = std::env::temp_dir().join(format!("allium-refresh-in-place-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let paths = [root.join("A.chd"), root.join("B.chd")];
    std::fs::write(root.join("gamelist.xml"),
        "<gameList><game><path>A.chd</path><name>Zulu</name></game><game><path>B.chd</path><name>Alpha</name></game></gameList>").unwrap();
    let res = list().res;
    let mut parent = EntryList::new(
        Rect::new(0, 54, 640, 376),
        res.clone(),
        GamesSort::Alphabetical(Directory::new(root.clone())),
    )
    .unwrap();
    let mut child = EntryList::new(
        Rect::new(0, 54, 640, 376),
        res,
        GamesSort::Alphabetical(Directory::new(root.clone())),
    )
    .unwrap();
    for list in [&mut parent, &mut child] {
        list.entries = paths
            .iter()
            .map(|path| {
                let mut game = Game::new(path.clone());
                game.image = LazyImage::Found("/retained-cover.png".into());
                game.favorite = true;
                Entry::Game(game)
            })
            .collect();
        list.list.set_items(vec!["A".into(), "B".into()], false);
        list.list.select(0);
        list.enable_orbit();
    }
    parent.child = Some(Box::new(child));
    common::media::check_directory(&root);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while common::media::cached_name(&paths[0]).as_deref() != Some("Zulu") {
        common::media::poll_name_indexes();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    crate::entry::load_work::take();
    #[cfg(feature = "perf-bench")]
    let io = common::perf_trace::frontend_io_ops();
    assert!(parent.refresh_names());
    #[cfg(feature = "perf-bench")]
    assert_eq!(common::perf_trace::frontend_io_ops() - io, 0);
    let work = crate::entry::load_work::take();
    assert_eq!(
        work.sql + work.directory_reads + work.metadata + work.file_types,
        0
    );
    for list in [&parent, parent.child.as_ref().unwrap()] {
        assert_eq!(list.list.selected(), 1);
        assert_eq!(list.entries[1].path(), paths[0]);
        assert_eq!(list.entries[1].name(), "Zulu");
        assert!(list.orbit.is_some());
        assert!(list.entries.iter().all(|entry| matches!(entry,
            Entry::Game(game) if game.image == LazyImage::Found("/retained-cover.png".into()))));
    }
    assert!(!parent.refresh_names());
    std::fs::remove_dir_all(root).unwrap();
    common::media::invalidate();
}

#[cfg(feature = "perf-bench")]
fn bench_rows_content_ready(
    model: &common::theme::frame::FrameInputs,
    handle: &common::theme::ThemeHandle,
) -> bool {
    use common::theme::pack::{Arrangement, Binding, Kind};
    handle.pack().screens[&model.screen]
        .components
        .iter()
        .all(|c| {
            if c.layout == Some(Arrangement::Hidden) {
                return true;
            }
            if c.kind == Kind::Image {
                match c.binding {
                    Some(Binding::SelectedCover) => return model.art.selected_cover.is_some(),
                    Some(Binding::SelectedHero) => return model.art.selected_hero.is_some(),
                    Some(Binding::SelectedLogo) => return model.art.selected_logo.is_some(),
                    _ => {}
                }
            }
            if c.logo && model.art.selected_logo.is_none() {
                return false;
            }
            if c.kind == Kind::Entries
                && (c.layout == Some(Arrangement::Shelf) || c.art_size.unwrap_or(0) > 0)
            {
                return model.rows.get(&Binding::Entries).is_none_or(|rows| {
                    rows.iter()
                        .all(|row| model.art.entries.contains_key(&row.id))
                });
            }
            true
        })
}

#[test]
fn name_refresh_retries_cold_gamelist_cover_without_title_change() {
    use crate::entry::lazy_image::LazyImage;
    let mut list = list();
    common::media::invalidate();
    common::media::defer_cold_sources();
    let root = std::env::temp_dir().join(format!("allium-refresh-cover-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("Game.chd");
    let cover = root.join("unrelated-cover.png");
    let mut pixels = tiny_skia::Pixmap::new(16, 16).unwrap();
    pixels.fill(tiny_skia::Color::from_rgba8(35, 120, 210, 255));
    pixels.save_png(&cover).unwrap();
    std::fs::write(root.join("gamelist.xml"),
        "<gameList><game><path>Game.chd</path><name>Game</name><image>unrelated-cover.png</image></game></gameList>").unwrap();
    list.entries = vec![Entry::Game(Game::new(path.clone()))];
    list.list.set_items(vec!["Game".into()], false);
    assert!(list.entries[0].image().is_none());
    assert!(matches!(&list.entries[0], Entry::Game(game) if game.image == LazyImage::NotFound));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while common::media::cached_name(&path).as_deref() != Some("Game") {
        common::media::poll_name_indexes();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(list.refresh_names());
    let mut display = DefaultPlatform::new().unwrap().display().unwrap();
    let res = list.res.clone();
    let styles = res.get::<Stylesheet>();
    assert!(list.draw(&mut display, &styles).unwrap());
    assert!(matches!(&list.entries[0], Entry::Game(game) if game.image == LazyImage::Found(cover.clone())));
    assert_eq!(serde_json::to_value(&list.image).unwrap()["path"], cover.to_str().unwrap());
    drop(styles);
    // A second refresh in the same generation must leave failed lookups cached.
    if let Entry::Game(game) = &mut list.entries[0] {
        game.image = LazyImage::NotFound;
    }
    assert!(!list.refresh_names());
    assert!(matches!(&list.entries[0], Entry::Game(game) if game.image == LazyImage::NotFound));
    std::fs::remove_dir_all(root).unwrap();
    common::media::invalidate();
}

#[test]
fn name_refresh_prepares_new_legacy_shelf_cover_and_reuses_images() {
    use crate::view::games::GamesSort;
    let res = list().res;
    let root = std::env::temp_dir().join(format!("allium-refresh-shelf-{}", std::process::id()));
    std::fs::create_dir_all(root.join("Imgs")).unwrap();
    let mut pixels = tiny_skia::Pixmap::new(101, 101).unwrap();
    pixels.fill(tiny_skia::Color::from_rgba8(35, 120, 210, 255));
    for stem in ["A", "H"] {
        pixels.save_png(root.join("Imgs").join(format!("{stem}.orbit.png"))).unwrap();
    }
    let mut hero = tiny_skia::Pixmap::new(360, 210).unwrap();
    hero.fill(tiny_skia::Color::from_rgba8(35, 120, 210, 255));
    hero.save_png(root.join("Imgs/A.hero.png")).unwrap();
    let mut list = EntryList::new(Rect::new(0, 54, 640, 376), res,
        GamesSort::Alphabetical(Directory::new(root.clone()))).unwrap();
    common::media::invalidate();
    common::media::defer_cold_sources();
    std::fs::write(root.join("gamelist.xml"),
        "<gameList><game><path>H.chd</path><name>AA</name></game></gameList>").unwrap();
    list.entries = ('A'..='H').map(|stem| Entry::Game(Game::new(root.join(format!("{stem}.chd"))))).collect();
    list.list.set_items(list.entries.iter().map(|entry| entry.name().into()).collect(), false);
    list.enable_orbit();
    let before = list.orbit.as_ref().unwrap().theme_art(&list.entries, 0, 0, 8);
    let retained = before.selected_hero.unwrap();
    assert_eq!(list.orbit.as_ref().unwrap().cache_entries(), 5);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while common::media::cached_name(&root.join("H.chd")).as_deref() != Some("AA") {
        common::media::poll_name_indexes();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(list.refresh_names());
    assert_eq!(list.list.selected(), 0);
    assert_eq!(list.entries[1].path(), root.join("H.chd"));
    let mut display = DefaultPlatform::new().unwrap().display().unwrap();
    let res = list.res.clone();
    let styles = res.get::<Stylesheet>();
    assert!(list.draw(&mut display, &styles).unwrap());
    assert_eq!(display.pixmap().pixel(230, 300).unwrap(),
        tiny_skia::ColorU8::from_rgba(35, 120, 210, 255).premultiply());
    let after = list.orbit.as_ref().unwrap().theme_art(&list.entries, 0, 0, 8);
    assert!(std::sync::Arc::ptr_eq(&retained, after.selected_hero.as_ref().unwrap()));
    std::fs::remove_dir_all(root).unwrap();
    common::media::invalidate();
}
