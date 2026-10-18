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
        let mut contexts = std::collections::HashSet::new();
        for (name, page) in &mut pages {
            let mut models = page
                .theme_models(&theme)
                .unwrap()
                .expect("descriptor Settings page");
            let input = &mut models[0];
            assert_eq!(input.screen, ScreenId::SettingsPage);
            assert!(contexts.insert(input.text[&Binding::Subtitle].clone()), "each page needs distinct context");
            assert!(!input.text[&Binding::Message].contains('\n'));
            assert_eq!(input.text[&Binding::Message].matches('.').count(), 1);
            assert!(!input.text[&Binding::Breadcrumb].is_empty());
            let components = &theme.pack().screens[&ScreenId::SettingsPage].components;
            if components.iter().any(|c| c.id == "section-pane" || c.id == "context-sheet") {
                for binding in [Binding::Subtitle, Binding::Message, Binding::Breadcrumb] {
                    assert!(components.iter().any(|c| c.binding == Some(binding)), "side panels must consume page context");
                }
            }
            for c in components {
                if c.binding == Some(Binding::Breadcrumb) {
                    assert!(c.paint.as_ref().unwrap().breadcrumb_icon);
                }
            }
            if *name == "system" {
                let values: Vec<_> = input.rows[&Binding::SettingsRows].iter().map(|row| row.value.as_str()).collect();
                assert!(values.contains(&common::product::VERSION));
                assert!(values.contains(&"Manual updates"));
                assert!(!values.iter().any(|value| value.contains("Checking")));
                assert!(!input.hints.iter().any(|hint| hint.button == "A"));
            }
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
        display.handle_key_event(KeyEvent::Pressed(Key::A), tx.clone(), &mut bubble).await.unwrap();
        let entered = display.theme_models(&theme).unwrap().unwrap();
        assert!(entered[0].rows[&Binding::SettingsRows].iter().any(|row| row.value.starts_with("‹ ")));
        assert_eq!(entered[0].hints.iter().map(|hint| hint.label.as_str()).collect::<Vec<_>>(), ["Change", "Save", "Cancel"]);
        let mut edit_frame = entered[0].clone();
        crate::theme_adapter::shell(&mut edit_frame, &res, 3);
        parity_save(&variant, "settings-edit", PreparedFrame::prepare(theme.clone(), &edit_frame).unwrap().pixels());
        for key in [Key::Down] {
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

#[tokio::test]
async fn datetime_editing_hints_match_field_selection_and_value_keys() {
    let (_, theme) = parity_variants().into_iter().next().unwrap();
    let mut map = type_map::TypeMap::new();
    map.insert(Stylesheet::load().unwrap());
    map.insert(Locale::new("en-US"));
    map.insert(common::geom::Size { w: 640, h: 480 });
    map.insert(Some(theme.clone()));
    let res = Resources::new(map);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut bubble = VecDeque::new();
    for selected in [0, 1] {
        let mut clock = Clock::new(Rect::new(0, 0, 640, 480), res.clone(), Some(ChildState { selected }));
        clock.handle_key_event(KeyEvent::Pressed(Key::A), tx.clone(), &mut bubble).await.unwrap();
        let models = clock.theme_models(&theme).unwrap().unwrap();
        let hints: Vec<_> = models[0].hints.iter().map(|hint| (hint.button.as_str(), hint.label.as_str())).collect();
        if selected == 0 {
            assert_eq!(hints, [("Up/Down", "Change"), ("Left/Right", "Select field"), ("A", "Save"), ("B", "Cancel")]);
            let before = models[0].rows[&Binding::SettingsRows][0].value.clone();
            clock.handle_key_event(KeyEvent::Pressed(Key::Right), tx.clone(), &mut bubble).await.unwrap();
            clock.handle_key_event(KeyEvent::Pressed(Key::Up), tx.clone(), &mut bubble).await.unwrap();
            let after = clock.theme_models(&theme).unwrap().unwrap()[0].rows[&Binding::SettingsRows][0].value.clone();
            assert_ne!(before, after);
        } else {
            assert_eq!(hints, [("Left/Right", "Change"), ("A", "Save"), ("B", "Cancel")]);
        }
        clock.handle_key_event(KeyEvent::Pressed(Key::B), tx.clone(), &mut bubble).await.unwrap();
    }
}

#[test]
#[ignore = "explicit 640x480 launcher polish evidence"]
fn polish_capture_settings() {
    for (variant, theme) in parity_variants().into_iter().filter(|(id, _)|
        id == "compact-plus-square-orange" || id == "orbit-row-square-default") {
        let locale = Locale::new("en-US");
        let mut map = type_map::TypeMap::new();
        map.insert(Locale::new("en-US"));
        let res = Resources::new(map);
        for (name, key, rows) in [
            ("date-time", "settings-clock", vec![("Date & Time", "2026-10-18 13:04"), ("Timezone", "UTC-3")]),
            ("language", "settings-language", vec![("Language", "English (United States)")]),
            ("about", "settings-about", vec![("Product", "OrbitOS"), ("Version", common::product::VERSION), ("Build", common::product::BUILD_ID), ("Model Name", "Miyoo Mini+ (MY354)")]),
            ("system-update", "settings-system-update-menu", vec![("Version", common::product::VERSION), ("System Update", "Manual updates")]),
        ] {
            let mut input = common::theme::frame::FrameInputs::new(ScreenId::SettingsPage);
            input.text.insert(Binding::Title, locale.t(key));
            page_context(&mut input, &locale, key);
            input.rows.insert(Binding::SettingsRows, rows.into_iter().enumerate().map(|(i, (label,value))| common::theme::frame::Row {
                id: format!("setting-{i}"), label: label.into(), value: value.into(), subtitle: String::new(), enabled: true,
            }).collect());
            input.focus = common::theme::frame::Focus { component: crate::theme_adapter::component_id(&theme, ScreenId::SettingsPage, Binding::SettingsRows), item: Some("setting-0".into()) };
            crate::theme_adapter::shell(&mut input, &res, 3);
            input.status = Status { clock: Some("13:04".into()), battery: Some(100), network: Some("online".into()), ..Default::default() };
            parity_save(&variant, name, PreparedFrame::prepare(theme.clone(), &input).unwrap().pixels());
        }
    }
}

#[test]
fn settings_panel_context_uses_each_pages_localized_copy_and_existing_icon() {
    let locale = Locale::new("en-US");
    let mut headings = std::collections::HashSet::new();
    for (key, page, icon) in [
        ("settings-system-update-menu", "update", "apps"), ("settings-wifi", "wifi", "wifi"),
        ("settings-clock", "clock", "recents"), ("settings-power", "power", "power"),
        ("settings-display", "display", "display"), ("settings-theme", "theme", "settings"),
        ("settings-language", "language", "library"), ("settings-about", "about", "apps"),
    ] {
        let mut input = common::theme::frame::FrameInputs::new(ScreenId::SettingsPage);
        page_context(&mut input, &locale, key);
        let heading = &input.text[&Binding::Subtitle];
        let description = &input.text[&Binding::Message];
        assert_eq!(heading, &locale.t(&format!("settings-context-{page}-heading")));
        assert!(headings.insert(heading.clone()), "page context must be distinct");
        assert_eq!(description, &locale.t(&format!("settings-context-{page}-description")));
        assert!(!description.starts_with("settings-") && !description.contains('\n'));
        assert_eq!(description.matches('.').count(), 1, "one sentence per page");
        assert_eq!(input.text[&Binding::Breadcrumb], icon);
    }
}
