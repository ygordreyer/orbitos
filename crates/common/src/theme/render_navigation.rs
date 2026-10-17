//! UI-thread navigation cache work. A stamp expires on every presented model preparation.
use super::{
    frame::FrameInputs,
    pack::{Component, Rect},
};
use std::sync::{Arc, Weak};
use tiny_skia::Pixmap;

#[derive(Clone, Debug)]
pub struct NavigationStamp(pub(super) Weak<()>);
pub(super) enum NavigationRasterPart {
    Hero,
    Title,
    ShelfRow(usize),
    Peek,
    InkComponent,
    InkShelfRow(usize),
    InkListRow(usize),
    InkPeek,
}
pub struct NavigationRasterWarm {
    pub(super) stamp: NavigationStamp,
    pub(super) component: Component,
    pub(super) input: Arc<FrameInputs>,
    pub(super) part: NavigationRasterPart,
}
pub struct NavigationImageWarm {
    pub(super) stamp: NavigationStamp,
    pub(super) component: Component,
    pub(super) source: Arc<Pixmap>,
    pub(super) rect: Rect,
    pub(super) radius: u32,
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::theme::{
        FontBank, ThemeHandle,
        frame::Row,
        pack::{Binding, Kind, ScreenId},
        render::PreparedFrame,
        runtime::ThemeSnapshot,
    };
    use std::{collections::BTreeMap, path::Path, time::Instant};

