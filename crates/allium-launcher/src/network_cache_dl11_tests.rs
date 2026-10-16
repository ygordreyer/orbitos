fn dl11_download_fixture(label: &str, bytes: usize) -> Fixture {
    let mut f = Fixture::new(label, false);
    fs::remove_dir_all(f.root.join(&f.game.cache_path)).unwrap();
    for payload in &mut f.game.files {
        dl11_set_payload_size(payload, bytes);
    }
    f
}
fn dl11_set_payload_size(payload: &mut Payload, bytes: usize) {
    payload.bytes = bytes as u64;
    let buffer = [b'a'; WINDOW];
    let mut hash = Sha256::new();
    for chunk in (0..bytes).step_by(WINDOW) {
        hash.update(&buffer[..(bytes - chunk).min(WINDOW)]);
    }
    payload.sha256 = digest_hex(hash.finalize().as_slice());
}
fn dl11_run(f: &Fixture, cache: &Cache) -> (Result<CacheState>, Vec<Progress>) {
    let (tx, rx) = mpsc::sync_channel(16);
    let collector = thread::spawn(move || rx.into_iter().collect());
    let result = cache.run(
        &f.game,
        &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
        &Cancellation::default(),
        &FilesystemSpace,
        &tx,
    );
    drop(tx);
    (result, collector.join().unwrap())
}
#[test]
fn dl11_streamed_download_never_full_rereads() {
    let f = dl11_download_fixture("dl11-streamed", 1024 * 1024);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    dl11_run(&f, &cache).0.unwrap();
    assert!(cache.has_index(&f.game).unwrap());
    assert_eq!(cache.fs.test_payload_read_bytes(), 0);
    assert_eq!(
        cache
            .installed_metadata(&f.game, &Cancellation::default())
            .unwrap()
            .len(),
        3
    );
}
#[test]
fn dl11_streamed_hash_mismatch_fails_closed() {
    let f = dl11_download_fixture("dl11-mismatch", 1024 * 1024);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'z'; 3]);
    let error = dl11_run(&f, &cache).0.unwrap_err().to_string();
    assert!(error.contains("SHA/length mismatch"), "{error}");
    assert!(!cache.has_index(&f.game).unwrap());
    assert!(!f.root.join(&f.game.cache_path).exists());
    let journal = cache.journal(&f.game).unwrap();
    assert_eq!(journal.phase, "downloading");
    assert_eq!(
        fs::metadata(cache.payload(&f.game, 0)).unwrap().len(),
        f.game.files[0].bytes
    );
    assert_eq!(cache.fs.test_payload_read_bytes(), 0);
}
#[test]
fn dl11_resumed_partial_is_hashed_once_then_continues() {
    let f = dl11_download_fixture("dl11-resumed", 1024 * 1024);
    let cache = f.cache();
    cache.prepare(&f.game).unwrap();
    let mut journal = cache.journal(&f.game).unwrap();
    let prefix = vec![b'a'; 123456];
    fs::create_dir_all(cache.payload(&f.game, 0).parent().unwrap()).unwrap();
    fs::write(cache.payload(&f.game, 0), &prefix).unwrap();
    journal.partials.push(Partial {
        id: f.game.files[0].id.clone(),
        bytes: prefix.len() as u64,
        prefix_sha256: digest_hex(Sha256::digest(&prefix).as_slice()),
    });
    cache.save_journal(&journal).unwrap();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    let (result, progress) = dl11_run(&f, &cache);
    result.unwrap();
    assert!(cache.has_index(&f.game).unwrap());
    assert_eq!(
        cache.journal(&f.game).unwrap().partials[0].prefix_sha256,
        f.game.files[0].sha256
    );
    assert_eq!(cache.fs.test_payload_read_bytes(), prefix.len() as u64);
    assert!(progress.iter().any(|p| p.phase == "verifying"
        && p.bytes_done == prefix.len() as u64
        && p.bytes_total == prefix.len() as u64));
}
#[test]
fn dl11_full_verify_reports_hashed_bytes() {
    let f = sampled_payload_fixture("dl11-progress");
    let cache = f.cache();
    cache
        .adopt_existing(&f.game, &Cancellation::default())
        .unwrap();
    change_mtime(&f.root.join(&f.game.targets().unwrap()[1]));
    let (result, progress) = dl11_run(&f, &cache);
    result.unwrap();
    assert!(progress.iter().any(|p| p.phase == "verifying"
        && p.bytes_total == 1024 * 1024
        && p.bytes_done == p.bytes_total));
}
#[test]
fn dl11_host_last_byte_to_ready() {
    if std::env::var_os("DL11_TIMING").is_none() {
        return;
    }
    let mut f = dl11_download_fixture("dl11-timing", 112 * 1024 * 1024 / 3);
    dl11_set_payload_size(&mut f.game.files[2], 112 * 1024 * 1024 / 3 + 1);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    dl11_run(&f, &cache).0.unwrap();
    let elapsed = crate::network_transfer::test_last_byte().unwrap().elapsed();
    println!(
        "DL11_HOST_TIMING {{\"bytes\":{},\"last_byte_to_ready_ms\":{},\"full_read_bytes\":{}}}",
        f.game.files.iter().map(|f| f.bytes).sum::<u64>(),
        elapsed.as_secs_f64() * 1000.0,
        cache.fs.test_payload_read_bytes()
    );
}

