use super::{
    frame::{FrameInputs, Hint, Row, Status},
    pack::{Binding, Kind, NavGroup, ScreenId},
    render::PreparedFrame,
};
use crate as common;
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-fixtures/parity-b-support.rs"
));
#[test]
fn parity_b_generic_empty_lists() {
    for (variant, theme) in parity_variants() {
        for screen in [
            ScreenId::Library,
            ScreenId::Apps,
            ScreenId::ContextMenu,
            ScreenId::DiscPicker,
            ScreenId::GuideSelector,
            ScreenId::IngameMenu,
            ScreenId::SettingsPage,
            ScreenId::SettingsRoot,
        ] {
            let mut input = FrameInputs::new(screen);
            input.text.insert(Binding::Title, match screen {
                ScreenId::Library => "Library", ScreenId::Apps => "Apps", ScreenId::ContextMenu => "Game actions",
                ScreenId::DiscPicker => "Choose a disc", ScreenId::GuideSelector => "Guide", ScreenId::IngameMenu => "In-game menu",
                ScreenId::SettingsPage => "Display", ScreenId::SettingsRoot => "Settings", _ => unreachable!()
            }.into());
            empty_shell(&mut input);
            input.status = Status {
                clock: Some("12:34".into()),
                battery: Some(82),
                network: Some("online".into()),
                ..Default::default()
            };
            let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let lists = theme.pack().screens[&screen]
                .components
                .iter()
                .filter(|c| {
                    matches!(c.kind, Kind::Entries | Kind::SettingsRows)
                        && c.paint
                            .as_ref()
                            .is_some_and(|p| p.empty.is_some() && !p.collapse_empty)
                })
                .collect::<Vec<_>>();
            if lists.is_empty() {
                assert!(frame.text_boxes.iter().all(|(id, _)| !theme.pack().screens[&screen].components.iter()
                    .any(|c| &c.id == id && matches!(c.kind, Kind::Entries | Kind::SettingsRows))),
                    "{variant}/{screen:?}: undeclared empty text");
            }
            for c in lists {
                let offset = c.paint.as_ref().unwrap().empty.as_ref().unwrap().offset_y;
                assert!(
                    frame
                        .text_boxes
                        .iter()
                        .any(|(id, r)| id == &c.id && (screen.is_dialog() || r.1 == c.rect.1 + offset) && r.1 + r.3 <= 480),
                    "{variant}/{screen:?}: missing empty text run"
                );
            }
            let name = serde_json::to_value(screen).unwrap();
            let name = format!("empty_{}", name.as_str().unwrap());
            if screen.is_dialog() { parity_save_overlay(&variant, &name, &frame); }
            else { parity_save(&variant, &name, frame.pixels()); }
        }
        // Dedicated empty compositions keep their conditional content and footer.
        for screen in [ScreenId::Recents, ScreenId::SearchResults] {
            let mut input = FrameInputs::new(screen);
            input.text.insert(Binding::Title, if screen == ScreenId::Recents { "Recents" } else { "Search results" }.into());
            empty_shell(&mut input);
            let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            let dedicated = theme.pack().screens[&screen].components.iter()
                .any(|c| c.binding == Some(Binding::EmptyTitle));
            if dedicated {
                assert!(frame.text_boxes.iter().all(|(id, _)| !theme.pack().screens[&screen]
                    .components.iter().any(|c| &c.id == id && matches!(c.kind, Kind::Entries | Kind::SettingsRows))), "duplicate generic message");
            }
            let name = serde_json::to_value(screen).unwrap();
            parity_save(&variant, &format!("empty_{}", name.as_str().unwrap()), frame.pixels());
        }
    }
}

fn empty_shell(input: &mut FrameInputs) {
    input.status = Status { clock: Some("12:34".into()), battery: Some(82), network: Some("online".into()), ..Default::default() };
    input.navigation.insert(NavGroup::Primary, ["Recents", "Library", "Apps", "Settings"].into_iter().enumerate().map(|(index,label)| Row {
        id: format!("tab-{index}"), label: label.into(), value: String::new(), enabled: true
    }).collect());
    let active = match input.screen { ScreenId::Apps | ScreenId::GuideSelector => 2, ScreenId::SettingsRoot | ScreenId::SettingsPage => 3, ScreenId::Recents => 0, _ => 1 };
    input.selected.insert(Binding::Navigation, format!("tab-{active}"));
    input.hints.push(Hint { button: "B".into(), label: "Back".into() });
}
