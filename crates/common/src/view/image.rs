use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use async_trait::async_trait;
use fast_image_resize::{PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image as FirImage};
use image::{ImageDecoder, RgbaImage, imageops};
use log::{error, trace};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::command::Command;
use crate::display::Display;
use crate::display::image::round;
use crate::geom::{Alignment, Point, Rect};
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::stylesheet::Stylesheet;
use crate::view::View;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ImageMode {
    /// Don't scale the image
    Raw,
    /// Scale the image to fill the rect, but maintain the aspect ratio.
    Cover,
    /// Scale the image to fit the rect, but maintain the aspect ratio.
    Contain,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Image {
    rect: Rect,
    path: Option<PathBuf>,
    #[serde(skip)]
    image: OnceLock<Option<RgbaImage>>,
    mode: ImageMode,
    border_radius: u32,
    alignment: Alignment,
    dirty: bool,
}

impl Image {
    pub fn new(rect: Rect, path: PathBuf, mode: ImageMode) -> Self {
        Self {
            rect,
            path: Some(path),
            image: OnceLock::new(),
            mode,
            border_radius: 0,
            alignment: Alignment::Left,
            dirty: true,
        }
    }

    pub fn set_border_radius(&mut self, radius: u32) -> &mut Self {
        if radius != self.border_radius {
            self.border_radius = radius;
            self.dirty = true;
        }
        self
    }

    pub fn empty(rect: Rect, mode: ImageMode) -> Self {
        Self {
            rect,
            path: None,
            image: OnceLock::new(),
            mode,
            border_radius: 0,
            alignment: Alignment::Left,
            dirty: true,
        }
    }

    pub fn set_path(&mut self, path: Option<PathBuf>) -> &mut Self {
        if path != self.path {
            self.image = OnceLock::new();
            self.dirty = true;
            self.path = path;
        }
        self
    }

    pub fn set_alignment(&mut self, alignment: Alignment) -> &mut Self {
        self.alignment = alignment;
        self
    }

    fn resize_image(src_image: &RgbaImage, new_width: u32, new_height: u32) -> Option<RgbaImage> {
        #[cfg(feature = "perf-bench")]
        crate::perf::hit(crate::perf::Op::ArtResize);
        let src = FirImage::from_vec_u8(
            src_image.width(),
            src_image.height(),
            src_image.as_raw().clone(),
            PixelType::U8x4,
        )
        .ok()?;

        let mut dst = FirImage::new(new_width, new_height, PixelType::U8x4);

        let mut resizer = Resizer::new();
        resizer
            .resize(
                &src,
                &mut dst,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(
                    fast_image_resize::FilterType::Bilinear,
                )),
            )
            .ok()?;

        RgbaImage::from_raw(new_width, new_height, dst.into_vec())
    }

    fn image(
        &self,
        path: &Path,
        rect: Rect,
        mode: ImageMode,
        border_radius: u32,
    ) -> Option<RgbaImage> {
        #[cfg(feature = "perf-bench")]
        crate::perf::hit(crate::perf::Op::Read);
        let mut reader = ::image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
        let mut limits = ::image::Limits::default();
        limits.max_image_width = Some(2048);
        limits.max_image_height = Some(2048);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        let decoder = reader.into_decoder().ok()?;
        let (width, height) = decoder.dimensions();
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 2048 * 2048 {
            return None;
        }
        #[cfg(test)]
        SOURCE_DECODES.with(|n| n.set(n.get() + 1));
        let image = ::image::DynamicImage::from_decoder(decoder)
            .map_err(|e| error!("Failed to load image at {}: {}", path.display(), e))
            .ok()?;
        #[cfg(feature = "perf-bench")]
        crate::perf::hit(crate::perf::Op::Decode);
        let mut image = match mode {
            ImageMode::Raw => image.to_rgba8(),
            ImageMode::Cover => {
                if image.width() == rect.w && image.height() == rect.h {
                    image.to_rgba8()
                } else {
                    let rgba = image.to_rgba8();
                    Self::resize_image(&rgba, rect.w, rect.h)?
                }
            }
            ImageMode::Contain => {
                if image.width() == rect.w && image.height() == rect.h {
                    image.to_rgba8()
                } else {
                    let new_height = rect.h.min(rect.w * image.height() / image.width());
                    let new_width = rect.w.min(rect.h * image.width() / image.height());
                    let rgba = image.to_rgba8();
                    Self::resize_image(&rgba, new_width, new_height)?
                }
            }
        };
        let (w, h) = image.dimensions();
        if border_radius != 0 {
            let border_radius = border_radius.min(w / 2).min(h / 2);
            round(&mut image, border_radius);
        }
        let image = if w != rect.w || h != rect.h {
            let mut bg = RgbaImage::new(rect.w, rect.h);
            let x = match self.alignment {
                Alignment::Left => 0,
                Alignment::Center => rect.w.saturating_sub(w) / 2,
                Alignment::Right => rect.w.saturating_sub(w),
            };
            // vertical align top
            imageops::overlay(&mut bg, &image, x as i64, 0);
            bg
        } else {
            image
        };

        Some(image)
    }
}

#[async_trait(?Send)]
impl View for Image {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        _styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(ref path) = self.path {
            self.image
                .get_or_init(|| self.image(path, self.rect, self.mode, self.border_radius));
        }

        display.load(self.rect)?;
        if let Some(Some(image)) = self.image.get() {
            trace!("drawing image: {:?}", self.rect);
            crate::display::image::draw_image(
                &mut display.pixmap_mut(),
                image,
                Point::new(self.rect.x, self.rect.y),
            );
        }

        // A failed load is cached, so asking to be drawn again can only spin
        self.dirty = false;
        Ok(true)
    }

    fn should_draw(&self) -> bool {
        self.dirty
    }

    fn set_should_draw(&mut self) {
        self.dirty = true;
    }

    async fn handle_key_event(
        &mut self,
        _event: KeyEvent,
        _command: Sender<Command>,
        _bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        Ok(false)
    }

    fn children(&self) -> Vec<&dyn View> {
        Vec::new()
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        Vec::new()
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, point: Point) {
        self.rect.x = point.x;
        self.rect.y = point.y;
        self.dirty = true;
    }
}

#[cfg(test)]
thread_local! {
    static SOURCE_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod media_safety_tests {
    use super::*;

    #[test]
    fn legacy_cover_rejects_tiny_huge_jpeg_before_decode() {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8).unwrap();
        let sof = bytes.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        bytes[sof + 5..sof + 7].copy_from_slice(&8192u16.to_be_bytes());
        bytes[sof + 7..sof + 9].copy_from_slice(&8192u16.to_be_bytes());
        assert!(bytes.len() < 1024);
        let path = std::env::temp_dir().join(format!("allium-legacy-huge-header-{}.jpg", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        SOURCE_DECODES.with(|n| n.set(0));
        for mode in [ImageMode::Raw, ImageMode::Cover, ImageMode::Contain] {
            let view = Image::new(Rect::new(0, 0, 140, 140), path.clone(), mode);
            assert!(view.image(&path, view.rect, mode, 0).is_none());
        }
        SOURCE_DECODES.with(|n| assert_eq!(n.get(), 0));
        std::fs::remove_file(path).unwrap();
    }
}