#[test]
fn dl12_fresh_download_reports_verification_before_sync_and_during_sync_wait() {
    let f = dl11_download_fixture("dl12-verify-events", 1024 * 1024);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    let mut cancel = Cancellation::default();
    let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::sync_channel::<Progress>(32);
    let received = updates.clone();
    let collector = thread::spawn(move || {
        for p in rx {
            received.lock().unwrap().push(p);
        }
    });
    let before_sync = updates.clone();
    cancel.test_sync_hook(move || {
        thread::sleep(std::time::Duration::from_millis(230));
        assert!(
            before_sync
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.phase == "verifying"
                    && p.bytes_done == 0
                    && p.bytes_total == SAMPLE_TOTAL)
                .count()
                >= 2,
            "verification must be visible and refreshed while sync is blocked"
        );
    });
    cache
        .run(
            &f.game,
            &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
            &cancel,
            &FilesystemSpace,
            &tx,
        )
        .unwrap();
    drop(tx);
    collector.join().unwrap();
    let events = cancel.test_verify_events();
    let first = |name| events.iter().position(|e| *e == name).unwrap();
    assert!(first("verify_started") < first("verify_sync_begin"));
    assert!(first("verify_sync_begin") < first("verify_progress"));
    assert!(first("verify_progress") < first("verify_sync_end"));
    assert!(first("verify_sync_end") < first("verify_readback_begin"));
    assert!(first("verify_readback_begin") < first("verify_readback_end"));
    assert!(first("verify_readback_end") < first("verify_finished"));
    assert!(
        updates
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.phase == "verifying"
                && p.bytes_total == SAMPLE_TOTAL
                && p.bytes_done == SAMPLE_TOTAL)
    );
    assert!(cache.has_index(&f.game).unwrap());
    let results = cancel.test_verify_results();
    let finished: Vec<_> = results
        .iter()
        .filter(|(event, _)| *event == "verify_finished")
        .collect();
    assert_eq!(finished.len(), 3);
    assert!(
        finished.iter().all(|(_, cause)| *cause == "complete"),
        "{results:?}"
    );
}

#[test]
fn dl12_cancelled_final_sync_preserves_partial_without_ready() {
    let f = dl11_download_fixture("dl12-sync-cancel", 1024 * 1024);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    let mut cancel = Cancellation::default();
    let trigger = cancel.clone();
    cancel.test_sync_hook(move || trigger.cancel());
    let (tx, _rx) = mpsc::sync_channel(64);
    let result = cache.run(
        &f.game,
        &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
        &cancel,
        &FilesystemSpace,
        &tx,
    );
    assert!(result.unwrap_err().to_string().contains("Cancelled"));
    assert!(!cache.has_index(&f.game).unwrap());
    assert!(!f.root.join(&f.game.cache_path).exists());
    assert_eq!(
        fs::metadata(cache.payload(&f.game, 0)).unwrap().len(),
        f.game.files[0].bytes
    );
}

