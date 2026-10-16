use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::product;
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::{ButtonHint, ButtonHints, Label, SettingsList, View};
use tokio::sync::mpsc::Sender;

use crate::view::settings::{ChildState, SettingsChild};

pub struct About {
    rect: Rect,
    res: Resources,
    rows: Vec<(String, String)>,
    list: SettingsList,
    button_hints: ButtonHints<String>,
}

fn identity_rows(locale: &Locale) -> Vec<(String, String)> {
    [
        ("settings-about-product", product::NAME.to_owned()),
        ("settings-about-version", product::VERSION.to_owned()),
        ("settings-about-build", product::BUILD_ID.to_owned()),
        (
            "settings-system-model-name",
            DefaultPlatform::device_model(),
        ),
        ("settings-about-channel", product::UPDATE_CHANNEL.to_owned()),
        ("settings-about-upstream", product::UPSTREAM_NAME.to_owned()),
        ("settings-about-license", product::LICENSE.to_owned()),
        (
            "settings-about-authors",
            product::UPSTREAM_AUTHORS.to_owned(),
        ),
        ("settings-about-source", product::UPSTREAM_SOURCE.to_owned()),
        ("settings-about-warranty", product::LICENSE_NOTE.to_owned()),
    ]
    .into_iter()
    .map(|(key, value)| (locale.t(key), value))
    .collect()
}

impl About {
    pub fn new(rect: Rect, res: Resources, state: Option<ChildState>) -> Self {
        let Rect { x, y, w, .. } = rect;

        let firmware = DefaultPlatform::firmware();

        let mut sys = sysinfo::System::new();
        sys.refresh_memory();

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

        let mut rows = identity_rows(&locale);
        rows.extend([
            (locale.t("settings-system-firmware-version"), firmware),
            (
                locale.t("settings-system-operating-system-version"),
                sysinfo::System::long_os_version()
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|| locale.t("settings-system-unknown-value")),
            ),
            (
                locale.t("settings-system-kernel-version"),
                sysinfo::System::kernel_version()
                    .unwrap_or_else(|| locale.t("settings-system-unknown-value")),
            ),
            (
                locale.t("settings-system-memory-used"),
                format!(
                    "{}MB / {}MB",
                    sys.used_memory() / (1024 * 1024),
                    sys.total_memory() / (1024 * 1024)
                ),
            ),
        ]);
        let mut list = SettingsList::new(
            res.clone(),
            Rect::new(
                x + styles.ui.margin_x,
                y,
                w - styles.ui.margin_x as u32 * 2,
                list_height,
            ),
            rows.iter().map(|(label, _)| label.clone()).collect(),
            rows.iter()
                .map(|(_, value)| {
                    Box::new(Label::new(
                        Point::zero(),
                        value.clone(),
                        Alignment::Right,
                        None,
                    )) as Box<dyn View>
                })
                .collect(),
            styles.ui.ui_font.size + styles.ui.padding_y as u32,
        );
        if let Some(state) = state {
            list.select(state.selected);
        }

        std::mem::drop(locale);
        std::mem::drop(styles);

        Self {
            rect,
            res,
            rows,
            list,
            button_hints,
        }
    }
}

