use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;

use common::display::settings::DisplaySettings;
use common::geom::{Alignment, Point, Rect, Size};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Label, Percentage, SettingsList, View};

use tokio::sync::mpsc::Sender;

use crate::view::settings::{ChildState, SettingsChild};

pub struct Display {
    rect: Rect,
    settings: DisplaySettings,
    list: SettingsList,
    button_hints: ButtonHints<String>,
    edit_button: Option<ButtonHint<String>>,
}

impl Display {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;

        let settings = DisplaySettings::load().unwrap();

        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();

        let mut button_hints = ButtonHints::new(
            res.clone(),
            vec![],
            vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::B,
                locale.t("button-back"),
                Alignment::Right,
            )],
        );

        let button_hints_rect = button_hints.bounding_box(&styles);
        let list_height = (button_hints_rect.y - y) as u32;

        let edit_button = Some(ButtonHint::new(
            res.clone(),
            Point::zero(),
            Key::A,
            locale.t("button-edit"),
            Alignment::Right,
        ));

        let mut list = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                list_height,
            ),
            vec![
                locale.t("settings-display-screen-resolution"),
                locale.t("settings-display-luminance"),
                locale.t("settings-display-hue"),
                locale.t("settings-display-saturation"),
                locale.t("settings-display-contrast"),
                locale.t("settings-display-red"),
                locale.t("settings-display-green"),
                locale.t("settings-display-blue"),
            ],
            vec![
                Box::new(Label::new(
                    Point::zero(),
                    {
                        let size = res.get::<Size>();
                        format!("{}x{}", size.w, size.h)
                    },
                    Alignment::Right,
                    None,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.luminance),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.hue),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.saturation),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.contrast),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.r),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.g),
                    0,
                    100,
                    Alignment::Right,
                )),
                Box::new(Percentage::new(
                    Point::zero(),
                    i32::from(settings.b),
                    0,
                    100,
                    Alignment::Right,
                )),
            ],
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        if let Some(state) = state {
            list.select(state.selected);
        }

        Self {
            rect,
            settings,
            list,
            button_hints,
            edit_button,
        }
    }
}

#[async_trait(?Send)]
impl View for Display {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
        drawn |= self.button_hints.should_draw() && self.button_hints.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.list.should_draw() || self.button_hints.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.list.set_should_draw();
        self.button_hints.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if self
            .list
            .handle_key_event(event, commands.clone(), bubble)
            .await?
        {
            if self.list.selected() == 0 && self.button_hints.right().len() == 2 {
                self.edit_button = Some(self.button_hints.right_mut().remove(0));
            } else if let Some(button) = self.edit_button.take()
                && self.button_hints.right().len() == 1
            {
                self.button_hints.right_mut().insert(0, button);
            }
            while let Some(command) = bubble.pop_front() {
                if let Command::ValueChanged(i, val) = command {
                    match i {
                        0 => {}
                        1 => self.settings.luminance = val.as_int().unwrap() as u8,
                        2 => self.settings.hue = val.as_int().unwrap() as u8,
                        3 => self.settings.saturation = val.as_int().unwrap() as u8,
                        4 => self.settings.contrast = val.as_int().unwrap() as u8,
                        5 => self.settings.r = val.as_int().unwrap() as u8,
                        6 => self.settings.g = val.as_int().unwrap() as u8,
                        7 => self.settings.b = val.as_int().unwrap() as u8,
                        _ => unreachable!("Invalid index"),
                    }

                    commands
                        .send(Command::SaveDisplaySettings(Box::new(
                            self.settings.clone(),
                        )))
                        .await?;
                }
            }
            return Ok(true);
        }

        match event {
            KeyEvent::Pressed(Key::B) => {
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.list, &self.button_hints]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.list, &mut self.button_hints]
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

impl SettingsChild for Display {
    fn clear_theme(&mut self) -> Result<()> { self.list.set_visible_count_override(None) }
    fn theme_models(&mut self, handle: &common::theme::ThemeHandle)
        -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        Ok(Some(super::page_models(&mut self.list, handle, "settings-display")?))
    }

    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}
