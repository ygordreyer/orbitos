use super::*;

#[test]
fn compact_plus_stacked_clock_fits_every_minute_without_ellipsis() {
    let theme = crate::theme::render_navigation::tests::theme("compact-plus", "square");
    for screen in [ScreenId::Recents, ScreenId::GameDetail] {
        let status = theme.pack().screens[&screen].components.iter()
            .find(|c| c.kind == Kind::Status).unwrap();
        let mut frame = PreparedFrame::unpainted(theme.clone(), screen);
        let mut input = FrameInputs::new(screen);
        input.status.battery = Some(100);
        input.status.charging = true;
        input.status.network = Some("online".into());
        for minute in 0..24 * 60 {
            let clock = format!("{:02}:{:02}", minute / 60, minute % 60);
            input.status.clock = Some(clock.clone());
            TEXT_PLAN.with(|plan| *plan.borrow_mut() = Some(Vec::new()));
            let result = frame.ink_status_contents(status, &input);
            let texts = TEXT_PLAN.with(|plan| plan.borrow_mut().take().unwrap());
            result.unwrap();
            let text = texts.iter().find(|text| text.value == clock).unwrap();
            assert_eq!(text.ty.lines(&text.value, text.rect.2, text.count, text.overflow),
                vec![clock.clone()], "{screen:?} {clock}");
            assert!(text.rect.0 >= status.rect.0 && text.rect.0 + text.rect.2 <= status.rect.0 + status.rect.2);
        }
    }
}

pub(super) fn reference_lines(layout: &InkTextLayout, text: &str, width: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let measured = |s: &str| layout.metrics(&layout.value(s)).0.ceil();
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            continue;
        }
        if !line.is_empty() && measured(&format!("{line}{ch}")) > width as f32 {
            if let Some(space) = line
                .char_indices()
                .filter(|(_, c)| c.is_whitespace())
                .map(|(i, _)| i)
                .last()
                .filter(|i| *i > 0)
            {
                lines.push(line[..space].trim_end().into());
                line = line[space..].trim_start().into();
            } else {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() && measured(&format!("{line}{ch}")) > width as f32 {
                lines.push(std::mem::take(&mut line));
            }
        }
        line.push(ch);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn themes() -> Vec<(String, ThemeHandle)> {
    use std::collections::BTreeMap;
    let root = root();
    let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let mut snapshots = Vec::new();
    for id in ["orbit", "compact"] {
        snapshots.push((id.to_owned(), ThemeSnapshot::builtin(id).unwrap()));
    }
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        let path = root.join("themes").join(base);
        let pack =
            crate::theme::pack::Pack::parse(&std::fs::read(path.join("theme.json")).unwrap())
                .unwrap();
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
            snapshots.push((
                id,
                ThemeSnapshot::load_with_settings(&path, &values).unwrap(),
            ));
        }
    }
    snapshots
        .into_iter()
        .map(|(id, snapshot)| (id, ThemeHandle::prepare(snapshot, &bank, 1).unwrap()))
        .collect()
}

