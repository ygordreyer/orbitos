use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use tokio::sync::mpsc::Sender;

use crate::constants::CLOCK_UPDATE_INTERVAL;
use crate::geom::{Alignment, Point, Rect};
use crate::platform::{DefaultPlatform, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::Stylesheet;
use crate::view::{Command, Label, View};

#[derive(Debug, Clone)]
pub struct Clock {
    label: Label<String>,
    last_updated: Instant,
}

impl Clock {
    pub fn new(res: Resources, point: Point, alignment: Alignment) -> Self {
        let styles = res.get::<Stylesheet>();
        let mut label = Label::new(point, text(), alignment, None);
        label.font_size(styles.status_bar.font_size);
        label.color(crate::stylesheet::StylesheetColor::StatusBar);
        label.stroke_color(crate::stylesheet::StylesheetColor::StatusBarStroke);

        Self {
            label,
            last_updated: Instant::now(),
        }
    }
    #[cfg(test)]
    pub(crate) fn fixture_expired(mut self) -> Self {
        self.label.set_text("stale fixture clock".into());
        self.last_updated = Instant::now() - CLOCK_UPDATE_INTERVAL;
        self
    }

}

#[async_trait(?Send)]
impl View for Clock {
    fn theme_status(&self) -> Option<crate::theme::frame::Status> {
        Some(crate::theme::frame::Status {
            clock: Some(self.label.text().into()),
            ..Default::default()
        })
    }
    fn update(&mut self, _dt: Duration) {
        if self.last_updated.elapsed() >= CLOCK_UPDATE_INTERVAL {
            self.label.set_text(text());
            self.last_updated = Instant::now();
        }
    }

    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        self.label.draw(display, styles)
    }

    fn should_draw(&self) -> bool {
        self.label.should_draw()
    }

    fn set_should_draw(&mut self) {
        self.label.set_text(text());
        self.last_updated = Instant::now();
        self.label.set_should_draw();
    }

    async fn handle_key_event(
        &mut self,
        _event: KeyEvent,
        _commands: Sender<Command>,
        _bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        Ok(false)
    }

    fn children(&self) -> Vec<&dyn View> {
        vec![&self.label]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        vec![&mut self.label]
    }

    fn bounding_box(&mut self, styles: &Stylesheet) -> Rect {
        self.label.bounding_box(styles)
    }

    fn set_position(&mut self, point: Point) {
        self.label.set_position(point);
    }
}

fn text() -> String {
    #[cfg(all(feature = "perf-bench", not(any(feature = "miyoo", feature = "simulator"))))]
    if let Ok(clock) = std::env::var("ALLIUM_PERF_CLOCK") {
        return clock;
    }
    format!("{}", crate::timezone::local_now().format("%H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn change_event_refreshes_clock_before_the_timer_expires() {
        let mut clock = Clock {
            label: Label::new(Point::zero(), "stale clock".to_owned(), Alignment::Right, None),
            last_updated: Instant::now(),
        };
        clock.set_should_draw();
        assert_eq!(clock.theme_status().unwrap().clock.unwrap(), text());
        assert!(clock.last_updated.elapsed() < CLOCK_UPDATE_INTERVAL);
    }
}
