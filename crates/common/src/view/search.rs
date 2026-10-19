use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

use crate::command::{Command, Value};
use crate::database::Database;
use crate::geom::{Point, Rect};
use crate::locale::Locale;
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::Stylesheet;
use crate::view::{Keyboard, View};

#[derive(Debug, Clone, PartialEq)]
pub enum SearchState {
    Inactive,
    Active,
    Searching,
}

#[derive(Debug)]
pub struct SearchView {
    res: Resources,
    keyboard: Option<Keyboard>,
    state: SearchState,
}

impl SearchView {
    pub fn new(res: Resources) -> Self {
        Self {
            res,
            keyboard: None,
            state: SearchState::Inactive,
        }
    }

    pub fn theme_inputs(&self) -> Option<crate::view::input::keyboard::KeyboardThemeInputs> {
        self.keyboard.as_ref().map(Keyboard::theme_inputs)
    }

    pub fn activate(&mut self) {
        self.state = SearchState::Active;
        self.keyboard = Some(Keyboard::new(self.res.clone(), String::new(), false));
    }

    pub fn activate_with_value(&mut self, initial_value: String) {
        self.state = SearchState::Active;
        self.keyboard = Some(Keyboard::new(self.res.clone(), initial_value, false));
    }

    pub fn deactivate(&mut self) {
        self.state = SearchState::Inactive;
        self.keyboard = None;
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, SearchState::Inactive)
    }

    pub fn state(&self) -> &SearchState {
        &self.state
    }

    async fn check_database(&self, commands: &Sender<Command>) -> Result<()> {
        if !self.res.get::<Database>().has_indexed()? {
            let toast = self.res.get::<Locale>().t("populating-database");
            commands.send(Command::Toast(toast, None)).await?;
            commands.send(Command::PopulateDb).await?;
            commands
                .send(Command::Toast(String::new(), Some(Duration::ZERO)))
                .await?;
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl View for SearchView {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if let Some(keyboard) = self.keyboard.as_mut()
            && matches!(self.state, SearchState::Active)
        {
            return keyboard.draw(display, styles);
        }
        Ok(false)
    }

    fn should_draw(&self) -> bool {
        matches!(self.state, SearchState::Active)
            && self
                .keyboard
                .as_ref()
                .map(|k| k.should_draw())
                .unwrap_or(false)
    }

    fn set_should_draw(&mut self) {
        if matches!(self.state, SearchState::Active)
            && let Some(keyboard) = self.keyboard.as_mut()
        {
            keyboard.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if !matches!(self.state, SearchState::Active) {
            return Ok(false);
        }

        if let Some(keyboard) = self.keyboard.as_mut()
            && keyboard
                .handle_key_event(event, commands.clone(), bubble)
                .await?
        {
            let mut query = None;
            bubble.retain_mut(|c| match c {
                Command::ValueChanged(_, val) => {
                    if let Value::String(val) = val {
                        query = Some(val.clone());
                    }
                    false
                }
                Command::CloseView => {
                    self.deactivate();
                    false
                }
                _ => true,
            });

            if let Some(query) = query {
                self.check_database(&commands).await?;
                self.state = SearchState::Searching;
                commands.send(Command::Search(query)).await?;
            }

            return Ok(true);
        }

        Ok(false)
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![]
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        self.keyboard
            .as_mut()
            .map(|k| k.bounding_box(styles))
            .unwrap_or_else(|| Rect::new(0, 0, 0, 0))
    }

    fn set_position(&mut self, _point: Point) {}
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::platform::Key;

    async fn submit_search(indexed: bool) -> Vec<Command> {
        let res = Resources::new(type_map::TypeMap::new());
        let database = Database::in_memory().unwrap();
        if indexed { database.set_has_indexed(true).unwrap(); }
        assert_eq!(database.has_indexed().unwrap(), indexed);
        res.insert(database);
        res.insert(Locale::new("en-US"));
        res.insert(Stylesheet::new());
        let mut search = SearchView::new(res);
        search.activate_with_value("ko".into());
        let (send, mut recv) = tokio::sync::mpsc::channel(8);
        let mut bubble = VecDeque::new();
        assert!(search.handle_key_event(KeyEvent::Pressed(Key::Start), send, &mut bubble).await.unwrap());
        assert!(bubble.is_empty());
        assert!(matches!(recv.try_recv(), Ok(Command::Redraw)));
        let mut commands = Vec::new();
        while let Ok(command) = recv.try_recv() { commands.push(command); }
        commands
    }

    #[tokio::test]
    async fn search_first_index_queues_typed_query_after_populate() {
        let commands = submit_search(false).await;
        assert_eq!(commands.len(), 4);
        assert!(matches!(&commands[0], Command::Toast(_, None)));
        assert!(matches!(&commands[1], Command::PopulateDb));
        assert!(matches!(&commands[2], Command::Toast(text, Some(duration)) if text.is_empty() && duration.is_zero()));
        assert!(matches!(&commands[3], Command::Search(query) if query == "ko"));
    }

    #[tokio::test]
    async fn search_indexed_database_queues_only_typed_query() {
        let commands = submit_search(true).await;
        assert_eq!(commands.len(), 1);
        assert!(matches!(&commands[0], Command::Search(query) if query == "ko"));
    }
}
