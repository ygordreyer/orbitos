use std::time::Instant;

use anyhow::Result;
use common::{
    display::{
        Display, draw_speaker_icon, draw_sun_icon, fill_rounded_rect, overlay::PreparedOverlay,
    },
    geom::Rect,
    osd::{OSD_DURATION, OsdEvent, OsdKind},
    stylesheet::Stylesheet,
    theme::{
        ThemeHandle,
        frame::{FrameInputs, Hint},
        pack::Binding,
        render::PreparedFrame,
    },
};
use log::{info, warn};

struct Active {
    event: OsdEvent,
    expires: Instant,
    overlay: PreparedOverlay,
}

#[derive(Default)]
pub(crate) struct LauncherOsd {
    active: Option<Active>,
    dirty: bool,
}

impl std::fmt::Debug for LauncherOsd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LauncherOsd")
            .field("event", &self.active.as_ref().map(|a| a.event))
            .finish()
    }
}

impl LauncherOsd {
    pub fn receive(
        &mut self,
        event: OsdEvent,
        now: Instant,
        theme: Option<ThemeHandle>,
        title: &str,
        styles: &Stylesheet,
        display: &impl Display,
    ) -> Result<()> {
        let overlay = theme.and_then(|theme| {
            let mut input = FrameInputs::new(event.kind.screen());
            input
                .text
                .insert(Binding::Title, title.chars().take(512).collect());
            input.progress = Some(f32::from(event.percent) / 100.0);
            input.hints.push(Hint {
                button: "B".into(),
                label: "Back".into(),
            });
            PreparedFrame::prepare_osd_overlay(theme, &input, false)
                .map_err(|error| warn!("Launcher themed OSD unavailable: {error:#}"))
                .ok()
        });
        let overlay = match overlay {
            Some(overlay) => overlay,
            None => legacy_overlay(event, styles, display)?,
        };
        self.active = Some(Active {
            event,
            expires: now + OSD_DURATION,
            overlay,
        });
        self.dirty = true;
        info!(
            "osd compose seq={} kind={:?} percent={}",
            event.seq, event.kind, event.percent
        );
        Ok(())
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.active.as_ref().map(|a| a.expires)
    }
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn expire(&mut self, now: Instant) {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            let active = self.active.take().unwrap();
            self.dirty = true;
            info!("osd expire seq={}", active.event.seq);
        }
    }

    pub fn present<D: Display>(&self, display: &mut D) -> Result<()> {
        let Some(active) = &self.active else {
            return display.flush();
        };
        // Keep the logical frame clean, including when flush fails, so subsequent
        // repaint/repair never blends the scrim over an already composed frame.
        let base = display.pixmap().to_owned();
        active.overlay.compose(&mut display.pixmap_mut());
        let result = display.flush();
        display.pixmap_mut().data_mut().copy_from_slice(base.data());
        result
    }
}

fn legacy_overlay(
    event: OsdEvent,
    styles: &Stylesheet,
    display: &impl Display,
) -> Result<PreparedOverlay> {
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
    let icon = Rect::new(
        rect.x + px as i32,
        rect.y + ((h - icon) / 2) as i32,
        icon,
        icon,
    );
    let bar = Rect::new(
        rect.x + (px + icon.w + px) as i32,
        rect.y + ((h - bar_h) / 2) as i32,
        bar_w,
        bar_h,
    );
    let mut pixels = tiny_skia::Pixmap::new(display.width(), display.height()).unwrap();
    fill_rounded_rect(&mut pixels.as_mut(), rect, px, styles.menu.background_color);
    match event.kind {
        OsdKind::Volume => draw_speaker_icon(&mut pixels.as_mut(), icon, styles.ui.text_color),
        OsdKind::Brightness => draw_sun_icon(&mut pixels.as_mut(), icon, styles.ui.text_color),
    }
    let radius = bar.h / 2;
    fill_rounded_rect(&mut pixels.as_mut(), bar, radius, styles.ui.disabled_color);
    let width = (bar.w as f32 * f32::from(event.percent) / 100.0).round() as u32;
    if width > 0 {
        fill_rounded_rect(
            &mut pixels.as_mut(),
            Rect::new(bar.x, bar.y, width, bar.h),
            radius.min(width / 2),
            styles.ui.highlight_color,
        );
    }
    PreparedOverlay::from_legacy_frame(
        event.kind.screen(),
        pixels,
        common::theme::pack::Rect(rect.x as u32, rect.y as u32, rect.w, rect.h),
    )
}

