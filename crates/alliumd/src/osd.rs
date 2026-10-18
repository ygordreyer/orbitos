use std::time::Duration;

use anyhow::{Result, ensure};
use common::constants::UI_FRAME_INTERVAL;
use common::display::{
    Display, RectHold, draw_speaker_icon, draw_sun_icon, fill_rounded_rect,
    overlay::{OverlaySurface, PreparedOverlay},
};
use common::geom::Rect;
use common::platform::Platform;
use common::stylesheet::Stylesheet;
use common::theme::{
    ThemeHandle,
    frame::FrameInputs,
    pack::{Binding, ScreenId},
    render::PreparedFrame,
};
use log::{info, warn};
use tokio::time::Instant;

const HIDE_TIMEOUT: Duration = common::osd::OSD_DURATION;
const UI_REDRAW_PERIOD: Duration = Duration::from_micros(UI_FRAME_INTERVAL.as_micros() as u64 / 2);

pub use common::osd::OsdKind;

#[derive(Clone, Copy)]
struct Plate {
    rect: Rect,
    icon: Rect,
    bar: Rect,
    radius: u32,
}
impl Plate {
    fn new(display: &impl Display, styles: &Stylesheet) -> Self {
        let font = styles.ui.ui_font.size;
        let px = styles.ui.margin_x.max(0) as u32;
        let py = styles.ui.margin_y.max(0) as u32;
        let icon = font * 2 / 3;
        let bar_w = display.width() / 4;
        let bar_h = font / 3;
        let w = px + icon + px + bar_w + px;
        let h = font + py * 2;
        let hint = styles.button_size().max(styles.button_hint_font_size()) as u32;
        let rect = Rect::new(
            (display.width() as i32 - w as i32) / 2,
            display.height() as i32 - (h + px + hint + py) as i32,
            w,
            h,
        );
        Self {
            rect,
            icon: Rect::new(
                rect.x + px as i32,
                rect.y + ((h - icon) / 2) as i32,
                icon,
                icon,
            ),
            bar: Rect::new(
                rect.x + (px + icon + px) as i32,
                rect.y + ((h - bar_h) / 2) as i32,
                bar_w,
                bar_h,
            ),
            radius: px,
        }
    }
}

