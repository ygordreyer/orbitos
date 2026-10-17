use std::collections::VecDeque;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

use crate::display::Display;
use crate::geom::{Alignment, Point, Rect};
use crate::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use crate::resources::Resources;
use crate::stylesheet::{Stylesheet, StylesheetColor};
use crate::view::{Command, Label, View};

/// A listing of selectable entries. Assumes that all entries have the same size.
#[derive(Debug, Clone)]
pub struct ScrollList {
    res: Resources,
    rect: Rect,
    /// All entries.
    items: Vec<String>,
    /// Visible entries.
    children: Vec<Label<String>>,
    alignment: Alignment,
    entry_height: u32,
    top: usize,
    visible_count_override: Option<usize>,
    selected: usize,
    background_color: Option<StylesheetColor>,
    dirty: bool,
}

impl ScrollList {
    pub fn new(
        res: Resources,
        mut rect: Rect,
        items: Vec<String>,
        alignment: Alignment,
        entry_height: u32,
    ) -> Self {
        match alignment {
            Alignment::Left => {}
            Alignment::Center => {
                rect.x += rect.w as i32 / 2;
            }
            Alignment::Right => {
                rect.x = rect.x + rect.w as i32 - 1;
            }
        }
        let mut this = Self {
            res,
            rect,
            items: Vec::new(),
            children: Vec::new(),
            alignment,
            entry_height,
            top: 0,
            visible_count_override: None,
            selected: 0,
            background_color: None,
            dirty: true,
        };

        this.set_items(items, false);

        this
    }

    pub fn set_background_color(&mut self, color: Option<StylesheetColor>) {
        self.background_color = color;
        self.dirty = true;
    }

    pub fn set_item(&mut self, index: usize, item: String) {
        if index >= self.items.len() {
            return;
        }

        self.items[index].clone_from(&item);
        self.children[index - self.top].set_text(item);
        self.dirty = true;
    }

    pub fn set_items(&mut self, items: Vec<String>, preserve_selection: bool) {
        if items.is_empty() {
            self.items = items;
            self.children.clear();
            self.dirty = true;
            return;
        }

        let selected = if preserve_selection {
            self.items
                .get(self.selected)
                .and_then(|selected| items.iter().position(|s| s == selected))
                .unwrap_or_else(|| self.selected.clamp(0, items.len() - 1))
        } else {
            0
        };
        self.items = items;

        self.children.clear();

        {
            let styles = self.res.get::<Stylesheet>();
            let mut y = self.rect.y + styles.ui.padding_y;
            for i in 0..self.visible_count() {
                self.children.push(Label::new(
                    Point::new(self.rect.x + styles.ui.padding_x * self.alignment.sign(), y),
                    self.items[i].to_owned(),
                    self.alignment,
                    Some(self.rect.w - styles.ui.padding_x as u32 * 2),
                ));
                y += self.entry_height as i32 + styles.ui.list_margin;
            }
        }

        self.select(selected);
        self.update_children();

        self.dirty = true;
    }

    fn selection_window(&self, index: usize) -> (usize, usize) {
        if self.visible_count() == 0 { return (self.selected, self.top); }
        let index = index.min(self.items.len() - 1);
        let top = if index >= self.top + self.visible_count() {
            (index - self.visible_count() + 1).min(self.items.len() - 1)
        } else if index < self.top { index } else { self.top };
        (index, top)
    }

    pub fn navigation_selection(&self, direction: isize) -> Option<(usize, usize)> {
        if self.items.is_empty() { return None; }
        Some(self.selection_window((self.selected as isize + direction)
            .rem_euclid(self.items.len() as isize) as usize))
    }

    pub fn select(&mut self, index: usize) {
        if self.visible_count() == 0 {
            return;
        }

        if let Some(child) = self.children.get_mut(self.selected - self.top) {
            child.scroll(false);
            child.blur();
        }

        (self.selected, self.top) = self.selection_window(index);
        self.update_children();

        if let Some(child) = self.children.get_mut(self.selected - self.top) {
            child.scroll(true);
            child.focus();
        }

        self.dirty = true;
    }

    pub fn set_visible_count_override(&mut self, count: Option<usize>) -> Result<()> {
        anyhow::ensure!(
            count.is_none_or(|n| (1..=64).contains(&n)),
            "Invalid visible row capacity"
        );
        if self.visible_count_override == count {
            return Ok(());
        }
        self.visible_count_override = count;
        let capacity = self.visible_count();
        self.top = self
            .top
            .min(self.items.len().saturating_sub(capacity))
            .min(self.selected);
        self.children.clear();
        {
            let styles = self.res.get::<Stylesheet>();
            let mut y = self.rect.y + styles.ui.padding_y;
            for i in 0..capacity {
                self.children.push(Label::new(
                    Point::new(self.rect.x + styles.ui.padding_x * self.alignment.sign(), y),
                    self.items[self.top + i].clone(),
                    self.alignment,
                    Some(self.rect.w.saturating_sub(styles.ui.padding_x as u32 * 2)),
                ));
                y += self.entry_height as i32 + styles.ui.list_margin;
            }
        }
        if capacity > 0 {
            self.select(self.selected);
        }
        self.dirty = true;
        Ok(())
    }

