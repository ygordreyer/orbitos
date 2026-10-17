use super::*;
use crate::theme::render_navigation::tests::{arts, model, theme};

fn identical(frame: &PreparedFrame, theme: &ThemeHandle, input: &FrameInputs) {
    let cold = navigation_reference(theme.clone(), input).unwrap();
    assert_eq!(frame.pixels().data().len(), 1_228_800);
    assert!(equal_words(frame.pixels().data(), cold.pixels().data()));
    assert_eq!(frame.targets, cold.targets);
    assert_eq!(frame.text_boxes, cold.text_boxes);
    assert_eq!(frame.is_opaque(), cold.is_opaque());
    assert_eq!(frame.screen(), cold.screen());
}

fn live_bytes(frame: &PreparedFrame, base: &PreparedFrame) -> usize {
    640 * 480 * 4 + retained::storage_bytes(frame.previous_input.as_ref().unwrap())
        + frame.ink_cache.navigation_unique_bytes(&base.ink_cache)
        + frame.targets.capacity() * std::mem::size_of::<Target>()
        + frame.targets.iter().map(|t| t.component.capacity() + t.item.capacity()).sum::<usize>()
        + frame.text_boxes.capacity() * std::mem::size_of::<(String, Rect)>()
        + frame.text_boxes.iter().map(|(s, _)| s.capacity()).sum::<usize>()
}

#[test]
fn frameprep_complete_forks_match_reference_and_continue_after_adoption() {
    let arts = arts();
    for name in ["compact", "compact-plus", "orbit-hero", "orbit-row", "big-picture"] {
        let theme = theme(name, "square");
        let input = model(&theme, ScreenId::Recents, 0, &arts);
        let base = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let saved = base.pixels().data().to_vec();
        let stamp = base.navigation_stamp();
        let next = model(&theme, ScreenId::Recents, 1, &arts);
        let (candidate, charge) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap()
            .unwrap_or_else(|| panic!("{name} declined neighbor, charge {:?}", base.neighbor_charge(&next)));
        assert!(base.navigation_is_current(&stamp));
        assert!(equal_words(base.pixels().data(), &saved));
        assert!(candidate.neighbor_matches(&theme, &next));
        assert!(charge <= 4 * 1024 * 1024);
        identical(&candidate, &theme, &next);
        let mut display = tests::TestDisplay(base.pixels.as_ref().to_owned());
        candidate.paint_damage(&mut display);
        assert!(equal_words(display.0.data(), candidate.pixels().data()));
        let mut adopted = candidate;
        for selected in [2, 0, 7, 3, 12, 1] {
            let mut next = model(&theme, ScreenId::Recents, selected, &arts);
            if selected == 3 { next.status.clock = Some("15:21".into()); }
            adopted = PreparedFrame::prepare_reusing(theme.clone(), &next, Some(adopted)).unwrap();
            identical(&adopted, &theme, &next);
            adopted.paint_damage(&mut display);
            assert!(equal_words(display.0.data(), adopted.pixels().data()));
        }
    }
}