enum LegacyRefresh {
    Periodic { next: Instant },
    Continuous(RectHold),
}
struct Legacy<D: Display> {
    display: D,
    plate: Plate,
    refresh: Option<LegacyRefresh>,
    surface: Option<OverlaySurface>,
    faulted: bool,
}
impl<D: Display> Legacy<D> {
    fn new(mut display: D, styles: &Stylesheet) -> Result<Self> {
        let plate = Plate::new(&display, styles);
        display.read_rect(plate.rect)?;
        display.save()?;
        Ok(Self {
            display,
            plate,
            refresh: None,
            surface: None,
            faulted: false,
        })
    }
    fn stop_writer(&mut self) -> Result<()> {
        ensure!(
            !self.faulted,
            "Legacy OSD faulted; explicit recovery required"
        );
        OverlaySurface::replace(&mut self.display, &mut self.surface, None)
            .inspect_err(|_| self.faulted = true)?;
        if let Some(LegacyRefresh::Continuous(hold)) = self.refresh.take()
            && let Err(error) = hold.stop_and_join()
        {
            self.faulted = true;
            return Err(error);
        }
        Ok(())
    }
    fn draw(&mut self, styles: &Stylesheet, kind: OsdKind, fraction: f32) -> Result<()> {
        let Plate {
            rect,
            icon,
            bar,
            radius,
        } = self.plate;
        fill_rounded_rect(
            &mut self.display.pixmap_mut(),
            rect,
            radius,
            styles.menu.background_color,
        );
        match kind {
            OsdKind::Volume => {
                draw_speaker_icon(&mut self.display.pixmap_mut(), icon, styles.ui.text_color)
            }
            OsdKind::Brightness => {
                draw_sun_icon(&mut self.display.pixmap_mut(), icon, styles.ui.text_color)
            }
        }
        let radius = bar.h / 2;
        fill_rounded_rect(
            &mut self.display.pixmap_mut(),
            bar,
            radius,
            styles.ui.disabled_color,
        );
        let w = (bar.w as f32 * fraction.clamp(0.0, 1.0)).round() as u32;
        if w > 0 {
            fill_rounded_rect(
                &mut self.display.pixmap_mut(),
                Rect::new(bar.x, bar.y, w, bar.h),
                radius.min(w / 2),
                styles.ui.highlight_color,
            );
        }
        Ok(())
    }
    fn update(
        &mut self,
        styles: &Stylesheet,
        kind: OsdKind,
        fraction: f32,
        repainting: bool,
    ) -> Result<()> {
        self.stop_writer()?;
        self.display.pixmap_mut().data_mut().fill(0);
        self.draw(styles, kind, fraction)?;
        let rect = self.plate.rect;
        let candidate = PreparedOverlay::from_legacy_frame(
            kind.screen(),
            self.display.pixmap().to_owned(),
            common::theme::pack::Rect(rect.x as u32, rect.y as u32, rect.w, rect.h),
        );
        match candidate {
            Ok(candidate) => {
                let candidate = candidate.with_repainting(repainting);
                match self.display.check_overlay(&candidate) {
                    Ok(()) => return OverlaySurface::replace(&mut self.display, &mut self.surface, Some(&candidate))
                        .inspect_err(|_| self.faulted = true),
                    Err(error) => info!("osd legacy masked backend unavailable error={error:#}"),
                }
            }
            Err(error) => info!("osd legacy prepare error={error:#}"),
        }
        self.display.load(rect)?;
        if let Err(error) = self.draw(styles, kind, fraction)
            .and_then(|()| self.display.flush_rect(rect))
        {
            self.faulted = true;
            return Err(error);
        }
        let now = Instant::now();
        self.refresh = Some(if repainting {
            match self.display.hold_rect(self.plate.rect, self.plate.radius) {
                Ok(Some(hold)) => LegacyRefresh::Continuous(hold),
                Ok(None) => LegacyRefresh::Periodic {
                    next: now + UI_REDRAW_PERIOD,
                },
                Err(error) => {
                    self.faulted = true;
                    return Err(error);
                }
            }
        } else {
            LegacyRefresh::Periodic {
                next: now + UI_REDRAW_PERIOD,
            }
        });
        Ok(())
    }
    fn hide(&mut self) -> Result<()> {
        ensure!(
            !self.faulted,
            "Legacy OSD faulted; explicit recovery required"
        );
        if self.surface.is_some() {
            return OverlaySurface::replace(&mut self.display, &mut self.surface, None)
                .inspect_err(|_| self.faulted = true);
        }
        match self.refresh.take() {
            Some(LegacyRefresh::Periodic { .. }) => {
                self.display.load(self.plate.rect)?;
                self.display.flush_rect(self.plate.rect)
            }
            Some(LegacyRefresh::Continuous(hold)) => hold.stop_and_join(),
            None => Ok(()),
        }
        .inspect_err(|_| self.faulted = true)
    }
}

struct Themed<D: Display> {
    display: D,
    surface: Option<OverlaySurface>,
}
impl<D: Display> Themed<D> {
    fn install(mut display: D, candidate: &PreparedOverlay) -> Result<Self> {
        let mut surface = None;
        let result = OverlaySurface::replace(&mut display, &mut surface, Some(candidate));
        match result {
            Ok(()) => Ok(Self { display, surface }),
            Err(error) if surface.is_some() => {
                info!("osd install cleanup fault error={error:#}");
                Ok(Self { display, surface })
            },
            Err(error) => Err(error),
        }
    }
    fn replace(&mut self, candidate: &PreparedOverlay) -> Result<()> {
        if let Some(surface) = self.surface.as_mut()
            && surface.update(&mut self.display, candidate)? {
                info!("osd branch=themed_update_in_place");
                return Ok(());
            }
        info!("osd branch=themed_replace");
        OverlaySurface::replace(&mut self.display, &mut self.surface, Some(candidate))
    }
    fn hide(&mut self) -> Result<()> {
        OverlaySurface::replace(&mut self.display, &mut self.surface, None)
    }
    fn theme(&self) -> Option<ThemeHandle> {
        self.surface
            .as_ref()
            .and_then(|surface| surface.captured().snapshot().cloned())
    }
    fn faulted(&self) -> bool {
        self.surface.as_ref().is_some_and(OverlaySurface::faulted)
    }
}

