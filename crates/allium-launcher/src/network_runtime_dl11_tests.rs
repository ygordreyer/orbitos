#[test]
#[serial_test::serial(orbit_fixture)]
fn dl11_verification_progress_requests_a_frame() {
    let f = StartupFixture::new("dl11-frame-progress");
    f.configured();
    initialize(f.root.clone()).unwrap();
    settle();
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
        let mut duplicated = (*r.catalog).clone();
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

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_absent_profile_refresh_history_never_queries_database() {
    let f = StartupFixture::new("boot-no-history");
    initialize(f.root.clone()).unwrap();
    // Default has no connection. Any database query would panic.
    refresh_history(&common::database::Database::default()).unwrap();
    assert!(!active());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_unknown_status_is_asynchronous_and_does_not_authorize_launch() {
    let f = StartupFixture::new("boot-unknown");
    f.configured();
    std::fs::remove_file(
        f.root
            .join(".allium/state/network-library/installed")
            .join(format!("{}.json", f.game.id)),
    )
    .unwrap();
    initialize(f.root.clone()).unwrap();
    let path = f.root.join(&f.game.game_path);
    assert_eq!(badge(&path).as_deref(), Some("Unknown"));
    assert!(active(), "status completion must wake the event loop");
    assert!(guard_launch(&path).is_err());
    settle();
    assert_eq!(badge(&path).as_deref(), Some("NAS"));
    assert!(guard_launch(&path).is_err());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_late_inventory_cannot_overwrite_foreground_status() {
    let f = StartupFixture::new("boot-inventory-order");
    f.configured();
    initialize(f.root.clone()).unwrap();
    settle();
    let (tx, result) = mpsc::sync_channel(1);
    with((), |r| {
        r.inventory = Some(Inventory {
            result,
            cancel: Cancellation::default(),
        });
        r.status.insert(f.game.id.clone(), "Downloading".into());
    });
    tx.send(HashMap::from([(f.game.id.clone(), "NAS".into())]))
        .unwrap();
    poll();
    assert_eq!(
        badge(&f.root.join(&f.game.game_path)).as_deref(),
        Some("Downloading")
    );
    assert!(!active());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_missing_inventory_has_no_per_game_cache_probes() {
    let f = StartupFixture::new("boot-probe-count");
    f.configured();
    let state = f.root.join(".allium/state/network-library");
    std::fs::remove_file(state.join("installed").join(format!("{}.json", f.game.id))).unwrap();
    let mut catalog: Catalog =
        serde_json::from_slice(&std::fs::read(state.join("catalog.json")).unwrap()).unwrap();
    catalog.games = (0..512)
        .map(|at| {
            let mut game = f.game.clone();
            game.id = format!("missing-{at}");
            game
        })
        .collect();
    let fs = crate::network_fs::AnchoredFs::new(&f.root).unwrap();
    let cache = Cache::new(CacheConfig::bind(f.root.clone(), None).unwrap()).unwrap();
    let before = crate::network_fs::test_exists_probes();
    let status = validate_installed(&fs, &state, &cache, &catalog).unwrap();
    assert_eq!(
        crate::network_fs::test_exists_probes() - before,
        1,
        "only the installed directory may be probed, with zero per-game probes"
    );
    assert!(status.is_empty());
    let before = crate::network_fs::test_exists_probes();
    for game in &catalog.games {
        assert_eq!(cached_status(&cache, game).unwrap(), "NAS");
    }
    assert_eq!(crate::network_fs::test_exists_probes() - before, 512 * 5);
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_history_only_records_catalog_paths() {
    let f = StartupFixture::new("boot-history-subset");
    f.configured();
    initialize(f.root.clone()).unwrap();
    let database = common::database::Database::in_memory().unwrap();
    let path = f.root.join(&f.game.game_path);
    for path in [&path, &f.root.join("Roms/PS/Unrelated.chd")] {
        database
            .increment_play_count(&common::database::NewGame {
                name: "Fixture".into(),
                path: path.clone(),
                image: None,
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: vec![],
                favorite: false,
            })
            .unwrap();
    }
    refresh_history(&database).unwrap();
    let expected = database.select_games(&[&path]).unwrap()[0]
        .as_ref()
        .unwrap()
        .last_played;
    with((), |r| {
        assert_eq!(
            r.last_played,
            HashMap::from([(f.game.id.clone(), expected)])
        )
    });
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn boot_history_skips_malformed_catalog_rows_and_keeps_valid_history() {
    let f = StartupFixture::new("boot-history-malformed");
    f.configured();
    initialize(f.root.clone()).unwrap();
    let database = common::database::Database::new().unwrap();
    let mut paths = vec![f.root.join(&f.game.game_path)];
    with((), |r| {
        let mut catalog = (*r.catalog).clone();
        for suffix in ["bad-date", "bad-rating"] {
            let mut game = f.game.clone();
            game.id = suffix.into();
            game.game_path = format!("Roms/PS/{suffix}.chd");
            game.cache_path = game.game_path.clone();
            game.launch_path = game.game_path.clone();
            paths.push(f.root.join(&game.game_path));
            catalog.games.push(game);
        }
        r.catalog = Arc::new(catalog);
        r.path_index = None;
        r.last_played.insert("stale".into(), 999);
    });
    for path in &paths {
        database
            .increment_play_count(&common::database::NewGame {
                name: "History fixture".into(),
                path: path.clone(),
                image: None,
                core: None,
                rating: None,
                release_date: None,
                developer: None,
                publisher: None,
                genres: vec![],
                favorite: false,
            })
            .unwrap();
    }
    let expected = database
        .select_game(&paths[0])
        .unwrap()
        .unwrap()
        .last_played;
    // Database exposes no raw connection API; corrupt the isolated host fixture
    // through a second SQLite connection without adding a production API.
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg("import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.execute(\"UPDATE games SET release_date = 'invalid' WHERE path = ?\", (sys.argv[2],)); c.execute('UPDATE games SET rating = 256 WHERE path = ?', (sys.argv[3],)); c.commit()")
        .arg(common::constants::ALLIUM_DATABASE.as_path())
        .arg(&paths[1])
        .arg(&paths[2])
        .status().unwrap();
    assert!(status.success());
    assert!(database.select_game(&paths[1]).is_err());
    assert!(database.select_game(&paths[2]).is_err());
    refresh_history(&database).unwrap();
    with((), |r| {
        assert_eq!(
            r.last_played,
            HashMap::from([(f.game.id.clone(), expected)])
        )
    });
    for path in paths {
        database.delete_game(&path).unwrap();
    }
}
