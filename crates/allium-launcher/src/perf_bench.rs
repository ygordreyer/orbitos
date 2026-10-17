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
            let warm_started = Instant::now();
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
                ensure!(
                    warm_started.elapsed() < Duration::from_secs(10),
                    "warm work did not finish"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
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

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit disposable completion and presentation fixture"]
async fn completion_presentation_proofs() -> Result<()> {
    use crate::view::orbit::{WarmArt, proof_art_completion};
    use common::theme::pack::{Binding, ScreenId};
    let _cleanup = fixture()?;
    let logos = ALLIUM_GAMES_DIR.join("media/logos");
    fs::create_dir_all(&logos)?;
    for i in 0..20 {
        art(&logos.join(format!("Game {i:02}.png")), 192, 48, 70 + i)?;
    }
    for theme in [
        "orbit",
        "orbit-hero",
        "big-picture",
        "compact-plus",
        "completion-modal",
        "completion-metadata",
    ] {
        let modal = theme == "completion-modal";
        let changed_metadata = theme == "completion-metadata";
        let source_theme = if modal || changed_metadata {
            "orbit-hero"
        } else {
            theme
        };
        if theme != "orbit" {
            copy_tree(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../themes")
                    .join(source_theme),
                &ALLIUM_SD_ROOT.join("Themes").join(theme),
            )?;
        }
        if modal || changed_metadata {
            let path = ALLIUM_SD_ROOT.join("Themes").join(theme).join("theme.json");
            let mut pack: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
            pack["id"] = theme.into();
            if modal {
                let compact: serde_json::Value = serde_json::from_slice(&fs::read(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../themes/compact-plus/theme.json"),
                )?)?;
                let artless_list = compact["screens"]["library"]["components"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["kind"] == "entries")
                    .unwrap()
                    .clone();
                for c in pack["screens"]["library"]["components"]
                    .as_array_mut()
                    .unwrap()
                {
                    if c["kind"] == "entries" {
                        *c = artless_list.clone();
                    }
                }
            }
            fs::write(path, serde_json::to_vec(&pack)?)?;
        }
        crate::view::details::invalidate_metadata();
        let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
        crate::theme_adapter::choose(&app.res, Some(theme))?;
        app.view.proof_select_tab(1);
        let games = if changed_metadata {
            // The sidecar contract requires a console directory below ALLIUM_GAMES_DIR.
            let games = ALLIUM_GAMES_DIR.join("PS");
            fs::create_dir_all(&games)?;
            for directory in ["Imgs", "media"] {
                copy_tree(&ALLIUM_GAMES_DIR.join(directory), &games.join(directory))?;
            }
            fs::copy(
                ALLIUM_GAMES_DIR.join("gamelist.xml"),
                games.join("gamelist.xml"),
            )?;
            for i in 0..20 {
                let path = games.join(format!("Game {i:02}.chd"));
                fs::write(&path, [])?;
                let mut record: common::database::NewGame = Game::new(path).into();
                record.image = Some(games.join(format!("Imgs/Game {i:02}.png")));
                app.res.get::<Database>().update_games(&[record])?;
            }
            app.view.proof_games_directory(games.clone());
            games
        } else {
            ALLIUM_GAMES_DIR.to_path_buf()
        };
        let mut overlay = false;
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        if modal {
            event(&mut app, KeyEvent::Pressed(Key::Start), &tx).await?;
            ensure!(
                app.view.proof_game_theme_inputs().last().unwrap().screen == ScreenId::GameDetail,
                "details did not open"
            );
        }
        app.display.record_presentations();
        app.present_frame(false, &mut overlay).await?;
        app.display.take_presented_frames();
        let model = app.view.proof_game_theme_inputs().remove(0);
        let selected = model.focus.item.as_ref().unwrap().clone();
        if modal {
            let handle = crate::theme_adapter::snapshot(&app.res).unwrap();
            ensure!(
                !handle.pack().screens[&ScreenId::Library]
                    .components
                    .iter()
                    .any(|c| c.binding == Some(Binding::SelectedCover)
                        || c.art_size.unwrap_or(0) > 0),
                "modal fixture Library depends on cover"
            );
        }
        if changed_metadata {
            fs::write(
                ALLIUM_BASE_DIR.join("metadata/game-details.json"),
                serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1, "platform": "PS", "entries": [{
                        "id": "completion-game", "title": "Completion game", "aliases": [model.text[&Binding::SelectedTitle]],
                        "year": 1999, "first_release_year": 1999, "release_region": "USA",
                        "developer": "Completion studio", "publisher": "Completion publisher", "genres": ["Racing"],
                        "description": "A changed description received from the metadata worker.",
                        "source": {"title": "Fixture", "url": "https://example.org/fixture", "license": {"name": "Fixture", "url": "https://example.org/license"}}
                    }]
                }))?,
            )?;
        }
        let visible: Vec<_> = model.rows[&common::theme::pack::Binding::Entries]
            .iter()
            .map(|row| row.id.clone())
            .collect();
        let peek =
            crate::theme_adapter::entry_id(&games.join(format!("Game {:02}.chd", visible.len())));
        proof_art_completion();
        let started = Instant::now();
        let mut metadata_noops = 0;
        let mut offscreen_noops = 0;
        let mut redraws = 0;
        let mut logo_redraws = 0;
        let mut modal_cover_redraws = 0;
        let mut metadata_redraws = 0;
        loop {
            ensure!(
                !app.view.should_draw(),
                "previous completion did not settle"
            );
            let pixels = app.display.pixmap().data().to_vec();
            let pending = app.view.idle_media_step();
            let task = proof_art_completion();
            if let Some(task) = &task {
                let offscreen = match task {
                    WarmArt::Hero(e) | WarmArt::Logo(e) => {
                        crate::theme_adapter::entry_id(e.path()) != selected
                    }
                    WarmArt::Cover(e) | WarmArt::CoverFallback(e) => {
                        let id = crate::theme_adapter::entry_id(e.path());
                        !visible.contains(&id) && id != peek
                    }
                    _ => false,
                };
                if matches!(task, WarmArt::Metadata(_)) && !app.view.should_draw() {
                    metadata_noops += 1;
                }
                if offscreen {
                    offscreen_noops += 1;
                    ensure!(
                        !app.view.should_draw(),
                        "offscreen art invalidated {theme}: {task:?}"
                    );
                }
            }
            let dirty = app.view.should_draw();
            app.present_frame(false, &mut overlay).await?;
            let frames = app.display.take_presented_frames();
            let fresh = app.view.proof_fresh_game_pixels();
            if fresh != pixels {
                ensure!(
                    dirty,
                    "visible completion was not invalidated for {theme}: {task:?}"
                );
                if matches!(&task, Some(WarmArt::Logo(e)) if crate::theme_adapter::entry_id(e.path()) == selected)
                {
                    logo_redraws += 1;
                }
                if modal
                    && matches!(&task, Some(WarmArt::Cover(e) | WarmArt::CoverFallback(e)) if crate::theme_adapter::entry_id(e.path()) == selected)
                {
                    modal_cover_redraws += 1;
                }
                if changed_metadata
                    && app.view.proof_game_theme_inputs()[0].text[&Binding::SelectedMetadata]
                        .contains("1999")
                {
                    metadata_redraws += 1;
                }
                ensure!(
                    frames.len() == 1,
                    "visible completion did not present exactly once: {task:?}"
                );
                ensure!(
                    app.display.pixmap().data() != pixels,
                    "completion presented unchanged pixels"
                );
                redraws += 1;
            } else {
                ensure!(frames.is_empty(), "no-op completion presented {theme}");
                ensure!(
                    app.display.pixmap().data() == pixels,
                    "no-op completion changed pixels"
                );
            }
            ensure!(
                app.display.pixmap().data() == fresh,
                "completion differs from fresh {theme}: {task:?}, dirty={dirty}"
            );
            app.present_frame(false, &mut overlay).await?;
            ensure!(
                app.display.take_presented_frames().is_empty(),
                "completion presented twice"
            );
            if !pending {
                break;
            }
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "media work did not finish"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ensure!(
            metadata_noops > 0 && offscreen_noops > 0 && redraws > 0,
            "missing completion cases for {theme}"
        );
        ensure!(
            logo_redraws > 0,
            "logo arrival was not exercised for {theme}"
        );
        ensure!(
            !modal || modal_cover_redraws > 0,
            "modal cover arrival was not exercised"
        );
        ensure!(
            !changed_metadata || metadata_redraws > 0,
            "changed metadata was not exercised"
        );
        ensure!(!app.view.idle_media_step(), "settled media restarted work");
        if modal {
            event(&mut app, KeyEvent::Pressed(Key::B), &tx).await?;
            app.present_frame(false, &mut overlay).await?;
            app.display.take_presented_frames();
        }

        let trace = ALLIUM_SD_ROOT.join(format!("{theme}-completion.jsonl"));
        common::perf_trace::start_probe(trace.to_str().unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        for key in [Key::Right, Key::Left, Key::Down, Key::Up, Key::R, Key::L] {
            let pixels = app.display.pixmap().data().to_vec();
            perf::take();
            {
                let _input = common::perf_trace::phase(common::perf_trace::Phase::Input);
                event(&mut app, KeyEvent::Released(key), &tx).await?;
            }
            app.present_frame(false, &mut overlay).await?;
            ensure!(perf::take().frames == 0, "release prepared a model");
            ensure!(
                common::perf_trace::probe_counts()["stages"]["phases"]["layout"]["begin_monotonic_us"]
                    == 0,
                "release entered layout"
            );
            ensure!(
                app.display.take_presented_frames().is_empty(),
                "release presented"
            );
            ensure!(
                app.display.pixmap().data() == pixels,
                "release changed pixels"
            );
        }
        app.view.proof_theme_redraw();
        app.present_frame(false, &mut overlay).await?;
        ensure!(
            app.display.take_presented_frames().is_empty(),
            "empty damage flushed"
        );
        ensure!(
            fs::read_to_string(&trace)?.is_empty(),
            "no-op presentation logged a drawn frame"
        );
        app.display.presenter();
        app.present_frame(false, &mut overlay).await?;
        ensure!(
            app.display.take_presented_frames().len() == 1,
            "first backing was not initialized"
        );
        app.display.presenter().assert_visible();
        for backing_change in [false, true] {
            if backing_change {
                app.display.presenter().replace_backing();
            } else {
                app.display.presenter().set_page(1);
            }
            let pixels = app.display.pixmap().data().to_vec();
            app.view.proof_theme_redraw();
            app.present_frame(false, &mut overlay).await?;
            ensure!(
                app.display.pixmap().data() == pixels,
                "repair recomposed different pixels"
            );
            ensure!(
                app.display.take_presented_frames().len() == 1,
                "empty damage skipped display repair, backing_change={backing_change}"
            );
            app.display.presenter().assert_visible();
            app.present_frame(false, &mut overlay).await?;
            ensure!(
                app.display.take_presented_frames().is_empty(),
                "repaired display presented again"
            );
        }
        let pixels = app.display.pixmap().data().to_vec();
        app.display.save()?;
        app.handle_command(Command::Redraw).await?;
        app.present_frame(false, &mut overlay).await?;
        ensure!(
            app.display.take_presented_frames().len() == 1,
            "wake redraw did not present"
        );
        ensure!(
            app.display.pixmap().data() == pixels,
            "wake redraw changed composition"
        );
        app.display.presenter().assert_visible();
        app.display.pop();
        println!(
            "COMPLETION_PROOF theme={theme} visible_redraws={redraws} logo_redraws={logo_redraws} modal_cover_redraws={modal_cover_redraws} metadata_redraws={metadata_redraws} metadata_noops={metadata_noops} offscreen_noops={offscreen_noops} release_models=0 empty_damage_presents=0 display_repairs=3 wake_presents=1"
        );
        app.view.stop_workers();
    }
    Ok(())
}