fn corpus() -> Vec<String> {
    use std::collections::BTreeSet;
    fn json(v: &serde_json::Value, selected: bool, texts: &mut BTreeSet<String>) {
        match v {
            serde_json::Value::String(s) if selected => {
                texts.insert(s.clone());
            }
            serde_json::Value::Object(o) => {
                for (key, value) in o {
                    json(
                        value,
                        selected
                            || matches!(
                                key.as_str(),
                                "desc"
                                    | "description"
                                    | "synopsis"
                                    | "selected_blurb"
                                    | "selected_blurb_runs"
                            ),
                        texts,
                    );
                }
            }
            serde_json::Value::Array(a) => {
                for value in a {
                    json(value, selected, texts);
                }
            }
            _ => {}
        }
    }
    fn descriptions(source: &str, texts: &mut BTreeSet<String>) {
        for part in source.split("<desc>").skip(1) {
            if let Some((text, _)) = part.split_once("</desc>") {
                texts.insert(quick_xml::escape::unescape(text).unwrap().into_owned());
            }
        }
    }
    fn walk(path: &std::path::Path, texts: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                if !matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                    walk(&path, texts);
                }
                continue;
            }
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("json" | "xml" | "rs" | "txt")
            ) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(value) = serde_json::from_str(&source) {
                json(&value, false, texts);
            }
            if path.extension().is_some_and(|s| s == "rs") {
                if path.file_name().unwrap() == "render_ink_details_tests.rs"
                    || !["synopsis", "description", "<desc>"]
                        .iter()
                        .any(|s| source.contains(s))
                {
                    continue;
                }
                // Include prose literals from source fixtures as well as on-disk metadata.
                let mut rest = source.as_str();
                while let Some(start) = rest.find('"') {
                    let context: String = rest[..start].lines().rev().take(2).collect();
                    let relevant = ["desc", "synopsis"].iter().any(|s| context.contains(s));
                    rest = &rest[start + 1..];
                    let mut escaped = false;
                    let Some(end) = rest.char_indices().find_map(|(i, ch)| {
                        if escaped {
                            escaped = false;
                            None
                        } else if ch == '\\' {
                            escaped = true;
                            None
                        } else if ch == '"' {
                            Some(i)
                        } else {
                            None
                        }
                    }) else {
                        break;
                    };
                    if let Ok(value) =
                        serde_json::from_str::<String>(&format!("\"{}\"", &rest[..end]))
                    {
                        descriptions(&value, texts);
                        if (value.chars().count() >= 24 || relevant || value.contains("synopsis"))
                            && !value.contains(['<', '>'])
                        {
                            texts.insert(value);
                        }
                    }
                    rest = &rest[end + 1..];
                }
            } else {
                descriptions(&source, texts);
            }
        }
    }
    let mut texts = BTreeSet::new();
    for dir in ["static", "themes", "test-fixtures", "crates"] {
        walk(&root().join(dir), &mut texts);
    }
    if let Ok(path) = std::env::var("ALLIUM_DETAILS_METADATA_FIXTURE") {
        json(
            &serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap(),
            false,
            &mut texts,
        );
    }
    texts.extend([
        "".into(),
        "AV To VA WA Yo Ta ffi ﬃ Straße ß ŉ ΐ Ελληνικά é e\u{301} 日本語 中國".into(),
        "\n\n leading  spaces \t and\twords\r\ntrailing spaces   \n".into(),
        "AVTo".repeat(512),
        "日本語中國".repeat(60),
        "e\u{301}\u{200b}\u{200d}\u{feff}".repeat(64),
        "\u{200b}".repeat(512),
        "A long description with accents é and a game story. ".repeat(40),
        "A bounded fixture description. ".repeat(12),
    ]);
    texts.into_iter().collect()
}