    pub(in crate::theme) fn theme(id: &str, shape: &str) -> ThemeHandle {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let snapshot = if id == "compact" {
            ThemeSnapshot::builtin(id).unwrap()
        } else {
            ThemeSnapshot::load_with_settings(
                &root.join("themes").join(id),
                &BTreeMap::from([("cover_shape".into(), shape.into())]),
            )
            .unwrap()
        };
        ThemeHandle::prepare(snapshot, &bank, 37).unwrap()
    }
    pub(in crate::theme) fn arts() -> Vec<Arc<Pixmap>> {
        (0..16)
            .map(|seed| {
                let mut image = Pixmap::new(93 + seed, 71).unwrap();
                for (i, pixel) in image.data_mut().chunks_exact_mut(4).enumerate() {
                    pixel.copy_from_slice(&[
                        ((i + seed as usize * 31) % 173) as u8,
                        ((i / 11 + seed as usize * 19) % 173) as u8,
                        91,
                        173,
                    ]);
                }
                Arc::new(image)
            })
            .collect()
    }
    pub(in crate::theme) fn model(
        theme: &ThemeHandle,
        screen: ScreenId,
        selected: usize,
        arts: &[Arc<Pixmap>],
    ) -> FrameInputs {
        let mut input = FrameInputs::new(screen);
        let entries = theme.pack().screens[&screen]
            .components
            .iter()
            .find(|c| c.kind == Kind::Entries)
            .unwrap();
        let count = entries.visible_count.unwrap() as usize;
        let first = selected.saturating_sub(count - 1);
        let rows: Vec<_> = (first..first + count)
            .map(|i| Row {
                id: i.to_string(),
                label: format!("Game {i}: the golden hour"),
                subtitle: String::new(),
                value: String::new(),
                enabled: true,
            })
            .collect();
        input.focus.component = entries.id.clone();
        input.focus.item = Some(selected.to_string());
        input
            .selected
            .insert(Binding::Entries, selected.to_string());
        input.rows.insert(Binding::Entries, rows);
        input.status.clock = Some("12:34".into());
        input.status.battery = Some(73);
        for (binding, value) in [
            (Binding::Title, "Library".into()),
            (Binding::Breadcrumb, "Library".into()),
            (
                Binding::SelectedTitle,
                format!("Game {selected}: the golden hour"),
            ),
            (
                Binding::SelectedMetadata,
                format!("PlayStation · {}", 1997 + selected),
            ),
            (
                Binding::SelectedBlurb,
                format!("Story {selected}. A bounded fixture description."),
            ),
        ] {
            input.text.insert(binding, value);
        }
        input.art.selected_cover = Some(arts[selected].clone());
        input.art.selected_hero = Some(arts[selected].clone());
        input.art.selected_logo = Some(arts[(selected + 5) % arts.len()].clone());
        input.art.next_cover = Some(arts[(first + count) % arts.len()].clone());
        for i in first..first + count {
            input
                .art
                .entries
                .insert(i.to_string(), arts[i % arts.len()].clone());
        }
        input
    }
    fn warm(frame: &mut PreparedFrame, next: &FrameInputs) {
        let pixels = frame.pixels().data().to_vec();
        let targets = frame.targets.clone();
        let text = frame.text_boxes.clone();
        for task in frame.navigation_images(next) {
            frame.prewarm_navigation_image(task).unwrap();
        }
        for task in frame.navigation_rasters(next) {
            frame.prewarm_navigation_raster(task).unwrap();
            assert!(
                frame.pixels().data() == pixels,
                "idle work changed presented pixels"
            );
            assert_eq!(frame.targets, targets);
            assert_eq!(frame.text_boxes, text);
        }
    }
    fn traverse(benchmark: bool) {
        let trace = std::env::temp_dir().join(format!(
            "nav-prepared-{:?}.log",
            std::thread::current().id()
        ));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        let arts = arts();
        for id in ["orbit-hero", "big-picture", "compact-plus", "compact"] {
            for shape in ["square", "tall"] {
                if id == "compact" && shape == "tall" {
                    continue;
                }
                let theme = theme(id, shape);
                for screen in [ScreenId::Library, ScreenId::Recents] {
                    let mut frame =
                        PreparedFrame::prepare(theme.clone(), &model(&theme, screen, 0, &arts))
                            .unwrap();
                    assert!(frame.can_prewarm_navigation(), "{id} {shape} {screen:?}");
                    for selected in 1..9 {
                        let next = model(&theme, screen, selected, &arts);
                        warm(&mut frame, &next);
                        crate::perf_trace::begin_render();
                        #[cfg(feature = "perf-bench")]
                        crate::perf::take();
                        let started = Instant::now();
                        frame = PreparedFrame::prepare_reusing(theme.clone(), &next, Some(frame))
                            .unwrap();
                        let elapsed = started.elapsed().as_micros();
                        let counts = crate::perf_trace::probe_counts();
                        #[cfg(feature = "perf-bench")]
                        {
                            let perf = crate::perf::take();
                            assert_eq!(
                                perf.fits, 0,
                                "{id} {shape} {screen:?} {selected}: {perf:?}"
                            );
                            assert_eq!(perf.decodes + perf.art_resizes, 0);
                        }
                        assert_eq!(
                            counts["image_misses"], 0,
                            "{id} {shape} {screen:?} {selected}: {counts}"
                        );
                        assert_ne!(
                            counts["render_reason"], "ink_initial",
                            "{id} {shape} {screen:?} {selected}: {counts}"
                        );
                        assert_ne!(counts["render_reason"], "unsupported_layout");
                        if benchmark {
                            println!(
                                "NAV_FORWARD theme={id} shape={shape} screen={screen:?} selected={selected} wall_us={elapsed} compose_us={} image_misses={}",
                                counts["compose_paint_cpu_us"], counts["image_misses"]
                            );
                        }
                        let fresh =
                            crate::theme::render::navigation_reference(theme.clone(), &next)
                                .unwrap();
                        assert!(
                            frame.pixels().data() == fresh.pixels().data(),
                            "{id} {shape} {screen:?} {selected}"
                        );
                        assert_eq!(frame.targets, fresh.targets);
                        assert_eq!(frame.text_boxes, fresh.text_boxes);
                    }
                }
            }
        }
        std::fs::remove_file(trace).unwrap();
    }
    #[test]
    fn navigation_prepared_forward_has_no_image_misses_and_matches_fresh() {
        traverse(false);
    }

