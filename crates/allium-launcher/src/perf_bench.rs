use super::*;
use anyhow::ensure;
use common::constants::{
    ALLIUM_APPS_DIR, ALLIUM_BASE_DIR, ALLIUM_DATABASE, ALLIUM_GAMES_DIR, ALLIUM_SD_ROOT,
};
use common::perf::{self, Counts};
use common::theme::{runtime::ThemeSnapshot, selection::Selection};
use sha2::{Digest, Sha256};
use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::Duration,
};
use tiny_skia::Pixmap;

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::env::var_os("ALLIUM_PERF_KEEP_FIXTURE").is_some() {
            return;
        }
        if let Err(e) = fs::remove_dir_all(&self.0) {
            eprintln!("fixture cleanup failed: {e}");
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "fixture source contains a symlink");
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn art(path: &Path, w: u32, h: u32, seed: u8) -> Result<()> {
    let mut pixels = Pixmap::new(w, h).unwrap();
    for (i, p) in pixels.data_mut().chunks_exact_mut(4).enumerate() {
        let x = (i as u32 % w) as u8;
        let y = (i as u32 / w) as u8;
        p.copy_from_slice(&[
            x.wrapping_add(seed),
            y.wrapping_add(seed),
            x ^ y ^ seed,
            255,
        ]);
    }
    pixels.save_png(path)?;
    Ok(())
}

fn fixture() -> Result<Cleanup> {
    let root = ALLIUM_SD_ROOT.as_path();
    let temp = std::env::temp_dir().canonicalize()?;
    ensure!(
        root.parent() == Some(temp.as_path())
            && root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("orbit-perf."),
        "requires a fresh orbit-perf.* directory under the canonical temp dir"
    );
    if std::env::var_os("ALLIUM_PERF_REUSE_FIXTURE").is_some() {
        ensure!(
            root.join("Roms/PS/gamelist.xml").is_file(),
            "missing reusable fixture"
        );
        perf::take();
        return Ok(Cleanup(root.to_path_buf()));
    }
    ensure!(
        fs::read_dir(root)?.next().is_none(),
        "fixture must be empty"
    );
    ensure!(*ALLIUM_BASE_DIR == root.join(".allium"), "base override");
    ensure!(*ALLIUM_GAMES_DIR == root.join("Roms/PS"), "games override");
    ensure!(*ALLIUM_APPS_DIR == root.join("Apps"), "apps override");
    ensure!(
        *ALLIUM_DATABASE == root.join("Saves/CurrentProfile/allium.db"),
        "database override"
    );

    let cleanup = Cleanup(root.to_path_buf());
    for dir in [
        ".allium/state",
        ".allium/metadata",
        "Themes/Allium",
        "Roms/PS/Imgs",
        "Apps",
        "Saves/CurrentProfile",
    ] {
        fs::create_dir_all(root.join(dir))?;
    }

    let static_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static/.allium");
    for dir in ["config", "fonts", "locales"] {
        copy_tree(&static_root.join(dir), &root.join(".allium").join(dir))?;
    }

    let mut styles = Stylesheet::default();
    styles.wallpaper = None;
    styles.recents.use_recents_carousel = false;
    fs::write(
        root.join("Themes/Allium/stylesheet.json"),
        serde_json::to_vec(&styles)?,
    )?;
    let snapshot = ThemeSnapshot::builtin("orbit")?;
    fs::write(
        root.join(".allium/state/ui-theme.json"),
        serde_json::to_vec(&Selection::from_snapshot(&snapshot, 1))?,
    )?;

    let db = Database::new()?;
    let mut xml = String::from("<gameList>");
    for i in 0..20u8 {
        let stem = format!("Game {i:02}");
        let path = ALLIUM_GAMES_DIR.join(format!("{stem}.chd"));
        let images = ALLIUM_GAMES_DIR.join("Imgs");
        fs::write(&path, [])?;
        art(&images.join(format!("{stem}.png")), 240, 240, i)?;
        art(&images.join(format!("{stem}.orbit.png")), 101, 101, i)?;
        art(&images.join(format!("{stem}.hero.png")), 360, 210, i)?;

        let mut record: common::database::NewGame = Game::new(path).into();
        record.image = Some(images.join(format!("{stem}.png")));
        db.update_games(&[record.clone()])?;
        db.increment_play_count(&record)?;
        xml.push_str(&format!(
            "<game><path>./{stem}.chd</path>\
             <desc>{}</desc></game>",
            "A bounded fixture description. ".repeat(12)
        ));
    }
    xml.push_str("</gameList>");
    fs::write(ALLIUM_GAMES_DIR.join("gamelist.xml"), &xml)?;
    db.set_gamelist_fingerprint(&ALLIUM_GAMES_DIR, xml.len() as i64)?;
    drop(db);
    perf::take();
    Ok(cleanup)
}