thread_local! {
    static LOOP_INPUT: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedReceiver<KeyEvent>>> = const { std::cell::RefCell::new(None) };
    static LOOP_STATE: std::cell::RefCell<Option<std::rc::Rc<LoopState>>> = const { std::cell::RefCell::new(None) };
}
#[derive(Default)]
struct LoopState {
    held: std::cell::Cell<bool>,
    inputs: std::cell::Cell<usize>,
    polls: std::cell::Cell<usize>,
    iterations: std::cell::Cell<usize>,
    idle: std::cell::Cell<bool>,
    input: tokio::sync::Notify,
    settled: tokio::sync::Notify,
    completion_generation: std::cell::Cell<u64>,
    adopted_held: std::cell::Cell<bool>,
    presented_held: tokio::sync::Notify,
}
pub(super) fn has_loop_input() -> bool {
    LOOP_INPUT.with(|input| input.borrow().is_some())
}
pub(super) async fn poll_loop_input() -> KeyEvent {
    std::future::poll_fn(|cx| {
        LOOP_INPUT.with(|input| input.borrow_mut().as_mut().unwrap().poll_recv(cx))
    })
    .await
    .unwrap()
}
pub(super) fn loop_input(keys: &EnumMap<Key, bool>) {
    LOOP_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            state.held.set(keys[Key::Right]);
            state.inputs.set(state.inputs.get() + 1);
            state.idle.set(false);
            state.input.notify_one();
        }
    });
}
pub(super) fn loop_media_poll(pending: bool) {
    LOOP_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            state.polls.set(state.polls.get() + 1);
            let generation = crate::view::orbit::completion_generation();
            if generation != state.completion_generation.replace(generation) && state.held.get() {
                state.adopted_held.set(true);
            }
            if !pending {
                state.settled.notify_one();
            }
        }
    });
}
pub(super) fn loop_iteration() {
    LOOP_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            state.iterations.set(state.iterations.get() + 1);
        }
    });
}
pub(super) fn loop_idle() {
    LOOP_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            state.idle.set(true);
            state.settled.notify_one();
        }
    });
}
pub(super) fn loop_present() {
    LOOP_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            if state.held.get() && state.adopted_held.get() {
                state.presented_held.notify_one();
            }
        }
    });
}
struct LoopReset;
impl Drop for LoopReset {
    fn drop(&mut self) {
        LOOP_INPUT.with(|input| *input.borrow_mut() = None);
        LOOP_STATE.with(|state| *state.borrow_mut() = None);
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit disposable event loop fixture"]
async fn held_completion_release_and_settled_media_event_loop() -> Result<()> {
    let _cleanup = fixture()?;
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    LOOP_INPUT.with(|input| *input.borrow_mut() = Some(receiver));
    let state = std::rc::Rc::new(LoopState::default());
    state
        .completion_generation
        .set(crate::view::orbit::completion_generation());
    LOOP_STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    let _reset = LoopReset;
    sender.send(KeyEvent::Pressed(Key::Right))?;
    let checks = async {
        state.input.notified().await;
        ensure!(state.held.get(), "event loop did not record held key");
        let polls = state.polls.get();
        loop {
            tokio::select! {
                _ = state.presented_held.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(2)) => {
                    sender.send(KeyEvent::Autorepeat(Key::Right))?;
                }
            }
        }
        ensure!(
            state.inputs.get() > 1,
            "test did not exercise queued repeats"
        );
        ensure!(
            state.held.get(),
            "completion was not presented until release"
        );
        ensure!(
            state.polls.get() > polls,
            "held key blocked completion adoption"
        );
        perf::take();
        sender.send(KeyEvent::Released(Key::Right))?;
        while state.held.get() {
            state.input.notified().await;
        }
        ensure!(!state.held.get(), "release failed to clear held state");
        ensure!(
            perf::take().frames == 0,
            "release prepared a frame in event loop"
        );
        while !state.idle.get() {
            state.settled.notified().await;
        }
        let polls = state.polls.get();
        let iterations = state.iterations.get();
        tokio::time::sleep(Duration::from_millis(600)).await;
        ensure!(state.polls.get() == polls, "settled media kept polling");
        let settled_wakes = state.iterations.get() - iterations;
        ensure!(
            settled_wakes <= 2,
            "settled frame timer kept waking: {settled_wakes}"
        );
        println!(
            "EVENT_LOOP_PROOF inputs={} adopted_and_presented_before_release=true release_models=0 held_state_cleared=true settled_extra_polls=0 settled_wakes_600ms={settled_wakes} total_media_polls={polls}",
            state.inputs.get()
        );
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            result = app.run_event_loop() => anyhow::bail!("event loop stopped: {result:?}"),
            result = checks => result,
        }
    })
    .await??;
    app.view.stop_workers();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit disposable event loop fixture"]
