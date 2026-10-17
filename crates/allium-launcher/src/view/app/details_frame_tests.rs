use super::{prepare_theme_frames, NeighborGraveyard};
use common::theme::{
    fonts::FontBank,
    frame::{DetailLine, FrameInputs, Pagination, Row},
    pack::{Binding, ScreenId},
    render::PreparedFrame,
    runtime::ThemeSnapshot,
    ThemeHandle,
};

fn theme(generation: u64) -> ThemeHandle {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    ThemeHandle::prepare(ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, generation).unwrap()
}

fn details(theme: &ThemeHandle) -> FrameInputs {
    let mut input = FrameInputs::new(ScreenId::GameDetail);
    input.text.insert(Binding::SelectedTitle, "AV To é 日本語".into());
    input.status.clock = Some("12:34".into());
    input.status.battery = Some(73);
    input.pagination = Some(Pagination { page: 1, total: 3 });
    input.detail_lines.push(DetailLine { text: "Synopsis: a golden hour of adventure.".into(), heading: false, gap_before: 0 });
    input.facts = vec![("Developer".into(), "Human Entertainment".into())];
    input.rows.insert(Binding::Actions, ["play", "back"].map(|id| Row {
        id: id.into(), label: id.into(), subtitle: String::new(), value: String::new(), enabled: true,
    }).into());
    input.focus.component = theme.pack().screens[&ScreenId::GameDetail].components.iter()
        .find(|c| c.binding == Some(Binding::Actions)).unwrap().id.clone();
    input.focus.item = Some("play".into());
    input
}

fn assert_cold(frame: &PreparedFrame, input: &FrameInputs) {
    let cold = PreparedFrame::prepare(theme(999), input).unwrap();
    assert_eq!(frame.pixels().data(), cold.pixels().data());
    assert_eq!(frame.targets, cold.targets);
    assert_eq!(frame.text_boxes, cold.text_boxes);
    assert!(frame.input_matches(input));
}

