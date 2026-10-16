use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::geom::{Point, Rect};
use common::platform::{DefaultPlatform, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::view::View;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

pub mod recents_carousel;
pub mod recents_list;

pub use recents_carousel::{RecentsCarousel, RecentsCarouselState};
pub use recents_list::{RecentsList, RecentsListState, RecentsSort};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecentsState {
    Carousel(RecentsCarouselState),
    List(RecentsListState),
}

impl Default for RecentsState {
    fn default() -> Self {
        RecentsState::List(RecentsListState {
            sort: RecentsSort::LastPlayed,
            selected: 0,
            selected_path: None,
            child: None,
        })
    }
}

#[derive(Debug)]
pub enum Recents {
    Carousel(Box<RecentsCarousel>),
    List(Box<RecentsList>),
}

impl Recents {
    pub(crate) fn idle_media_step(&mut self, handle: &common::theme::ThemeHandle) -> bool {
        match self {
            Self::List(list) => list.idle_media_step(handle),
            Self::Carousel(_) => false,
        }
    }

    pub(crate) fn root_tab_navigation_ready(&self) -> bool {
        match self {
            Self::List(list) => list.root_tab_navigation_ready(),
            Self::Carousel(_) => true,
        }
    }

    pub fn load_or_new(rect: Rect, res: Resources, state: Option<RecentsState>) -> Result<Self> {
        let use_carousel = res.get::<Stylesheet>().recents.use_recents_carousel;

        if use_carousel {
            let carousel_state = match state {
                Some(RecentsState::Carousel(s)) => Some(s),
                _ => None,
            };
            Ok(Self::Carousel(Box::new(RecentsCarousel::load_or_new(
                rect,
                res,
                carousel_state,
            )?)))
        } else {
            let list_state = match state {
                Some(RecentsState::List(s)) => Some(s),
                _ => None,
            };
            Ok(Self::List(Box::new(RecentsList::load_or_new(
                rect, res, list_state,
            )?)))
        }
    }

    pub(crate) fn clear_theme(&mut self) -> Result<()> {
        match self {
            Self::List(list) => list.clear_theme(),
            Self::Carousel(_) => Ok(()),
        }
    }
    pub(crate) fn theme_models(
        &mut self,
        handle: &common::theme::ThemeHandle,
    ) -> Result<Option<Vec<common::theme::frame::FrameInputs>>> {
        match self {
            Self::List(list) => Ok(Some(list.theme_models(handle)?)),
            Self::Carousel(_) => Ok(None),
        }
    }
    pub fn save(&self) -> RecentsState {
        match self {
            Self::Carousel(c) => RecentsState::Carousel(c.save()),
            Self::List(l) => RecentsState::List(l.save()),
        }
    }
}

#[async_trait(?Send)]
impl View for Recents {
    fn update(&mut self, dt: std::time::Duration) {
        match self {
            Self::Carousel(carousel) => carousel.update(dt),
            Self::List(list) => list.update(dt),
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        match self {
            Self::Carousel(c) => c.draw(display, styles),
            Self::List(l) => l.draw(display, styles),
        }
    }

    fn should_draw(&self) -> bool {
        match self {
            Self::Carousel(c) => c.should_draw(),
            Self::List(l) => l.should_draw(),
        }
    }

    fn set_should_draw(&mut self) {
        match self {
            Self::Carousel(c) => c.set_should_draw(),
            Self::List(l) => l.set_should_draw(),
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        match self {
            Self::Carousel(c) => c.handle_key_event(event, commands, bubble).await,
            Self::List(l) => l.handle_key_event(event, commands, bubble).await,
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        match self {
            Self::Carousel(c) => c.children(),
            Self::List(l) => l.children(),
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        match self {
            Self::Carousel(c) => c.children_mut(),
            Self::List(l) => l.children_mut(),
        }
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        match self {
            Self::Carousel(c) => c.bounding_box(styles),
            Self::List(l) => l.bounding_box(styles),
        }
    }

    fn set_position(&mut self, point: Point) {
        match self {
            Self::Carousel(c) => c.set_position(point),
            Self::List(l) => l.set_position(point),
        }
    }
}