#[cfg(unix)]
pub(crate) fn bind() -> std::io::Result<tokio::net::UnixDatagram> {
    bind_at(common::constants::LAUNCHER_OSD_SOCKET)
}

#[cfg(unix)]
pub(crate) fn bind_at(path: impl AsRef<std::path::Path>) -> std::io::Result<tokio::net::UnixDatagram> {
    let path = path.as_ref();
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    tokio::net::UnixDatagram::bind(path)
}

#[cfg(unix)]
pub(crate) enum Wake {
    Event(OsdEvent),
    Expired,
}

#[cfg(unix)]
pub(crate) async fn wait(
    socket: &mut Option<tokio::net::UnixDatagram>,
    deadline: Option<Instant>,
) -> Wake {
    let mut bytes = [0; common::osd::OSD_EVENT_BYTES + 1];
    loop {
        tokio::select! {
            biased;
            received = async {
                match socket.as_ref() {
                    Some(socket) => socket.recv(&mut bytes).await,
                    None => std::future::pending().await,
                }
            } => match received {
                Ok(length) => {
                    if let Some(event) = OsdEvent::decode(&bytes[..length]) {
                        return Wake::Event(event);
                    }
                }
                Err(error) => {
                    warn!("Launcher OSD receive failed: {error}");
                    *socket = None;
                }
            },
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                    None => std::future::pending().await,
                }
            } => return Wake::Expired,
        }
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod tests {
    use super::*;
    type MockDisplay = <common::platform::DefaultPlatform as common::platform::Platform>::Display;

    #[test]
    fn latest_arrival_wins_extends_expiry_and_accepts_daemon_restart() {
        let mut osd = LauncherOsd::default();
        let display = MockDisplay::new();
        let styles = Stylesheet::default();
        let now = Instant::now();
        osd.receive(
            OsdEvent::new(OsdKind::Volume, 0.3, 1, 99),
            now,
            None,
            "Volume +",
            &styles,
            &display,
        )
        .unwrap();
        let later = now + OSD_DURATION / 2;
        let latest = OsdEvent::new(OsdKind::Brightness, 0.7, -1, 1);
        osd.receive(latest, later, None, "Brightness -", &styles, &display)
            .unwrap();
        assert_eq!(osd.active.as_ref().unwrap().event, latest);
        assert_eq!(osd.deadline(), Some(later + OSD_DURATION));
        assert!(osd.take_dirty());
        assert!(!osd.take_dirty());
        osd.expire(now + OSD_DURATION);
        assert!(osd.is_active());
        assert!(!osd.take_dirty());
        osd.expire(later + OSD_DURATION);
        assert!(!osd.is_active());
        assert_eq!(osd.deadline(), None);
        assert!(osd.take_dirty());
        osd.expire(later + OSD_DURATION * 2);
        assert!(!osd.take_dirty());
    }

    #[test]
    fn legacy_composition_survives_repaint_without_alpha_accumulating_and_clears_on_expiry() {
        let mut display = MockDisplay::new();
        display.record_presentations();
        let mut osd = LauncherOsd::default();
        let now = Instant::now();
        osd.receive(
            OsdEvent::new(OsdKind::Volume, 0.4, 1, 1),
            now,
            None,
            "Volume +",
            &Stylesheet::default(),
            &display,
        )
        .unwrap();
        for color in [
            common::display::color::Color::new(80, 90, 100),
            common::display::color::Color::new(100, 90, 80),
        ] {
            display.clear(color).unwrap();
            let base = display.pixmap().to_owned();
            osd.present(&mut display).unwrap();
            osd.present(&mut display).unwrap();
            let frames = display.take_presented_frames();
            assert_ne!(frames[0].data(), base.data());
            assert_eq!(frames[0].data(), frames[1].data());
            assert_eq!(display.pixmap().data(), base.data());
        }
        osd.expire(now + OSD_DURATION);
        osd.present(&mut display).unwrap();
        assert_eq!(
            display.take_presented_frames()[0].data(),
            display.pixmap().data()
        );
    }
}