#[test]
fn details_slot_reopens_with_less_capture_and_allocation_work_and_current_dependencies() {
    let theme = theme(1);
    let lower = FrameInputs::new(ScreenId::Recents);
    let mut input = details(&theme);
    let mut parked = None;
    let mut frames = prepare_theme_frames(&theme, &[lower.clone(), input.clone()], vec![], &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
    let original = frames[1].pixels().data().as_ptr();
    #[cfg(feature = "perf-bench")]
    let trace = std::env::temp_dir().join(format!("details-slot-{:?}.log", std::thread::current().id()));
    #[cfg(feature = "perf-bench")]
    common::perf_trace::start_probe(trace.to_str().unwrap());
    for change in 0..5 {
        frames = prepare_theme_frames(&theme, std::slice::from_ref(&lower), frames, &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
        assert!(parked.is_some());
        match change {
            1 => {
                input.text.insert(Binding::SelectedTitle, "A different game".into());
                input.facts[0].1 = "Different developer".into();
                input.detail_lines[0].text = "A different synopsis.".into();
            }
            2 => {
                let mut art = tiny_skia::Pixmap::new(101, 101).unwrap();
                art.fill(tiny_skia::Color::from_rgba8(27, 90, 150, 173));
                let art = std::sync::Arc::new(art);
                input.art.selected_cover = Some(art.clone());
                input.art.selected_hero = Some(art.clone());
                input.art.selected_logo = Some(art);
            }
            3 => {
                input.status.clock = Some("12:35".into());
                input.status.battery = Some(41);
                input.status.charging = true;
            }
            4 => {
                input.art = Default::default();
                input.text.insert(Binding::SelectedTitle, "Missing art fallback".into());
            }
            _ => {}
        }
        #[cfg(feature = "perf-bench")]
        common::perf_trace::begin_render();
        frames = prepare_theme_frames(&theme, &[lower.clone(), input.clone()], frames, &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
        assert!(parked.is_none());
        assert_eq!(frames[1].pixels().data().as_ptr(), original);
        #[cfg(feature = "perf-bench")]
        {
            use common::perf_trace::{self, Count};
            let warm = perf_trace::probe_counts()["measurement"]["frame"]["counters"].clone();
            perf_trace::begin_render();
            let _fresh = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let fresh = perf_trace::probe_counts()["measurement"]["frame"]["counters"].clone();
            for counter in [Count::CaptureBytes, Count::Allocations] {
                let index = counter as usize;
                assert!(warm[index].as_u64().unwrap() < fresh[index].as_u64().unwrap(),
                    "change {change}, counter {index}: warm={warm}, fresh={fresh}");
            }
        }
        assert_cold(&frames[1], &input);
    }
    #[cfg(feature = "perf-bench")]
    std::fs::remove_file(trace).unwrap();
}

#[test]
fn details_slot_releases_on_theme_identity_lower_screen_and_stack_changes() {
    for change in 0..5 {
        let theme = theme(1);
        let lower = FrameInputs::new(ScreenId::Recents);
        let input = details(&theme);
        let mut parked = None;
        let frames = prepare_theme_frames(&theme, &[lower.clone(), input], vec![], &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
        let frames = prepare_theme_frames(&theme, std::slice::from_ref(&lower), frames, &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
        assert!(parked.is_some());
        let mut models = vec![lower];
        let next = match change {
            0 => self::theme(2),
            1 => self::theme(1), // A reload can replace the handle without changing its generation.
            2 => { models[0].screen = ScreenId::Library; theme }
            3 => { models.push(FrameInputs::new(ScreenId::ContextMenu)); theme }
            _ => { models.clear(); theme }
        };
        let _frames = prepare_theme_frames(&next, &models, frames, &mut parked, &mut vec![], &mut NeighborGraveyard::default(), None, &mut false).unwrap();
        assert!(parked.is_none(), "change {change}");
    }
}

#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(orbit_fixture)]
async fn details_slot_network_play_releases_and_suppresses_parking_until_cancel_or_error_dismissal() {
    use super::*;
    use crate::consoles::ConsoleMapper;
    use crate::network_runtime as network;
    use common::database::Database;

    for prepare_closed in [true, false] {
        for error in [false, true] {
            let (_fixture, game) = network::tests::details_slot_fixture();
            let database = Database::in_memory().unwrap();
            let row: common::database::NewGame = game.clone().into();
            database.update_games(&[row.clone()]).unwrap();
            database.increment_play_count(&row).unwrap();
            let mut style = Stylesheet::new();
            style.load_fonts().unwrap();
            style.recents.use_recents_carousel = false;
            let mut map = type_map::TypeMap::new();
            map.insert(style.clone());
            map.insert(Locale::new("en-US"));
            map.insert(ConsoleMapper::new());
            map.insert(common::geom::Size::new(640, 480));
            map.insert(database);
            let res = Resources::new(map);
            res.insert(Some(theme(901)));
            let rect = Rect::new(0, 54, 640, 376);
            let views = (
                Recents::load_or_new(rect, res.clone(), None).unwrap(),
                Games::load_or_new(rect, res.clone(), None).unwrap(),
                Apps::load_or_new(rect, res.clone(), None).unwrap(),
                Settings::new(rect, res.clone(), Default::default()).unwrap(),
            );
            let mut app = App::new(Rect::new(0, 0, 640, 480), res, views, 0,
                DefaultPlatform::new().unwrap().battery().unwrap()).unwrap();
            let mut display = <DefaultPlatform as Platform>::Display::default();
            let (tx, mut rx) = tokio::sync::mpsc::channel(100);
            app.prepare_theme().unwrap();
            app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
            assert!(app.details_active());
            app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
            assert!(!app.details_active());
            if prepare_closed {
                app.prepare_theme().unwrap();
                app.draw(&mut display, &style).unwrap();
                assert!(app.parked_details.is_some());
            }
            if error {
                let bytes = std::fs::read(&game.path).unwrap();
                std::fs::write(&game.path, vec![b'x'; bytes.len()]).unwrap();
            }
            app.handle_key_event(KeyEvent::Pressed(Key::A), tx.clone(), &mut VecDeque::new()).await.unwrap();
            assert!(network::active());
            assert!(network::blocks_navigation_warm());
            assert!(app.parked_details.is_none(), "network Play retained closed Details");
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
            assert!(network::draw(&mut display, &style));
            assert!(app.parked_details.is_none(), "outgoing Details was parked during Play");
            if !error {
                app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while network::active() {
                assert!(std::time::Instant::now() < deadline, "network Play did not settle");
                network::poll();
                app.prepare_theme().unwrap();
                app.draw(&mut display, &style).unwrap();
                network::draw(&mut display, &style);
                assert!(app.parked_details.is_none());
                network::frame_presented();
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            if error {
                assert!(network::blocks_navigation_warm(), "error modal must keep parking blocked");
                assert!(network::draw(&mut display, &style));
                for _ in 0..2 {
                    app.prepare_theme().unwrap();
                    app.draw(&mut display, &style).unwrap();
                    assert!(app.parked_details.is_none());
                }
                app.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut VecDeque::new()).await.unwrap();
            }
            assert!(!network::blocks_navigation_warm());
            assert!(network::action().is_none());
            assert!(rx.try_recv().is_err(), "network Play must not send Exec");
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
            assert!(app.parked_details.is_none());
            app.handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
            app.handle_key_event(KeyEvent::Pressed(Key::B), tx, &mut VecDeque::new()).await.unwrap();
            app.prepare_theme().unwrap();
            app.draw(&mut display, &style).unwrap();
            assert!(app.parked_details.is_some(), "parking did not resume after network dismissal");
            common::media::invalidate();
        }
    }
}
