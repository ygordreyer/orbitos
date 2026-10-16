use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::display::Display;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::theme::{
    ThemeHandle,
    frame::{Focus, FrameInputs, Hint, Row as ThemeRow, Status},
    pack::{Binding, ScreenId},
    render::PreparedFrame,
};
use common::view::{ButtonHint, ButtonHints, ScrollList, View};
use tokio::sync::mpsc::Sender;

use crate::view::text_reader::TextReader;

pub struct GuideSelector {
    rect: Rect,
    res: Resources,
    list: ScrollList,
    button_hints: ButtonHints<String>,
    guides: Vec<PathBuf>,
    text_reader: Option<Box<TextReader>>,
    theme_frame: Option<PreparedFrame>,
    theme_dirty: bool,
    status: Status,
}

impl GuideSelector {
    pub fn new(
        rect: Rect,
        res: Resources,
        guides: Vec<PathBuf>,
        selected: usize,
        text_reader_open: bool,
    ) -> Self {
        let styles = res.get::<Stylesheet>();
        let locale = res.get::<Locale>();

        let guide_names: Vec<String> = guides
            .iter()
            .filter_map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .collect();

        let mut button_hints = ButtonHints::new(
            res.clone(),
            vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::Menu,
                locale.t("ingame-menu-continue"),
                Alignment::Left,
            )],
            vec![
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::A,
                    locale.t("button-select"),
                    Alignment::Right,
                ),
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::B,
                    locale.t("button-back"),
                    Alignment::Right,
                ),
            ],
        );

        let button_hints_rect = button_hints.bounding_box(&styles);
        let list_rect = Rect::new(
            rect.x + styles.ui.margin_x,
            rect.y,
            rect.w - (styles.ui.margin_x * 2) as u32,
            button_hints_rect.y as u32 - rect.y as u32,
        );

        let mut list = ScrollList::new(
            res.clone(),
            list_rect,
            guide_names,
            Alignment::Left,
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );

        list.select(selected);

        let text_reader = if (text_reader_open || guides.len() == 1)
            && let Some(path) = guides.get(selected)
        {
            Some(Box::new(TextReader::new(rect, res.clone(), path.clone())))
        } else {
            None
        };

        drop(locale);
        drop(styles);

        Self {
            rect,
            res,
            list,
            button_hints,
            guides,
            text_reader,
            theme_frame: None,
            theme_dirty: false,
            status: Status::default(),
        }
    }

    #[allow(dead_code)]
    pub fn selected(&self) -> usize {
        self.list.selected()
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.guides.get(self.list.selected()).map(|p| p.as_path())
    }

    pub fn is_text_reader_open(&self) -> bool {
        self.text_reader.is_some()
    }

    pub fn text_reader(&self) -> Option<&TextReader> {
        self.text_reader.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn prepared_theme_frame(&self) -> Option<&PreparedFrame> {
        self.theme_frame.as_ref()
    }

    fn open_text_reader(&mut self) {
        if let Some(path) = self.guides.get(self.list.selected()) {
            self.text_reader = Some(Box::new(TextReader::new(
                self.rect,
                self.res.clone(),
                path.clone(),
            )));
        }
    }

    fn close_text_reader(&mut self) {
        if let Some(reader) = self.text_reader.take() {
            reader.save_cursor();
        }
        self.set_should_draw();
    }

    pub fn refresh_theme(&mut self, status: Status) -> Result<()> {
        self.status = status;
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let Some(theme) = theme else {
            self.theme_frame = None;
            self.theme_dirty = false;
            self.list.set_visible_count_override(None)?;
            return Ok(());
        };
        if let Some(reader) = &mut self.text_reader {
            reader.refresh_theme(self.status.clone())?;
            self.theme_frame = None;
            self.theme_dirty = false;
            return Ok(());
        }
        let count = theme_capacity(&theme, ScreenId::GuideSelector, Binding::Guides);
        self.list.set_visible_count_override(Some(count))?;
        let first = self.list.window_start();
        let selected = self.list.selected();
        let locale = self.res.get::<Locale>();
        let mut input = FrameInputs::new(ScreenId::GuideSelector);
        input
            .text
            .insert(Binding::Title, locale.t("ingame-menu-guide"));
        super::game_context(&mut input, &self.res, &theme);
        input.rows.insert(
            Binding::Guides,
            self.guides
                .iter()
                .enumerate()
                .skip(first)
                .take(count)
                .map(|(index, path)| ThemeRow {
                    id: format!("guide-{index}"),
                    label: path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    subtitle: {
                        let cursor = self.res.get::<common::database::Database>().get_guide_cursor(path).unwrap_or(0).max(0);
                        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0).max(1);
                        format!("Bookmark · {:.0}%", (cursor as f64 / bytes as f64 * 100.0).clamp(0.0, 100.0))
                    },
                    value: String::new(),
                    enabled: true,
                })
                .collect(),
        );
        input.pagination = Some(common::theme::frame::Pagination {page: selected/count.max(1)+1,total: self.guides.len().max(1).div_ceil(count.max(1))});
        input.focus = Focus {
            component: theme_component(&theme, ScreenId::GuideSelector, Binding::Guides),
            item: Some(format!("guide-{selected}")),
        };
        input
            .selected
            .insert(Binding::Guides, format!("guide-{selected}"));
        super::guide_shell(&mut input, &locale);
        input.hints = vec![
            Hint {
                button: "MENU".into(),
                label: locale.t("ingame-menu-continue"),
            },
            Hint {
                button: "A".into(),
                label: locale.t("button-select"),
            },
            Hint {
                button: "B".into(),
                label: locale.t("button-back"),
            },
        ];
        input.status = self.status.clone();
        drop(locale);
        self.theme_frame = Some(PreparedFrame::prepare(theme, &input)?);
        self.theme_dirty = true;
        Ok(())
    }

    pub(crate) fn disable_theme(&mut self) {
        self.theme_frame = None;
        self.theme_dirty = false;
        if let Err(error) = self.list.set_visible_count_override(None) {
            log::warn!("failed to reset Legacy guide viewport: {error:#}");
        }
        if let Some(reader) = &mut self.text_reader {
            reader.disable_theme();
        }
        self.list.set_should_draw();
        self.button_hints.set_should_draw();
    }

    #[cfg(test)]
    pub(crate) fn text_reader_mut(&mut self) -> Option<&mut TextReader> {
        self.text_reader.as_deref_mut()
    }

    #[cfg(test)]
    pub(crate) fn theme_is_disabled_recursively(&self) -> bool {
        self.theme_frame.is_none()
            && self
                .text_reader
                .as_deref()
                .is_none_or(TextReader::theme_is_disabled)
    }
}