    pub fn window_start(&self) -> usize {
        self.top
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn visible_count(&self) -> usize {
        if let Some(count) = self.visible_count_override {
            return count.min(self.items.len());
        }
        let styles = self.res.get::<Stylesheet>();
        ((self.rect.h as usize) / (self.entry_height as usize + styles.ui.list_margin as usize))
            .min(self.items.len())
    }

    fn update_children(&mut self) {
        for (i, child) in self.children.iter_mut().enumerate() {
            child.set_text(self.items[self.top + i].to_owned());
        }
    }
}

#[async_trait(?Send)]
impl View for ScrollList {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        if self.should_draw() {
            display.load(self.bounding_box(styles))?;

            let mut pixmap = display.pixmap_mut();

            // Draw optional background
            if let Some(bg_color) = self.background_color {
                crate::display::fill_rect(&mut pixmap, self.rect, bg_color.to_color(styles));
            }

            // Draw highlight for selected item
            if let Some(selected) = self.children.get_mut(self.selected - self.top) {
                let rect = selected.bounding_box(styles);

                let highlight_rect = Rect::new(
                    rect.x - styles.ui.padding_x,
                    rect.y - styles.ui.padding_y,
                    rect.w + styles.ui.padding_x as u32 * 2,
                    rect.h + styles.ui.padding_y as u32 * 2,
                );
                crate::display::fill_rounded_rect(
                    &mut pixmap,
                    highlight_rect,
                    rect.h + styles.ui.padding_y as u32 * 2,
                    styles.ui.highlight_color,
                );
            }

            for child in self.children.iter_mut() {
                child.draw(display, styles)?;
            }

            self.dirty = false;

            return Ok(true);
        }

        let mut drawn = false;
        for child in self.children.iter_mut() {
            if child.should_draw() && child.draw(display, styles)? {
                drawn = true;
            }
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        self.dirty || self.children.iter().any(|v| v.should_draw())
    }

    fn set_should_draw(&mut self) {
        self.dirty = true;
        for entry in &mut self.children {
            entry.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        _command: Sender<Command>,
        _bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if !self.items.is_empty() {
            match event {
                KeyEvent::Pressed(Key::Up) | KeyEvent::Autorepeat(Key::Up) => {
                    self.select(self.navigation_selection(-1).unwrap().0);
                    self.dirty = true;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::Down) | KeyEvent::Autorepeat(Key::Down) => {
                    self.select(self.navigation_selection(1).unwrap().0);
                    self.dirty = true;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::L) | KeyEvent::Autorepeat(Key::L) => {
                    self.select(
                        (self.selected as isize - 5).clamp(0, self.items.len() as isize - 1)
                            as usize,
                    );
                    self.dirty = true;
                    Ok(true)
                }
                KeyEvent::Pressed(Key::R) | KeyEvent::Autorepeat(Key::R) => {
                    self.select((self.selected + 5).clamp(0, self.items.len() - 1));
                    self.dirty = true;
                    Ok(true)
                }
                _ => Ok(false),
            }
        } else {
            Ok(false)
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        self.children.iter().map(|c| c as &dyn View).collect()
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        self.children
            .iter_mut()
            .map(|c| c as &mut dyn View)
            .collect()
    }

    fn bounding_box(&mut self, _styles: &Stylesheet) -> Rect {
        match self.alignment {
            Alignment::Left => self.rect,
            Alignment::Center => Rect::new(
                self.rect.x - self.rect.w as i32 / 2,
                self.rect.y,
                self.rect.w,
                self.rect.h,
            ),
            Alignment::Right => Rect::new(
                self.rect.x - self.rect.w as i32 + 1,
                self.rect.y,
                self.rect.w,
                self.rect.h,
            ),
        }
    }

    fn set_position(&mut self, point: Point) {
        let styles = self.res.get::<Stylesheet>();

        self.rect.x = point.x;
        self.rect.y = point.y;

        let mut y = self.rect.y + styles.ui.padding_y;
        for child in self.children.iter_mut() {
            child.set_position(Point::new(
                point.x + styles.ui.padding_x * self.alignment.sign(),
                y,
            ));
            y += self.entry_height as i32 + styles.ui.list_margin;
        }

        self.dirty = true;
    }
}