// These stages mirror the idle, nonmodal loop with MockPlatform presentation.
fn pump(app: &mut AlliumLauncher<DefaultPlatform>) -> Result<(u128, u128)> {
    if crate::network_runtime::poll() {
        app.view.set_should_draw();
    }
    app.view.update(Duration::ZERO);
    let start = Instant::now();
    if app.view.should_draw() {
        app.view.prepare_theme()?;
    }
    let prepare_us = start.elapsed().as_micros();
    let start = Instant::now();
    let styles = crate::theme_adapter::drawing_style(
        &app.res.get::<Stylesheet>(),
        crate::theme_adapter::snapshot(&app.res).as_ref(),
    );
    if app.view.should_draw() && app.view.draw(&mut app.display, &styles)? {
        app.display.flush()?;
    }
    Ok((prepare_us, start.elapsed().as_micros()))
}

fn report(
    name: &str,
    index: usize,
    total_us: u128,
    phases: (u128, u128),
    counts: Counts,
    samples: &mut Vec<u128>,
) {
    println!(
        "{name}[{index}] total_us={total_us} prepare_us={} paint_us={} \
         counts={counts:?}",
        phases.0, phases.1
    );
    println!(
        "PERF_SAMPLE {}",
        serde_json::json!({"name": name, "index": index, "total_us": total_us, "prepare_us": phases.0, "paint_us": phases.1, "counts": counts})
    );
    samples.push(total_us);
}

fn summary(name: &str, samples: &mut [u128]) {
    samples.sort_unstable();
    let at = |pct: usize| (samples.len() * pct).div_ceil(100) - 1;
    println!(
        "{name} n={} p50_us={} p95_us={}",
        samples.len(),
        samples[at(50)],
        samples[at(95)]
    );
}

fn frame_hash(name: &str, index: usize, app: &AlliumLauncher<DefaultPlatform>) {
    if matches!(name, "dialogs_and_noop" | "dialog_release") && index == 5 {
        if let Ok(dir) = std::env::var("ALLIUM_UX4_FRAME_PNG_DIR") {
            fs::create_dir_all(&dir).unwrap();
            app.display
                .pixmap()
                .to_owned()
                .save_png(Path::new(&dir).join(format!("{name}-{index}.png")))
                .unwrap();
        }
    }

    println!(
        "PERF_HASH {}",
        serde_json::json!({"name":name,"index":index,"sha256":const_hex::encode(Sha256::digest(app.display.pixmap().data()))})
    );
}