#[async_trait(?Send)]
impl View for GuideSelector {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(frame) = &self.theme_frame {
            if !self.theme_dirty {
                return Ok(false);
            }
            frame.paint(display);
            self.theme_dirty = false;
            return Ok(true);
        }
        if let Some(reader) = &mut self.text_reader {
            reader.draw(display, styles)
        } else {
            let mut drawn = false;
            drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
            if self.button_hints.should_draw() {
                display.load(self.button_hints.bounding_box(styles))?;
                drawn |= self.button_hints.draw(display, styles)?;
            }
            Ok(drawn)
        }
    }

    fn should_draw(&self) -> bool {
        if self.theme_frame.is_some() {
            return self.theme_dirty;
        }
        if let Some(reader) = &self.text_reader {
            reader.should_draw()
        } else {
            self.list.should_draw() || self.button_hints.should_draw()
        }
    }

    fn set_should_draw(&mut self) {
        if self.theme_frame.is_some() {
            self.theme_dirty = true;
            return;
        }
        if let Some(reader) = &mut self.text_reader {
            reader.set_should_draw();
        } else {
            self.list.set_should_draw();
            self.button_hints.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if let Some(reader) = &mut self.text_reader {
            if reader.handle_key_event(event, commands, bubble).await? {
                // Check if reader requested to close
                bubble.retain(|cmd| match cmd {
                    Command::CloseView => {
                        self.close_text_reader();
                        false
                    }
                    _ => true,
                });
                return Ok(true);
            }
            return Ok(false);
        }

        // Handle list navigation
        if self.list.handle_key_event(event, commands, bubble).await? {
            return Ok(true);
        }

        match event {
            KeyEvent::Pressed(Key::A) => {
                self.open_text_reader();
                Ok(true)
            }
            KeyEvent::Pressed(Key::B) => {
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        if let Some(reader) = &self.text_reader {
            vec![reader.as_ref()]
        } else {
            vec![&self.list]
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        if let Some(reader) = &mut self.text_reader {
            vec![reader.as_mut()]
        } else {
            vec![&mut self.list]
        }
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: common::geom::Point) {
        unimplemented!()
    }
}

fn theme_capacity(theme: &ThemeHandle, screen: ScreenId, binding: Binding) -> usize {
    theme.pack().screens[&screen]
        .components
        .iter()
        .find(|component| component.binding == Some(binding))
        .and_then(|component| component.visible_count)
        .unwrap_or(1)
        .clamp(1, 64) as usize
}

fn theme_component(theme: &ThemeHandle, screen: ScreenId, binding: Binding) -> String {
    theme.pack().screens[&screen]
        .components
        .iter()
        .find(|component| component.binding == Some(binding))
        .map(|component| component.id.clone())
        .unwrap_or_default()
}