#[test]
fn dl12_streamed_sample_readback_mismatch_fails_closed() {
    let f = dl11_download_fixture("dl12-readback-mismatch", 1024 * 1024);
    let cache = f.cache();
    crate::network_transfer::test_downloads(vec![b'a'; 3]);
    let path = cache.payload(&f.game, 0);
    let mut cancel = Cancellation::default();
    cancel.test_sync_hook(move || {
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"z").unwrap();
        file.sync_all().unwrap();
    });
    let (tx, _rx) = mpsc::sync_channel(64);
    let result = cache.run(
        &f.game,
        &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
        &cancel,
        &FilesystemSpace,
        &tx,
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("read-back mismatch")
    );
    let results = cancel.test_verify_results();
    assert!(results.iter().any(|(event, _)| *event == "verify_finished"));
    assert!(
        !results.contains(&("verify_finished", "complete")),
        "{results:?}"
    );
    assert!(!cache.has_index(&f.game).unwrap());
    assert!(!f.root.join(&f.game.cache_path).exists());
}

#[test]
fn dl13_verification_transition_survives_saturated_download_queue() {
    let f = Fixture::new("dl13-saturated-progress", false);
    let (tx, rx) = mpsc::sync_channel(16);
    for _ in 0..16 {
        tx.send(Progress {
            game_id: f.game.id.clone(),
            phase: "downloading",
            bytes_done: 100,
            bytes_total: 100,
        })
        .unwrap();
    }
    let cancel = verification_reporting(&Cancellation::default(), &f.game, &tx);
    drop(tx);
    let (finished, wait) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut verification = cancel.verification(SAMPLE_TOTAL);
        verification.advance(SAMPLE_TOTAL);
        drop(verification);
        finished.send(cancel.test_verify_events()).unwrap();
    });
    let early_finish = wait
        .recv_timeout(std::time::Duration::from_millis(200))
        .ok();
    for _ in 0..16 {
        assert_eq!(rx.recv().unwrap().phase, "downloading");
    }
    let transition = rx.recv_timeout(std::time::Duration::from_secs(2));
    let events = early_finish.unwrap_or_else(|| {
        wait.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
    });
    worker.join().unwrap();
    let transition = transition.expect("verify_started must survive a saturated queue");
    assert_eq!(transition.phase, "verifying");
    assert_eq!(
        (transition.bytes_done, transition.bytes_total),
        (0, SAMPLE_TOTAL)
    );
    let first = |event| events.iter().position(|value| *value == event).unwrap();
    assert!(first("verify_started") < first("verify_finished"));
}

#[test]
fn dl13_verification_transition_returns_when_receiver_is_gone() {
    let f = Fixture::new("dl13-disconnected-progress", false);
    let (tx, rx) = mpsc::sync_channel(16);
    drop(rx);
    let cancel = verification_reporting(&Cancellation::default(), &f.game, &tx);
    let (finished, wait) = mpsc::channel();
    let worker = thread::spawn(move || {
        drop(cancel.verification(SAMPLE_TOTAL));
        finished.send(()).unwrap();
    });
    wait.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    worker.join().unwrap();
}

