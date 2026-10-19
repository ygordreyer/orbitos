fn collapsed_network_fixture(label: &str) -> StartupFixture {
    let mut f = StartupFixture::new(label);
    std::fs::remove_file(f.root.join(&f.game.game_path)).unwrap();
    f.game.cache_path = "Roms/PS/Folder Game".into();
    f.game.game_path = "Roms/PS/Folder Game/disc.cue".into();
    f.game.launch_path = f.game.game_path.clone();
    f.game.install_kind = "directory".into();
    std::fs::create_dir_all(f.root.join(&f.game.cache_path)).unwrap();
    f.game.files = [("cue", "disc.cue", "cue", b"FILE \"disc.bin\" BINARY\n".as_slice()),
        ("bin", "disc.bin", "rom", b"fixture content".as_slice())]
        .into_iter().map(|(id, name, role, body)| {
            std::fs::write(f.root.join(&f.game.cache_path).join(name), body).unwrap();
            Payload {
                id: id.into(), relative_path: name.into(), bytes: body.len() as u64,
                sha256: crate::network_transfer::digest_hex(Sha256::digest(body).as_slice()),
                role: role.into(),
                http_route: format!("/v1/files/{}/{}/{id}", f.game.id, f.game.bundle_revision),
            }
        }).collect();
    f
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_installed_network_folder_launches_local_content_and_keeps_history_identity() {
    let name = "network_runtime::tests::collapsed_installed_network_folder_launches_local_content_and_keeps_history_identity";
    if std::env::var("ALLIUM_FOLDER_TEST_CHILD").as_deref() != Ok(name) {
        let config = StartupFixture::new("folder-config");
        let base = config.root.join(".allium");
        std::fs::create_dir_all(base.join("config")).unwrap();
        std::fs::create_dir_all(base.join("state")).unwrap();
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static/.allium/locales"), base.join("locales")).unwrap();
        std::fs::write(base.join("config/consoles.toml"), include_str!("../../../static/.allium/config/consoles.toml")).unwrap();
        std::fs::write(base.join("config/cores.toml"), "[cores.pcsx_rearmed]\nname = 'Fixture'\nretroarch = 'pcsx_rearmed'\n").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("ALLIUM_FOLDER_TEST_CHILD", name)
            .env("ALLIUM_BASE_DIR", base)
            .env("ALLIUM_SD_ROOT", &config.root)
            .status().unwrap();
        assert!(status.success());
        return;
    }
    let f = collapsed_network_fixture("installed-folder");
    f.configured();
    let state = f.root.join(".allium/state/network-library");
    let index = state.join("installed").join(format!("{}.json", f.game.id));
    let mut ready: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    ready.as_object_mut().unwrap().remove("metadata");
    std::fs::write(&index, serde_json::to_vec(&ready).unwrap()).unwrap();
    initialize(f.root.clone()).unwrap();
    settle();
    let folder = f.root.join(&f.game.cache_path);
    let content = f.root.join(&f.game.launch_path);
    let database = common::database::Database::in_memory().unwrap();
    let mut mapper = crate::consoles::ConsoleMapper::new();
    mapper.load_config().unwrap();
    let game = Game::new(folder.clone());
    let ui_thread = std::thread::current().id();
    let awake = f.root.join("verification-awake");
    let worker_awake = awake.clone();
    let jobs = state.join("jobs");
    let worker_jobs = jobs.clone();
    let verified = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_verified = verified.clone();
    with((), |r| {
        r.config.stay_awake = Some(awake.clone());
        let cache = Cache::new(r.config.clone()).unwrap();
        cache.test_space(1_900 * 1024 * 1024, 4096);
        *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
            assert_ne!(std::thread::current().id(), ui_thread);
            assert!(worker_awake.exists());
            assert!(!worker_jobs.exists());
            worker_verified.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
    });
    assert!(guard_launch(&folder).is_err());
    assert!(begin(&game, true).unwrap());
    with((), |r| {
        assert!(r.job.is_some());
        assert_eq!(r.pending.as_ref().unwrap().game.path, folder);
        let progress = r.job.as_ref().unwrap().progress.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(progress.phase, "verifying");
        assert!(progress.bytes_total > 0);
    });
    settle();
    let Some(Action::Launch(ready, true)) = action() else { panic!("verified folder continuation"); };
    assert_eq!(ready.path, folder);
    with((), |r| assert_eq!(r.permit.as_deref(), Some(folder.as_path())));
    assert!(guard_launch(&folder).is_ok());
    assert_eq!(crate::entry::library::launch_path(&ready.path, &mapper).unwrap(), content);
    finish_action();
    frame_presented();
    assert_eq!(verified.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!awake.exists());
    assert!(!jobs.exists());
    let upgraded = std::fs::read(&index).unwrap();
    let ready: serde_json::Value = serde_json::from_slice(&upgraded).unwrap();
    assert_eq!(ready["metadata"].as_array().unwrap().len(), f.game.files.len());
    with((), |r| {
        let cache = Cache::new(r.config.clone()).unwrap();
        *cache.verification_hook.lock().unwrap() = Some(Box::new(|| panic!("fast path rehashed payloads")));
    });
    for indexed in [true, false] {
        with((), |r| r.path_index = if indexed { catalog_path_index(&r.catalog, &r.config.root) } else { None });
        for path in [&folder, &content] {
            assert!(registered(path));
            assert_eq!(badge(path).as_deref(), Some("Cached"));
            assert!(download_id(path).is_none());
            assert!(!has_download(path));
            assert!(download_offer_id(path).is_none());
        }
        let mut entries = Directory::new(f.root.join("Roms/PS"))
            .entries(&database, &mapper, &common::locale::Locale::new("en-US")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), folder);
        let Entry::Game(game) = &mut entries[0] else { panic!("collapsed game"); };
        game.favorite = true;
        let mut search_entries = vec![Entry::Game(game.clone())];
        search(&f.game.display_title, &mut search_entries);
        assert_eq!(search_entries.len(), 1);
        let Entry::Game(found) = &search_entries[0] else { panic!("search game"); };
        assert_eq!(found, game);
        for reset in [false, true] {
            assert!(!begin(game, reset).unwrap());
            assert!(!active());
            assert!(!blocks_navigation_warm());
            assert!(action().is_none());
            assert!(guard_launch(&folder).is_ok());
            let Some(common::command::Command::Exec(command)) = mapper.launch_game(&database, game, reset).unwrap() else { panic!("local Exec"); };
            assert!(command.get_args().any(|arg| arg == content.as_os_str()));
            assert_eq!(game.path, folder);
            assert!(database.select_game(&content).unwrap().is_none());
        }
    }
    assert_eq!(database.select_game(&folder).unwrap().unwrap().play_count, 4);
    refresh_history(&database).unwrap();
    with((), |r| assert!(r.last_played.contains_key(&f.game.id)));
    let cache = Cache::new(CacheConfig::bind(f.root.clone(), None).unwrap()).unwrap();
    std::fs::write(f.root.join(".allium/state/current_game"), serde_json::to_vec(&serde_json::json!({"path": folder})).unwrap()).unwrap();
    assert!(cache.resume_protected(&f.game).unwrap());
    assert_eq!(std::fs::read(&index).unwrap(), upgraded);
    assert!(!state.join("jobs").exists());
    with((), |r| {
        let cache = Cache::new(r.config.clone()).unwrap();
        assert!(cache.verification_hook.lock().unwrap().take().is_some());
    });
    std::fs::write(&content, b"FILE \"evil.bin\" BINARY\n").unwrap();
    assert!(guard_launch(&folder).is_err());
    assert!(begin(&game, false).unwrap());
    settle();
    assert!(action().is_none());
    assert!(guard_launch(&folder).is_err());
    assert_eq!(std::fs::read(&index).unwrap(), upgraded);
    assert!(!jobs.exists());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_network_folder_with_metadata_launches_without_full_verification() {
    let f = collapsed_network_fixture("metadata-folder");
    f.configured();
    initialize(f.root.clone()).unwrap();
    settle();
    let game = Game::new(f.root.join(&f.game.cache_path));
    with((), |r| {
        let cache = Cache::new(r.config.clone()).unwrap();
        *cache.verification_hook.lock().unwrap() = Some(Box::new(|| panic!("metadata fast path rehashed payloads")));
    });
    for indexed in [true, false] {
        with((), |r| r.path_index = if indexed { catalog_path_index(&r.catalog, &r.config.root) } else { None });
        assert!(!begin(&game, false).unwrap());
        assert!(!active());
        assert!(action().is_none());
        with((), |r| assert!(r.permit.is_none()));
        assert!(guard_launch(&game.path).is_ok());
    }
    with((), |r| {
        let cache = Cache::new(r.config.clone()).unwrap();
        assert!(cache.verification_hook.lock().unwrap().is_some());
    });
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_network_folder_below_state_floor_launches_only_verified_content_without_upgrade() {
    for missing_lock in [false, true] {
        let f = collapsed_network_fixture(if missing_lock { "state-floor-no-lock" } else { "state-floor-lock" });
        f.configured();
        let state = f.root.join(".allium/state/network-library");
        let index = state.join("installed").join(format!("{}.json", f.game.id));
        let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
        record.as_object_mut().unwrap().remove("metadata");
        let before = serde_json::to_vec(&record).unwrap();
        std::fs::write(&index, &before).unwrap();
        if missing_lock {
            std::fs::remove_file(state.join("active.lock")).unwrap();
        }
        initialize(f.root.clone()).unwrap();
        settle();
        let verified = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_verified = verified.clone();
        with((), |r| {
            let cache = Cache::new(r.config.clone()).unwrap();
            cache.test_space(crate::network_library::STATE_FLOOR_BYTES, 4096);
            *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
                worker_verified.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        });
        let game = Game::new(f.root.join(&f.game.cache_path));
        assert!(begin(&game, true).unwrap());
        settle();
        let Some(Action::Launch(ready, true)) = action() else { panic!("low-space verified folder continuation"); };
        assert_eq!(ready.path, game.path);
        with((), |r| assert_eq!(r.permit.as_deref(), Some(game.path.as_path())));
        assert!(guard_launch(&game.path).is_ok());
        assert_eq!(verified.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert!(!index.with_extension("new").exists());
        assert!(!state.join("jobs").exists());
        assert_eq!(state.join("active.lock").exists(), !missing_lock);
        finish_action();
        let payload = game.path.join("disc.bin");
        std::fs::write(&payload, b"changed content").unwrap();
        assert!(begin(&game, false).unwrap());
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        std::fs::write(&payload, b"fixture content").unwrap();
        std::fs::write(game.path.join("evil.bin"), b"unexpected member").unwrap();
        assert!(begin(&game, false).unwrap());
        settle();
        assert!(action().is_none());
        assert!(guard_launch(&game.path).is_err());
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert!(!state.join("jobs").exists());
    }
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_cloud_network_folder_below_download_reserve_refuses_data_writes() {
    let f = collapsed_network_fixture("cloud-reserve-floor");
    f.configured();
    std::fs::remove_dir_all(f.root.join(&f.game.cache_path)).unwrap();
    let state = f.root.join(".allium/state/network-library");
    std::fs::remove_file(state.join("installed").join(format!("{}.json", f.game.id))).unwrap();
    initialize(f.root.clone()).unwrap();
    settle();
    with((), |r| Cache::new(r.config.clone()).unwrap().test_space(1_900 * 1024 * 1024, 4096));
    let game = Game::new(f.root.join(&f.game.cache_path));
    assert!(begin(&game, false).unwrap());
    settle();
    assert!(action().is_none());
    assert!(guard_launch(&game.path).is_err());
    with((), |r| {
        assert!(r.permit.is_none());
        let Some(Modal::Error(error)) = &r.modal else { panic!("download reserve error"); };
        assert_eq!(error, "Not enough free space: needs 2.1 GB, 1.9 GB free");
    });
    assert!(!state.join("jobs").exists());
    assert!(!game.path.exists());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_network_folder_changed_stat_verifies_once_without_download() {
    let f = collapsed_network_fixture("changed-stat-folder");
    f.configured();
    let content = f.root.join(&f.game.launch_path);
    std::fs::File::open(&content).unwrap().set_times(std::fs::FileTimes::new().set_modified(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
    )).unwrap();
    initialize(f.root.clone()).unwrap();
    settle();
    let game = Game::new(f.root.join(&f.game.cache_path));
    assert!(guard_launch(&game.path).is_err());
    assert!(begin(&game, false).unwrap());
    settle();
    let Some(Action::Launch(ready, false)) = action() else { panic!("changed stat continuation"); };
    assert_eq!(ready.path, game.path);
    assert!(guard_launch(&ready.path).is_ok());
    finish_action();
    assert!(!begin(&game, false).unwrap());
    assert!(guard_launch(&game.path).is_ok());
    assert!(!f.root.join(".allium/state/network-library/jobs").exists());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_network_folder_verification_can_be_cancelled_without_upgrading_record() {
    let f = collapsed_network_fixture("cancel-verification-folder");
    f.configured();
    let state = f.root.join(".allium/state/network-library");
    let index = state.join("installed").join(format!("{}.json", f.game.id));
    let mut ready: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    ready.as_object_mut().unwrap().remove("metadata");
    let before = serde_json::to_vec(&ready).unwrap();
    std::fs::write(&index, &before).unwrap();
    initialize(f.root.clone()).unwrap();
    settle();
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    with((), |r| {
        let cache = Cache::new(r.config.clone()).unwrap();
        *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        }));
    });
    let game = Game::new(f.root.join(&f.game.cache_path));
    assert!(begin(&game, false).unwrap());
    entered.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(active());
    assert!(guard_launch(&game.path).is_err());
    assert!(event(KeyEvent::Pressed(Key::B)).unwrap());
    release.send(()).unwrap();
    settle();
    assert!(action().is_none());
    assert!(guard_launch(&game.path).is_err());
    assert_eq!(std::fs::read(&index).unwrap(), before);
    assert!(!state.join("jobs").exists());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn collapsed_uncached_network_folder_uses_existing_download_flow() {
    let f = collapsed_network_fixture("missing-folder");
    f.configured();
    std::fs::remove_dir_all(f.root.join(&f.game.cache_path)).unwrap();
    std::fs::remove_file(f.root.join(".allium/state/network-library/installed").join(format!("{}.json", f.game.id))).unwrap();
    initialize(f.root.clone()).unwrap();
    settle();
    let folder = f.root.join(&f.game.cache_path);
    let game = Game::new(folder.clone());
    for indexed in [true, false] {
        with((), |r| r.path_index = if indexed { catalog_path_index(&r.catalog, &r.config.root) } else { None });
        for path in [&folder, &f.root.join(&f.game.game_path)] {
            assert_eq!(badge(path).as_deref(), Some("NAS"));
            assert_eq!(download_id(path), Some(f.game.id.clone()));
            assert!(has_download(path));
            assert_eq!(download_offer_id(path), Some(f.game.id.clone()));
            assert!(guard_launch(path).is_err());
        }
    }
    assert!(begin(&game, false).unwrap());
    with((), |r| {
        assert!(r.job.is_some());
        assert_eq!(r.pending.as_ref().unwrap().game.path, folder);
        r.job.as_ref().unwrap().cancel.cancel();
    });
    settle();
    assert!(action().is_none());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn single_file_network_game_keeps_verified_worker_launch_route() {
    let f = StartupFixture::new("folder-single-file-control");
    f.configured();
    initialize(f.root.clone()).unwrap();
    settle();
    let game = Game::new(f.root.join(&f.game.game_path));
    assert_eq!(badge(&game.path).as_deref(), Some("Cached"));
    assert!(download_id(&game.path).is_none());
    assert!(guard_launch(&game.path).is_err());
    assert!(begin(&game, false).unwrap());
    settle();
    let Some(Action::Launch(ready, false)) = action() else { panic!("verified continuation"); };
    assert_eq!(ready.path, game.path);
    assert!(guard_launch(&ready.path).is_ok());
    finish_action();
    assert!(guard_launch(&ready.path).is_err());
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn nested_collapsed_network_folder_uses_catalog_alias_without_changing_identity() {
    let mut f = collapsed_network_fixture("nested-folder");
    let cache = f.root.join(&f.game.cache_path);
    let folder = cache.join("nested");
    std::fs::create_dir(&folder).unwrap();
    for file in &mut f.game.files {
        std::fs::rename(cache.join(&file.relative_path), folder.join(&file.relative_path)).unwrap();
        file.relative_path = format!("nested/{}", file.relative_path);
    }
    f.game.game_path = format!("{}/nested/disc.cue", f.game.cache_path);
    f.game.launch_path = f.game.game_path.clone();
    f.configured();
    initialize(f.root.clone()).unwrap();
    settle();
    let mapper = crate::consoles::ConsoleMapper::library_fixture();
    let game = Game::new(folder.clone());
    for indexed in [true, false] {
        with((), |r| r.path_index = if indexed { catalog_path_index(&r.catalog, &r.config.root) } else { None });
        for path in [&cache, &folder] {
            assert!(registered(path));
            assert_eq!(badge(path).as_deref(), Some("Cached"));
            assert!(download_id(path).is_none());
            assert!(!has_download(path));
        }
        assert!(!begin(&game, false).unwrap());
        assert!(!active());
        assert!(guard_launch(&folder).is_ok());
        assert_eq!(crate::entry::library::launch_path(&game.path, &mapper).unwrap(), folder.join("disc.cue"));
        assert_eq!(game.path, folder);
    }
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn fix18_old_recents_cue_and_library_folder_verify_legacy_install_below_reserve() {
    for indexed in [true, false] {
        for recents in [true, false] {
            let f = collapsed_network_fixture("fix18-legacy-recents");
            let _runtime = library_catalog_fixture(&f.root, vec![f.game.clone()], true);
            let state = f.root.join(".allium/state/network-library");
            let index = state.join("installed").join(format!("{}.json", f.game.id));
            let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
            record.as_object_mut().unwrap().remove("metadata");
            std::fs::write(&index, serde_json::to_vec(&record).unwrap()).unwrap();
            initialize(f.root.clone()).unwrap();
            settle();
            let folder = f.root.join(&f.game.cache_path);
            let cue = f.root.join(&f.game.game_path);
            let database = common::database::Database::in_memory().unwrap();
            let mut old = Game::new(cue.clone());
            old.name = "Kowai Shashin: Shinrei shashin kitan (Japan)".into();
            database.increment_play_count(&old.into()).unwrap();
            let mapper = crate::consoles::ConsoleMapper::library_fixture();
            let locale = common::locale::Locale::new("en-US");
            use crate::entry::Sort;
            let mut entries = if recents {
                crate::view::recents::RecentsSort::LastPlayed.entries(&database, &mapper, &locale).unwrap()
            } else {
                Directory::new(f.root.join("Roms/PS")).entries(&database, &mapper, &locale).unwrap()
            };
            assert_eq!(entries.len(), 1);
            let Entry::Game(game) = entries.remove(0) else { panic!("game row"); };
            assert_eq!(game.path, if recents { cue.clone() } else { folder.clone() });
            let verified = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = verified.clone();
            with((), |r| {
                r.path_index = if indexed { catalog_path_index(&r.catalog, &r.config.root) } else { None };
                assert_eq!(r.descriptor(&cue).unwrap().id, r.descriptor(&folder).unwrap().id);
                let cache = Cache::new(r.config.clone()).unwrap();
                cache.test_space(1_948_752 * 1024, 4096);
                *cache.verification_hook.lock().unwrap() = Some(Box::new(move || {
                    assert!(!state.join("jobs").exists());
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }));
            });
            assert!(begin(&game, false).unwrap());
            settle();
            with((), |r| assert!(r.modal.is_none(), "unexpected modal for recents={recents}: {:?}",
                r.modal.as_ref().map(|modal| match modal { Modal::Error(error) => error.as_str(), _ => "other" })));
            let Some(Action::Launch(ready, false)) = action() else { panic!("verified continuation for recents={recents}"); };
            assert_eq!(ready.path, game.path);
            assert_eq!(verified.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(guard_launch(&ready.path).is_ok());
            assert!(!f.root.join(".allium/state/network-library/jobs").exists());
            assert_eq!(database.select_game(&cue).unwrap().unwrap().play_count, 1);
            finish_action();
        }
    }
}