fn reference<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            REFERENCE_WIDTHS.with(|r| r.set(self.0));
        }
    }
    let _reset = Reset(REFERENCE_WIDTHS.with(|r| r.replace(true)));
    f()
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "exhaustive outline oracle; run with --release --include-ignored"
)]
fn details_prefix_matches_whole_prefix_oracle_all_packs_and_corpus() {
    let themes = themes();
    let texts = corpus();
    let mut widths = std::collections::BTreeSet::from([0, 1, 37]);
    for (_, theme) in &themes {
        for c in &theme.pack().screens[&ScreenId::GameDetail].components {
            if c.binding == Some(Binding::SelectedBlurb)
                || theme.pack().id == "compact" && c.binding == Some(Binding::Entries)
            {
                widths.insert(c.rect.2);
            }
        }
    }
    println!(
        "DETAILS_CORPUS themes={} texts={} widths={widths:?}",
        themes.len(),
        texts.len()
    );
    let mut cases = 0;
    for (id, theme) in &themes {
        let mut slot = theme.pack().screens[&ScreenId::GameDetail]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::SelectedBlurb))
            .or_else(|| {
                theme.pack().screens[&ScreenId::GameDetail]
                    .components
                    .iter()
                    .find(|c| c.binding == Some(Binding::Entries))
            })
            .unwrap()
            .clone();
        // Compact has no synopsis slot; exercise its details text column and role.
        slot.paint.get_or_insert_with(Default::default);
        let c = &slot;
        for original in [ink_text_layout(theme, c, false), detail_heading(theme, c)] {
            for (upper, tracking) in [
                (original.upper, original.tracking),
                (true, 1.25),
                (false, -0.25),
            ] {
                let mut layout = original.clone();
                layout.upper = upper;
                layout.tracking = tracking;
                if !theme.pack().ink() {
                    layout.font = theme.role_font(c.role.unwrap());
                }
                for text in &texts {
                    let value = layout.value(text);
                    assert_eq!(
                        layout.advance_width(&value).to_bits(),
                        layout.metrics(&value).0.to_bits(),
                        "advance bits: {id}, upper {upper}, tracking {tracking}, text {text:?}"
                    );
                }
                for &width in &widths {
                    for text in &texts {
                        assert_eq!(
                            layout.detail_lines(text, width),
                            reference_lines(&layout, text, width),
                            "{id}, width {width}, upper {upper}, tracking {tracking}, text {text:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        for separate in [false, true] {
            let sections: Vec<_> = texts
                .iter()
                .map(|text| (Some("AV To ß SYNOPSIS".into()), text.clone()))
                .collect();
            let expected = reference(|| ink_detail_pages(theme, c, &sections, separate));
            assert_eq!(
                ink_detail_pages(theme, c, &sections, separate),
                expected,
                "pages: {id}/{separate}"
            );
        }
    }
    println!(
        "DETAILS_PARITY themes={} corpus={} widths={widths:?} line_cases={cases}",
        themes.len(),
        texts.len()
    );
}

#[test]
#[ignore = "host details wrapping micro-benchmark"]
fn benchmark_details_prefix() {
    let (_, theme) = themes()
        .into_iter()
        .find(|(id, _)| id.starts_with("orbit-hero"))
        .unwrap();
    let c = theme.pack().screens[&ScreenId::GameDetail]
        .components
        .iter()
        .find(|c| c.binding == Some(Binding::SelectedBlurb))
        .unwrap();
    let layout = ink_text_layout(&theme, c, false);
    let text =
        "AV To: A long synopsis with accents é, exploration and a story to discover. ".repeat(100);
    let mut old = Vec::new();
    let mut new = Vec::new();
    for _ in 0..9 {
        let start = std::time::Instant::now();
        std::hint::black_box(reference_lines(
            &layout,
            std::hint::black_box(&text),
            c.rect.2,
        ));
        old.push(start.elapsed().as_nanos());
        let start = std::time::Instant::now();
        std::hint::black_box(layout.detail_lines(std::hint::black_box(&text), c.rect.2));
        new.push(start.elapsed().as_nanos());
    }
    old.sort_unstable();
    new.sort_unstable();
    println!(
        "DETAILS_BENCH chars={} width={} samples=9 old_median_ns={} new_median_ns={} speedup={:.2}",
        text.chars().count(),
        c.rect.2,
        old[4],
        new[4],
        old[4] as f64 / new[4] as f64
    );
}

#[test]
fn details_prefix_zero_advance_work_is_linear() {
    let (_, theme) = themes()
        .into_iter()
        .find(|(id, _)| id.starts_with("orbit-hero"))
        .unwrap();
    let c = theme.pack().screens[&ScreenId::GameDetail]
        .components
        .iter()
        .find(|c| c.binding == Some(Binding::SelectedBlurb))
        .unwrap();
    let mut layout = ink_text_layout(&theme, c, false);
    layout.font = theme.ink_font(FontId::Roboto, 400);
    layout.tracking = 0.0;
    let source = "\u{200b}".repeat(8192);
    assert_eq!(layout.metrics("\u{200b}").0, 0.0);
    DETAIL_GLYPH_VISITS.with(|n| n.set(0));
    assert_eq!(layout.detail_lines(&source, 0), vec![source]);
    assert_eq!(DETAIL_GLYPH_VISITS.with(|n| n.get()), 8192);
}

#[test]
fn details_prefix_render_pixels_match_outline_width_reference() {
    use crate::theme::frame::{Hint, Pagination};
    for (id, theme) in themes() {
        if !theme.pack().ink() {
            continue;
        }
        let c = theme.pack().screens[&ScreenId::GameDetail]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::SelectedBlurb))
            .unwrap();
        let sections = vec![
            (
                Some("SYNOPSIS AV To ß".into()),
                "AV To é e\u{301} 日本語: A long synopsis about adventure and exploration. "
                    .repeat(8),
            ),
            (
                Some("PROVENANCE".into()),
                "First release: 1998\nSource: fixture".into(),
            ),
        ];
        let pages = ink_detail_pages(&theme, c, &sections, true);
        assert_eq!(
            pages,
            reference(|| ink_detail_pages(&theme, c, &sections, true))
        );
        for (page, lines) in pages.iter().enumerate() {
            let mut input = FrameInputs::new(ScreenId::GameDetail);
            input.detail_lines = lines.clone();
            input.text.insert(
                Binding::SelectedTitle,
                "AV To: A long title that needs an ellipsis, é 日本語".into(),
            );
            input.text.insert(
                Binding::SelectedBlurb,
                lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            input.pagination = Some(Pagination {
                page: page + 1,
                total: pages.len(),
            });
            input.status.clock = Some("12:34".into());
            input.status.battery = Some(87);
            input.status.charging = true;
            input.hints = vec![
                Hint {
                    button: "A".into(),
                    label: "Play AV To".into(),
                },
                Hint {
                    button: "B".into(),
                    label: "Back".into(),
                },
            ];
            let expected =
                reference(|| uncached(|| PreparedFrame::prepare(theme.clone(), &input).unwrap()));
            let actual = uncached(|| PreparedFrame::prepare(theme.clone(), &input).unwrap());
            assert_eq!(
                actual.pixels().data(),
                expected.pixels().data(),
                "pixels: {id}, page {page}"
            );
        }
    }
}
