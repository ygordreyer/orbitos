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
        "completed cover and logo misses must settle final_content"
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
