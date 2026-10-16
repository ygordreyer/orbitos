use super::*;
use std::time::Duration;
use tiny_skia::Pixmap;

pub(crate) async fn frame(
    app: &mut AlliumLauncher<DefaultPlatform>,
    overlay: &mut bool,
) -> Vec<Pixmap> {
    let dirty = crate::network_runtime::poll();
    if dirty {
        app.view.set_should_draw();
    }
    app.view.update(Duration::ZERO);
    app.present_frame(dirty, overlay).await.unwrap();
    app.display.take_presented_frames()
}

pub(crate) async fn launcher() -> (AlliumLauncher<DefaultPlatform>, Pixmap, bool) {
    let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
    crate::theme_adapter::choose(&app.res, Some("orbit")).unwrap();
    app.view.proof_select_tab(2);
    app.display.record_presentations();
    app.display.clear(Color::new(123, 45, 67)).unwrap();
    app.display.save().unwrap();
    app.view.set_should_draw();
    let mut overlay = false;
    let frames = frame(&mut app, &mut overlay).await;
    assert_eq!(frames.len(), 1);
    (app, frames.into_iter().next().unwrap(), overlay)
}

pub(crate) fn assert_frames(frames: &[Pixmap], expected: &Pixmap, stage: &str) {
    for (index, actual) in frames.iter().enumerate() {
        let differences = actual
            .data()
            .iter()
            .zip(expected.data())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differences, 0,
            "{stage}, presented frame {index} must contain the complete launcher composition"
        );
    }
}

#[tokio::test]
#[serial_test::serial(orbit_fixture)]
async fn dl15b_real_toast_show_idle_and_expiry_present_launcher() {
    let (mut app, baseline, mut overlay) = launcher().await;
    let text = "Toast expiry regression".to_string();
    app.handle_command(Command::Toast(
        text.clone(),
        Some(Duration::from_millis(100)),
    ))
    .await
    .unwrap();
    let mut reference = DefaultPlatform::new().unwrap().display().unwrap();
    reference
        .pixmap_mut()
        .data_mut()
        .copy_from_slice(baseline.data());
    let styles = crate::theme_adapter::drawing_style(
        &app.res.get::<Stylesheet>(),
        crate::theme_adapter::snapshot(&app.res).as_ref(),
    );
    Toast::new(text, None)
        .draw(&mut reference, &styles)
        .unwrap();
    let expected = reference.pixmap().to_owned();
    assert!(expected.data() != baseline.data());
    for _ in 0..3 {
        let frames = frame(&mut app, &mut overlay).await;
        assert_eq!(frames.len(), 1);
        assert_frames(&frames, &expected, "toast show/idle");
    }
    tokio::time::sleep(Duration::from_millis(110)).await;
    let frames = frame(&mut app, &mut overlay).await;
    assert!(app.toast.is_none());
    assert_eq!(frames.len(), 1);
    assert_frames(&frames, &baseline, "first frame after real toast expiry");
    for _ in 0..3 {
        assert_frames(
            &frame(&mut app, &mut overlay).await,
            &baseline,
            "idle after expiry",
        );
    }
}

#[tokio::test]
#[serial_test::serial(orbit_fixture)]
async fn dl15b_network_overlay_end_presents_launcher() {
    let (mut app, baseline, mut overlay) = launcher().await;
    let missing = ALLIUM_SD_ROOT.join("dl15b-absent-network-profile-root");
    assert!(!missing.exists());
    assert!(crate::network_runtime::initialize(missing).is_err());
    let mut reference = DefaultPlatform::new().unwrap().display().unwrap();
    reference
        .pixmap_mut()
        .data_mut()
        .copy_from_slice(baseline.data());
    let styles = crate::theme_adapter::drawing_style(
        &app.res.get::<Stylesheet>(),
        crate::theme_adapter::snapshot(&app.res).as_ref(),
    );
    assert!(crate::network_runtime::draw(&mut reference, &styles));
    let expected = reference.pixmap().to_owned();
    assert!(expected.data() != baseline.data());
    for _ in 0..3 {
        let frames = frame(&mut app, &mut overlay).await;
        assert_frames(&frames, &expected, "real network startup overlay show/idle");
    }
    assert!(crate::network_runtime::event(KeyEvent::Pressed(Key::B)).unwrap());
    let frames = frame(&mut app, &mut overlay).await;
    assert_eq!(frames.len(), 1);
    assert_frames(&frames, &baseline, "first frame after network overlay ends");
    for _ in 0..3 {
        assert_frames(
            &frame(&mut app, &mut overlay).await,
            &baseline,
            "idle after overlay ends",
        );
    }
    crate::network_runtime::initialize(ALLIUM_SD_ROOT.to_path_buf()).unwrap();
}

pub(crate) fn network_reference(
    app: &AlliumLauncher<DefaultPlatform>,
    baseline: &Pixmap,
) -> Pixmap {
    let mut reference = DefaultPlatform::new().unwrap().display().unwrap();
    reference
        .pixmap_mut()
        .data_mut()
        .copy_from_slice(baseline.data());
    let styles = crate::theme_adapter::drawing_style(
        &app.res.get::<Stylesheet>(),
        crate::theme_adapter::snapshot(&app.res).as_ref(),
    );
    assert!(crate::network_runtime::draw(&mut reference, &styles));
    reference.pixmap().to_owned()
}

#[tokio::test]
#[serial_test::serial(orbit_fixture)]
async fn dl15b_redraw_and_toast_dismiss_restore_before_presentation() {
    let (mut app, baseline, mut overlay) = launcher().await;
    app.handle_command(Command::Toast("Dismiss toast".into(), None))
        .await
        .unwrap();
    let shown = frame(&mut app, &mut overlay).await;
    assert_eq!(shown.len(), 1);
    assert!(shown[0].data() != baseline.data());
    for command in [Command::DismissToast, Command::Redraw] {
        app.handle_command(command).await.unwrap();
        assert!(
            app.display.take_presented_frames().is_empty(),
            "restore must not flush the saved background"
        );
        let frames = frame(&mut app, &mut overlay).await;
        assert_eq!(frames.len(), 1);
        assert_frames(&frames, &baseline, "redraw/dismiss toast");
    }
}
