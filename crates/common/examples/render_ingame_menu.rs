//! Offline native-renderer evidence: output directory, then optional game-logo PNG.
use anyhow::{Context, Result};
use common::theme::{
    FontBank, ThemeHandle,
    frame::{Focus, FrameInputs, Hint, Row},
    pack::{Binding, ScreenId},
    render::PreparedFrame,
    runtime::ThemeSnapshot,
};
use std::{path::PathBuf, sync::Arc};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let out = PathBuf::from(args.next().context("output directory required")?);
    let logo = args
        .next()
        .map(tiny_skia::Pixmap::load_png)
        .transpose()?
        .map(Arc::new);
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let bank = FontBank::bind(&repo.join("static/.allium/fonts"))?;
    std::fs::create_dir_all(&out)?;
    for name in [
        "compact",
        "orbit",
        "compact-plus",
        "big-picture",
        "orbit-hero",
        "orbit-row",
    ] {
        let snapshot = if matches!(name, "compact" | "orbit") {
            ThemeSnapshot::builtin(name)?
        } else {
            ThemeSnapshot::load(&repo.join("themes").join(name))?
        };
        let theme = ThemeHandle::prepare(snapshot, &bank, 1)?;
        let capacity = theme.pack().screens[&ScreenId::IngameMenu]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::Entries))
            .unwrap()
            .visible_count
            .unwrap() as usize;
        for (scene, with_logo, disk, slot) in [
            ("seven-logo", true, false, "Slot 0"),
            ("seven-text-auto", false, false, "Auto"),
            ("eight-logo-disk", true, true, "Slot 0"),
        ] {
            let mut input = FrameInputs::new(ScreenId::IngameMenu);
            input
                .text
                .insert(Binding::Title, "R4 - Ridge Racer Type 4".into());
            if with_logo {
                input.art.selected_logo = logo.clone();
            }
            let mut rows: Vec<Row> = [
                "Continue", "Save", "Load", "Guide", "Settings", "Reset", "Quit",
            ]
            .into_iter()
            .map(|label| Row {
                id: label.to_lowercase(),
                label: label.into(),
                subtitle: String::new(),
                value: if matches!(label, "Save" | "Load") {
                    slot.into()
                } else {
                    String::new()
                },
                enabled: true,
            })
            .collect();
            if disk {
                rows.insert(
                    1,
                    Row {
                        id: "disk".into(),
                        label: "Disk".into(),
                        value: "Disk 2".into(),
                        subtitle: String::new(),
                        enabled: true,
                    },
                );
            }
            let total = rows.len();
            rows.truncate(capacity);
            input.rows.insert(Binding::Entries, rows);
            input.focus = Focus {
                component: "rows".into(),
                item: Some("save".into()),
            };
            input.hints = [("Up/Down", "Choose"), ("A", "Select"), ("B", "Back")]
                .map(|(button, label)| Hint {
                    button: button.into(),
                    label: label.into(),
                })
                .into();
            let rendered = PreparedFrame::prepare(theme.clone(), &input)?;
            let mut pixels = common::theme::runtime::fit_image(
                &tiny_skia::Pixmap::load_png(repo.join("test-fixtures/synthetic-art/sample-b-hero.png"))?,
                640,
                480,
                common::theme::pack::Fit::Cover,
            )?;
            pixels.draw_pixmap(
                0,
                0,
                rendered.pixels().as_ref(),
                &tiny_skia::PixmapPaint::default(),
                tiny_skia::Transform::identity(),
                None,
            );
            let path = out.join(format!("{name}-{scene}.png"));
            pixels.save_png(&path)?;
            println!(
                "{}: {}/{total} rows",
                path.display(),
                input.rows[&Binding::Entries].len()
            );
        }
    }
    Ok(())
}
