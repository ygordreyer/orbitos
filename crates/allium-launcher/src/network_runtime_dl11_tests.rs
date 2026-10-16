#[test]
#[serial_test::serial(orbit_fixture)]
fn dl11_verification_progress_requests_a_frame() {
    let f = StartupFixture::new("dl11-frame-progress");
    f.configured();
    initialize(f.root.clone()).unwrap();
    poll();
    let (tx, progress) = mpsc::sync_channel(2);
    let (_result_tx, result) = mpsc::sync_channel(1);
    with((), |r| {
        r.job = Some(Job {
            epoch: r.epoch,
            prefill_id: None,
            cancel: Cancellation::default(),
            progress,
            result,
            thread: None,
        })
    });
    for bytes_done in [128, 256] {
        tx.send(Progress {
            game_id: f.game.id.clone(),
            phase: "verifying",
            bytes_done,
            bytes_total: 1024,
        })
        .unwrap();
        assert!(poll());
        with((), |r| {
            assert_eq!(r.progress_fraction, Some(bytes_done as f64 / 1024.0))
        });
        assert!(!poll());
    }
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn dl12_catalog_index_matches_scan_and_rebuilds_for_root_and_catalog() {
    let f = StartupFixture::new("dl12-index");
    f.configured();
    initialize(f.root.clone()).unwrap();
    with((), |r| {
        for game in &r.catalog.games {
            for path in
                std::iter::once(PathBuf::from(&game.game_path)).chain(game.targets().unwrap())
            {
                let path = r.config.root.join(path);
                assert_eq!(
                    r.descriptor(&path).unwrap().id,
                    descriptor_scan(&r.catalog, &r.config.root, &path)
                        .unwrap()
                        .id
                );
            }
        }
        let mut duplicated = r.catalog.clone();
        let mut second = duplicated.games[0].clone();
        second.id = "second".into();
        duplicated.games.push(second);
        let index = catalog_path_index(&duplicated, &r.config.root).unwrap();
        assert_eq!(
            index[&r.config.root.join(&duplicated.games[0].game_path)],
            0
        );
    });
    let old = f.root.join(&f.game.game_path);
    let other = StartupFixture::new("dl12-index-new-root");
    other.configured();
    initialize(other.root.clone()).unwrap();
    assert!(with(None, |r| r.descriptor(&old)).is_none());
    assert!(
        with(None, |r| r
            .descriptor(&other.root.join(&other.game.game_path)))
        .is_some()
    );
    let state = other
        .root
        .join(".allium/state/network-library/catalog.json");
    let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    catalog.games[0].id = "replacement".into();
    for payload in &mut catalog.games[0].files {
        payload.http_route = payload.http_route.replace("startup-fixture", "replacement");
    }
    catalog.games[0].game_path = "Roms/PS/Replacement.chd".into();
    catalog.games[0].cache_path = catalog.games[0].game_path.clone();
    catalog.games[0].launch_path = catalog.games[0].game_path.clone();
    std::fs::write(&state, serde_json::to_vec(&catalog).unwrap()).unwrap();
    initialize(other.root.clone()).unwrap();
    assert!(
        with(None, |r| r
            .descriptor(&other.root.join(&other.game.game_path)))
        .is_none()
    );
    assert!(
        with(None, |r| r
            .descriptor(&other.root.join(&catalog.games[0].game_path)))
        .is_some()
    );
}

#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
#[tokio::test]
#[serial_test::serial(orbit_fixture)]
async fn dl12_successful_flush_precedes_single_dispatch_and_first_wait_cancel_suppresses_dispatch()
{
    use crate::allium_launcher::AlliumLauncher;
    for cancelled in [false, true] {
        let f = StartupFixture::new(if cancelled {
            "dl12-dispatch-cancel"
        } else {
            "dl12-dispatch"
        });
        f.configured();
        let mut app = AlliumLauncher::new(DefaultPlatform::new().unwrap()).unwrap();
        initialize(f.root.clone()).unwrap();
        let (_progress_tx, progress) = mpsc::sync_channel(1);
        let (result_tx, result) = mpsc::sync_channel(1);
        let cancel = Cancellation::default();
        if cancelled {
            cancel.cancel();
        }
        with((), |r| {
            r.pending = Some(Pending {
                epoch: r.epoch,
                game: Game::new(f.root.join(&f.game.game_path)),
                reset: true,
                disc: None,
            });
            r.job = Some(Job {
                epoch: r.epoch,
                prefill_id: None,
                cancel,
                progress,
                result,
                thread: None,
            });
        });
        result_tx.send(Ok(Output::Ready)).unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        LOOP_PROBE.with(|p| {
            *p.borrow_mut() = Some(Box::new(move |event| {
                if event == "resolve" {
                    return true;
                }
                events_tx.send(event).unwrap();
                event == "dispatch"
            }))
        });
        let observed = async {
            let mut events = vec![];
            while let Some(event) = events_rx.recv().await {
                events.push(event);
                if event == "wait" {
                    break;
                }
            }
            events
        };
        let events = tokio::select! {
            result=app.run_event_loop()=>panic!("event loop ended: {result:?}"),
            events=observed=>events,
            _=tokio::time::sleep(std::time::Duration::from_secs(5))=>panic!("first wait not reached"),
        };
        LOOP_PROBE.with(|p| *p.borrow_mut() = None);
        assert_eq!(events.last(), Some(&"wait"));
        assert!(events.contains(&"flush"));
        let dispatches: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| **e == "dispatch")
            .collect();
        assert_eq!(dispatches.len(), usize::from(!cancelled));
        if let Some((at, _)) = dispatches.first() {
            assert!(*at > 0);
            assert_eq!(events[*at - 1], "flush");
        }
        assert!(!COMPLETION_PENDING.with(Cell::get));
        assert!(action().is_none());
    }
}

#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
#[tokio::test]
#[serial_test::serial(orbit_fixture)]
async fn dl15b_runtime_network_overlay_dismissal_presents_launcher() {
    use crate::allium_launcher::present_dl15b_tests::{
        assert_frames, frame, launcher, network_reference,
    };
    let f = StartupFixture::new("dl15b-overlay-dismissal");
    f.configured();
    let (mut app, baseline, mut overlay) = launcher().await;
    initialize(f.root.clone()).unwrap();
    for modal in [
        Modal::Error("Offline presenter regression".into()),
        Modal::Completed,
    ] {
        with((), |runtime| {
            runtime.modal = Some(modal);
            runtime.dirty = true;
        });
        let expected = network_reference(&app, &baseline);
        assert!(expected.data() != baseline.data());
        for index in 0..3 {
            let frames = frame(&mut app, &mut overlay).await;
            if index == 0 {
                assert_eq!(frames.len(), 1);
            }
            assert_frames(&frames, &expected, "runtime network overlay show/idle");
        }
        assert!(event(KeyEvent::Pressed(Key::B)).unwrap());
        let frames = frame(&mut app, &mut overlay).await;
        assert_eq!(frames.len(), 1);
        assert_frames(
            &frames,
            &baseline,
            "first frame after runtime network overlay dismissal",
        );
        for _ in 0..3 {
            assert_frames(
                &frame(&mut app, &mut overlay).await,
                &baseline,
                "idle after runtime network overlay dismissal",
            );
        }
    }
}
