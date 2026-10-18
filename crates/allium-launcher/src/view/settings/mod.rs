mod about;
mod appearance;
mod clock;
mod display;
mod language;
mod power;
mod system;
mod wifi;

use crate::view::settings::clock::Clock;

use self::about::About;
use self::display::Display;
use self::language::Language;
use self::power::Power;
use self::system::SystemUpdate;
use self::wifi::Wifi;

use std::collections::VecDeque;
use std::fmt::Debug;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::display::Display as DisplayTrait;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, ScrollList, View};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsState {
    selected: usize,
    child: Option<ChildState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChildState {
    selected: usize,
}

trait SettingsChild: View {
    fn clear_theme(&mut self) -> Result<()> {
        Ok(())
    }
    fn theme_models(
        &mut self,
        _: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        Ok(None)
    }

    fn save(&self) -> ChildState;
}

impl Debug for dyn SettingsChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("dyn SettingsChild").finish()
    }
}

#[derive(Debug)]
pub struct Settings {
    rect: Rect,
    res: Resources,
    list: ScrollList,
    child: Option<Box<dyn SettingsChild>>,
    button_hints: ButtonHints<String>,
    has_wifi: bool,
    dirty: bool,
}

impl Settings {
    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        self.child.is_none()
    }

    pub fn new(rect: Rect, res: Resources, state: SettingsState) -> Result<Self> {
        let Rect { x, y, w, h } = rect;

        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();

        let has_wifi = DefaultPlatform::has_wifi();
        let mut labels = Vec::with_capacity(8);
        if has_wifi {
            labels.push(locale.t("settings-system-update-menu"));
            labels.push(locale.t("settings-wifi"));
        }
        labels.push(locale.t("settings-clock"));
        labels.push(locale.t("settings-power"));
        labels.push(locale.t("settings-display"));
        labels.push(locale.t("settings-theme"));
        labels.push(locale.t("settings-language"));
        labels.push(locale.t("settings-about"));

        let mut list = ScrollList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                h - styles.ui.margin_y as u32 - styles.ui.ui_font.size,
            ),
            labels,
            Alignment::Left,
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        list.select(state.selected);

        let child: Option<Box<dyn SettingsChild>> = if let Some(child) = state.child {
            let mut selected = state.selected;
            if !has_wifi {
                selected += 2;
            };
            match selected {
                0 => Some(Box::new(SystemUpdate::new(rect, res.clone(), Some(child)))),
                1 => Some(Box::new(Wifi::new(rect, res.clone(), Some(child)))),
                2 => Some(Box::new(Clock::new(rect, res.clone(), Some(child)))),
                3 => Some(Box::new(Power::new(rect, res.clone(), Some(child)))),
                4 => Some(Box::new(Display::new(rect, res.clone(), Some(child)))),
                5 => Some(Box::new(appearance::Appearance::new(rect, res.clone()))),
                6 => Some(Box::new(Language::new(rect, res.clone(), Some(child)))),
                7 => Some(Box::new(About::new(rect, res.clone(), Some(child)))),
                _ => None,
            }
        } else {
            None
        };

        let button_hints = ButtonHints::new(
            res.clone(),
            vec![],
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

        std::mem::drop(locale);

        Ok(Self {
            rect,
            res: res.clone(),
            list,
            child,
            button_hints,
            has_wifi,
            dirty: true,
        })
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        self.list.set_visible_count_override(None)?;
        if let Some(child) = self.child.as_mut() {
            child.clear_theme()?;
        }
        Ok(())
    }
    pub(crate) fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        use common::theme::{
            frame::{Focus, FrameInputs, Row},
            pack::{Binding, ScreenId},
        };
        if let Some(child) = self.child.as_mut() {
            return child.theme_models(handle);
        }
        let screen = ScreenId::SettingsRoot;
        let count = crate::theme_adapter::capacity(handle, screen, Binding::SettingsRows);
        self.list.set_visible_count_override(Some(count))?;
        let locale = self.res.get::<Locale>();
        let mut keys = vec![];
        if self.has_wifi {
            keys.extend(["settings-system-update-menu", "settings-wifi"]);
        }
        keys.extend([
            "settings-clock",
            "settings-power",
            "settings-display",
            "settings-theme",
            "settings-language",
            "settings-about",
        ]);
        let mut input = FrameInputs::new(screen);
        input.pagination = Some(common::theme::frame::Pagination {
            page: self.list.selected() / count.max(1) + 1,
            total: keys.len().max(1).div_ceil(count.max(1)),
        });
        input.text.insert(Binding::Title, common::product::NAME.into());
        input.rows.insert(
            Binding::SettingsRows,
            keys.iter()
                .enumerate()
                .skip(self.list.window_start())
                .take(count)
                .map(|(i, key)| Row {
                    subtitle: String::new(),
                    id: format!("setting-{i}"),
                    label: locale.t(key),
                    value: if *key == "settings-theme" {
                        handle.pack().name.clone()
                    } else {
                        String::new()
                    },
                    enabled: true,
                })
                .collect(),
        );
        input.focus = Focus {
            component: crate::theme_adapter::component_id(handle, screen, Binding::SettingsRows),
            item: Some(format!("setting-{}", self.list.selected())),
        };
        Ok(Some(vec![input]))
    }

    pub fn save(&self) -> SettingsState {
        SettingsState {
            selected: self.list.selected(),
            child: self.child.as_ref().map(|c| c.save()),
        }
    }

    async fn select_entry(&mut self, _commands: Sender<Command>) -> Result<()> {
        let mut selected = self.list.selected();
        if !self.has_wifi {
            selected += 2
        };
        match selected {
            0 => {
                self.child = Some(Box::new(SystemUpdate::new(
                    self.rect,
                    self.res.clone(),
                    None,
                )))
            }
            1 => self.child = Some(Box::new(Wifi::new(self.rect, self.res.clone(), None))),
            2 => self.child = Some(Box::new(Clock::new(self.rect, self.res.clone(), None))),
            3 => self.child = Some(Box::new(Power::new(self.rect, self.res.clone(), None))),
            4 => self.child = Some(Box::new(Display::new(self.rect, self.res.clone(), None))),
            5 => {
                self.child = Some(Box::new(appearance::Appearance::new(
                    self.rect,
                    self.res.clone(),
                )))
            }
            6 => self.child = Some(Box::new(Language::new(self.rect, self.res.clone(), None))),
            7 => self.child = Some(Box::new(About::new(self.rect, self.res.clone(), None))),
            _ => unreachable!("Invalid index"),
        }
        self.dirty = true;
        Ok(())
    }
}

