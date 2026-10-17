use super::*;
use std::time::{Duration, Instant};

struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
        common::media::invalidate();
    }
}

fn missing_art_launcher() -> (Fixture, AlliumLauncher<DefaultPlatform>) {
    let dir = ALLIUM_GAMES_DIR.join("trace-completed-miss-fixture");
    assert!(!dir.exists());
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["Alpha", "Beta", "Gamma", "Delta"] {
        std::fs::write(dir.join(format!("{name}.chd")), []).unwrap();
    }
    common::media::invalidate();
    let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let theme = common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&repo.join("themes/orbit-hero")).unwrap(),
        &bank,
        1,
    )
    .unwrap();
    app.res.insert(Some(theme));
    app.view.proof_games_directory(dir.clone());
    app.display.record_presentations();
    (Fixture(dir), app)
}

async fn trace_first_frame(app: &mut AlliumLauncher<DefaultPlatform>, path: &std::path::Path) {
    common::perf_trace::start_probe(path.to_str().unwrap());
    {
        let _input = common::perf_trace::phase(common::perf_trace::Phase::Input);
    }
    common::perf_trace::selection(41, false);
    app.present_frame(false, &mut false).await.unwrap();
    assert_eq!(app.display.take_presented_frames().len(), 1);
    assert!(!app.view.content_complete());
    assert_eq!(
        common::perf_trace::probe_counts()["measurement"]["final_content_monotonic_us"],
        0
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn completed_cover_and_logo_misses_record_final_content_without_presenting_pixels() {
    let (dir, mut app) = missing_art_launcher();
    let path = dir.0.join("completed.trace");
    trace_first_frame(&mut app, &path).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.view.idle_media_step() {
        app.present_frame(false, &mut false).await.unwrap();
        assert!(Instant::now() < deadline, "artwork worker timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
    app.present_frame(false, &mut false).await.unwrap();
    let inputs = app.view.proof_game_theme_inputs();
    assert!(inputs[0].art.selected_cover.is_none());
    assert!(inputs[0].art.selected_logo.is_none());
    assert!(
        app.display.take_presented_frames().is_empty(),
        "completed misses changed no pixels"
    );
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "first_feedback")
            .count(),
        1
    );
    let finals: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "final_content")
        .collect();
    assert_eq!(
        finals.len(),
        1,
        "completed cover and logo misses must settle final_content: {}",
        common::perf_trace::probe_counts()["measurement"]
    );
    assert_eq!(finals[0]["selection_generation"], 41);
    assert!(
        finals[0]["measurement"]["final_content_monotonic_us"]
            .as_u64()
            .unwrap()
            >= finals[0]["measurement"]["first_feedback_monotonic_us"]
                .as_u64()
                .unwrap()
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn pending_cover_and_logo_do_not_record_final_content() {
    let (dir, mut app) = missing_art_launcher();
    let path = dir.0.join("pending.trace");
    trace_first_frame(&mut app, &path).await;
    for _ in 0..3 {
        app.present_frame(false, &mut false).await.unwrap();
    }
    let trace = std::fs::read_to_string(&path).unwrap();
    assert!(trace.contains("first_feedback"));
    assert!(!trace.contains("\"event\":\"final_content\""));
    assert!(!app.view.content_complete());
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn hero_over_soft_budget_completes_without_repeated_visible_artwork() {
    let (dir, mut app) = missing_art_launcher();
    for (kind, w, h) in [("covers", 480, 480), ("backgrounds", 640, 480), ("logos", 480, 200)] {
        std::fs::create_dir_all(dir.0.join("media").join(kind)).unwrap();
        for (i, name) in ["Alpha", "Beta", "Delta", "Gamma"].into_iter().enumerate() {
            let mut pixels = tiny_skia::Pixmap::new(w, h).unwrap();
            pixels.fill(tiny_skia::Color::from_rgba8(40 + i as u8 * 30, 90, 120, 255));
            pixels.save_png(dir.0.join(format!("media/{kind}/{name}.png"))).unwrap();
        }
    }
    common::media::invalidate();
    let deadline = Instant::now() + Duration::from_secs(10);
    app.present_frame(false, &mut false).await.unwrap();
    while app.view.idle_media_step() {
        app.present_frame(false, &mut false).await.unwrap();
        assert!(Instant::now() < deadline, "initial artwork timeout");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    app.present_frame(false, &mut false).await.unwrap();
    assert!(app.view.content_complete());
    let previous = app.view.proof_game_theme_inputs();
    let art = &previous[0].art;
    assert!(art.next_cover_expected);
    let mut sources: Vec<_> = art.entries.values().cloned().collect();
    sources.extend([art.next_cover.clone().unwrap(), art.selected_logo.clone().unwrap(),
        art.selected_hero.clone().unwrap(), art.selected_hero.clone().unwrap()]);
    // An inactive list with its speculative hero consumes the shared allowance.
    let inactive = crate::view::orbit::Orbit::proof_source_cache(&sources);
    assert_eq!(inactive.cache_entries(), 7);
    let path = dir.0.join("hero-pressure.trace");
    common::perf_trace::start_probe(path.to_str().unwrap());
    { let _input = common::perf_trace::phase(common::perf_trace::Phase::Input); }
    common::perf_trace::selection(42, false);
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    app.view.handle_key_event(KeyEvent::Pressed(Key::Right), tx, &mut VecDeque::new()).await.unwrap();
    app.present_frame(false, &mut false).await.unwrap();
    crate::view::orbit::proof_art_completion();
    let mut completions = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pending = app.view.idle_media_step();
        if let Some(task) = crate::view::orbit::proof_art_completion() {
            assert!(completions.insert(format!("{task:?}")), "repeated artwork: {task:?}");
        }
        app.present_frame(false, &mut false).await.unwrap();
        assert!(Instant::now() < deadline, "Hero selection did not settle");
        if !pending { break; }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let inputs = app.view.proof_game_theme_inputs();
    assert!(inputs[0].art.selected_hero.is_some());
    assert!(inputs[0].art.selected_logo.is_some());
    assert_eq!(inputs[0].art.entries.len(), 3);
    assert!(app.view.content_complete(), "{}", common::perf_trace::probe_counts()["measurement"]);
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows.iter().filter(|r| r["event"] == "final_content" && r["selection_generation"] == 42).count(), 1);
    for _ in 0..8 {
        assert!(!app.view.idle_media_step());
        assert!(crate::view::orbit::proof_art_completion().is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn compact_plus_three_warm_games_present_once_per_down_press() {
    let dir = ALLIUM_GAMES_DIR.join("compact-plus-settled-fixture");
    assert!(!dir.exists());
    std::fs::create_dir_all(dir.join("Imgs")).unwrap();
    let _fixture = Fixture(dir.clone());
    let database = Database::in_memory().unwrap();
    for index in 0..3 {
        let stem = format!("Game {index:02}");
        let path = dir.join(format!("{stem}.chd"));
        std::fs::write(&path, []).unwrap();
        let mut cover = tiny_skia::Pixmap::new(480, 480).unwrap();
        cover.fill(tiny_skia::Color::from_rgba8(index * 70, 90, 120, 255));
        cover.save_png(dir.join(format!("Imgs/{stem}.png"))).unwrap();
        let mut hero = tiny_skia::Pixmap::new(360, 210).unwrap();
        hero.fill(tiny_skia::Color::from_rgba8(index * 70, 120, 90, 255));
        hero.save_png(dir.join(format!("Imgs/{stem}.hero.png"))).unwrap();
        let game: common::database::NewGame = Game::new(path).into();
        database.update_games(&[game.clone()]).unwrap();
        database.increment_play_count(&game).unwrap();
    }
    common::media::invalidate();
    let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
    app.res.insert(database);
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let theme = common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&repo.join("themes/compact-plus")).unwrap(),
        &bank, 1,
    ).unwrap();
    app.res.insert(Some(theme));
    app.view = App::load_or_new(app.display.bounding_box(), app.res.clone(), app.platform.battery().unwrap()).unwrap();
    app.view.proof_games_directory(dir.clone());
    app.display.record_presentations();
    let (tx, _rx) = tokio::sync::mpsc::channel(100);
    for tab in [0, 1] {
        app.view.proof_select_tab(tab);
        app.present_frame(false, &mut false).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.view.idle_media_step() {
            app.present_frame(false, &mut false).await.unwrap();
            assert!(Instant::now() < deadline, "tab {tab} artwork did not settle");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        app.present_frame(false, &mut false).await.unwrap();
        assert!(app.view.content_complete());
        app.display.take_presented_frames();
        for press in 0..8 {
            let mut work = None;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !app.view.prewarm_navigation_step(&mut work, 1).unwrap() {
                assert!(Instant::now() < deadline, "tab {tab} navigation warm did not settle");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            app.view.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.present_frame(false, &mut false).await.unwrap();
            assert_eq!(app.display.take_presented_frames().len(), 1, "tab {tab} press {press}");
            assert!(!app.view.should_draw());
            assert!(!app.view.idle_media_step(), "tab {tab} press {press} restarted art work");
            assert!(!app.view.should_draw(), "tab {tab} press {press} requested another draw");
            app.present_frame(false, &mut false).await.unwrap();
            assert!(app.display.take_presented_frames().is_empty());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn right_release_preserves_pending_press_measurement() {
    let (dir, mut app) = missing_art_launcher();
    let path = dir.0.join("release.trace");
    trace_first_frame(&mut app, &path).await;
    let before = common::perf_trace::probe_counts()["measurement"].clone();
    assert!(before["first_feedback_monotonic_us"].as_u64().unwrap() > 0);
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let mut keys = EnumMap::default();
    keys[Key::Right] = true;
    for key in [Key::Right, Key::Left, Key::Up, Key::Down, Key::L, Key::R] {
        app.apply_input_batch(KeyEvent::Released(key), &mut PendingInput::default(), &mut keys, &tx, &rx).await.unwrap();
        let after = common::perf_trace::probe_counts()["measurement"].clone();
        assert_eq!(after["selection_generation"], before["selection_generation"]);
        assert_eq!(after["first_feedback_monotonic_us"], before["first_feedback_monotonic_us"]);
        assert_eq!(after["final_content_monotonic_us"], 0);
    }
    assert!(!keys[Key::Right]);
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn wave2_pending_selected_cover_reports_readiness() {
    let (dir, mut app) = missing_art_launcher();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut pack: serde_json::Value = serde_json::from_slice(
        &std::fs::read(repo.join("themes/orbit-hero/theme.json")).unwrap(),
    ).unwrap();
    let hero = pack["screens"]["library"]["components"].as_array_mut().unwrap()
        .iter_mut().find(|c| c["id"] == "hero").unwrap();
    hero["binding"] = "selected_cover".into();
    let theme_dir = dir.0.join("pending-cover-theme");
    std::fs::create_dir(&theme_dir).unwrap();
    std::fs::create_dir(theme_dir.join("assets")).unwrap();
    for entry in std::fs::read_dir(repo.join("themes/orbit-hero/assets")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), theme_dir.join("assets").join(entry.file_name())).unwrap();
    }
    std::fs::write(theme_dir.join("theme.json"), serde_json::to_vec(&pack).unwrap()).unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    app.res.insert(Some(common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&theme_dir).unwrap(), &bank, 2,
    ).unwrap()));
    let path = dir.0.join("readiness.trace");
    trace_first_frame(&mut app, &path).await;
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    for event in ["first_feedback", "frame"] {
        let row = rows.iter().find(|row| row["event"] == event).unwrap();
        assert_eq!(row["measurement"]["content_pending_reason"], "selected_cover");
        assert_eq!(row["measurement"]["next_cover_expected"], true);
    }
}
