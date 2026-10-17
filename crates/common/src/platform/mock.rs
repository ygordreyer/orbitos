use anyhow::Result;
use async_trait::async_trait;
use tiny_skia::{Pixmap, PixmapMut, PixmapRef};

use crate::battery::Battery;
use crate::display::Display;
use crate::display::color::Color;
use crate::display::settings::DisplaySettings;
use crate::geom::Rect;
use crate::platform::{KeyEvent, Platform};

pub const SCREEN_WIDTH: u32 = 640;
pub const SCREEN_HEIGHT: u32 = 480;

pub struct MockPlatform;

#[async_trait(?Send)]
impl Platform for MockPlatform {
    type Display = MockDisplay;
    type Battery = MockBattery;
    type SuspendContext = ();

    fn new() -> Result<MockPlatform> {
        Ok(MockPlatform)
    }

    async fn poll(&mut self) -> KeyEvent {
        std::future::pending().await
    }

    fn display(&mut self) -> Result<Self::Display> {
        Ok(MockDisplay::new())
    }

    fn battery(&self) -> Result<Self::Battery> {
        Ok(MockBattery)
    }

    fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    fn suspend(&self) -> Result<Self::SuspendContext> {
        Ok(())
    }

    fn unsuspend(&self, _ctx: Self::SuspendContext) -> Result<()> {
        Ok(())
    }

    fn set_volume(&mut self, _volume: i32) -> Result<()> {
        Ok(())
    }

    fn get_brightness(&self) -> Result<u8> {
        Ok(50)
    }

    fn set_brightness(&mut self, _brightness: u8) -> Result<()> {
        Ok(())
    }

    fn set_display_settings(&mut self, _settings: &mut DisplaySettings) -> Result<()> {
        Ok(())
    }

    fn device_model() -> String {
        "Mock".into()
    }

    fn firmware() -> String {
        "00000000".to_string()
    }

    fn has_wifi() -> bool {
        false
    }

    fn has_lid() -> bool {
        false
    }
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

pub struct MockDisplay {
    #[cfg(feature = "perf-bench")]
    presenter: Option<crate::platform::PresenterProbe>,
    #[cfg(feature = "perf-bench")]
    probe_damage: Vec<Rect>,
    pixmap: Pixmap,
    recording: Option<Recording>,
}

struct Recording {
    saved: Vec<Pixmap>,
    presented: Vec<Pixmap>,
}

impl MockDisplay {
    #[cfg(feature = "perf-bench")]
    pub fn presenter(&mut self) -> &mut crate::platform::PresenterProbe {
        self.presenter.get_or_insert_with(|| {
            crate::platform::PresenterProbe::new(SCREEN_WIDTH, SCREEN_HEIGHT)
        })
    }
    #[cfg(feature = "perf-bench")]
    pub fn take_probe_damage(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.probe_damage)
    }

    pub fn record_presentations(&mut self) {
        self.recording = Some(Recording {
            saved: Vec::new(),
            presented: Vec::new(),
        });
    }

    pub fn take_presented_frames(&mut self) -> Vec<Pixmap> {
        std::mem::take(
            &mut self
                .recording
                .as_mut()
                .expect("recording enabled")
                .presented,
        )
    }

    pub fn new() -> Self {
        Self {
            pixmap: Pixmap::new(SCREEN_WIDTH, SCREEN_HEIGHT).expect("Failed to create mock pixmap"),
            recording: None,
            #[cfg(feature = "perf-bench")]
            presenter: None,
            #[cfg(feature = "perf-bench")]
            probe_damage: vec![],
        }
    }
}

impl Default for MockDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for MockDisplay {
    fn width(&self) -> u32 {
        SCREEN_WIDTH
    }

    fn height(&self) -> u32 {
        SCREEN_HEIGHT
    }

    fn pixmap(&self) -> PixmapRef<'_> {
        self.pixmap.as_ref()
    }

    fn pixmap_mut(&mut self) -> PixmapMut<'_> {
        self.pixmap.as_mut()
    }

    fn map_pixels<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(Color) -> Color,
    {
        for pixel in self.pixmap.pixels_mut() {
            let color: Color = (*pixel).into();
            *pixel = f(color).into();
        }
        Ok(())
    }

    fn composition_revision(&mut self) -> Result<u64> {
        #[cfg(feature = "perf-bench")]
        if let Some(presenter) = &mut self.presenter {
            return presenter.composition_revision();
        }
        Ok(0)
    }

    fn needs_present(&mut self) -> Result<bool> {
        #[cfg(feature = "perf-bench")]
        if let Some(presenter) = &mut self.presenter {
            return presenter.needs_present();
        }
        Ok(false)
    }

    fn flush(&mut self) -> Result<()> {
        #[cfg(feature = "perf-bench")]
        if let Some(presenter) = &mut self.presenter {
            presenter.flush(
                self.pixmap.as_ref(),
                &[Rect::new(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT)],
            )?;
        }
        #[cfg(feature = "perf-bench")]
        {
            self.probe_damage = vec![self.bounding_box()];
        }
        crate::perf_trace::count(
            crate::perf_trace::Count::FlushedPixels,
            self.width() as u64 * self.height() as u64,
        );
        if let Some(recording) = &mut self.recording {
            recording.presented.push(self.pixmap.clone());
        }
        Ok(())
    }

    fn flush_rects(&mut self, areas: &[Rect]) -> Result<()> {
        #[cfg(feature = "perf-bench")]
        if let Some(presenter) = &mut self.presenter {
            presenter.flush(self.pixmap.as_ref(), areas)?;
        }
        #[cfg(feature = "perf-bench")]
        {
            self.probe_damage = areas.to_vec();
        }
        crate::perf_trace::count(
            crate::perf_trace::Count::FlushedPixels,
            areas.iter().map(|a| a.w as u64 * a.h as u64).sum(),
        );
        if let Some(recording) = &mut self.recording {
            recording.presented.push(self.pixmap.clone());
        }
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        if let Some(recording) = &mut self.recording {
            recording.saved.push(self.pixmap.clone());
        }
        Ok(())
    }

    fn load(&mut self, area: Rect) -> Result<()> {
        if let Some(recording) = &self.recording {
            let saved = recording
                .saved
                .last()
                .ok_or_else(|| anyhow::anyhow!("No saved image"))?;
            let width = self.pixmap.width() as usize;
            for y in area.y.max(0)..area.bottom().min(self.pixmap.height() as i32) {
                let start = y as usize * width + area.x.max(0) as usize;
                let end = y as usize * width + area.right().min(width as i32) as usize;
                if start < end {
                    self.pixmap.pixels_mut()[start..end]
                        .copy_from_slice(&saved.pixels()[start..end]);
                }
            }
        }
        Ok(())
    }

    fn pop(&mut self) -> bool {
        self.recording
            .as_mut()
            .is_none_or(|recording| recording.saved.pop().is_some())
    }
}

pub struct MockBattery;

impl Battery for MockBattery {
    fn update(&mut self) -> Result<()> {
        Ok(())
    }

    fn percentage(&self) -> i32 {
        50
    }

    fn charging(&self) -> bool {
        false
    }
}
