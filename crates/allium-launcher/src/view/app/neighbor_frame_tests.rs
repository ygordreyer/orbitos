use super::*;
use common::theme::{frame::{FrameInputs, Row}, pack::{Binding, ScreenId}, render::PreparedFrame};

fn theme() -> common::theme::ThemeHandle {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = common::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    common::theme::ThemeHandle::prepare(common::theme::runtime::ThemeSnapshot::load(&root.join("themes/compact-plus")).unwrap(), &bank, 991).unwrap()
}
fn input(theme: &common::theme::ThemeHandle, selected: usize) -> FrameInputs {
    let mut input = FrameInputs::new(ScreenId::Recents);
    input.text.insert(Binding::SelectedTitle, format!("Game {selected}"));
    input.rows.insert(Binding::Entries, (0..6).map(|i| Row { id: i.to_string(),
        label: format!("Game {i}"), subtitle: String::new(), value: String::new(), enabled: true }).collect());
    input.focus.component = crate::theme_adapter::component_id(theme, ScreenId::Recents, Binding::Entries);
    input.focus.item = Some(selected.to_string());
    input.selected.insert(Binding::Entries, selected.to_string());
    input
}
fn neighbor(base: &PreparedFrame, input: &FrameInputs) -> PreparedNeighbor {
    let (frame, charge) = base.prepare_neighbor(input, NEIGHBOR_LIMIT).unwrap().unwrap();
    PreparedNeighbor { base: base.navigation_stamp(), frame, charge, generation: NeighborGeneration {
        media: 0, names: 0, metadata: 0, adoption: 0, artwork: 0, locale: 0, consoles: 0, language: String::new(),
    } }
}
fn identical(frame: &PreparedFrame, theme: &common::theme::ThemeHandle, input: &FrameInputs) {
    let reference = PreparedFrame::prepare(theme.clone(), input).unwrap();
    assert_eq!(frame.pixels().data().len(), 1_228_800);
    assert!(frame.pixels().data() == reference.pixels().data(), "screen {:?}", frame.screen());
    assert_eq!(frame.targets, reference.targets);
    assert_eq!(frame.text_boxes, reference.text_boxes);
    assert_eq!(frame.is_opaque(), reference.is_opaque());
    assert_eq!(frame.screen(), reference.screen());
}

#[test]
fn frameprep_adopts_only_the_clean_displayed_base_and_discards_sibling_damage() {
    let theme = theme();
    let base = PreparedFrame::prepare(theme.clone(), &input(&theme, 0)).unwrap();
    let displayed = base.navigation_stamp();
    let next = input(&theme, 1);
    let candidate = neighbor(&base, &next);
    let adopted_stamp = candidate.frame.navigation_stamp();
    let sibling = neighbor(&base, &input(&theme, 5));
    let mut slots = vec![candidate, sibling];
    let mut adopted = false;
    let frames = prepare_theme_frames(&theme, std::slice::from_ref(&next), vec![base], &mut None,
        &mut slots, Some(&displayed), &mut adopted).unwrap();
    assert!(adopted);
    assert!(frames[0].navigation_is_current(&adopted_stamp));
    assert!(slots.is_empty());
    identical(&frames[0], &theme, &next);
    assert!(!frames[0].navigation_is_current(&displayed));
}

#[test]
fn frameprep_wrong_base_repairs_repeat_batches_and_overlay_stacks_fall_back() {
    let theme = theme();
    for case in ["wrong base", "repair", "repeat batch", "menu", "details", "status"] {
        let base_input = input(&theme, 0);
        let mut base = PreparedFrame::prepare(theme.clone(), &base_input).unwrap();
        let stamp = base.navigation_stamp();
        let next = input(&theme, 1);
        let candidate = neighbor(&base, &next);
        let candidate_stamp = candidate.frame.navigation_stamp();
        let mut slots = vec![candidate];
        let mut models = vec![next];
        let mut displayed = Some(&stamp);
        match case {
            "wrong base" => base = PreparedFrame::prepare_reusing(theme.clone(), &input(&theme, 3), Some(base)).unwrap(),
            "repair" => displayed = None,
            "repeat batch" => models[0] = input(&theme, 2),
            "menu" => models.push(FrameInputs::new(ScreenId::ContextMenu)),
            "details" => models.push(FrameInputs::new(ScreenId::GameDetail)),
            "status" => models[0].status.battery = Some(17),
            _ => unreachable!(),
        }
        let mut adopted = true;
        let frames = prepare_theme_frames(&theme, &models, vec![base], &mut None, &mut slots, displayed, &mut adopted).unwrap();
        assert!(!adopted, "{case} retained adoption flag");
        assert!(!frames[0].navigation_is_current(&candidate_stamp), "{case}");
        assert!(slots.is_empty(), "{case}");
        let first = if frames.last().unwrap().is_opaque() { frames.len() - 1 } else { 0 };
        for (frame, model) in frames[first..].iter().zip(&models[first..]) { identical(frame, &theme, model); }
    }
}

#[test]
fn frameprep_completed_misses_keep_actual_readiness_independent_of_pixels() {
    let theme = theme();
    let mut model = input(&theme, 0);
    let before = PreparedFrame::prepare(theme.clone(), &model).unwrap();
    assert!(!theme_content_complete(&model, &theme));
    model.art.completed_misses = common::theme::frame::ArtMisses {
        selected_cover: true, selected_hero: true, selected_logo: true, next_cover: true,
        entries: model.rows[&Binding::Entries].iter().map(|r| r.id.clone()).collect(),
    };
    let after = PreparedFrame::prepare(theme.clone(), &model).unwrap();
    assert!(before.pixels().data() == after.pixels().data());
    assert!(theme_content_complete(&model, &theme));
}

#[test]
#[serial_test::serial(orbit_fixture)]
fn frameprep_eager_generation_checks_reject_equal_visible_value_changes() {
    let mut map = type_map::TypeMap::new();
    map.insert(Locale::new("en-US"));
    map.insert(crate::consoles::ConsoleMapper::new());
    let res = Resources::new(map);
    for change in ["media", "metadata", "locale revision", "console revision", "language"] {
        let before = NeighborGeneration::current(&res);
        match change {
            "media" => common::media::invalidate(),
            "metadata" => super::super::details::invalidate_metadata(),
            "locale revision" => res.insert(Locale::new("en-US")),
            "console revision" => res.insert(crate::consoles::ConsoleMapper::new()),
            "language" => res.insert(Locale::new("pt-BR")),
            _ => unreachable!(),
        }
        assert_ne!(before, NeighborGeneration::current(&res), "{change}");
    }
}

#[test]
fn frameptr_empty_preparation_resets_presentation_flag() {
    let mut adopted = true;
    let frames = prepare_theme_frames(&theme(), &[], vec![], &mut None, &mut vec![], None, &mut adopted).unwrap();
    assert!(frames.is_empty());
    assert!(!adopted);
}