enum Presentation<D: Display> {
    Legacy(Legacy<D>),
    Themed(Themed<D>),
}
impl<D: Display> Presentation<D> {
    fn faulted(&self) -> bool {
        match self {
            Self::Legacy(legacy) => legacy.faulted,
            Self::Themed(themed) => themed.faulted(),
        }
    }
}
struct Shown<D: Display> {
    presentation: Presentation<D>,
    hide_at: Instant,
}

pub struct Osd<P: Platform> {
    styles: Stylesheet,
    shown: Option<Shown<P::Display>>,
}
impl<P: Platform> Osd<P> {
    pub fn new(styles: Stylesheet) -> Self {
        Self {
            styles: styles.display_style(),
            shown: None,
        }
    }
    pub fn is_visible(&self) -> bool {
        self.shown.is_some()
    }
    pub fn next_wake(&self) -> Option<Instant> {
        self.shown.as_ref().and_then(|shown| {
            if shown.presentation.faulted() { return None; }
            Some(match &shown.presentation {
                Presentation::Legacy(Legacy {
                    refresh: Some(LegacyRefresh::Periodic { next }),
                    ..
                }) => shown.hide_at.min(*next),
                _ => shown.hide_at,
            })
        })
    }
    pub fn show(
        &mut self,
        platform: &mut P,
        kind: OsdKind,
        title: &str,
        fraction: f32,
        repainting: bool,
        opening_theme: Option<ThemeHandle>,
    ) -> Result<()> {
        info!("osd show kind={kind:?} repainting={repainting} theme={} visible_before={}", opening_theme.is_some(), self.is_visible());
        let start = std::time::Instant::now();
        let result = self.show_inner(platform, kind, title, fraction, repainting, opening_theme);
        if result.is_err() && self.shown.as_ref().is_some_and(|shown| shown.presentation.faulted()) {
            let _ = self.hide();
        }
        match &result {
            Ok(()) => info!("osd show installed_ms={}", start.elapsed().as_millis()),
            Err(error) => info!("osd show error={error:#} elapsed_ms={}", start.elapsed().as_millis()),
        }
        result
    }
    fn show_inner(
        &mut self, platform: &mut P, kind: OsdKind, title: &str, fraction: f32,
        repainting: bool, opening_theme: Option<ThemeHandle>,
    ) -> Result<()> {
        let fraction = fraction.clamp(0.0, 1.0);
        if let Some(shown) = self.shown.as_mut() {
            ensure!(
                !shown.presentation.faulted(),
                "OSD faulted; explicit recovery required"
            );
            match &mut shown.presentation {
                Presentation::Legacy(legacy) => {
                    info!("osd branch=legacy");
                    legacy.update(&self.styles, kind, fraction, repainting)?
                }
                Presentation::Themed(themed) => {
                    let theme = themed
                        .theme()
                        .ok_or_else(|| anyhow::anyhow!("Visible themed OSD lost its snapshot"))?;
                    let candidate = prepare_overlay(theme, kind, title, fraction, repainting)?;
                    themed.replace(&candidate)?;
                }
            }
            shown.hide_at = Instant::now() + HIDE_TIMEOUT;
            return Ok(());
        }
        if let Some(theme) = opening_theme {
            match prepare_overlay(theme, kind, title, fraction, repainting) {
                Ok(candidate) => {
                    let mut display = platform.display_partial()?;
                    if let Err(error) = display.check_overlay(&candidate) {
                        warn!(
                            "themed OSD unavailable before presentation, using Legacy: {error:#}"
                        );
                    } else {
                        info!("osd branch=themed_replace");
                        match Themed::install(display, &candidate) {
                            Ok(themed) if !themed.faulted() => {
                                self.shown = Some(Shown {
                                    presentation: Presentation::Themed(themed),
                                    hide_at: Instant::now() + HIDE_TIMEOUT,
                                });
                                return Ok(());
                            }
                            Ok(themed) => {
                                self.shown = Some(Shown {
                                    presentation: Presentation::Themed(themed),
                                    hide_at: Instant::now() + HIDE_TIMEOUT,
                                });
                                anyhow::bail!("Themed OSD installation faulted during cleanup");
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                Err(error) => warn!("failed to prepare themed OSD, using Legacy: {error:#}"),
            }
        }
        info!("osd branch=legacy");
        let mut legacy = Legacy::new(platform.display_partial()?, &self.styles)?;
        legacy.update(&self.styles, kind, fraction, repainting)?;
        self.shown = Some(Shown {
            presentation: Presentation::Legacy(legacy),
            hide_at: Instant::now() + HIDE_TIMEOUT,
        });
        Ok(())
    }
    pub fn tick(&mut self) -> Result<()> {
        let Some(shown) = self.shown.as_mut() else {
            return Ok(());
        };
        let now = Instant::now();
        if now >= shown.hide_at || shown.presentation.faulted() {
            info!("osd hide reason={}", if shown.presentation.faulted() { "fault" } else { "timeout" });
            return self.hide();
        }
        if let Presentation::Legacy(Legacy {
            display,
            plate,
            refresh: Some(LegacyRefresh::Periodic { next }),
            ..
        }) = &mut shown.presentation
        {
            *next = now + UI_REDRAW_PERIOD;
            display.flush_rect(plate.rect)?;
        }
        Ok(())
    }
    pub fn hide(&mut self) -> Result<()> {
        info!("osd hide visible_before={}", self.is_visible());
        let Some(shown) = self.shown.as_mut() else {
            return Ok(());
        };
        let result = match &mut shown.presentation {
            Presentation::Legacy(legacy) => legacy.hide(),
            Presentation::Themed(themed) => themed.hide(),
        };
        if result.is_ok() {
            self.shown.take();
        }
        result.inspect_err(|error| info!("osd hide error={error:#}"))
    }

    pub fn warm_theme(theme: ThemeHandle) -> Result<()> {
        let start = std::time::Instant::now();
        for kind in [OsdKind::Volume, OsdKind::Brightness] {
            for repainting in [false, true] {
                prepare_overlay(theme.clone(), kind, "Level", 0.5, repainting)?;
            }
        }
        info!("osd warm theme={} elapsed_ms={}", theme.pack().id, start.elapsed().as_millis());
        Ok(())
    }
}

#[cfg(test)]
fn prepare_frame(
    theme: ThemeHandle,
    kind: OsdKind,
    title: &str,
    fraction: f32,
) -> Result<PreparedFrame> {
    let mut input = FrameInputs::new(kind.screen());
    input
        .text
        .insert(Binding::Title, title.chars().take(512).collect());
    input.progress = Some(fraction.clamp(0.0, 1.0));
    input.hints.push(common::theme::frame::Hint { button: "B".into(), label: "Back".into() });
    PreparedFrame::prepare(theme, &input)
}
fn prepare_overlay(
    theme: ThemeHandle,
    kind: OsdKind,
    title: &str,
    fraction: f32,
    repainting: bool,
) -> Result<PreparedOverlay> {
    let mut input = FrameInputs::new(kind.screen());
    input.text.insert(Binding::Title, title.chars().take(512).collect());
    input.progress = Some(fraction.clamp(0.0, 1.0));
    input.hints.push(common::theme::frame::Hint { button: "B".into(), label: "Back".into() });
    let start = std::time::Instant::now();
    let result = PreparedFrame::prepare_osd_overlay(theme, &input, repainting);
    match &result {
        Ok(overlay) => info!("osd prepare kind={kind:?} repainting={repainting} elapsed_ms={} non_transparent_pixels={}", start.elapsed().as_millis(), overlay.covered_bytes() / 4),
        Err(error) => info!("osd prepare error={error:#} elapsed_ms={}", start.elapsed().as_millis()),
    }
    result
}

#[cfg(test)]
mod theme_overlay_tests {
    use super::*;
    use common::constants::ALLIUM_FONTS_DIR;
    use common::theme::{FontBank, runtime::ThemeSnapshot};

    #[test]
    fn repainting_flag_selects_plate_and_launcher_keeps_full_screen() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        for pack in ["compact-plus", "big-picture"] {
            let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes").join(pack)).unwrap(), &bank, 42).unwrap();
            for kind in [OsdKind::Volume, OsdKind::Brightness] {
                let game = prepare_overlay(theme.clone(), kind, "Level", 0.6, true).unwrap();
                assert_eq!(game.bounds(), theme.pack().component(kind.screen(), "plate").unwrap().rect);
                let launcher = prepare_overlay(theme.clone(), kind, "Level", 0.6, false).unwrap();
                assert_eq!(launcher.bounds(), common::theme::pack::Rect(0, 0, 640, 480));
                assert_eq!(launcher.covered_bytes(), prepare_frame(theme.clone(), kind, "Level", 0.6).unwrap().into_overlay().unwrap().covered_bytes());
            }
        }
    }

    fn custom_handle(root: &std::path::Path) -> ThemeHandle {
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../common/src/theme/packs/orbit.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(source).unwrap()).unwrap();
        manifest["tokens"]["colors"]["surface"] = "#40608080".into();
        for screen in ["osd_volume", "osd_brightness"] {
            let id_screen = screen.replace('_', "-");
            for (index, component) in manifest["screens"][screen]["components"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                component["id"] = format!("custom-{id_screen}-{index}").into();
            }
        }
        std::fs::write(
            root.join("theme.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../common/src/theme/packs/accent-band.png"),
            root.join("assets/accent-band.png"),
        )
        .unwrap();
        let bank = FontBank::bind(&ALLIUM_FONTS_DIR).unwrap();
        ThemeHandle::prepare(ThemeSnapshot::load(root).unwrap(), &bank, 41).unwrap()
    }

    #[test]
    fn descriptor_consumer_uses_custom_ids_partial_alpha_holes_and_exact_snapshot() {
        let root = std::env::current_dir().unwrap().join("out").join(format!(
            "allium-osd-custom-theme-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let theme = custom_handle(&root);
        let frame = prepare_frame(theme.clone(), OsdKind::Volume, "Volume +", 0.65).unwrap();
        assert_eq!(frame.snapshot().generation(), 41);
        let alphas: Vec<_> = frame
            .pixels()
            .data()
            .iter()
            .skip(3)
            .step_by(4)
            .copied()
            .collect();
        assert!(alphas.contains(&0));
        assert!(alphas.iter().any(|alpha| (1..255).contains(alpha)));
        let overlay = frame.into_overlay().unwrap();
        assert_eq!(overlay.screen(), ScreenId::OsdVolume);
        assert_eq!(
            overlay.bounds(),
            common::theme::pack::Rect(128, 360, 384, 88)
        );
        assert!(overlay.covered_bytes() < 384 * 88 * 4);
        assert!(overlay.accounted_bytes() < 16 * 1024 * 1024);
        let fallback = prepare_overlay(theme.clone(), OsdKind::Volume, "Volume +", 0.65, true).unwrap();
        assert_eq!(fallback.bounds(), overlay.bounds(), "missing plate preserves the descriptor path");
        assert_eq!(fallback.covered_bytes(), overlay.covered_bytes());
        if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
            std::fs::create_dir_all(&proof).unwrap();
            prepare_frame(theme.clone(), OsdKind::Volume, "Volume +", 0.65)
                .unwrap()
                .pixels()
                .save_png(std::path::PathBuf::from(&proof).join("native-orbit-volume-osd.png"))
                .unwrap();
            prepare_frame(theme, OsdKind::Brightness, "Brightness +", 0.40)
                .unwrap()
                .pixels()
                .save_png(std::path::PathBuf::from(proof).join("native-orbit-brightness-osd.png"))
                .unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod parity_b_tests {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/parity-b-support.rs"
    ));
    #[test]
    fn parity_b_osd_footer() {
        for (variant, theme) in parity_variants() {
            for (kind, name, title) in [
                (OsdKind::Volume, "osd_volume", "Volume"),
                (OsdKind::Brightness, "osd_brightness", "Brightness"),
            ] {
                let frame = prepare_frame(theme.clone(), kind, title, 0.6).unwrap();
                parity_save_overlay(&variant, name, &frame);
                let hints = theme.pack().screens[&kind.screen()]
                    .components
                    .iter()
                    .find(|c| c.kind == common::theme::pack::Kind::Hints)
                    .unwrap();
                let r = hints.rect;
                assert!(
                    (r.1..r.1 + r.3).any(|y| (r.0..r.0 + r.2)
                        .any(|x| frame.pixels().pixels()[(y * 640 + x) as usize].alpha() > 0)),
                    "{variant} footer missing"
                );
                assert!(frame.into_overlay().is_ok());
            }
        }
    }
}
