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
use log::warn;
use tokio::time::Instant;

const HIDE_TIMEOUT: Duration = Duration::from_millis(1000);
const UI_REDRAW_PERIOD: Duration = Duration::from_micros(UI_FRAME_INTERVAL.as_micros() as u64 / 2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdKind {
    Volume,
    Brightness,
}
impl OsdKind {
    fn screen(self) -> ScreenId {
        match self {
            Self::Volume => ScreenId::OsdVolume,
            Self::Brightness => ScreenId::OsdBrightness,
        }
    }
}

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
            faulted: false,
        })
    }
    fn stop_writer(&mut self) -> Result<()> {
        ensure!(
            !self.faulted,
            "Legacy OSD faulted; explicit recovery required"
        );
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
        self.display.load(rect)?;
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
        self.display.flush_rect(rect)
    }
    fn update(
        &mut self,
        styles: &Stylesheet,
        kind: OsdKind,
        fraction: f32,
        repainting: bool,
    ) -> Result<()> {
        self.stop_writer()?;
        if let Err(error) = self.draw(styles, kind, fraction) {
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
            Err(_) if surface.is_some() => Ok(Self { display, surface }),
            Err(error) => Err(error),
        }
    }
    fn replace(&mut self, candidate: &PreparedOverlay) -> Result<()> {
        OverlaySurface::replace(&mut self.display, &mut self.surface, Some(candidate))
    }
    fn hide(&mut self) -> Result<()> {
        OverlaySurface::replace(&mut self.display, &mut self.surface, None)
    }
    fn theme(&self) -> Option<ThemeHandle> {
        self.surface
            .as_ref()
            .map(|surface| surface.captured().snapshot().clone())
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
            if shown.presentation.faulted() {
                return None;
            }
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
        let fraction = fraction.clamp(0.0, 1.0);
        if let Some(shown) = self.shown.as_mut() {
            ensure!(
                !shown.presentation.faulted(),
                "OSD faulted; explicit recovery required"
            );
            match &mut shown.presentation {
                Presentation::Legacy(legacy) => {
                    legacy.update(&self.styles, kind, fraction, repainting)?
                }
                Presentation::Themed(themed) => {
                    let theme = themed
                        .theme()
                        .ok_or_else(|| anyhow::anyhow!("Visible themed OSD lost its snapshot"))?;
                    let candidate = prepare_overlay(theme, kind, title, fraction)?;
                    themed.replace(&candidate)?;
                }
            }
            shown.hide_at = Instant::now() + HIDE_TIMEOUT;
            return Ok(());
        }
        if let Some(theme) = opening_theme {
            match prepare_overlay(theme, kind, title, fraction) {
                Ok(candidate) => {
                    let mut display = platform.display_partial()?;
                    if let Err(error) = display.check_overlay(&candidate) {
                        warn!(
                            "themed OSD unavailable before presentation, using Legacy: {error:#}"
                        );
                    } else {
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
        ensure!(
            !shown.presentation.faulted(),
            "OSD faulted; explicit recovery required"
        );
        let now = Instant::now();
        if now >= shown.hide_at {
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
        result
    }
}

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
) -> Result<PreparedOverlay> {
    prepare_frame(theme, kind, title, fraction)?.into_overlay()
}

#[cfg(test)]
mod theme_overlay_tests {
    use super::*;
    use common::constants::ALLIUM_FONTS_DIR;
    use common::theme::{FontBank, runtime::ThemeSnapshot};

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