#[test]
fn dl14_cancel_saturated_queue_releases_transfer_lock_and_awake() {
    for streamed in [true, false] {
        let f = if streamed {
            dl11_download_fixture("dl14-saturated-stream", 1024 * 1024)
        } else {
            Fixture::new("dl14-saturated-fallback", false)
        };
        let awake = f.root.join("stay_awake");
        let cache =
            Cache::new(CacheConfig::bind(f.root.clone(), Some(awake.clone())).unwrap()).unwrap();
        if !streamed {
            cache
                .adopt_existing(&f.game, &Cancellation::default())
                .unwrap();
            change_mtime(&f.root.join(&f.game.targets().unwrap()[0]));
        }
        let (tx, rx) = mpsc::sync_channel(16);
        for _ in 0..16 {
            tx.send(Progress {
                game_id: f.game.id.clone(),
                phase: "downloading",
                bytes_done: 1,
                bytes_total: 1,
            })
            .unwrap();
        }
        let game = f.game.clone();
        let cancel = Cancellation::default();
        let trigger = cancel.clone();
        let (finished, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            crate::network_transfer::test_downloads(vec![b'a'; 3]);
            let result = cache.run(
                &game,
                &Endpoint::lan("127.0.0.1:9".parse().unwrap()).unwrap(),
                &cancel,
                &FilesystemSpace,
                &tx,
            );
            finished
                .send(result.map(|_| ()).map_err(|e| e.to_string()))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !awake.exists() && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(awake.exists(), "transfer never acquired awake lease");
        let cache = f.cache();
        let lock_path = cache.state().join("active.lock");
        assert!(CacheLock::acquire(&cache.fs, &lock_path).is_err());
        if streamed {
            while !trigger.test_verify_events().contains(&"verify_started")
                && std::time::Instant::now() < deadline
            {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(trigger.test_verify_events().contains(&"verify_started"));
        }
        trigger.cancel();
        let returned = wait.recv_timeout(std::time::Duration::from_secs(2));
        // Unblock the old implementation so a failed assertion never leaves a worker behind.
        drop(rx);
        worker.join().unwrap();
        assert!(
            returned.is_ok(),
            "cancelled transfer blocked on live saturated receiver (streamed={streamed})"
        );
        assert!(returned.unwrap().unwrap_err().contains("Cancelled"));
        assert!(!awake.exists());
        let _lock = CacheLock::acquire(&cache.fs, &lock_path).unwrap();
        if streamed {
            assert!(!cache.has_index(&f.game).unwrap());
            assert!(!f.root.join(&f.game.cache_path).exists());
        }
    }
}

fn dl14_assert_rejected_verification(cancel: &Cancellation) {
    let results = cancel.test_verify_results();
    assert_eq!(
        results
            .iter()
            .filter(|(event, _)| *event == "verify_finished")
            .map(|(_, cause)| *cause)
            .collect::<Vec<_>>(),
        vec!["interrupted"],
        "{results:?}"
    );
}

#[test]
fn dl14_same_length_wrong_sha_install_reports_interrupted() {
    let f = dl11_download_fixture("dl14-wrong-install", 4);
    let cache = f.cache();
    cache.prepare(&f.game).unwrap();
    let mut journal = cache.journal(&f.game).unwrap();
    fs::create_dir_all(cache.job(&f.game).join("payloads")).unwrap();
    for i in 0..3 {
        fs::write(cache.payload(&f.game, i), b"zzzz").unwrap();
    }
    let cancel = Cancellation::default();
    let lock = CacheLock::acquire(&cache.fs, &cache.state().join("active.lock")).unwrap();
    let error = cache
        .install(&mut journal, &cancel, &FilesystemSpace, &lock)
        .unwrap_err();
    assert!(
        error.to_string().contains("SHA/length mismatch"),
        "{error:#}"
    );
    dl14_assert_rejected_verification(&cancel);
    assert!(!cache.has_index(&f.game).unwrap());
    assert!(!f.root.join(&f.game.cache_path).exists());
    assert_eq!(fs::read(cache.payload(&f.game, 0)).unwrap(), b"zzzz");
}

#[test]
fn dl14_same_length_wrong_sha_member_reports_interrupted() {
    let f = Fixture::new("dl14-wrong-member", false);
    let cache = f.cache();
    let path = f.root.join(&f.game.targets().unwrap()[0]);
    fs::write(&path, b"zzzz").unwrap();
    let cancel = Cancellation::default();
    let error = cache
        .verify_member(&f.game, 0, &path, &cancel)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("SHA/length mismatch"),
        "{error:#}"
    );
    dl14_assert_rejected_verification(&cancel);
    assert!(!cache.has_index(&f.game).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"zzzz");
}

#[test]
fn dl14_same_length_wrong_sha_unload_reports_interrupted() {
    let f = Fixture::new("dl14-wrong-unload", false);
    let cache = f.cache();
    cache
        .adopt_existing(&f.game, &Cancellation::default())
        .unwrap();
    cache
        .ensure_unload_intent(
            &f.game,
            &cache.unload_index(&f.game),
            &Cancellation::default(),
        )
        .unwrap();
    let before_index = fs::read(cache.index(&f.game)).unwrap();
    let before_intent = fs::read(cache.unload_index(&f.game)).unwrap();
    let targets = f.game.targets().unwrap();
    fs::write(f.root.join(&targets[0]), b"zzzz").unwrap();
    let cancel = Cancellation::default();
    let error = cache.unload_registered(&f.game, &cancel).unwrap_err();
    assert!(
        error.to_string().contains("Changed unload payload"),
        "{error:#}"
    );
    dl14_assert_rejected_verification(&cancel);
    assert_eq!(fs::read(cache.index(&f.game)).unwrap(), before_index);
    assert_eq!(
        fs::read(cache.unload_index(&f.game)).unwrap(),
        before_intent
    );
    for (i, path) in targets.iter().enumerate() {
        assert_eq!(
            fs::read(f.root.join(path)).unwrap(),
            if i == 0 { b"zzzz" } else { b"data" }
        );
    }
}
