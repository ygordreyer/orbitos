use super::*;
use crate::theme::{fonts::FontBank, frame::Hint, pack::NavGroup};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Instant};

fn theme(id: &str) -> ThemeHandle {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    ThemeHandle::prepare(
        ThemeSnapshot::load(&root.join("themes").join(id)).unwrap(),
        &bank,
        1,
    )
    .unwrap()
}
fn model(
    theme: &ThemeHandle,
    screen: ScreenId,
    selected: usize,
    arts: &[Arc<Pixmap>],
) -> FrameInputs {
    let mut input = FrameInputs::new(screen);
    input.status.clock = Some("12:34".into());
    input.status.battery = Some(73);
    input.status.network = Some("online".into());
    input.hints = [
        ("A", "Launch"),
        ("X", "Details"),
        ("Y", "Favorite"),
        ("L/R", "Tabs"),
    ]
    .into_iter()
    .map(|(button, label)| Hint {
        button: button.into(),
        label: label.into(),
    })
    .collect();
    let entries = theme.pack().screens[&screen]
        .components
        .iter()
        .find(|c| c.kind == Kind::Entries);
    let count = entries.map_or(3, |c| c.visible_count.unwrap_or(3).min(5)) as usize;
    let rows: Vec<_> = (0..count)
        .map(|i| Row {
            subtitle: String::new(),
            id: i.to_string(),
            label: format!("Game {i}: Racing the golden hour"),
            value: "PlayStation".into(),
            enabled: true,
        })
        .collect();
    if let Some(c) = entries {
        input.focus.component = c.id.clone();
        input.focus.item = Some((selected % count).to_string());
    }
    input.rows.insert(Binding::Entries, rows);
    input.rows.insert(
        Binding::Actions,
        vec![Row {
            subtitle: String::new(),
            id: "launch".into(),
            label: "Launch".into(),
            value: String::new(),
            enabled: true,
        }],
    );
    input
        .selected
        .insert(Binding::Entries, (selected % count).to_string());
    input.selected.insert(
        Binding::Navigation,
        if screen == ScreenId::Recents {
            "recents"
        } else {
            "library"
        }
        .into(),
    );
    input.navigation.insert(
        NavGroup::Primary,
        ["recents", "library", "apps", "settings"]
            .into_iter()
            .map(|id| Row {
                subtitle: String::new(),
                id: id.into(),
                label: id.into(),
                value: String::new(),
                enabled: true,
            })
            .collect(),
    );
    for (binding, value) in [
        (Binding::Title, format!("{screen:?}")),
        (Binding::Breadcrumb, format!("{screen:?}")),
        (
            Binding::SelectedTitle,
            format!("Game {selected}: Racing the golden hour"),
        ),
        (
            Binding::SelectedMetadata,
            format!("PlayStation · {}", 1997 + selected),
        ),
        (
            Binding::SelectedBlurb,
            format!(
                "Story {selected}. A bounded fixture description. Race the golden hour. Find your line."
            ),
        ),
        (Binding::SelectedFacts, format!("Facts {selected}")),
        (Binding::Pagination, "1 / 3".into()),
    ] {
        input.text.insert(binding, value);
    }
    input.facts = vec![
        ("Developer".into(), format!("Studio {selected}")),
        ("Released".into(), "1998".into()),
    ];
    input.art.selected_cover = Some(arts[selected % arts.len()].clone());
    input.art.selected_hero = Some(arts[selected % arts.len()].clone());
    input.art.next_cover = Some(arts[(selected + 1) % arts.len()].clone());
    for i in 0..count {
        input
            .art
            .entries
            .insert(i.to_string(), arts[i % arts.len()].clone());
    }
    input
}
fn art() -> Vec<Arc<Pixmap>> {
    (0..3)
        .map(|seed| {
            let mut p = Pixmap::new(240, 240).unwrap();
            for (i, p) in p.data_mut().chunks_exact_mut(4).enumerate() {
                let a = if i % 11 == 0 { 173 } else { 255 };
                p.copy_from_slice(&[
                    ((i + seed * 41) % (a as usize + 1)) as u8,
                    ((i / 240 + seed * 17) % (a as usize + 1)) as u8,
                    (seed * 53) as u8,
                    a,
                ]);
            }
            Arc::new(p)
        })
        .collect()
}

