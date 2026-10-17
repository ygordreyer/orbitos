use super::*;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::{Notify, mpsc};

thread_local! {
    static INPUT: RefCell<Option<mpsc::UnboundedReceiver<KeyEvent>>> = const { RefCell::new(None) };
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}
struct Probe {
    sender: mpsc::UnboundedSender<KeyEvent>,
    injected: bool,
    loads: usize,
    handled: bool,
    done: Rc<Notify>,
}
pub(crate) fn has_input() -> bool {
    INPUT.with(|input| input.borrow().is_some())
}
pub(crate) async fn poll_input() -> KeyEvent {
    std::future::poll_fn(|cx| {
        INPUT.with(|input| input.borrow_mut().as_mut().unwrap().poll_recv(cx))
    })
    .await
    .unwrap()
}
pub(crate) fn art_load() {
    PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            if !probe.handled {
                probe.loads += 1;
            }
            if !probe.injected {
                probe.injected = true;
                probe.sender.send(KeyEvent::Pressed(Key::Right)).unwrap();
            }
        }
    });
}
pub(crate) fn handled_input() {
    PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            probe.handled = true;
            probe.done.notify_one();
        }
    });
}
struct Reset(std::path::PathBuf);
impl Drop for Reset {
    fn drop(&mut self) {
        INPUT.with(|input| *input.borrow_mut() = None);
        PROBE.with(|probe| *probe.borrow_mut() = None);
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn launcher_fixture() -> (Reset, AlliumLauncher<DefaultPlatform>) {
    let dir = ALLIUM_GAMES_DIR.join("dl17b-warm-input-fixture");
    assert!(!dir.exists());
    let reset = Reset(dir.clone());
    std::fs::create_dir_all(dir.join("Imgs")).unwrap();
    for i in 0..12 {
        std::fs::write(dir.join(format!("Warm {i:02}.chd")), []).unwrap();
        for (suffix, w, h) in [("hero", 360, 210), ("orbit", 101, 101)] {
            let mut art = tiny_skia::Pixmap::new(w, h).unwrap();
            art.fill(tiny_skia::Color::from_rgba8(i * 16, 90, 120, 255));
            art.save_png(dir.join(format!("Imgs/Warm {i:02}.{suffix}.png")))
                .unwrap();
        }
    }
    let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
    crate::theme_adapter::choose(&app.res, Some("orbit")).unwrap();
    app.view.proof_games_directory(dir);
    (reset, app)
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn dl17b_input_during_cold_warm_preempts_further_work() {
    let (_reset, mut app) = launcher_fixture();
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let before = app.view.proof_game_theme_inputs()[0].focus.item.clone();
    let (sender, receiver) = mpsc::unbounded_channel();
    INPUT.with(|input| *input.borrow_mut() = Some(receiver));
    let done = Rc::new(Notify::new());
    PROBE.with(|probe| {
        *probe.borrow_mut() = Some(Probe {
            sender,
            injected: false,
            loads: 0,
            handled: false,
            done: done.clone(),
        })
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            result = app.run_event_loop() => panic!("event loop stopped: {result:?}"),
            _ = done.notified() => {}
        }
    })
    .await
    .expect("cold artwork warm never handled injected key");
    PROBE.with(|probe| {
        let probe = probe.borrow();
        let probe = probe.as_ref().unwrap();
        assert!(probe.handled);
        assert_eq!(
            probe.loads, 1,
            "pending input must precede further cold artwork work"
        );
    });
    let after = app.view.proof_game_theme_inputs()[0].focus.item.clone();
    assert_ne!(before, after, "the real controller must handle the key");
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn dl17b_local_overlays_exclude_navigation_warm() {
    for key in [Key::Select, Key::Start, Key::X] {
        let (_reset, mut app) = launcher_fixture();
        let mut overlay = false;
        app.present_frame(false, &mut overlay).await.unwrap();
        assert!(app.navigation_warm_allowed());
        let (tx, mut rx) = mpsc::channel(100);
        let mut bubble = VecDeque::new();
        app.view
            .handle_key_event(KeyEvent::Pressed(key), tx, &mut bubble)
            .await
            .unwrap();
        while let Ok(command) = rx.try_recv() {
            app.handle_command(command).await.unwrap();
        }
        app.present_frame(false, &mut overlay).await.unwrap();
        assert!(!app.navigation_warm_allowed(), "overlay opened by {key:?}");
    }
    let (_reset, mut app) = launcher_fixture();
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    assert!(app.navigation_warm_allowed());
    app.toast = Some(Toast::new("warm guard".into(), None));
    assert!(!app.navigation_warm_allowed());
    app.toast = None;
    app.view.search("Warm".into()).unwrap();
    app.present_frame(false, &mut overlay).await.unwrap();
    assert!(!app.navigation_warm_allowed());
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn dl17b_each_warm_step_preserves_prepared_pixels_and_input_targets() {
    let (_reset, mut app) = launcher_fixture();
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let scene = app.view.proof_navigation_scene();
    for direction in [1, -1] {
        let mut work = None;
        let started = std::time::Instant::now();
        for step in 0.. {
            let finished = app
                .view
                .prewarm_navigation_step(&mut work, direction)
                .unwrap();
            assert!(
                app.view.proof_navigation_scene() == scene,
                "warm step {step}"
            );
            if finished {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "warm plan did not finish");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn pending_artwork_does_not_redraw_until_worker_completion() {
    let (_reset,mut app)=launcher_fixture();
    let mut overlay=false;
    app.present_frame(false,&mut overlay).await.unwrap();
    assert!(!app.view.should_draw());
    let gate=std::sync::Arc::new(std::sync::Barrier::new(2));
    crate::view::orbit::proof_art_gate(Some(gate.clone()));
    assert!(app.view.idle_media_step());
    crate::view::orbit::proof_art_gate(None);
    let waiting_draw=app.view.should_draw();
    gate.wait();
    assert!(!waiting_draw,"a pending decode invalidated the prepared frame");
    let started=std::time::Instant::now();
    while app.view.idle_media_step() {
        assert!(started.elapsed()<std::time::Duration::from_secs(10));
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(app.view.should_draw(),"completed artwork must invalidate the scene");
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn details_shadow_batches_yield_to_input_without_changing_presentation_or_selection() {
    let (_reset, mut app) = launcher_fixture();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../themes/compact-plus").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let handle = common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&root).unwrap(), &bank, 901,
    ).unwrap();
    app.res.insert(Some(handle));
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let scene = app.view.proof_navigation_scene();
    let selected = app.view.proof_game_theme_inputs()[0].focus.item.clone();
    let mut work = None;
    assert!(!app.view.prewarm_navigation_step(&mut work, 1).unwrap());
    assert!(app.view.proof_detail_shadow_warming(&work));
    let started = std::time::Instant::now();
    while app.view.navigation_warm_kind(&work) != "detail_shadow" {
        assert!(!app.view.prewarm_navigation_step(&mut work, 1).unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    // The first shadow tick creates a job, subsequent ticks advance bounded scanlines.
    for batch in 0..12 {
        assert!(!app.view.prewarm_navigation_step(&mut work, 1).unwrap());
        assert!(app.view.proof_detail_shadow_warming(&work));
        assert_eq!(app.view.proof_navigation_scene(), scene, "batch {batch}");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        sender.send(KeyEvent::Pressed(Key::Right)).unwrap();
        let mut checks = 0;
        assert!(!app.view.prewarm_navigation_step_with_pending(&mut work, 1, || {
            checks += 1;
            checks == 2 && !receiver.is_empty()
        }).unwrap());
        assert!(matches!(receiver.try_recv().unwrap(), KeyEvent::Pressed(Key::Right)));
        assert_eq!(checks, 2);
        assert_eq!(app.view.proof_navigation_scene(), scene);
        assert_eq!(app.view.proof_game_theme_inputs()[0].focus.item, selected);
        assert!(!app.view.should_draw());
    }
    let started = std::time::Instant::now();
    while !app.view.prewarm_navigation_step(&mut work, 1).unwrap() {
        assert_eq!(app.view.proof_navigation_scene(), scene);
        assert_eq!(app.view.proof_game_theme_inputs()[0].focus.item, selected);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn artwork_precedes_incomplete_shadows_after_selection_restarts() {
    let (_reset, mut app) = launcher_fixture();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../themes/compact-plus").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    let handle = common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&root).unwrap(), &bank, 902,
    ).unwrap();
    app.res.insert(Some(handle));
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let mut work = None;
    let (tx, _rx) = mpsc::channel(100);
    for selection in 0..3 {
        assert!(!app.view.prewarm_navigation_step(&mut work, 1).unwrap());
        assert_eq!(app.view.navigation_warm_kind(&work), "artwork");
        let started = std::time::Instant::now();
        let mut steps = 0;
        while app.view.navigation_warm_kind(&work) == "artwork" {
            assert!(!app.view.prewarm_navigation_step(&mut work, 1).unwrap());
            steps += 1;
            assert!(steps < 100, "artwork stalled behind incomplete shadows");
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(app.view.proof_detail_shadow_warming(&work));
        if selection < 2 {
            let before = app.view.proof_game_theme_inputs()[0].focus.item.clone();
            app.view.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new())
                .await.unwrap();
            app.present_frame(false, &mut overlay).await.unwrap();
            assert_ne!(app.view.proof_game_theme_inputs()[0].focus.item, before);
            // The event loop drops navigation plans on input.
            work = None;
        }
    }
}

fn recents_fixture() -> (Reset, AlliumLauncher<DefaultPlatform>) {
    use crate::view::{Recents, Games, Apps, Settings};
    let (reset, mut app) = launcher_fixture();
    let database = Database::in_memory().unwrap();
    for i in 0..12 {
        let game: common::database::NewGame = Game::new(
            ALLIUM_GAMES_DIR.join(format!("dl17b-warm-input-fixture/Warm {i:02}.chd")),
        ).into();
        database.update_games(&[game.clone()]).unwrap();
        database.increment_play_count(&game).unwrap();
    }
    app.res.insert(database);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes/compact-plus").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&common::constants::ALLIUM_FONTS_DIR).unwrap();
    app.res.insert(Some(common::theme::ThemeHandle::prepare(
        common::theme::runtime::ThemeSnapshot::load(&root).unwrap(), &bank, 992,
    ).unwrap()));
    let rect = common::geom::Rect::new(0, 54, 640, 376);
    let views = (
        Recents::load_or_new(rect, app.res.clone(), None).unwrap(),
        Games::load_or_new(rect, app.res.clone(), None).unwrap(),
        Apps::load_or_new(rect, app.res.clone(), None).unwrap(),
        Settings::new(rect, app.res.clone(), Default::default()).unwrap(),
    );
    app.view = App::new(common::geom::Rect::new(0, 0, 640, 480), app.res.clone(), views, 0,
        app.platform.battery().unwrap()).unwrap();
    (reset, app)
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn adopted_graveyard_drains_on_non_warmable_tabs_only_after_input_is_idle() {
    for tab in [2, 3] {
        let (_reset, mut app) = recents_fixture();
        let mut overlay = false;
        app.present_frame(false, &mut overlay).await.unwrap();
        let mut work = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !app.view.prewarm_navigation_step(&mut work, 1).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let (tx, _rx) = mpsc::channel(100);
        app.view.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.present_frame(false, &mut overlay).await.unwrap();
        assert!(app.view.proof_retired_neighbors(), "the press must adopt a neighbor");
        for _ in 0..tab {
            app.view.handle_key_event(KeyEvent::Pressed(Key::R), tx.clone(), &mut VecDeque::new()).await.unwrap();
        }
        app.present_frame(false, &mut overlay).await.unwrap();
        assert!(!app.navigation_warm_allowed(), "tab {tab} allows navigation warming");
        assert!(app.view.proof_retired_neighbors(), "tab switch destroyed retired data on the press path");
        let (sender, receiver) = mpsc::unbounded_channel();
        INPUT.with(|input| *input.borrow_mut() = Some(receiver));
        sender.send(KeyEvent::Pressed(Key::Down)).unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), app.run_event_loop()).await.is_err());
        assert!(app.view.proof_retired_neighbors(), "held input must defer destruction");
        sender.send(KeyEvent::Released(Key::Down)).unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(1200), app.run_event_loop()).await.is_err());
        assert!(!app.view.proof_retired_neighbors(), "idle on non-warmable tab {tab} retained the graveyard");
    }
}

#[cfg(feature = "perf-bench")]
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn fpdiag_page_repair_preserves_neighbors_but_remap_rejects_them() {
    let (_reset, mut app) = recents_fixture();
    app.display.presenter();
    let path = std::env::temp_dir().join(format!("allium-fpdiag-pages-{}.log", std::process::id()));
    common::perf_trace::start_probe(path.to_str().unwrap());
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let (tx, _rx) = mpsc::channel(100);
    for (step, repair_before_press, remap) in [(0, true, false), (1, false, false), (2, false, true)] {
        let mut work = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !app.view.prewarm_navigation_step(&mut work, 1).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(common::perf_trace::probe_counts()["measurement"]["neighbor_build"], "built");
        let base = app.proof_frame();
        app.display.presenter().set_page((step + 1) % 2);
        assert!(app.display.needs_present().unwrap());
        if repair_before_press {
            app.present_frame(false, &mut overlay).await.unwrap();
            assert_eq!(app.proof_frame().data(), base.data(), "repair changed logical pixels");
            app.display.presenter().assert_visible();
        }
        if remap { app.display.presenter().replace_backing(); }
        { let _input = common::perf_trace::input(KeyEvent::Pressed(Key::Down)); }
        app.view.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut VecDeque::new()).await.unwrap();
        app.present_frame(false, &mut overlay).await.unwrap();
        let trace = common::perf_trace::probe_counts();
        assert_eq!(trace["neighbor_hits"], if remap { 0 } else { 1 }, "{trace}");
        assert_eq!(trace["neighbor_pointer_adopts"], 0, "mock display cannot adopt");
        assert_eq!(trace["neighbor_damage_copies"], if remap { 0 } else { 1 });
        assert_eq!(trace["measurement"]["neighbor_adopt"], if remap { "no_candidate" } else { "hit" });
        if remap {
            assert_eq!(trace["measurement"]["neighbor_build"], "invalidated:display_contents_or_layout");
        }
        assert_eq!(app.proof_frame().data(), app.view.proof_fresh_navigation_pixels());
        app.display.presenter().assert_visible();
    }
    common::perf_trace::stop_probe();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn compose_prepared_recents_skips_styles_until_a_toast_consumes_them() {
    let (_reset, mut app) = recents_fixture();
    let mut overlay = false;
    app.present_frame(false, &mut overlay).await.unwrap();
    let mut work = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !app.view.prewarm_navigation_step(&mut work, 1).unwrap() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let (tx, _rx) = mpsc::channel(100);
    app.view.handle_key_event(KeyEvent::Pressed(Key::Down), tx, &mut VecDeque::new()).await.unwrap();
    let builds = crate::theme_adapter::DRAWING_STYLE_BUILDS.with(std::cell::Cell::get);
    app.present_frame(false, &mut overlay).await.unwrap();
    assert!(app.view.proof_retired_neighbors(), "the press must adopt a neighbor");
    assert_eq!(crate::theme_adapter::DRAWING_STYLE_BUILDS.with(std::cell::Cell::get), builds,
        "prepared adoption must not construct a drawing stylesheet");
    assert!(!overlay);
    #[cfg(feature = "perf-bench")]
    assert_eq!(app.proof_frame().data(), app.view.proof_fresh_navigation_pixels());

    app.toast = Some(Toast::new("compose style consumer".into(), None));
    app.present_frame(false, &mut overlay).await.unwrap();
    assert_eq!(crate::theme_adapter::DRAWING_STYLE_BUILDS.with(std::cell::Cell::get), builds + 1);
    assert!(overlay);

    app.toast = None;
    app.res.insert(None::<common::theme::ThemeHandle>);
    app.view.set_should_draw();
    app.present_frame(false, &mut overlay).await.unwrap();
    assert_eq!(crate::theme_adapter::DRAWING_STYLE_BUILDS.with(std::cell::Cell::get), builds + 2,
        "legacy drawing must still construct its stylesheet");
}
