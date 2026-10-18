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
}

fn cycle(index: usize, count: usize, backwards: bool) -> usize {
    if backwards {
        (index + count - 1) % count
    } else {
        (index + 1) % count
    }
}
fn rows(choices: &[ThemeChoice], current: Option<&ThemeHandle>) -> Vec<Row> {
    let current = current.filter(|h| {
        choices
            .iter()
            .any(|c| c.id.as_deref() == Some(&h.pack().id))
    });
    let mut rows = vec![Row {
        subtitle: String::new(),
        id: "appearance-0".into(),
        label: "Theme".into(),
        value: current
            .map(|h| {
                choices
                    .iter()
                    .find(|c| c.id.as_deref() == Some(&h.pack().id))
                    .unwrap()
                    .name
                    .clone()
            })
            .unwrap_or_else(|| "Choose a theme".into()),
        enabled: !choices.is_empty(),
    }];
    if let Some(current) = current {
        for setting in &current.pack().settings {
            let option = setting
                .options
                .iter()
                .find(|o| current.settings().get(&setting.id) == Some(&o.id))
                .unwrap();
            rows.push(Row {
                subtitle: String::new(),
                id: format!("appearance-{}", rows.len()),
                label: setting.label.clone(),
                value: option.label.clone(),
                enabled: true,
            });
        }
    }
    rows
}
impl Appearance {
    pub fn new(rect: Rect, res: Resources) -> Self {
        let (choices, error) = match crate::theme_adapter::choices(&res) {
            Ok(c) => (c.choices, None),
            Err(e) => (vec![], Some(e.to_string())),
        };
        let labels = rows(&choices, crate::theme_adapter::snapshot(&res).as_ref())
            .iter()
            .map(|r| format!("{}: {}", r.label, r.value))
            .collect();
        let list = ScrollList::new(res.clone(), rect, labels, Alignment::Left, 40);
        Self {
            res,
            rect,
            choices,
            list,
            error,
        }
    }
    fn refresh(&mut self) {
        let selected = self.list.selected();
        let labels: Vec<_> = rows(
            &self.choices,
            crate::theme_adapter::snapshot(&self.res).as_ref(),
        )
        .iter()
        .map(|r| format!("{}: {}", r.label, r.value))
        .collect();
        let index = selected.min(labels.len() - 1);
        self.list.set_items(labels, false);
        self.list.select(index);
    }
    fn change(&mut self, backwards: bool) -> Result<()> {
        let current = crate::theme_adapter::snapshot(&self.res);
        if self.list.selected() == 0 {
            if self.choices.is_empty() {
                return Ok(());
            }
            let index = current.as_ref().and_then(|h| {
                self.choices
                    .iter()
                    .position(|c| c.id.as_deref() == Some(&h.pack().id))
            });
            let index = index.map_or(if backwards { self.choices.len() - 1 } else { 0 }, |i| {
                cycle(i, self.choices.len(), backwards)
            });
            crate::theme_adapter::choose(&self.res, self.choices[index].id.as_deref())?;
        } else if let Some(current) = current {
            if let Some(setting) = current.pack().settings.get(self.list.selected() - 1) {
                let index = setting
                    .options
                    .iter()
                    .position(|o| current.settings().get(&setting.id) == Some(&o.id))
                    .unwrap();
                let option = &setting.options[cycle(index, setting.options.len(), backwards)];
                crate::theme_adapter::choose_setting(&self.res, &setting.id, &option.id)?;
            }
        }
        self.refresh();
        Ok(())
    }
    pub fn theme_models(&mut self, handle: &ThemeHandle) -> Result<Option<Vec<FrameInputs>>> {
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
        let mut input = FrameInputs::new(screen);
        input.pagination = Some(common::theme::frame::Pagination {page:self.list.selected()/count.max(1)+1,total:rows(&self.choices,Some(handle)).len().max(1).div_ceil(count.max(1))});
        let locale = self.res.get::<common::locale::Locale>();
        input.text.insert(Binding::Title, locale.t("settings-theme"));
        super::page_context(&mut input, &locale, "settings-theme");
        input.rows.insert(
            Binding::SettingsRows,
            rows(&self.choices, Some(handle))
                .into_iter()
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
        input.hints = vec![
            Hint {
                button: "Dpad".into(),
                label: "Left/Right: change".into(),
            },
            Hint {
                button: "B".into(),
                label: "Back".into(),
            },
        ];
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
        self.list.draw(d, s)
    }
    fn should_draw(&self) -> bool {
        self.list.should_draw()
    }
    fn set_should_draw(&mut self) {
        self.list.set_should_draw();
    }
    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if self.error.is_some() {
            if event == KeyEvent::Pressed(Key::B) {
                self.error = None;
                self.set_should_draw();
            }
            return Ok(true);
        }
        if !matches!(
            event,
            KeyEvent::Pressed(Key::Left | Key::Right | Key::A)
                | KeyEvent::Autorepeat(Key::Left | Key::Right)
        ) {
            if let Err(error) = crate::theme_adapter::flush_settings(&self.res) {
                self.error = Some(format!("{error:#}"));
                self.set_should_draw();
                return Ok(true);
            }
        }
        match event {
            KeyEvent::Pressed(Key::Left | Key::Right | Key::A)
            | KeyEvent::Autorepeat(Key::Left | Key::Right) => {
                if let Err(error) = self.change(matches!(
                    event,
                    KeyEvent::Pressed(Key::Left) | KeyEvent::Autorepeat(Key::Left)
                )) {
                    self.error = Some(format!("{error:#}"));
                }
                self.set_should_draw();
                Ok(true)
            }
            KeyEvent::Pressed(Key::B) => {
                bubble.push_back(Command::CloseView);
                Ok(true)
            }
            _ => self.list.handle_key_event(event, commands, bubble).await,
        }
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
impl Drop for Appearance {
    fn drop(&mut self) {
        if let Err(error) = crate::theme_adapter::flush_settings(&self.res) {
            log::warn!("Theme settings could not be saved on view exit: {error:#}");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme_adapter::{SharedThemeService, ThemeService};
    use common::theme::{Boundary, ThemeRuntime};
    use std::{cell::RefCell, rc::Rc};

    #[tokio::test]
    async fn settings_rows_navigation_wrap_live_change_and_reopen() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("allium-appearance-settings-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let mut runtime = ThemeRuntime::bind(
            &root,
            &repo.join("themes"),
            &repo.join("static/.allium/fonts"),
        )
        .unwrap();
        let active = runtime.choose("compact-plus").unwrap();
        let mut map = type_map::TypeMap::new();
        map.insert(Stylesheet::new());
        map.insert(common::locale::Locale::new("en-US"));
        map.insert(active.snapshot.clone());
        map.insert(Rc::new(RefCell::new(ThemeService {
            runtime: Some(runtime),
            status: active.status,
        })) as SharedThemeService);
        let res = Resources::new(map);
        let mut view = Appearance::new(Rect::new(0, 0, 640, 400), res.clone());
        assert_eq!(view.choices.len(), 4);
        let current = crate::theme_adapter::snapshot(&res).unwrap();
        assert_eq!(
            rows(&view.choices, Some(&current))
                .iter()
                .map(|r| (r.label.as_str(), r.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Theme", "Compact+"),
                ("Cover shape", "Square"),
                ("Color scheme", "Orange")
            ]
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut bubble = VecDeque::new();
        for key in [Key::Down, Key::Left] {
            assert!(
                view.handle_key_event(KeyEvent::Pressed(key), tx.clone(), &mut bubble)
                    .await
                    .unwrap()
            );
        }
        assert_eq!(view.list.selected(), 1);
        let tall = crate::theme_adapter::snapshot(&res).unwrap();
        assert_eq!(tall.settings()["cover_shape"], "tall");
        assert_ne!(tall.digest(), current.digest());
        view.handle_key_event(KeyEvent::Pressed(Key::Down), tx.clone(), &mut bubble)
            .await
            .unwrap();
        view.handle_key_event(KeyEvent::Pressed(Key::Left), tx.clone(), &mut bubble)
            .await
            .unwrap();
        let black = crate::theme_adapter::snapshot(&res).unwrap();
        assert_eq!(black.settings()["color_scheme"], "black");
        let read_record = || -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(root.join("ui-theme.json")).unwrap()).unwrap()
        };
        let before_release = read_record();
        assert_eq!(before_release["settings"]["color_scheme"], "orange");
        view.handle_key_event(KeyEvent::Released(Key::Left), tx.clone(), &mut bubble)
            .await
            .unwrap();
        let saved = read_record();
        assert_eq!(saved["settings"]["color_scheme"], "black");
        assert_eq!(
            saved["generation"].as_u64().unwrap(),
            before_release["generation"].as_u64().unwrap() + 1
        );
        view.handle_key_event(KeyEvent::Released(Key::Left), tx.clone(), &mut bubble)
            .await
            .unwrap();
        assert_eq!(read_record(), saved);
        let models = view.theme_models(&black).unwrap().unwrap();
        models[0].validate(&black).unwrap();
        common::theme::render::PreparedFrame::prepare(black.clone(), &models[0]).unwrap();
        assert_eq!(models[0].focus.item.as_deref(), Some("appearance-2"));
        assert_eq!(models[0].rows[&Binding::SettingsRows][2].value, "Black");
        view.handle_key_event(KeyEvent::Autorepeat(Key::Right), tx.clone(), &mut bubble)
            .await
            .unwrap();
        assert_eq!(
            crate::theme_adapter::snapshot(&res).unwrap().settings()["color_scheme"],
            "orange"
        );
        view.list.select(0);
        view.handle_key_event(KeyEvent::Pressed(Key::Right), tx.clone(), &mut bubble)
            .await
            .unwrap();
        let first = crate::theme_adapter::snapshot(&res).unwrap();
        assert_eq!(first.pack().id, "orbit-hero");
        assert_eq!(first.settings()["cover_shape"], "square");
        assert_eq!(rows(&view.choices, Some(&first)).len(), 2);
        view.handle_key_event(KeyEvent::Pressed(Key::Left), tx.clone(), &mut bubble)
            .await
            .unwrap();
        assert_eq!(
            crate::theme_adapter::snapshot(&res).unwrap().pack().id,
            "compact-plus"
        );
        let reopened = Appearance::new(view.rect, res.clone());
        assert_eq!(
            rows(
                &reopened.choices,
                crate::theme_adapter::snapshot(&res).as_ref()
            )
            .len(),
            3
        );
        let reloaded = ThemeRuntime::bind(
            &root,
            &repo.join("themes"),
            &repo.join("static/.allium/fonts"),
        )
        .unwrap()
        .activate(Boundary::Launcher)
        .snapshot
        .unwrap();
        assert_eq!(
            reloaded.digest(),
            crate::theme_adapter::snapshot(&res).unwrap().digest()
        );
        assert!(bubble.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
