use super::*;
use common::theme::{
    frame::Status,
    pack::{Binding, ScreenId},
    render::PreparedFrame,
};
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-fixtures/parity-b-support.rs"
));
#[tokio::test]
async fn parity_b_settings_pages_and_edit_previews() {
    let rect = Rect::new(0, 0, 640, 480);
    for (variant, theme) in parity_variants() {
        let mut map = type_map::TypeMap::new();
        map.insert(Stylesheet::load().unwrap());
        map.insert(Locale::new("en-US"));
        map.insert(common::geom::Size { w: 640, h: 480 });
        map.insert(Some(theme.clone()));
        let res = Resources::new(map);
        let mut pages: Vec<(&str, Box<dyn SettingsChild>)> = vec![
            ("clock", Box::new(Clock::new(rect, res.clone(), None))),
            ("power", Box::new(Power::new(rect, res.clone(), None))),
            ("display", Box::new(Display::new(rect, res.clone(), None))),
            ("language", Box::new(Language::new(rect, res.clone(), None))),
            ("about", Box::new(About::new(rect, res.clone(), None))),
            ("wifi", Box::new(Wifi::new(rect, res.clone(), None))),
            (
                "system",
                Box::new(SystemUpdate::new(rect, res.clone(), None)),
            ),
        ];
        for (name, page) in &mut pages {
            let mut models = page
                .theme_models(&theme)
                .unwrap()
                .expect("descriptor Settings page");
            let input = &mut models[0];
            assert_eq!(input.screen, ScreenId::SettingsPage);
            assert!(!input.rows[&Binding::SettingsRows].is_empty());
            assert!(
                input.rows[&Binding::SettingsRows]
                    .iter()
                    .any(|r| !r.value.is_empty())
            );
            crate::theme_adapter::shell(input, &res, 3);
            input.status = Status {
                clock: Some("12:34".into()),
                battery: Some(82),
                network: Some("online".into()),
                ..Default::default()
            };
            parity_save(
                &variant,
                &format!("settings-{name}"),
                PreparedFrame::prepare(theme.clone(), input)
                    .unwrap()
                    .pixels(),
            );
            page.clear_theme().unwrap();
        }
        let mut display = Display::new(rect, res.clone(), Some(ChildState { selected: 1 }));
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut bubble = VecDeque::new();
        let before = display.theme_models(&theme).unwrap().unwrap()[0].rows[&Binding::SettingsRows]
            .iter()
            .find(|r| r.id == "setting-1")
            .unwrap()
            .value
            .clone();
        for key in [Key::A, Key::Down] {
            display
                .handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut bubble)
                .await
                .unwrap();
        }
        let editing = display.theme_models(&theme).unwrap().unwrap();
        let preview = &editing[0].rows[&Binding::SettingsRows]
            .iter()
            .find(|r| r.id == "setting-1")
            .unwrap()
            .value;
        assert_ne!(
            &before, preview,
            "{variant}: edit preview must reflect widget state"
        );
        display
            .handle_key_event(KeyEvent::Pressed(Key::B), tx, &mut bubble)
            .await
            .unwrap();
        let cancelled = display.theme_models(&theme).unwrap().unwrap();
        assert_eq!(
            before,
            cancelled[0].rows[&Binding::SettingsRows]
                .iter()
                .find(|r| r.id == "setting-1")
                .unwrap()
                .value
        );
    }
}
