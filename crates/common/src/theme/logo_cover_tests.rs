use super::*;
use crate::theme::pack::{Kind, Pack};
use std::collections::BTreeMap;

fn variants() -> Vec<(String, ThemeSnapshot)> {
    let mut themes = Vec::new();
    for id in ["orbit", "compact"] {
        themes.push((id.into(), ThemeSnapshot::builtin(id).unwrap()));
    }
    for base in super::super::choices::LAUNCHER_PACKS {
        let root = ink_pack_root().join(base);
        let pack = Pack::parse(&std::fs::read(root.join("theme.json")).unwrap()).unwrap();
        let mut variants = vec![(base.to_owned(), BTreeMap::new())];
        for setting in &pack.settings {
            variants = variants
                .into_iter()
                .flat_map(|(id, values)| {
                    setting.options.iter().map(move |option| {
                        let mut values = values.clone();
                        values.insert(setting.id.clone(), option.id.clone());
                        (format!("{id}-{}", option.id), values)
                    })
                })
                .collect();
        }
        for (id, values) in variants {
            themes.push((
                id,
                ThemeSnapshot::load_with_settings(&root, &values).unwrap(),
            ));
        }
    }
    themes
}

fn art(width: u32, height: u32, color: tiny_skia::Color) -> Arc<tiny_skia::Pixmap> {
    let mut image = tiny_skia::Pixmap::new(width, height).unwrap();
    image.fill(color);
    Arc::new(image)
}

fn hero_banner(id: &str, screen: ScreenId) -> bool {
    screen == ScreenId::Library
        && (id == "orbit"
            || ["orbit-hero-", "orbit-row-", "big-picture-"]
                .iter()
                .any(|prefix| id.starts_with(prefix)))
}