#[async_trait(?Send)]
impl View for About {
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
        match event {
            KeyEvent::Pressed(Key::B) => {
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            _ => self.list.handle_key_event(event, commands, bubble).await,
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

impl SettingsChild for About {
    fn clear_theme(&mut self) -> Result<()> {
        self.list.set_visible_count_override(None)
    }

    fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        use common::theme::{
            frame::{Focus, FrameInputs, Hint, Row},
            pack::{Binding, ScreenId},
        };
        let screen = ScreenId::SettingsPage;
        let count = crate::theme_adapter::capacity(handle, screen, Binding::SettingsRows);
        self.list.set_visible_count_override(Some(count))?;
        let mut input = FrameInputs::new(screen);
        input.text.insert(Binding::Title, product::NAME.into());
        input.rows.insert(
            Binding::SettingsRows,
            self.rows
                .iter()
                .enumerate()
                .skip(self.list.window_start())
                .take(count)
                .map(|(i, (label, value))| Row {
                    id: format!("about-{i}"),
                    label: label.clone(),
                    value: value.clone(),
                    enabled: true,
                })
                .collect(),
        );
        input.focus = Focus {
            component: crate::theme_adapter::component_id(handle, screen, Binding::SettingsRows),
            item: Some(format!("about-{}", self.list.selected())),
        };
        input.hints = vec![Hint {
            button: "B".into(),
            label: self.res.get::<Locale>().t("button-back"),
        }];
        Ok(Some(vec![input]))
    }

    fn save(&self) -> ChildState {
        ChildState {
            selected: self.list.selected(),
        }
    }
}

#[cfg(all(test, not(any(feature = "miyoo", feature = "simulator"))))]
mod identity_tests {
    use super::*;
    use common::theme::{
        ThemeHandle,
        fonts::FontBank,
        frame::FrameInputs,
        pack::{Binding, ScreenId},
        render::PreparedFrame,
        runtime::ThemeSnapshot,
    };
    use std::path::Path;

    #[test]
    fn orbitos_about_identity_locales_and_all_presentations_render() {
        let locale = Locale::new("en-US");
        let expected = identity_rows(&locale);
        assert_eq!(
            expected
                .iter()
                .map(|(_, v)| v.as_str())
                .take(7)
                .collect::<Vec<_>>(),
            [
                product::NAME,
                product::VERSION,
                product::BUILD_ID,
                "Mock",
                product::UPDATE_CHANNEL,
                product::UPSTREAM_NAME,
                product::LICENSE
            ]
        );
        for language in locale.languages() {
            let fallback = identity_rows(&Locale::new(&language));
            assert_eq!(fallback.len(), expected.len());
            for (label, value) in fallback {
                assert!(
                    !label.is_empty() && !label.starts_with("settings-"),
                    "{language}: {label}"
                );
                assert!(!value.is_empty());
            }
        }
        let mut resources = type_map::TypeMap::new();
        resources.insert(Locale::new("en-US"));
        resources.insert(Stylesheet::load().unwrap());
        resources.insert(common::geom::Size::new(640, 480));
        let res = Resources::new(resources);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let fonts = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        for id in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            let handle = ThemeHandle::prepare(
                ThemeSnapshot::load(&root.join("themes").join(id)).unwrap(),
                &fonts,
                1,
            )
            .unwrap();
            let mut about = About::new(Rect::new(0, 56, 640, 384), res.clone(), None);
            let mut seen = std::collections::BTreeSet::new();
            for selected in [0, 3, 6, 9] {
                about.list.select(selected);
                let models = about.theme_models(&handle).unwrap().unwrap();
                let model = &models[0];
                assert_eq!(model.screen, ScreenId::SettingsPage);
                assert_eq!(model.text[&Binding::Title], product::NAME);
                for row in &model.rows[&Binding::SettingsRows] {
                    seen.insert(row.value.clone());
                }
                let frame = PreparedFrame::prepare(handle.clone(), model).unwrap();
                assert!(
                    frame.text_boxes.iter().any(|(id, _)| id == "title")
                );
                if let Some(dir) = std::env::var_os("ORBITOS_IDENTITY_PROOF_DIR") {
                    let dir = std::path::PathBuf::from(dir);
                    std::fs::create_dir_all(&dir).unwrap();
                    frame
                        .pixels()
                        .save_png(dir.join(format!("about-{id}-{selected}.png")))
                        .unwrap();
                }
            }
            for (_, value) in expected.iter().take(10) {
                assert!(seen.contains(value), "About omits {value} in {id}");
            }
            let mut settings = super::super::Settings::new(
                Rect::new(0, 56, 640, 384),
                res.clone(),
                Default::default(),
            )
            .unwrap();
            let models: Vec<FrameInputs> = settings.theme_models(&handle).unwrap().unwrap();
            assert_eq!(models[0].text[&Binding::Title], product::NAME);
        }
    }
}