    fn fitted_input(theme: &ThemeHandle, input: &FrameInputs) -> FrameInputs {
        let targets = crate::theme::art::Targets::for_screens(theme.pack(), &[input.screen]);
        let fit = |source: &Arc<Pixmap>, target: Option<crate::theme::art::Target>, logo: bool| {
            if let Some(target) = target {
                let target = if logo { target.logo(source.width(), source.height()) } else { target };
                let pixels = crate::theme::runtime::fit_image(source, target.width, target.height, target.fit).unwrap();
                let identity = crate::theme::runtime::fit_image(&pixels, target.width, target.height, target.fit).unwrap();
                assert_eq!(pixels.data(), identity.data(), "identity fit changed bytes");
                Arc::new(pixels)
            } else { source.clone() }
        };
        let mut fitted = input.clone();
        fitted.art.selected_hero = input.art.selected_hero.as_ref().map(|source| fit(source, targets.hero, false));
        fitted.art.selected_logo = input.art.selected_logo.as_ref().map(|source| fit(source, targets.logo, true));
        fitted.art.selected_cover = input.art.selected_cover.as_ref().map(|source| fit(source, targets.cover, false));
        fitted.art.next_cover = input.art.next_cover.as_ref().map(|source| fit(source, targets.cover, false));
        for (id, source) in &input.art.entries {
            fitted.art.entries.insert(id.clone(), fit(source, targets.cover, false));
        }
        fitted
    }