#[test]
fn frameprep_exact_inputs_reject_every_model_dependency_and_live_art_change() {
    let arts = arts();
    let theme = theme("compact-plus", "square");
    let base_input = model(&theme, ScreenId::Recents, 0, &arts);
    let base = PreparedFrame::prepare(theme.clone(), &base_input).unwrap();
    let next = model(&theme, ScreenId::Recents, 1, &arts);
    let (candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
    let changes: &[(&str, fn(&mut FrameInputs))] = &[
        ("screen", |m| m.screen = ScreenId::Library),
        ("equal-visible revision", |m| m.raster_revision = Some("new generation".into())),
        ("text", |m| { m.text.insert(Binding::Title, "changed".into()); }),
        ("rows", |m| m.rows.get_mut(&Binding::Entries).unwrap().reverse()),
        ("cloud", |m| m.rows.get_mut(&Binding::Entries).unwrap()[0].value = "Cloud".into()),
        ("row enabled", |m| m.rows.get_mut(&Binding::Entries).unwrap()[0].enabled = false),
        ("selected", |m| { m.selected.insert(Binding::Entries, "other".into()); }),
        ("pagination", |m| m.pagination = Some(crate::theme::frame::Pagination { page: 3, total: 4 })),
        ("detail lines", |m| m.detail_lines.push(crate::theme::frame::DetailLine { text: "a".into(), heading: true, gap_before: 1 })),
        ("metadata", |m| m.metadata.push(("a".into(), "b".into()))),
        ("facts", |m| m.facts.push(("a".into(), "b".into()))),
        ("navigation", |m| { m.navigation.insert(crate::theme::pack::NavGroup::Primary, vec![]); }),
        ("focus component", |m| m.focus.component = "other".into()),
        ("focus item", |m| m.focus.item = Some("other".into())),
        ("hints", |m| m.hints.push(crate::theme::frame::Hint { button: "A".into(), label: "changed".into() })),
        ("suppressed hints", |m| m.suppress_hints = !m.suppress_hints),
        ("clock", |m| m.status.clock = Some("13:00".into())),
        ("battery", |m| m.status.battery = Some(1)),
        ("charging", |m| m.status.charging = true),
        ("network", |m| m.status.network = Some("busy".into())),
        ("progress", |m| m.progress = Some(0.0)),
        ("keyboard", |m| m.keyboard.push("a".into())),
        ("cursor", |m| m.keyboard_cursor = (1, 1)),
        ("art eviction", |m| m.art.selected_hero = None),
        ("art replacement", |m| m.art.selected_cover = m.art.selected_hero.as_ref().map(|p| std::sync::Arc::new((**p).clone()))),
        ("row art keys", |m| { m.art.entries.remove("0"); }),
        ("next expected", |m| m.art.next_cover_expected = !m.art.next_cover_expected),
        ("completed miss", |m| m.art.completed_misses.selected_logo = true),
    ];
    for (name, change) in changes {
        let mut actual = next.clone();
        change(&mut actual);
        assert!(!candidate.neighbor_matches(&theme, &actual), "{name}");
    }
    for revision in ["list", "media", "name", "metadata", "adoption", "locale", "console", "theme"] {
        let mut actual = next.clone();
        actual.raster_revision = Some(format!("{revision}:2"));
        assert!(!candidate.neighbor_matches(&theme, &actual), "equal-visible {revision}");
    }
    let replacement = crate::theme::render_navigation::tests::theme("compact-plus", "square");
    assert_eq!(replacement.digest(), theme.digest());
    assert_eq!(replacement.generation(), theme.generation());
    assert!(!candidate.neighbor_matches(&replacement, &next), "equal generation/digest, different handle");
    let generation = theme.generation_copy(theme.generation() + 1);
    assert_eq!(generation.digest(), theme.digest());
    assert!(!candidate.neighbor_matches(&generation, &next), "same digest, new generation");
    let mut missing = next.clone();
    missing.art.selected_logo = None;
    let (missing_frame, _) = base.prepare_neighbor(&missing, 4 * 1024 * 1024).unwrap().unwrap();
    missing.art.selected_logo = Some(arts[3].clone());
    assert!(!missing_frame.neighbor_matches(&theme, &missing), "arrival");
    let mut expired = next.clone();
    expired.art.selected_logo = Some(std::sync::Arc::new((*arts[3]).clone()));
    let (expired_frame, _) = base.prepare_neighbor(&expired, 4 * 1024 * 1024).unwrap().unwrap();
    drop(expired.art.selected_logo.take());
    expired.art.selected_logo = Some(std::sync::Arc::new((*arts[3]).clone()));
    assert!(!expired_frame.neighbor_matches(&theme, &expired), "expired weak source");
}

#[test]
fn frameprep_admission_reserves_two_frames_and_rejects_long_labels_before_fork() {
    let arts = arts();
    let theme = theme("compact-plus", "square");
    let input = model(&theme, ScreenId::Recents, 0, &arts);
    let base = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let next = model(&theme, ScreenId::Recents, 1, &arts);
    let (first, charge) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
    let other = model(&theme, ScreenId::Recents, 11, &arts);
    let (second, second_charge) = base.prepare_neighbor(&other, 4 * 1024 * 1024 - charge).unwrap().unwrap();
    assert!(charge + second_charge <= 4 * 1024 * 1024);
    assert!(live_bytes(&first, &base) <= charge);
    assert!(live_bytes(&second, &base) <= second_charge);
    assert!(live_bytes(&first, &base) + live_bytes(&second, &base) <= 4 * 1024 * 1024);
    assert!(base.prepare_neighbor(&other, charge - 1).unwrap().is_none());
    let mut long = next.clone();
    long.rows.get_mut(&Binding::Entries).unwrap()[0].label = "x".repeat(128 * 1024);
    assert!(base.neighbor_charge(&long).is_none());
    assert!(base.prepare_neighbor(&long, 4 * 1024 * 1024).unwrap().is_none());
    identical(&first, &theme, &next);
    identical(&second, &theme, &other);
}

#[test]
fn fpdiag_wifi_status_allows_neighbors_and_rejects_changed_status() {
    let arts = arts();
    let theme = theme("compact-plus", "square");
    for network in ["online", "offline"] {
        let mut input = model(&theme, ScreenId::Recents, 0, &arts);
        input.status.network = Some(network.into());
        let base = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        assert!(base.can_prepare_neighbor(), "Wi-Fi status {network} blocked idle preparation");
        let mut next = model(&theme, ScreenId::Recents, 1, &arts);
        next.status = input.status.clone();
        let (candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
        assert!(candidate.neighbor_matches(&theme, &next));
        identical(&candidate, &theme, &next);
        let mut display = tests::TestDisplay(base.pixels.as_ref().to_owned());
        candidate.paint_damage(&mut display);
        assert!(equal_words(display.0.data(), candidate.pixels().data()));
        next.status.network = Some("changed".into());
        assert!(!candidate.neighbor_matches(&theme, &next));
        next.status.network = None;
        assert!(!candidate.neighbor_matches(&theme, &next));
    }
}

#[test]
fn frameptr_unsupported_display_keeps_damage_copy_fallback() {
    let arts = arts();
    let theme = theme("compact-plus", "square");
    let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
    let next = model(&theme, ScreenId::Recents, 1, &arts);
    let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
    let mut display = tests::TestDisplay(base.pixels.as_ref().to_owned());
    candidate.recycle_neighbor_base(base);
    let trace = std::env::temp_dir().join(format!("frameptr-fallback-{}.jsonl", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    candidate.adopt_neighbor_damage(&mut display);
    assert_eq!(crate::perf_trace::probe_counts()["neighbor_damage_copies"], 1);
    assert_eq!(crate::perf_trace::probe_counts()["neighbor_pointer_adopts"], 0);
    assert!(crate::perf_trace::probe_counts()["copied_pixels"].as_u64().unwrap() > 0);
    crate::perf_trace::stop_probe();
    std::fs::remove_file(trace).unwrap();
    identical(&candidate, &theme, &next);
    assert!(equal_words(display.0.data(), candidate.pixels().data()));
}

#[test]
fn retained_model_requires_every_original_artwork_object() {
    use std::sync::Arc;
    for expired in ["logo", "hero", "cover", "next_cover", "entry"] {
        let mut input = FrameInputs::new(ScreenId::Recents);
        input.art.next_cover_expected = true;
        input.art.selected_logo = Some(Arc::new(Pixmap::new(1, 1).unwrap()));
        input.art.selected_hero = Some(Arc::new(Pixmap::new(1, 1).unwrap()));
        input.art.selected_cover = Some(Arc::new(Pixmap::new(1, 1).unwrap()));
        input.art.next_cover = Some(Arc::new(Pixmap::new(1, 1).unwrap()));
        input.art.entries.insert("entry".into(), Arc::new(Pixmap::new(1, 1).unwrap()));
        let saved = retained::capture(&input);
        let restored = retained::restore_model(&saved).unwrap();
        assert!(restored == input);
        assert!(Arc::ptr_eq(restored.art.selected_logo.as_ref().unwrap(), input.art.selected_logo.as_ref().unwrap()));
        assert!(Arc::ptr_eq(restored.art.selected_hero.as_ref().unwrap(), input.art.selected_hero.as_ref().unwrap()));
        assert!(Arc::ptr_eq(restored.art.selected_cover.as_ref().unwrap(), input.art.selected_cover.as_ref().unwrap()));
        assert!(Arc::ptr_eq(restored.art.next_cover.as_ref().unwrap(), input.art.next_cover.as_ref().unwrap()));
        assert!(Arc::ptr_eq(&restored.art.entries["entry"], &input.art.entries["entry"]));
        drop(restored);
        match expired {
            "logo" => input.art.selected_logo = None,
            "hero" => input.art.selected_hero = None,
            "cover" => input.art.selected_cover = None,
            "next_cover" => input.art.next_cover = None,
            "entry" => { input.art.entries.clear(); },
            _ => unreachable!(),
        }
        assert!(retained::restore_model(&saved).is_none(), "{expired}");
    }
}