#[test]
fn media_logo_cover_exclusion_all_packs_variants_and_views() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let logo = art(96, 24, tiny_skia::Color::from_rgba8(250, 10, 80, 255));
    let cover = art(96, 128, tiny_skia::Color::from_rgba8(20, 90, 180, 255));
    let hero = art(160, 90, tiny_skia::Color::from_rgba8(30, 50, 70, 255));
    let mut violations = Vec::new();
    let mut covered_views = 0;
    let mut logo_views = 0;
    let mut hero_views = 0;
    for (id, snapshot) in variants() {
        let theme = ThemeHandle::prepare(snapshot, &bank, 903).unwrap();
        for screen in ScreenId::ALL {
            let components = &theme.pack().screens[&screen].components;
            let slots: Vec<_> = components
                .iter()
                .filter(|c| c.logo || c.binding == Some(Binding::SelectedLogo))
                .collect();
            if slots.is_empty() {
                continue;
            }
            logo_views += 1;
            let has_cover = components.iter().any(|c| {
                c.binding == Some(Binding::SelectedCover)
                    || c.kind == Kind::Entries && c.art_size.unwrap_or(0) > 0
            });
            let hero_banner = hero_banner(&id, screen);
            for slot in &slots {
                if slot.logo_when_no_cover != (has_cover && !hero_banner) {
                    violations.push(format!("{id}/{screen:?}/{}: incorrect cover rule", slot.id));
                }
            }
            let mut input = frame(screen);
            input.art = Default::default();
            input.focus = Focus::default();
            input.art.selected_hero = Some(hero.clone());
            let text = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            input.art.selected_logo = Some(logo.clone());
            let fallback = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            for slot in &slots {
                let r = slot.rect;
                assert!(
                    (r.1..r.1 + r.3).any(|y| (r.0..r.0 + r.2)
                        .any(|x| text.pixels().pixel(x, y) != fallback.pixels().pixel(x, y))),
                    "{id}/{screen:?}/{}: missing cover must show logo",
                    slot.id
                );
            }
            input.art.selected_cover = Some(cover.clone());
            for row in &input.rows[&Binding::Entries] {
                input.art.entries.insert(row.id.clone(), cover.clone());
            }
            let covered = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            input.art.selected_logo = None;
            let covered_text = PreparedFrame::prepare(theme.clone(), &input).unwrap();
            input.art.selected_logo = Some(logo.clone());
            if has_cover {
                covered_views += 1;
                if hero_banner {
                    hero_views += 1;
                    if covered.pixels().data() == covered_text.pixels().data() {
                        violations.push(format!("{id}/{screen:?}: hero banner must keep its logo"));
                    }
                } else if covered.pixels().data() != covered_text.pixels().data() {
                    violations.push(format!("{id}/{screen:?}: logo drawn beside a single cover"));
                }
                if let Ok(out) = std::env::var("ALLIUM_LOGO_PROOF_DIR") {
                    let dir = Path::new(&out);
                    std::fs::create_dir_all(dir).unwrap();
                    let view = serde_json::to_value(screen)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned();
                    covered
                        .pixels()
                        .save_png(dir.join(format!("{id}-{view}-cover-logo.png")))
                        .unwrap();
                    fallback
                        .pixels()
                        .save_png(dir.join(format!("{id}-{view}-logo-only.png")))
                        .unwrap();
                }
            } else {
                assert_ne!(
                    covered.pixels().data(),
                    covered_text.pixels().data(),
                    "{id}/{screen:?}: a loaded cover outside this layout must not hide the logo"
                );
            }
            let reused =
                PreparedFrame::prepare_reusing(theme.clone(), &input, Some(fallback)).unwrap();
            assert_eq!(
                reused.pixels().data(),
                covered.pixels().data(),
                "{id}/{screen:?}: cover arrival"
            );
            input.art.selected_cover = None;
            input.art.entries.clear();
            let reused =
                PreparedFrame::prepare_reusing(theme.clone(), &input, Some(reused)).unwrap();
            assert_eq!(
                reused.pixels().data(),
                PreparedFrame::prepare(theme.clone(), &input)
                    .unwrap()
                    .pixels()
                    .data(),
                "{id}/{screen:?}: cover removal"
            );
        }
    }
    assert_eq!((covered_views - hero_views, hero_views, logo_views), (63, 7, 98));
    println!(
        "LOGO_COVER_AUDIT {} single-cover views, {hero_views} hero banner views, {logo_views} logo views, {} violations",
        covered_views - hero_views,
        violations.len()
    );
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn media_logo_cover_shelf_keeps_hero_title_with_selected_cover() {
    let bank = FontBank::bind(&font_root()).unwrap();
    let logo = art(96, 24, tiny_skia::Color::from_rgba8(250, 10, 80, 255));
    let cover = art(96, 128, tiny_skia::Color::from_rgba8(20, 90, 180, 255));
    for (id, snapshot) in variants() {
        let theme = ThemeHandle::prepare(snapshot, &bank, 904).unwrap();
        let components = &theme.pack().screens[&ScreenId::Library].components;
        if !components
            .iter()
            .any(|c| c.kind == Kind::Entries && c.art_size.unwrap_or(0) > 0)
        {
            continue;
        }
        let mut input = frame(ScreenId::Library);
        input.focus = Focus::default();
        input.art = Default::default();
        input.art.entries.insert("r4".into(), cover.clone());
        input.art.entries.insert("wipeout".into(), cover.clone());
        let text = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        input.art.selected_logo = Some(logo.clone());
        let covered = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        assert!(hero_banner(&id, ScreenId::Library), "{id}: expected hero shelf");
        assert_ne!(
            covered.pixels().data(),
            text.pixels().data(),
            "{id}: selected shelf cover must not hide the hero title"
        );
        let covered_pixels = covered.pixels().data().to_vec();
        input.art.entries.remove("r4");
        let fallback =
            PreparedFrame::prepare_reusing(theme.clone(), &input, Some(covered)).unwrap();
        assert_eq!(
            fallback.pixels().data(),
            PreparedFrame::prepare(theme.clone(), &input)
                .unwrap()
                .pixels()
                .data(),
            "{id}: retained shelf cover removal"
        );
        input.art.selected_logo = None;
        assert_ne!(
            fallback.pixels().data(),
            PreparedFrame::prepare(theme.clone(), &input)
                .unwrap()
                .pixels()
                .data(),
            "{id}: another game's cover must not hide the logo"
        );
        input.art.selected_logo = Some(logo.clone());
        input.art.entries.insert("r4".into(), cover.clone());
        let restored =
            PreparedFrame::prepare_reusing(theme.clone(), &input, Some(fallback)).unwrap();
        assert_eq!(
            restored.pixels().data(),
            covered_pixels,
            "{id}: retained shelf cover arrival"
        );
    }
}
