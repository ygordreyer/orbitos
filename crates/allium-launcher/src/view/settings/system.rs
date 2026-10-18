use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Label, SettingsList, View};
use tokio::sync::mpsc::Sender;

use crate::view::settings::{ChildState, SettingsChild};

pub struct SystemUpdate {
    rect: Rect,
    list: SettingsList,
    button_hints: ButtonHints<String>,
}

impl SystemUpdate {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;
        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();
        let labels = vec![locale.t("settings-about-version"), locale.t("settings-system-update-menu")];
        let values: Vec<Box<dyn View>> = vec![
            Box::new(Label::new(Point::zero(), common::product::VERSION.to_owned(), Alignment::Right, None)),
            Box::new(Label::new(Point::zero(), locale.t("settings-update-manual"), Alignment::Right, None)),
        ];
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

        drop(locale);
        drop(styles);

        let styles = res.get::<Stylesheet>();
        let mut list = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                list_height,
            ),
            labels,
            values,
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        drop(styles);
        if let Some(state) = state {
            list.select(state.selected);
        }

        Self { rect, list, button_hints }
    }
}

#[async_trait(?Send)]
impl View for SystemUpdate {
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
        if self.list.handle_key_event(event, commands, bubble).await? { return Ok(true); }

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

impl SettingsChild for SystemUpdate {
    fn clear_theme(&mut self) -> Result<()> { self.list.set_visible_count_override(None) }
    fn theme_models(&mut self, handle: &common::theme::ThemeHandle)
        -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        Ok(Some(super::page_models(&mut self.list, handle, "settings-system-update-menu")?))
    }

    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}