    #[test]
    fn navigation_fitted_pixels_match_source_with_effects_and_replacement() {
        for id in ["orbit-hero", "big-picture", "compact-plus"] {
            for shape in ["square", "tall"] {
                let theme = theme(id, shape);
                let mut sources = arts();
                for (index, &(w, h)) in [(101, 101), (79, 213), (137, 83)].iter().enumerate() {
                    let mut image = Pixmap::new(w, h).unwrap();
                    for y in 1..h - 1 {
                        for x in 1..w - 1 {
                            let at = ((y * w + x) * 4) as usize;
                            let alpha = ((x * 13 + y * 7) % 256) as u8;
                            image.data_mut()[at..at + 4].copy_from_slice(&[alpha / 3, alpha / 2, alpha, alpha]);
                        }
                    }
                    sources[index] = Arc::new(image);
                }
                for screen in [ScreenId::Library, ScreenId::Recents] {
                    let mut retained = None;
                    for selected in [0, 1, 2, 0] {
                        let original = model(&theme, screen, selected, &sources);
                        let reference = crate::theme::render::navigation_reference(theme.clone(), &original).unwrap();
                        let fitted = fitted_input(&theme, &original);
                        // A cold fitted source, a reconstructed disk payload, and the
                        // same RAM Arc all pass through the complete effects pipeline.
                        for path in 0..3 {
                            let mut input = fitted.clone();
                            if path == 1 {
                                let reload = |source: &Arc<Pixmap>| Arc::new(Pixmap::from_vec(source.data().to_vec(), tiny_skia::IntSize::from_wh(source.width(), source.height()).unwrap()).unwrap());
                                input.art.selected_hero = input.art.selected_hero.as_ref().map(reload);
                                input.art.selected_cover = input.art.selected_cover.as_ref().map(reload);
                                input.art.selected_logo = input.art.selected_logo.as_ref().map(reload);
                                input.art.next_cover = input.art.next_cover.as_ref().map(reload);
                                for source in input.art.entries.values_mut() { *source = reload(source); }
                            }
                            let frame = PreparedFrame::prepare_reusing(theme.clone(), &input, retained.take()).unwrap();
                            assert!(frame.pixels().data() == reference.pixels().data(), "{id} {shape} {screen:?} {selected} path={path}");
                            retained = Some(frame);
                        }
                        // Replacement keeps the path/row identity but changes pixels.
                        let mut replacement = (*sources[selected]).clone();
                        replacement.fill(tiny_skia::Color::from_rgba8(21, 39, 13, 197));
                        sources[selected] = Arc::new(replacement);
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "explicit monotonic forward traversal benchmark"]
    fn navigation_forward_benchmark() {
        traverse(true);
    }

    #[test]
    #[ignore = "explicit unprepared forward traversal baseline"]
    fn navigation_forward_unprepared_benchmark() {
        let trace = std::env::temp_dir().join(format!(
            "nav-unprepared-{:?}.log",
            std::thread::current().id()
        ));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        let arts = arts();
        for id in ["orbit-hero", "big-picture", "compact-plus", "compact"] {
            for shape in ["square", "tall"] {
                if id == "compact" && shape == "tall" {
                    continue;
                }
                let theme = theme(id, shape);
                for screen in [ScreenId::Library, ScreenId::Recents] {
                    let mut frame =
                        PreparedFrame::prepare(theme.clone(), &model(&theme, screen, 0, &arts))
                            .unwrap();
                    for selected in 1..9 {
                        let next = model(&theme, screen, selected, &arts);
                        crate::perf_trace::begin_render();
                        let started = Instant::now();
                        frame = PreparedFrame::prepare_reusing(theme.clone(), &next, Some(frame))
                            .unwrap();
                        let counts = crate::perf_trace::probe_counts();
                        println!(
                            "NAV_UNPREPARED theme={id} shape={shape} screen={screen:?} selected={selected} wall_us={} compose_us={} image_misses={}",
                            started.elapsed().as_micros(),
                            counts["compose_paint_cpu_us"],
                            counts["image_misses"]
                        );
                    }
                }
            }
        }
        std::fs::remove_file(trace).unwrap();
    }

    #[test]
    fn navigation_snapshot_comparison_preserves_art_outside_the_entries_binding() {
        let theme = theme("big-picture", "square");
        let arts = arts();
        let mut input = model(&theme, ScreenId::Library, 0, &arts);
        input
            .art
            .entries
            .insert("not-a-visible-row".into(), arts[15].clone());
        let frame = PreparedFrame::prepare(theme, &input).unwrap();
        assert!(frame.input_matches(&input));
        assert!(frame.navigation_model().unwrap().art == input.art);
        input
            .art
            .entries
            .insert("not-a-visible-row".into(), arts[14].clone());
        assert!(!frame.input_matches(&input));
    }

    #[test]
    fn navigation_rejects_expired_frame_theme_settings_and_media_work() {
        let theme = theme("big-picture", "square");
        let arts = arts();
        let input = model(&theme, ScreenId::Library, 0, &arts);
        let next = model(&theme, ScreenId::Library, 1, &arts);
        let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let stamp = frame.navigation_stamp();
        let work = frame.navigation_rasters(&next);
        let mut changed = input.clone();
        changed.art.selected_cover = Some(arts[2].clone());
        let mut frame =
            PreparedFrame::prepare_reusing(theme.clone(), &changed, Some(frame)).unwrap();
        assert!(!frame.navigation_is_current(&stamp));
        let before = frame.pixels().data().to_vec();
        for task in work {
            frame.prewarm_navigation_raster(task).unwrap();
        }
        assert!(frame.pixels().data() == before);
        let work = frame.navigation_rasters(&next);
        let other = super::tests::theme("big-picture", "tall");
        let mut frame = PreparedFrame::prepare_reusing(
            other.clone(),
            &model(&other, ScreenId::Library, 0, &arts),
            Some(frame),
        )
        .unwrap();
        let before = frame.pixels().data().to_vec();
        for task in work {
            frame.prewarm_navigation_raster(task).unwrap();
        }
        assert!(frame.pixels().data() == before);
    }
}
