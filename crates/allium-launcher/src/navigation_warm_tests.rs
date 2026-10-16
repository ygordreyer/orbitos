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