async fn event(
    app: &mut AlliumLauncher<DefaultPlatform>,
    event: KeyEvent,
    tx: &tokio::sync::mpsc::Sender<Command>,
) -> Result<()> {
    let mut bubble = VecDeque::new();
    app.view
        .handle_key_event(event, tx.clone(), &mut bubble)
        .await?;
    ensure!(bubble.is_empty(), "unexpected bubbled command");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit disposable host performance fixture"]
async fn orbit_hot_paths() -> Result<()> {
    let _cleanup = fixture()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    perf::take();
    let start = Instant::now();
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    ensure!(
        crate::theme_adapter::snapshot(&app.res).is_some_and(|h| h.pack().id == "orbit"),
        "benchmark must use the actual Orbit theme"
    );
    let bg = app.res.get::<Stylesheet>().ui.background_color;
    app.display.clear(bg)?;
    app.display.save()?;
    let phases = pump(&mut app)?;
    let total = start.elapsed().as_micros();
    report(
        "startup_first_frame",
        0,
        total,
        phases,
        perf::take(),
        &mut vec![],
    );
    frame_hash("startup_first_frame", 0, &app);
    ensure!(!app.view.should_draw(), "first frame did not settle");

    let mut samples = vec![];
    for i in 0..30 {
        perf::take();
        let start = Instant::now();
        app.view.set_should_draw();
        let phases = pump(&mut app)?;
        let total = start.elapsed().as_micros();
        report("full_screen", i, total, phases, perf::take(), &mut samples);
        frame_hash("full_screen", i, &app);
        black_box(app.display.pixmap().data());
    }
    summary("full_screen", &mut samples);

    // Traversal includes new heroes, new windows and bounded-cache eviction.
    let mut samples = vec![];
    for i in 0..38 {
        let key = if i < 19 { Key::Down } else { Key::Up };
        perf::take();
        let start = Instant::now();
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        let phases = pump(&mut app)?;
        let total = start.elapsed().as_micros();
        report(
            "navigation_press",
            i,
            total,
            phases,
            perf::take(),
            &mut samples,
        );
        frame_hash("navigation_press", i, &app);

        perf::take();
        let start = Instant::now();
        event(&mut app, KeyEvent::Released(key), &tx).await?;
        let phases = pump(&mut app)?;
        let total = start.elapsed().as_micros();
        report(
            "navigation_release",
            i,
            total,
            phases,
            perf::take(),
            &mut vec![],
        );
        frame_hash("navigation_release", i, &app);
        black_box(app.display.pixmap().data());
    }
    summary("navigation_press", &mut samples);

    // Prime both selections, then measure cache-warm adjacent steps.
    for key in [Key::Down, Key::Up] {
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        pump(&mut app)?;
    }
    let mut samples = vec![];
    for i in 0..100 {
        let key = if i % 2 == 0 { Key::Down } else { Key::Up };
        perf::take();
        let start = Instant::now();
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        let phases = pump(&mut app)?;
        let total = start.elapsed().as_micros();
        report(
            "warm_navigation",
            i,
            total,
            phases,
            perf::take(),
            &mut samples,
        );
        frame_hash("warm_navigation", i, &app);
    }
    summary("warm_navigation", &mut samples);

    for (i, key) in [
        Key::Menu,
        Key::Start,
        Key::Down,
        Key::Up,
        Key::B,
        Key::Select,
        Key::B,
    ]
    .into_iter()
    .enumerate()
    {
        perf::take();
        let start = Instant::now();
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        while let Ok(command) = rx.try_recv() {
            ensure!(
                matches!(command, Command::Redraw),
                "unexpected dialog command: {command:?}"
            );
            app.view.set_should_draw();
        }
        let phases = pump(&mut app)?;
        report(
            "dialogs_and_noop",
            i,
            start.elapsed().as_micros(),
            phases,
            perf::take(),
            &mut vec![],
        );
        frame_hash("dialogs_and_noop", i, &app);
        event(&mut app, KeyEvent::Released(key), &tx).await?;
        pump(&mut app)?;
        frame_hash("dialog_release", i, &app);
    }

    let mut samples = vec![];
    for i in 0..12 {
        let key = if i % 2 == 0 { Key::R } else { Key::L };
        perf::take();
        let start = Instant::now();
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        let phases = pump(&mut app)?;
        let total = start.elapsed().as_micros();
        report("tab_switch", i, total, phases, perf::take(), &mut samples);
        frame_hash("tab_switch", i, &app);
    }
    summary("tab_switch", &mut samples);

    // Exercise the separate legacy RecentsCarousel decode/resize path.
    let mut carousel = crate::view::recents::RecentsCarousel::load_or_new(
        geom::Rect::new(0, 54, 640, 376),
        app.res.clone(),
        None,
    )?;
    let styles = crate::theme_adapter::drawing_style(
        &app.res.get::<Stylesheet>(),
        crate::theme_adapter::snapshot(&app.res).as_ref(),
    );
    carousel.draw(&mut app.display, &styles)?;
    let mut samples = vec![];
    for i in 0..18 {
        perf::take();
        let start = Instant::now();
        let mut bubble = VecDeque::new();
        carousel
            .handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut bubble)
            .await?;
        carousel.draw(&mut app.display, &styles)?;
        app.display.flush()?;
        let total = start.elapsed().as_micros();
        report(
            "legacy_recents_step",
            i,
            total,
            (0, 0),
            perf::take(),
            &mut samples,
        );
    }
    summary("legacy_recents_step", &mut samples);
    ensure!(rx.try_recv().is_err(), "benchmark emitted a domain command");
    black_box(app.display.pixmap().data());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit disposable device proxy fixture"]
async fn dl16_device_proxy() -> Result<()> {
    let _cleanup = fixture()?;
    let pack = std::env::var("ALLIUM_UX4_PACK")?;
    copy_tree(
        Path::new(&pack),
        &ALLIUM_SD_ROOT.join("Themes/orbit-refined"),
    )?;
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    crate::theme_adapter::choose(&app.res, Some("orbit-refined"))?;
    app.view.proof_select_tab(1);
    let (tx, _rx) = tokio::sync::mpsc::channel(100);
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await?;
    let mut presenter = common::platform::PresenterProbe::new(640, 480);
    presenter.present(app.display.pixmap(), &[app.display.bounding_box()], 0)?;
    app.display.take_probe_damage();
    common::perf_trace::start_probe(&std::env::var("DL16_TRACE")?);
    for i in 0..20 {
        let key = if i % 2 == 0 { Key::Right } else { Key::Left };
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        app.view.update(Duration::ZERO);
        let start = Instant::now();
        app.present_frame(false, &mut overlay).await?;
        let mut row = common::perf_trace::probe_counts();
        row["index"] = i.into();
        row["wall_us"] = (start.elapsed().as_micros() as u64).into();
        let damage = app.display.take_probe_damage();
        common::perf_trace::begin_render();
        presenter.present(app.display.pixmap(), &damage, (i % 2) as u32)?;
        let present = common::perf_trace::probe_counts();
        for key in [
            "converted_pixels",
            "flushed_pixels",
            "damage_rects",
            "damage_pixels",
            "full_present_reason",
        ] {
            row[key] = present[key].clone();
        }
        row["sha256"] = const_hex::encode(Sha256::digest(app.display.pixmap().data())).into();
        println!("DEVICE_PROXY {}", row);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit disposable frontend proxy fixture"]
async fn dl17_frontend_proxy() -> Result<()> {
    let _cleanup = fixture()?;
    let pack = std::env::var("ALLIUM_UX4_PACK")?;
    copy_tree(
        Path::new(&pack),
        &ALLIUM_SD_ROOT.join("Themes/orbit-refined"),
    )?;
    for i in 0..8 {
        let dir = ALLIUM_APPS_DIR.join(format!("App {i}"));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("launch.sh"), "#!/bin/sh\n")?;
    }
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    crate::theme_adapter::choose(&app.res, Some("orbit-refined"))?;
    app.view.proof_select_tab(1);
    let (tx, _rx) = tokio::sync::mpsc::channel(100);
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await?;
    if std::env::var_os("DL17_PREWARM").is_some() {
        let warm_start = Instant::now();
        let warm_io = common::perf_trace::frontend_io_ops();
        let before = app.view.proof_game_theme_inputs();
        let pixels = app.display.pixmap().data().to_vec();
        let scene = app.view.proof_navigation_scene();
        for direction in [1, -1] {
            let mut work = None;
            let mut step = 0;
            loop {
                let start = Instant::now();
                let io = common::perf_trace::frontend_io_ops();
                let kind = app.view.navigation_warm_kind(&work);
                let finished = app.view.prewarm_navigation_step(&mut work, direction)?;
                println!(
                    "FRONTEND_WARM_STEP {}",
                    serde_json::json!({"direction":direction,"step":step,"kind":kind,"wall_us":start.elapsed().as_micros(),"io_ops":common::perf_trace::frontend_io_ops()-io})
                );
                ensure!(
                    app.view.proof_navigation_scene() == scene,
                    "warm step changed prepared pixels or input targets"
                );
                step += 1;
                if finished {
                    break;
                }
                ensure!(step < 100, "warm work did not finish");
                tokio::task::yield_now().await;
            }
        }
        ensure!(
            app.view.proof_game_theme_inputs() == before,
            "idle warm changed controller model"
        );
        ensure!(
            app.display.pixmap().data() == pixels,
            "idle warm presented pixels"
        );
        ensure!(
            !app.view.should_draw(),
            "idle warm invalidated presentation"
        );
        println!(
            "FRONTEND_WARM {}",
            serde_json::json!({"wall_us":warm_start.elapsed().as_micros(),
            "io_ops":common::perf_trace::frontend_io_ops()-warm_io,"controller_unchanged":true,"pixels_unchanged":true})
        );
    }
    let mut presenter = common::platform::PresenterProbe::new(640, 480);
    presenter.present(app.display.pixmap(), &[app.display.bounding_box()], 0)?;
    app.display.take_probe_damage();
    common::perf_trace::start_probe(&std::env::var("DL16_TRACE")?);
    let mut sequence = vec![("library_cold", Key::Right), ("library_cold", Key::Left)];
    sequence
        .extend((0..12).map(|i| ("library_move", if i % 2 == 0 { Key::Down } else { Key::Up })));
    sequence.push(("recents_entry", Key::L));
    sequence
        .extend((0..12).map(|i| ("recents_move", if i % 2 == 0 { Key::Down } else { Key::Up })));
    sequence.extend([("tab_switch", Key::L), ("tab_switch", Key::L)]);
    sequence.extend((0..12).map(|i| ("apps_move", if i % 2 == 0 { Key::Down } else { Key::Up })));
    sequence.push(("tab_switch", Key::R));
    sequence.extend((0..12).map(|i| {
        (
            "settings_move",
            if i % 2 == 0 { Key::Down } else { Key::Up },
        )
    }));
    sequence.extend([
        ("tab_switch", Key::R),
        ("tab_switch", Key::R),
        ("tab_switch", Key::R),
        ("tab_switch", Key::R),
    ]);
    for (i, (scenario, key)) in sequence.into_iter().enumerate() {
        let io = common::perf_trace::frontend_io_ops();
        let start = Instant::now();
        event(&mut app, KeyEvent::Pressed(key), &tx).await?;
        let input_us = start.elapsed().as_micros() as u64;
        let input_io = common::perf_trace::frontend_io_ops() - io;
        app.view.update(Duration::ZERO);
        app.present_frame(false, &mut overlay).await?;
        let mut row = common::perf_trace::probe_counts();
        row["scenario"] = scenario.into();
        row["index"] = i.into();
        row["input_us"] = input_us.into();
        row["input_io_ops"] = input_io.into();
        row["wall_us"] = (start.elapsed().as_micros() as u64).into();
        row["io_ops"] = (common::perf_trace::frontend_io_ops() - io).into();
        let damage = app.display.take_probe_damage();
        common::perf_trace::begin_render();
        presenter.present(app.display.pixmap(), &damage, (i % 2) as u32)?;
        let present = common::perf_trace::probe_counts();
        for key in [
            "converted_pixels",
            "flushed_pixels",
            "damage_rects",
            "damage_pixels",
            "full_present_reason",
        ] {
            row[key] = present[key].clone();
        }
        row["sha256"] = const_hex::encode(Sha256::digest(app.display.pixmap().data())).into();
        if std::env::var_os("DL17_ASSERT").is_some() {
            ensure!(
                input_io == 0,
                "navigation input performed IO at {scenario} {i}"
            );
            if scenario == "library_cold" {
                ensure!(
                    row["image_misses"] == 0,
                    "idle warm lost transformed art at {i}: {row}"
                );
            }
            if scenario == "recents_entry"
                || scenario == "recents_move"
                || scenario == "settings_move"
                || scenario == "apps_move"
            {
                ensure!(
                    row["io_ops"] == 0,
                    "text list preparation performed IO at {scenario} {i}"
                );
            }
        }
        println!("FRONTEND_PROXY {}", row);
    }
    Ok(())
}