#[async_trait(?Send)]
impl View for Settings {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        if self.dirty {
            display.load(self.bounding_box(styles))?;
            if crate::theme_adapter::legacy_orbit(&self.res) {
                common::display::fill_rect(
                    &mut display.pixmap_mut(),
                    self.rect,
                    styles.ui.background_color,
                );
            }
            self.dirty = false;
        }

        if let Some(ref mut child) = self.child {
            return child.draw(display, styles);
        }

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;
        drawn |= self.button_hints.should_draw() && self.button_hints.draw(display, styles)?;

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        if let Some(child) = self.child.as_ref() {
            child.should_draw()
        } else {
            self.list.should_draw() || self.button_hints.should_draw()
        }
    }

    fn set_should_draw(&mut self) {
        self.dirty = true;
        if let Some(ref mut child) = self.child {
            child.set_should_draw();
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
        if let Some(ref mut child) = self.child {
            if child.handle_key_event(event, commands, bubble).await? {
                bubble.retain(|cmd| match cmd {
                    Command::CloseView => {
                        self.child = None;
                        self.set_should_draw();
                        false
                    }
                    _ => true,
                });
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            match event {
                KeyEvent::Pressed(Key::A) => {
                    self.select_entry(commands).await?;
                    Ok(true)
                }
                _ => self.list.handle_key_event(event, commands, bubble).await,
            }
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        if let Some(child) = self.child.as_deref() {
            vec![child as &dyn View]
        } else {
            vec![&self.list, &self.button_hints]
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        if let Some(child) = self.child.as_deref_mut() {
            vec![child as &mut dyn View]
        } else {
            vec![&mut self.list, &mut self.button_hints]
        }
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        self.rect
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

fn page_context(input: &mut common::theme::frame::FrameInputs, locale: &Locale, title_key: &str) {
    use common::theme::pack::Binding;
    let (page, icon) = match title_key {
        "settings-clock" => ("clock", "recents"),
        "settings-system-update-menu" => ("update", "apps"),
        "settings-wifi" => ("wifi", "wifi"),
        "settings-power" => ("power", "power"),
        "settings-theme" => ("theme", "settings"),
        "settings-language" => ("language", "library"),
        "settings-about" => ("about", "apps"),
        _ => ("display", "display"),
    };
    input.text.insert(Binding::Subtitle, locale.t(&format!("settings-context-{page}-heading")));
    input.text.insert(Binding::Message, locale.t(&format!("settings-context-{page}-description")));
    input.text.insert(Binding::Breadcrumb, icon.into());
}

fn page_models(
    list: &mut common::view::SettingsList,
    handle: &common::theme::ThemeHandle,
    title_key: &str,
) -> Result<Vec<common::theme::frame::FrameInputs>> {
    use common::theme::{
        frame::{Focus, FrameInputs, Hint},
        pack::{Binding, ScreenId},
    };
    let screen = ScreenId::SettingsPage;
    let count = crate::theme_adapter::capacity(handle, screen, Binding::SettingsRows);
    list.set_visible_count_override(Some(count))?;
    let locale = list.resources().get::<Locale>();
    let mut input = FrameInputs::new(screen);
    let title = if title_key == "settings-about" {
        common::product::NAME.to_owned()
    } else {
        locale.t(title_key)
    };
    input.text.insert(Binding::Title, title.clone());
    page_context(&mut input, &locale, title_key);
    input.rows.insert(Binding::SettingsRows, list.theme_rows());
    input.focus = Focus {
        component: crate::theme_adapter::component_id(handle, screen, Binding::SettingsRows),
        item: Some(format!("setting-{}", list.selected())),
    };
    let editable = title_key != "settings-about" && title_key != "settings-system-update-menu"
        && !(title_key == "settings-display" && list.selected() == 0)
        && !(title_key == "settings-wifi" && list.selected() == 1);
    if list.editing() {
        if title_key == "settings-clock" && list.selected() == 0 {
            input.hints.extend([
                Hint { button: "Up/Down".into(), label: locale.t("button-change") },
                Hint { button: "Left/Right".into(), label: locale.t("button-select-field") },
            ]);
        } else {
            input.hints.push(Hint { button: "Left/Right".into(), label: locale.t("button-change") });
        }
        input.hints.extend([
            Hint { button: "A".into(), label: locale.t("button-save") },
            Hint { button: "B".into(), label: locale.t("button-cancel") },
        ]);
    } else {
        if editable {
            input.hints.push(Hint { button: "A".into(), label: locale.t("button-edit") });
        }
        input.hints.push(Hint { button: "B".into(), label: locale.t("button-back") });
    }
    let mut models = vec![input];
    if let Some(capture) = list.theme_keyboard() {
        let mut keyboard = FrameInputs::new(ScreenId::Keyboard);
        keyboard.text.insert(Binding::Title, title);
        keyboard
            .text
            .insert(Binding::KeyboardValue, capture.display_value);
        keyboard.keyboard = capture.labels;
        keyboard.keyboard_cursor = capture.cursor;
        keyboard.hints = vec![
            Hint {
                button: "START".into(),
                label: locale.t("button-select"),
            },
            Hint {
                button: "B".into(),
                label: locale.t("button-back"),
            },
        ];
        models.push(keyboard);
    }
    Ok(models)
}

#[cfg(test)]
mod parity_b_tests;
