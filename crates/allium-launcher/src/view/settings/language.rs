use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;

use common::geom::{Alignment, Point, Rect};
use common::locale::{Locale, LocaleSettings};
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Select, SettingsList, View};

use tokio::sync::mpsc::Sender;

use crate::view::settings::{ChildState, SettingsChild};

pub struct Language {
    rect: Rect,
    langs: Vec<String>,
    settings: LocaleSettings,
    list: SettingsList,
    button_hints: ButtonHints<String>,
    has_changed: bool,
}

impl Language {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;

        let settings = LocaleSettings::load().unwrap();

        let locale = res.get::<Locale>();
        let langs = locale.languages();
        let lang = langs.iter().position(|l| l == &settings.lang).unwrap();

        let styles = res.get::<Stylesheet>();

        let mut button_hints = ButtonHints::new(
            res.clone(),
            vec![],
            vec![
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::A,
                    locale.t("button-edit"),
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
        let list_height = (button_hints_rect.y - y) as u32;

        let mut list = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                list_height,
            ),
            vec![locale.t("settings-language-language")],
            vec![Box::new(Select::new(
                Point::zero(),
                lang,
                langs
                    .iter()
                    .map(|l| {
                        let name = locale.t(&format!("lang-{}", l));
                        if name.is_empty() { l.clone() } else { name }
                    })
                    .collect(),
                Alignment::Right,
            ))],
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        if let Some(state) = state {
            list.select(state.selected);
        }

        Self {
            rect,
            langs,
            settings,
            list,
            button_hints,
            has_changed: false,
        }
    }
}

#[async_trait(?Send)]
impl View for Language {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        if self.list.should_draw() && self.list.draw(display, styles)? {
            drawn = true;
        }

        if self.button_hints.should_draw() && self.button_hints.draw(display, styles)? {
            drawn = true;
        }

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
            while let Some(command) = bubble.pop_front() {
                if let Command::ValueChanged(i, val) = command {
                    match i {
                        0 => self
                            .settings
                            .lang
                            .clone_from(&self.langs[val.as_int().unwrap() as usize]),
                        _ => unreachable!("Invalid index"),
                    }

                    self.has_changed |= i == 0;

                    commands
                        .send(Command::SaveLocaleSettings(self.settings.clone()))
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

impl SettingsChild for Language {
    fn clear_theme(&mut self) -> Result<()> { self.list.set_visible_count_override(None) }
    fn theme_models(&mut self, handle: &common::theme::ThemeHandle)
        -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        Ok(Some(super::page_models(&mut self.list, handle, "settings-language")?))
    }

    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}
