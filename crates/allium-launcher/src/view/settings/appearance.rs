use super::{ChildState, SettingsChild};
use anyhow::Result;
use async_trait::async_trait;
use common::{
    command::Command,
    geom::{Alignment, Point, Rect},
    platform::{DefaultPlatform, Key, KeyEvent, Platform},
    resources::Resources,
    stylesheet::Stylesheet,
    theme::{
        ThemeChoice, ThemeHandle,
        frame::{Focus, FrameInputs, Hint, Row},
        pack::{Binding, ScreenId},
    },
    view::{ScrollList, View},
};
use std::collections::VecDeque;
use tokio::sync::mpsc::Sender;
pub struct Appearance {
    res: Resources,
    rect: Rect,
    choices: Vec<ThemeChoice>,
    list: ScrollList,
    error: Option<String>,
    legacy: Option<Box<super::theme::Theme>>,
}
impl Appearance {
    pub fn new(rect: Rect, res: Resources) -> Self {
        let (choices, error) = match crate::theme_adapter::choices(&res) {
            Ok(c) => (c.choices, None),
            Err(e) => (vec![], Some(e.to_string())),
        };
        let selected = crate::theme_adapter::snapshot(&res)
            .and_then(|h| {
                choices
                    .iter()
                    .position(|c| c.id.as_deref() == Some(h.pack().id.as_str()))
            })
            .unwrap_or(0);
        let mut labels = choices.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        labels.push("Typography (read only)".into());
        labels.push("Legacy appearance settings".into());
        let mut list = ScrollList::new(res.clone(), rect, labels, Alignment::Left, 40);
        list.select(selected);
        Self {
            res,
            rect,
            choices,
            list,
            error,
            legacy: None,
        }
    }
    pub fn theme_models(&mut self, handle: &ThemeHandle) -> Result<Option<Vec<FrameInputs>>> {
        if self.legacy.is_some() {
            return Ok(None);
        }
        if let Some(error) = &self.error {
            let mut input = FrameInputs::new(ScreenId::NetworkDialog);
            input
                .text
                .insert(Binding::Title, "Theme unavailable".into());
            input.text.insert(Binding::Message, error.clone());
            input.hints = vec![Hint {
                button: "B".into(),
                label: "Back".into(),
            }];
            return Ok(Some(vec![input]));
        }
        let screen = ScreenId::SettingsPage;
        let count = crate::theme_adapter::capacity(handle, screen, Binding::SettingsRows);
        self.list.set_visible_count_override(Some(count))?;
        let current = crate::theme_adapter::snapshot(&self.res);
        let mut rows = self
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| Row {
                id: format!("appearance-{i}"),
                label: c.name.clone(),
                value: if c.id.as_deref() == current.as_ref().map(|h| h.pack().id.as_str()) {
                    "Active".into()
                } else {
                    String::new()
                },
                enabled: true,
            })
            .collect::<Vec<_>>();
        rows.push(Row {
            id: format!("appearance-{}", rows.len()),
            label: "Typography (read only)".into(),
            value: format!(
                "{} px rows · pack fonts",
                handle.pack().tokens.typography[&common::theme::pack::Role::Row].size
            ),
            enabled: true,
        });
        rows.push(Row {
            id: format!("appearance-{}", rows.len()),
            label: "Legacy appearance settings".into(),
            value: if current.is_none() {
                "Available".into()
            } else {
                "Choose Legacy first".into()
            },
            enabled: true,
        });
        let mut input = FrameInputs::new(screen);
        input.text.insert(Binding::Title, "Appearance".into());
        input.rows.insert(
            Binding::SettingsRows,
            rows.into_iter()
                .skip(self.list.window_start())
                .take(count)
                .collect(),
        );
        input.focus = Focus {
            component: handle.pack().screens[&screen]
                .components
                .iter()
                .find(|c| c.binding == Some(Binding::SettingsRows))
                .unwrap()
                .id
                .clone(),
            item: Some(format!("appearance-{}", self.list.selected())),
        };
        Ok(Some(vec![input]))
    }
}
#[async_trait(?Send)]
impl View for Appearance {
    fn draw(
        &mut self,
        d: &mut <DefaultPlatform as Platform>::Display,
        s: &Stylesheet,
    ) -> Result<bool> {
        if let Some(legacy) = self.legacy.as_mut() {
            legacy.draw(d, s)
        } else {
            self.list.draw(d, s)
        }
    }
    fn should_draw(&self) -> bool {
        self.legacy
            .as_ref()
            .map_or(self.list.should_draw(), |l| l.should_draw())
    }
    fn set_should_draw(&mut self) {
        self.list.set_should_draw();
        if let Some(l) = self.legacy.as_mut() {
            l.set_should_draw();
        }
    }
    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if let Some(legacy) = self.legacy.as_mut() {
            let handled = legacy.handle_key_event(event, commands, bubble).await?;
            if bubble.iter().any(|c| matches!(c, Command::CloseView)) {
                bubble.retain(|c| !matches!(c, Command::CloseView));
                self.legacy = None;
                self.set_should_draw();
            }
            return Ok(handled);
        }
        if self.error.is_some() && event == KeyEvent::Pressed(Key::B) {
            self.error = None;
            self.set_should_draw();
            return Ok(true);
        }
        if event == KeyEvent::Pressed(Key::A) {
            let selected = self.list.selected();
            if let Some(choice) = self.choices.get(selected) {
                if let Err(error) = crate::theme_adapter::choose(&self.res, choice.id.as_deref()) {
                    self.error = Some(error.to_string());
                }
            } else if selected == self.choices.len() + 1 {
                if crate::theme_adapter::snapshot(&self.res).is_none() {
                    self.legacy = Some(Box::new(super::theme::Theme::new(
                        self.rect,
                        self.res.clone(),
                        None,
                    )));
                } else {
                    self.error = Some("Choose Legacy to edit legacy fonts and colors.".into());
                }
            }
            self.set_should_draw();
            return Ok(true);
        }
        if event == KeyEvent::Pressed(Key::B) {
            bubble.push_back(Command::CloseView);
            return Ok(true);
        }
        self.list.handle_key_event(event, commands, bubble).await
    }
    fn children(&self) -> Vec<&dyn View> {
        vec![&self.list]
    }
    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.list]
    }
    fn bounding_box(&mut self, _: &Stylesheet) -> Rect {
        self.rect
    }
    fn set_position(&mut self, _: Point) {}
}
impl SettingsChild for Appearance {
    fn clear_theme(&mut self) -> Result<()> {
        self.list.set_visible_count_override(None)
    }
    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
    fn theme_models(&mut self, handle: &ThemeHandle) -> Result<Option<Vec<FrameInputs>>> {
        Appearance::theme_models(self, handle)
    }
}

impl std::fmt::Debug for Appearance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Appearance")
            .field("choices", &self.choices.len())
            .field("selected", &self.list.selected())
            .finish()
    }
}
