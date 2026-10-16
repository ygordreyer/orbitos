use common::display::color::Color;
use common::display::{Display, fill_rect, fill_rounded_rect};
use common::geom::{Point, Rect};
use common::platform::{DefaultPlatform, Platform};
use common::stylesheet::Stylesheet;

use super::orbit::{font, wrapped};
use crate::discs::Playlist;

#[derive(Debug)]
pub struct DiscPicker {
    pub playlist: Playlist,
    pub selected: usize,
    pub error: Option<String>,
    dirty: bool,
}

impl DiscPicker {
    pub fn new(playlist: Playlist) -> Self {
        Self {
            playlist,
            selected: 0,
            error: None,
            dirty: true,
        }
    }

    pub fn select(&mut self, forward: bool) {
        let count = self.playlist.discs.len();
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
        self.error = None;
        self.dirty = true;
    }

    pub fn redraw(&mut self) {
        self.dirty = true;
    }
    pub fn should_draw(&self) -> bool {
        self.dirty
    }

    pub fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) {
        let visible = self.playlist.discs.len().min(4);
        let first = self
            .selected
            .saturating_sub(3)
            .min(self.playlist.discs.len().saturating_sub(4));
        let height = 140 + visible as u32 * 44;
        let y =
            super::orbit::HEADER_HEIGHT + (super::orbit::CONTENT_HEIGHT as i32 - height as i32) / 2;
        let text = Color::new(35, 48, 51);
        let muted = Color::new(96, 118, 105);
        fill_rect(
            &mut display.pixmap_mut(),
            Rect::new(
                0,
                super::orbit::HEADER_HEIGHT,
                640,
                super::orbit::CONTENT_HEIGHT,
            ),
            Color::rgba(22, 37, 30, 145),
        );
        fill_rounded_rect(
            &mut display.pixmap_mut(),
            Rect::new(90, y, 460, height),
            14,
            Color::new(243, 244, 238),
        );
        font(styles, 28, text).draw(
            &mut display.pixmap_mut(),
            "Choose a disc",
            Point::new(108, y + 16),
        );
        for (index, disc) in self
            .playlist
            .discs
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
        {
            let row_y = y + 58 + (index - first) as i32 * 44;
            if index == self.selected {
                fill_rounded_rect(
                    &mut display.pixmap_mut(),
                    Rect::new(108, row_y - 6, 424, 39),
                    7,
                    Color::new(165, 235, 233),
                );
            }
            let label = format!(
                "Disc {}{}",
                index + 1,
                if disc.available { "" } else { "  (missing)" }
            );
            font(styles, 21, text).draw(&mut display.pixmap_mut(), &label, Point::new(120, row_y));
        }
        let subtitle = self
            .error
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| {
                self.playlist.discs[self.selected]
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let small = font(styles, 17, muted);
        for (index, line) in wrapped(&small, &subtitle, 424, 2).iter().enumerate() {
            small.draw(
                &mut display.pixmap_mut(),
                line,
                Point::new(108, y + height as i32 - 57 + index as i32 * 20),
            );
        }
        crate::network_runtime::footer(display, styles, "A Start disc    B Cancel");
        self.dirty = false;
    }
}