#[test]
#[ignore = "explicit host selection benchmark with original frame hashes"]
fn ink_selection_benchmark() {
    let trace = std::env::temp_dir().join(format!("allium-selection-trace-{}", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    let arts = art();
    for base in ["orbit-hero", "orbit-row", "compact-plus", "big-picture"] {
        for suffix in ["", "-tall"] {
            let id = format!("{base}{suffix}");
            let theme = theme(&id);
            for screen in [ScreenId::Library, ScreenId::Recents, ScreenId::GameDetail] {
                let mut frame =
                    PreparedFrame::prepare(theme.clone(), &model(&theme, screen, 0, &arts))
                        .unwrap();
                let mut display = super::tests::TestDisplay(Pixmap::new(640, 480).unwrap());
                frame.paint(&mut display);
                for step in 0..12 {
                    let mut input = model(&theme, screen, 1 + step % 2, &arts);
                    let operation = if step < 8 {
                        "selection"
                    } else if step < 10 {
                        "clock"
                    } else {
                        "tab"
                    };
                    if step >= 8 {
                        input = model(&theme, screen, 2, &arts);
                        input.status.clock = Some(format!("12:{}", 35 + step));
                    }
                    if step >= 10 {
                        input = model(
                            &theme,
                            if step == 10 {
                                ScreenId::Recents
                            } else {
                                screen
                            },
                            2,
                            &arts,
                        );
                    }
                    crate::perf_trace::begin_render();
                    let start = Instant::now();
                    frame =
                        PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
                    let damage = frame.paint_damage(&mut display);
                    let elapsed = start.elapsed().as_micros();
                    let mut counts = crate::perf_trace::probe_counts();
                    counts["theme"] = id.clone().into();
                    counts["screen"] = format!("{screen:?}").into();
                    counts["operation"] = operation.into();
                    counts["step"] = step.into();
                    counts["wall_us"] = (elapsed as u64).into();
                    counts["damage_pixels"] = damage
                        .iter()
                        .map(|r| r.w as u64 * r.h as u64)
                        .sum::<u64>()
                        .into();
                    counts["sha256"] = Sha256::digest(display.0.data())
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                        .into();
                    println!("SELECTION_BENCH {counts}");
                    std::hint::black_box(display.0.data());
                }
            }
        }
    }
    std::fs::remove_file(trace).unwrap();
}

#[test]
fn ink_retained_selection_status_and_tabs_match_fresh() {
    let arts = art();
    for base in ["orbit-hero", "orbit-row", "compact-plus", "big-picture"] {
        for suffix in ["", "-tall"] {
            let id = format!("{base}{suffix}");
            let theme = theme(&id);
            let mut previous = None;
            let mut display = super::tests::TestDisplay(Pixmap::new(640, 480).unwrap());
            for step in 0..48 {
                let screen =
                    [ScreenId::Library, ScreenId::Recents, ScreenId::GameDetail][(step / 12) % 3];
                let mut input = model(&theme, screen, step % 3, &arts);
                match step % 12 {
                    3 => input.status.clock = Some("12:35".into()),
                    4 => {
                        input.status.battery = None;
                        input.status.network = None;
                    }
                    5 => {
                        input.rows.get_mut(&Binding::Entries).unwrap().reverse();
                    }
                    6 => {
                        input.rows.get_mut(&Binding::Entries).unwrap().clear();
                        input.focus = Default::default();
                    }
                    7 => {
                        input.hints.clear();
                        input.art = Default::default();
                    }
                    8 => {
                        input.facts.reverse();
                        input.metadata = vec![("Test".into(), "Different".into())];
                    }
                    9 => input
                        .text
                        .insert(
                            Binding::SelectedBlurb,
                            "A different paragraph with a different length.".into(),
                        )
                        .map(|_| ())
                        .unwrap_or(()),
                    _ => {}
                }
                let fresh = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                let frame =
                    PreparedFrame::prepare_reusing(theme.clone(), &input, previous).unwrap();
                assert!(
                    frame.pixels.data() == fresh.pixels.data(),
                    "pixels {id} {screen:?} {step}"
                );
                assert_eq!(frame.targets, fresh.targets, "targets {id} {step}");
                assert_eq!(frame.text_boxes, fresh.text_boxes, "text {id} {step}");
                frame.paint_damage(&mut display);
                assert!(
                    display.0.data() == fresh.pixels.data(),
                    "damage {id} {step}"
                );
                previous = Some(frame);
            }
        }
    }
}

#[test]
fn ink_clock_tick_only_damages_status() {
    let arts = art();
    for base in ["orbit-hero", "orbit-row", "compact-plus", "big-picture"] {
        for suffix in ["", "-tall"] {
            let theme = theme(&format!("{base}{suffix}"));
            for screen in [ScreenId::Library, ScreenId::Recents, ScreenId::GameDetail] {
                let mut input = model(&theme, screen, 0, &arts);
                let before = PreparedFrame::prepare(theme.clone(), &input).unwrap();
                input.status.clock = Some("12:35".into());
                let after =
                    PreparedFrame::prepare_reusing(theme.clone(), &input, Some(before)).unwrap();
                let status = theme.pack().screens[&screen]
                    .components
                    .iter()
                    .find(|c| c.kind == Kind::Status)
                    .unwrap()
                    .rect;
                assert_eq!(after.damage, vec![status], "{base}{suffix} {screen:?}");
                assert!(
                    after.pixels.data()
                        == PreparedFrame::prepare(theme.clone(), &input)
                            .unwrap()
                            .pixels
                            .data()
                );
            }
        }
    }
}

#[test]
fn ink_opaque_art_does_not_make_a_transparent_base_opaque() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let mut snapshot = ThemeSnapshot::load(&root.join("themes/big-picture")).unwrap();
    let pack = &mut Arc::get_mut(&mut snapshot.0).unwrap().pack;
    pack.tokens
        .colors
        .insert(ColorRole::Background, "#00000000".into());
    let components = &mut pack.screens.get_mut(&ScreenId::Library).unwrap().components;
    let mut background = components
        .iter()
        .find(|c| c.kind == Kind::Background)
        .unwrap()
        .clone();
    background.paint.as_mut().unwrap().gradient = None;
    let mut image = components
        .iter()
        .find(|c| c.binding == Some(Binding::SelectedHero))
        .unwrap()
        .clone();
    image.rect = Rect(0, 0, 640, 480);
    image.radius = Some(0);
    *components = vec![background, image];
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
    let mut input = FrameInputs::new(ScreenId::Library);
    let mut art = Pixmap::new(32, 32).unwrap();
    art.fill(tiny_skia::Color::from_rgba8(17, 31, 43, 255));
    input.art.selected_hero = Some(Arc::new(art));
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    assert!(frame.is_opaque());
    input.art.selected_hero = None;
    let frame = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(frame)).unwrap();
    assert!(!frame.is_opaque());
    assert!(frame.pixels.data() == PreparedFrame::prepare(theme, &input).unwrap().pixels.data());
}
