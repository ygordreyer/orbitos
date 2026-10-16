use std::collections::VecDeque;
use std::env;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use common::command::Command;
use common::constants::ALLIUM_TIMEZONE;

use common::display::Display as DisplayTrait;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, DateTime, Select, SettingsList, View};

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Sender;

use crate::view::settings::{ChildState, SettingsChild};

pub struct Clock {
    rect: Rect,
    timezone: usize,
    list: SettingsList,
    button_hints: ButtonHints<String>,
}

// POSIX TZ offset are opposite of UTC naming convention:
// https://unix.stackexchange.com/questions/104088/why-does-tz-utc-8-produce-dates-that-are-utc8
const TIMEZONE_VALUES: [&str; 39] = [
    "UTC-0",
    "UTC-1",
    "UTC-2",
    "UTC-3",
    "UTC-3:30",
    "UTC-4",
    "UTC-4:30",
    "UTC-5",
    "UTC-5:30",
    "UTC-5:45",
    "UTC-6",
    "UTC-6:30",
    "UTC-7",
    "UTC-8",
    "UTC-8:45",
    "UTC-9",
    "UTC-9:30",
    "UTC-10",
    "UTC-10:30",
    "UTC-11",
    "UTC-12",
    "UTC-12:45",
    "UTC-13",
    "UTC-13:45",
    "UTC-14",
    "UTC+12",
    "UTC+11",
    "UTC+10",
    "UTC+9:30",
    "UTC+9",
    "UTC+8",
    "UTC+7",
    "UTC+6",
    "UTC+5",
    "UTC+4",
    "UTC+3",
    "UTC+3:30",
    "UTC+2",
    "UTC+1",
];

const TIMEZONE_NAMES: [&str; 39] = [
    "UTC+0",
    "UTC+1",
    "UTC+2",
    "UTC+3",
    "UTC+3:30",
    "UTC+4",
    "UTC+4:30",
    "UTC+5",
    "UTC+5:30",
    "UTC+5:45",
    "UTC+6",
    "UTC+6:30",
    "UTC+7",
    "UTC+8",
    "UTC+8:45",
    "UTC+9",
    "UTC+9:30",
    "UTC+10",
    "UTC+10:30",
    "UTC+11",
    "UTC+12",
    "UTC+12:45",
    "UTC+13",
    "UTC+13:45",
    "UTC+14",
    "UTC-12",
    "UTC-11",
    "UTC-10",
    "UTC-9:30",
    "UTC-9",
    "UTC-8",
    "UTC-7",
    "UTC-6",
    "UTC-5",
    "UTC-4",
    "UTC-3",
    "UTC-3:30",
    "UTC-2",
    "UTC-1",
];

impl Clock {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;

        let timezone = env::var("TZ")
            .map(|tz| TIMEZONE_VALUES.iter().position(|&s| s == tz).unwrap_or(0))
            .unwrap_or(0);
        let locale = res.get::<Locale>();
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
            vec![
                locale.t("settings-clock-datetime"),
                locale.t("settings-clock-timezone"),
            ],
            vec![
                Box::new(DateTime::new(
                    Point::zero(),
                    Local::now().naive_local(),
                    Alignment::Right,
                )),
                Box::new(Select::new(
                    Point::zero(),
                    timezone,
                    TIMEZONE_NAMES.iter().map(|s| s.to_string()).collect(),
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
            timezone,
            list,
            button_hints,
        }
    }
}

#[async_trait(?Send)]
impl View for Clock {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        let mut drawn = false;

        drawn |= self.list.should_draw() && self.list.draw(display, styles)?;

        if self.button_hints.should_draw() {
            let bbox = self.button_hints.bounding_box(styles);
            display.load(Rect::new(
                self.rect.x,
                bbox.y - styles.ui.margin_x,
                self.rect.w,
                bbox.h,
            ))?;
            drawn |= self.button_hints.draw(display, styles)?;
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
                        0 => {
                            let datetime = val.as_datetime().unwrap();
                            let datetime = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
                            tokio::process::Command::new("date")
                                .args(["--set", &datetime])
                                .spawn()?
                                .wait()
                                .await?;
                        }
                        1 => {
                            self.timezone = val.as_int().unwrap() as usize;
                            let timezone = TIMEZONE_VALUES[self.timezone];
                            File::create(ALLIUM_TIMEZONE.as_path())
                                .await?
                                .write_all(timezone.as_bytes())
                                .await?;
                            unsafe { env::set_var("TZ", timezone) };
                            self.list.set_right(
                                0,
                                Box::new(DateTime::new(
                                    Point::zero(),
                                    Local::now().naive_local(),
                                    Alignment::Right,
                                )),
                            );
                        }
                        _ => unreachable!("Invalid index"),
                    }
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

impl SettingsChild for Clock {
    fn clear_theme(&mut self) -> Result<()> { self.list.set_visible_count_override(None) }
    fn theme_models(&mut self, handle: &common::theme::ThemeHandle)
        -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        Ok(Some(super::page_models(&mut self.list, handle, "settings-clock")?))
    }

    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}
