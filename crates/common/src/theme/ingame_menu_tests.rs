use super::*;
use crate::theme::{FontBank, frame::Focus, pack::Role};

#[test]
fn every_pack_fits_eight_ingame_actions_and_full_slot_text() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
    for name in [
        "compact",
        "orbit",
        "compact-plus",
        "big-picture",
        "orbit-hero",
        "orbit-row",
    ] {
        for shape in ["square", "tall"] {
            let snapshot = if matches!(name, "compact" | "orbit") {
                ThemeSnapshot::builtin(name).unwrap()
            } else {
                ThemeSnapshot::load_with_settings(
                    &repo.join("themes").join(name),
                    &std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]),
                )
                .unwrap()
            };
            let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
            let components = &theme.pack().screens[&ScreenId::IngameMenu].components;
            let c = components
                .iter()
                .find(|c| c.binding == Some(Binding::Entries))
                .unwrap();
            let title = components
                .iter()
                .find(|c| c.binding == Some(Binding::Title))
                .unwrap();
            assert!(c.visible_count.unwrap() >= 8, "{name}/{shape}");
            assert_eq!(title.rect.3, 64);
            assert!(title.rect.1 + title.rect.3 <= c.rect.1);
            assert!(c.rect.1 + c.rect.3 <= 424);
            assert!(
                !components
                    .iter()
                    .any(|c| c.binding == Some(Binding::Subtitle))
            );
            for slot in ["Slot 0", "Auto"] {
                let mut input = FrameInputs::new(ScreenId::IngameMenu);
                input.text.insert(Binding::Title, "Gleylancer".into());
                input.rows.insert(
                    Binding::Entries,
                    [
                        "Continue", "Disk", "Save", "Load", "Reset", "Settings", "Guide", "Quit",
                    ]
                    .map(|label| Row {
                        id: label.to_lowercase(),
                        label: label.into(),
                        subtitle: String::new(),
                        value: if matches!(label, "Save" | "Load") {
                            slot.into()
                        } else if label == "Disk" {
                            "Disk 2".into()
                        } else {
                            String::new()
                        },
                        enabled: true,
                    })
                    .into(),
                );
                for focus in ["continue", "save", "load", "quit"] {
                    input.focus = Focus {
                        component: c.id.clone(),
                        item: Some(focus.into()),
                    };
                    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                    let targets: Vec<_> = frame
                        .targets
                        .iter()
                        .filter(|t| t.component == c.id)
                        .collect();
                    assert_eq!(targets.len(), 8, "{name}/{shape}/{slot}/{focus}");
                    for (row, target) in input.rows[&Binding::Entries].iter().zip(targets) {
                        let boxes: Vec<_> = frame
                            .text_boxes
                            .iter()
                            .filter(|(id, r)| {
                                id == &c.id
                                    && r.1 >= target.rect.1
                                    && r.1 < target.rect.1 + target.rect.3
                            })
                            .map(|(_, r)| *r)
                            .collect();
                        assert_eq!(boxes.len(), if row.value.is_empty() { 1 } else { 2 });
                        let focused = row.id == focus;
                        if theme.pack().ink() {
                            let ty = frame.ink_type(c, focused);
                            assert_eq!(
                                ty.lines(&row.label, boxes[0].2, 1, Overflow::Ellipsis),
                                [row.label.clone()],
                                "{name}: {}",
                                row.label
                            );
                            if !row.value.is_empty() {
                                let value_ty = frame.ink_subtype(
                                    c,
                                    c.paint.as_ref().unwrap().value.as_ref().unwrap(),
                                );
                                assert_eq!(value_ty.size, ty.size);
                                assert_eq!(
                                    value_ty.lines(&row.value, boxes[1].2, 1, Overflow::Ellipsis),
                                    [row.value.clone()],
                                    "{name}: {}",
                                    row.value
                                );
                            }
                        } else {
                            let ty = theme.text_style(Role::Row, Color::new(255, 255, 255));
                            assert_eq!(wrap(&ty, &row.label, boxes[0].2, 1), [row.label.clone()]);
                            if !row.value.is_empty() {
                                assert_eq!(
                                    wrap(&ty, &row.value, boxes[1].2, 1),
                                    [row.value.clone()]
                                );
                            }
                        }
                        if boxes.len() == 2 {
                            assert!(boxes[0].0 + boxes[0].2 < boxes[1].0);
                            assert_eq!(
                                boxes[1].0 + boxes[1].2,
                                target.rect.0 + target.rect.2
                                    - c.padding.unwrap().max(
                                        theme.pack().alignment.as_ref().map_or(0, |a| a.row_inset)
                                    )
                            );
                        }
                    }
                    assert!(
                        frame.text_boxes.iter().any(|(id, _)| id == &title.id),
                        "name fallback"
                    );
                    let mut logo = Pixmap::new(320, 64).unwrap();
                    logo.fill(tiny_skia::Color::from_rgba8(180, 50, 20, 255));
                    input.art.selected_logo = Some(Arc::new(logo));
                    input.art.selected_cover = Some(Arc::new(Pixmap::new(32, 32).unwrap()));
                    let with_logo = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                    assert!(
                        !with_logo.text_boxes.iter().any(|(id, _)| id == &title.id),
                        "{name}: logo replaces title even with cover"
                    );
                    assert_eq!(
                        PreparedFrame::logo_rect(title, input.art.selected_logo.as_ref().unwrap())
                            .3,
                        64
                    );
                    input.art = Default::default();
                }
            }
        }
    }
}