async fn medianoop_continuous_repeats_adopt_current_selection_before_release() -> Result<()> {
    let _cleanup = fixture()?;
    let root = ALLIUM_GAMES_DIR.join("held-current");
    fs::create_dir_all(root.join("Imgs"))?;
    fs::write(root.join("Current.chd"), [])?;
    art(&root.join("Imgs/Current.png"), 240, 240, 91)?;
    art(&root.join("Imgs/Current.hero.png"), 360, 210, 91)?;
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    app.view.proof_games_directory(root);
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    LOOP_INPUT.with(|input| *input.borrow_mut() = Some(receiver));
    let state = std::rc::Rc::new(LoopState::default());
    state.completion_generation.set(crate::view::orbit::completion_generation());
    LOOP_STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    let _reset = LoopReset;
    sender.send(KeyEvent::Pressed(Key::Right))?;
    for _ in 0..130 { sender.send(KeyEvent::Autorepeat(Key::Right))?; }
    let checks = async {
        state.input.notified().await;
        loop {
            tokio::select! {
                _ = state.presented_held.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(2)) => {
                    sender.send(KeyEvent::Autorepeat(Key::Right))?;
                }
            }
        }
        ensure!(state.inputs.get() >= 65, "did not exercise the bounded repeat batch");
        ensure!(state.held.get() && state.adopted_held.get(), "completion waited for release");
        println!("MEDIANOOP_REPEAT inputs={} polls={} completion_adopted_and_presented_while_held=true repeat_batch_bound=64",
            state.inputs.get(), state.polls.get());
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = app.run_event_loop() => anyhow::bail!("event loop stopped: {result:?}"),
            result = checks => result,
        }
    }).await??;
    app.view.stop_workers();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn perf_name_index_consumed_by_view_rearms_settled_media() -> Result<()> {
    use crate::view::orbit::proof_art_completion;
    use common::theme::pack::Binding;

    let root = ALLIUM_SD_ROOT.join("lane-a-name-index-regression");
    fs::create_dir_all(root.join("Imgs"))?;
    let _cleanup = Cleanup(root.clone());
    for i in 0..3 {
        fs::write(root.join(format!("Raw {i}.chd")), [])?;
        art(&root.join(format!("Imgs/Raw {i}.png")), 240, 240, i)?;
    }
    common::media::invalidate();
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    crate::theme_adapter::choose(&app.res, Some("orbit"))?;
    app.view.proof_games_directory(root.clone());
    app.display.record_presentations();
    app.display.save()?;
    let mut overlay = false;
    let mut media_pending = true;
    let mut generation = common::media::name_generation();
    let start = Instant::now();
    // Drain initial indexes and artwork before reproducing the completion race.
    while start.elapsed() < Duration::from_millis(300) || media_pending {
        app.update_view(Duration::ZERO, &mut generation, &mut media_pending);
        media_pending = app.view.idle_media_step();
        app.present_frame(false, &mut overlay).await?;
        app.display.take_presented_frames();
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "initial media did not settle"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    ensure!(!app.view.should_draw(), "initial scene did not settle");
    proof_art_completion();
    let pixels = app.display.pixmap().data().to_vec();
    let _ = crate::network_runtime::poll();
    generation = common::media::name_generation();
    // Queue only after network polling. The owning view must consume this result.
    fs::write(
        root.join("gamelist.xml"),
        "<gameList><game><path>Raw 0.chd</path><name>Indexed title zero</name></game><game><path>Raw 1.chd</path><name>Indexed title one</name></game><game><path>Raw 2.chd</path><name>Indexed title two</name></game></gameList>",
    )?;
    common::media::check_directory(&root);
    let before = generation;
    let start = Instant::now();
    while common::media::name_generation() == before {
        app.update_view(Duration::ZERO, &mut generation, &mut media_pending);
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "view did not consume the index"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    ensure!(
        media_pending,
        "view-consumed name index did not rearm settled media"
    );
    ensure!(
        app.view.should_draw(),
        "name index did not invalidate the themed scene"
    );
    ensure!(
        !crate::network_runtime::poll(),
        "later network poll rediscovered the consumed generation"
    );
    app.present_frame(false, &mut overlay).await?;
    ensure!(
        app.display.take_presented_frames().len() == 1,
        "name-index refresh did not present a frame"
    );
    ensure!(
        app.display.pixmap().data() != pixels,
        "name-index frame kept stale pixels"
    );
    ensure!(
        app.view.proof_game_theme_inputs()[0].text[&Binding::SelectedTitle]
            .starts_with("Indexed title"),
        "view kept the old list"
    );
    let start = Instant::now();
    let mut media_work = 0;
    while media_pending {
        media_pending = app.view.idle_media_step();
        if proof_art_completion().is_some() {
            media_work += 1;
        }
        app.present_frame(false, &mut overlay).await?;
        app.display.take_presented_frames();
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "rearmed media did not settle"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    ensure!(media_work > 0, "replacement list never loaded artwork");
    app.view.stop_workers();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn queued_navigation_repeats_present_once_and_preserve_boundaries() -> Result<()> {
    let root = ALLIUM_GAMES_DIR.join("PS/.loop-repeat-regression");
    fs::create_dir_all(&root)?;
    let _cleanup = Cleanup(root.clone());
    for i in 0..10 {
        fs::write(root.join(format!("Loop {i:02}.chd")), [])?;
    }
    let mut app = AlliumLauncher::new(DefaultPlatform::new()?)?;
    crate::theme_adapter::choose(&app.res, Some("orbit"))?;
    app.view.proof_games_directory(root.clone());
    app.display.save()?;
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await?;
    app.display.record_presentations();
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let mut keys = EnumMap::default();
    let mut pending = PendingInput::default();
    for _ in 0..4 {
        ensure!(
            app.apply_input_batch(
                KeyEvent::Pressed(Key::Right),
                &mut pending,
                &mut keys,
                &tx,
                &rx
            )
            .await?
                == 1
        );
        app.present_frame(false, &mut overlay).await?;
    }
    let separate_selection = app.view.proof_game_theme_inputs()[0].focus.item.clone();
    let separate_presents = app.display.take_presented_frames().len();
    ensure!(
        separate_presents == 4,
        "separate presses: {separate_presents}"
    );
    app.view.proof_games_directory(root.clone());
    app.present_frame(false, &mut overlay).await?;
    app.display.take_presented_frames();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    LOOP_INPUT.with(|input| *input.borrow_mut() = Some(receiver));
    let _reset = LoopReset;
    for _ in 0..4 {
        sender.send(KeyEvent::Autorepeat(Key::Right))?;
    }
    sender.send(KeyEvent::Released(Key::Right))?;
    sender.send(KeyEvent::Pressed(Key::A))?;
    sender.send(KeyEvent::Autorepeat(Key::Right))?;
    let first = pending.take(&mut app.platform).unwrap();
    ensure!(
        app.apply_input_batch(first, &mut pending, &mut keys, &tx, &rx)
            .await?
            == 4
    );
    app.present_frame(false, &mut overlay).await?;
    let batch_selection = app.view.proof_game_theme_inputs()[0].focus.item.clone();
    ensure!(
        batch_selection == separate_selection,
        "coalescing changed selection"
    );
    let batch_presents = app.display.take_presented_frames().len();
    ensure!(batch_presents == 1, "batch presents: {batch_presents}");
    ensure!(
        pending.0 == Some(KeyEvent::Released(Key::Right)),
        "crossed release boundary"
    );
    let release = pending.take(&mut app.platform).unwrap();
    ensure!(
        app.apply_input_batch(release, &mut pending, &mut keys, &tx, &rx)
            .await?
            == 1
    );
    ensure!(!keys[Key::Right]);
    let action = pending.take(&mut app.platform).unwrap();
    ensure!(action == KeyEvent::Pressed(Key::A));
    ensure!(
        app.apply_input_batch(action, &mut pending, &mut keys, &tx, &rx)
            .await?
            == 1
    );
    ensure!(
        app.view.proof_game_theme_inputs()[0].focus.item == batch_selection,
        "crossed action boundary"
    );
    let command = rx.try_recv()?;
    let Command::Exec(command) = command else {
        anyhow::bail!("launch did not produce Exec: {command:?}");
    };
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    ensure!(
        args.iter().any(|arg| arg.ends_with("Loop 06.chd")),
        "launch target changed: {args:?}"
    );
    ensure!(
        pending.is_ready(&mut app.platform),
        "readiness query lost event"
    );
    ensure!(
        pending.is_ready(&mut app.platform),
        "readiness query consumed event"
    );
    ensure!(pending.take(&mut app.platform) == Some(KeyEvent::Autorepeat(Key::Right)));
    for boundary in [
        KeyEvent::Pressed(Key::B),
        KeyEvent::Pressed(Key::Menu),
        KeyEvent::Autorepeat(Key::Left),
    ] {
        sender.send(KeyEvent::Autorepeat(Key::Right))?;
        sender.send(boundary)?;
        sender.send(KeyEvent::Autorepeat(Key::Right))?;
        let first = pending.take(&mut app.platform).unwrap();
        ensure!(
            app.apply_input_batch(first, &mut pending, &mut keys, &tx, &rx)
                .await?
                == 1
        );
        ensure!(
            pending.take(&mut app.platform) == Some(boundary),
            "crossed {boundary:?}"
        );
        ensure!(pending.take(&mut app.platform) == Some(KeyEvent::Autorepeat(Key::Right)));
    }
    println!(
        "REPEAT_PROOF separate_presents={separate_presents} batch_presents={batch_presents} applied_steps=4 launch_target=Loop_06 release_action_menu_direction_boundaries=preserved"
    );
    app.view.stop_workers();
    Ok(())
}
