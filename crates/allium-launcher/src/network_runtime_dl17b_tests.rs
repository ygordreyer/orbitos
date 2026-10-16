#[test]
#[serial_test::serial(orbit_fixture)]
fn dl17b_every_network_modal_and_pending_launch_blocks_warm() {
    let f = StartupFixture::new("warm-modal-guard");
    f.configured();
    initialize(f.root.clone()).unwrap();
    assert!(!blocks_navigation_warm());
    let game = Game::new(f.root.join(&f.game.game_path));
    for modal in [
        Modal::Working("Downloading".into()),
        Modal::Completed,
        Modal::Error("Error".into()),
        Modal::Storage {
            ids: vec![],
            selected: 0,
        },
        Modal::ConfirmUnload(f.game.id.clone()),
        Modal::ConfirmImport(f.game.id.clone()),
        Modal::Discs {
            picker: DiscPicker::new(crate::discs::Playlist {
                path: game.path.clone(),
                discs: vec![],
                core_compatible: true,
            }),
            game: game.clone(),
        },
    ] {
        with((), |r| r.modal = Some(modal));
        assert!(!active(), "confirmation/completion need no active job");
        assert!(blocks_navigation_warm());
    }
    with((), |r| {
        r.modal = None;
        r.pending = Some(Pending {
            epoch: r.epoch,
            game: game.clone(),
            reset: false,
            disc: None,
        });
    });
    assert!(blocks_navigation_warm());
    with((), |r| {
        r.pending = None;
        r.permit = Some(game.path.clone());
    });
    assert!(blocks_navigation_warm());
    with((), |r| {
        r.permit = None;
        r.actions.push_back(Action::Launch(game, false));
    });
    assert!(blocks_navigation_warm());
    with((), |r| r.actions.clear());
    let (_progress_tx, progress) = mpsc::sync_channel(1);
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
    assert!(blocks_navigation_warm());
    with((), |r| r.job = None);
    assert!(!blocks_navigation_warm());
    COMPLETION_PENDING.with(|pending| pending.set(true));
    assert!(blocks_navigation_warm());
    COMPLETION_PENDING.with(|pending| pending.set(false));
    STARTUP.with(|cell| {
        *cell.borrow_mut() = Startup::Blocked {
            root: f.root.clone(),
            modal: true,
            dirty: true,
        }
    });
    assert!(blocks_navigation_warm());
    startup_event(KeyEvent::Pressed(Key::B));
    assert!(!blocks_navigation_warm());
}
