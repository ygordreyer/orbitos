use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::{fs, mem};

use anyhow::Result;
use async_trait::async_trait;
use common::command::Command;
use common::database::Database;
use common::display::Display;
use common::display::font::{FontTextStyle, FontTextStyleBuilder};
use common::geom::{Alignment, Point, Rect};
use common::locale::Locale;
use common::platform::{DefaultPlatform, Key, KeyEvent, Platform};
use common::resources::Resources;
use common::stylesheet::Stylesheet;
use common::theme::{
    ThemeHandle,
    frame::{FrameInputs, Hint, Status},
    pack::{Binding, Role, ScreenId},
    render::PreparedFrame,
};
use common::view::{ButtonHint, ButtonHints, Keyboard, View};
use log::{error, trace};
use tokio::sync::mpsc::Sender;

const INK_PAGE_HISTORY_LIMIT: usize = 64;
const PAGINATION_PAGE_BUDGET: usize = 4;

struct PaginationIndex {
    digest: String,
    generation: u64,
    text_len: usize,
    starts: Vec<usize>,
    resume: usize,
    complete: bool,
}

impl PaginationIndex {
    fn pagination(&self, cursor: usize) -> Option<common::theme::frame::Pagination> {
        // A restored/search cursor may be beyond the indexed prefix. Never label it
        // with the last known page number while incremental indexing catches up.
        if !self.complete && cursor > self.resume {
            return None;
        }
        Some(common::theme::frame::Pagination {
            page: self.starts.partition_point(|at| *at <= cursor).max(1),
            total: self.starts.len(),
        })
    }
}

#[cfg(test)]
thread_local! {
    static LEGACY_MEASURE_CHAR_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEGACY_MEASURE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static INK_BACKWARD_LINE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct LegacyParagraph {
    start: usize,
    end: usize,
    starts: Vec<usize>,
    theme: Option<(String, u64)>,
    style: FontTextStyle,
    width: u32,
    height: u32,
}

pub struct TextReader {
    rect: Rect,
    res: Resources,
    path: PathBuf,
    text: String,
    lowercase_text: String,
    cursor: usize,
    ink_page_history: VecDeque<(usize, usize)>,
    ink_page_theme: Option<(String, u64)>,
    pagination_index: Option<PaginationIndex>,
    legacy_paragraph: Option<LegacyParagraph>,
    button_hints: ButtonHints<String>,
    keyboard: Option<Keyboard>,
    last_searched: String,
    dirty: bool,
    theme_frame: Option<PreparedFrame>,
    theme_dirty: bool,
    status: Status,
    #[cfg(test)]
    fail_next_theme_prepare: bool,
}

fn bounded_guide_line_count(requested: u32, content_height: u32) -> u32 {
    requested.min(content_height.min(480) / 12).min(40)
}

fn guide_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

impl TextReader {
    #[must_use]
    pub fn new(rect: Rect, res: Resources, path: PathBuf) -> Self {
        let text = fs::read_to_string(&path)
            .map_err(|e| error!("failed to load guide file: {}", e))
            .unwrap_or_default();
        let lowercase_text = text.to_lowercase();

        let cursor = guide_cursor(&text, load_cursor(&res.get::<Database>(), path.as_path()));

        let locale = res.get::<Locale>();
        let styles = res.get::<Stylesheet>();

        let button_hints = ButtonHints::new(
            res.clone(),
            vec![ButtonHint::new(
                res.clone(),
                Point::zero(),
                Key::Menu,
                locale.t("ingame-menu-continue"),
                Alignment::Left,
            )],
            vec![
                ButtonHint::new(
                    res.clone(),
                    Point::zero(),
                    Key::X,
                    locale.t("guide-button-search"),
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

        drop(locale);
        drop(styles);

        Self {
            rect,
            res,
            path,
            text,
            lowercase_text,
            cursor,
            ink_page_history: VecDeque::new(),
            ink_page_theme: None,
            pagination_index: None,
            legacy_paragraph: None,
            button_hints,
            keyboard: None,
            dirty: true,
            theme_frame: None,
            theme_dirty: false,
            status: Status::default(),
            last_searched: String::new(),
            #[cfg(test)]
            fail_next_theme_prepare: false,
        }
    }

    fn normalize_cursor(&mut self) {
        let cursor = guide_cursor(&self.text, self.cursor);
        if cursor != self.cursor {
            self.cursor = cursor;
            self.ink_page_history = VecDeque::new();
            self.set_should_draw();
        }
    }

    pub fn save_cursor(&self) {
        self.res
            .get::<Database>()
            .update_guide_cursor(&self.path, guide_cursor(&self.text, self.cursor) as i64)
            .map_err(|e| error!("failed to update guide cursor to database: {}", e))
            .ok();
    }

    fn visible_text(&self, styles: &Stylesheet, content_height: u32) -> Vec<&str> {
        let line_count = if let Some(theme) = self.res.get::<Option<ThemeHandle>>().as_ref() {
            theme.pack().screens[&ScreenId::TextReader]
                .components
                .iter()
                .find(|component| component.binding == Some(Binding::GuideText))
                .and_then(|component| component.max_lines)
                .unwrap_or(1)
        } else {
            content_height / styles.menu.guide_font.size
        };
        let line_count = bounded_guide_line_count(line_count, content_height);
        let mut lines = Vec::with_capacity(line_count as usize);
        let mut cursor = self.cursor;
        for _ in 0..line_count {
            let line = self.get_line(styles, cursor);
            if line.is_empty() && cursor < self.text.len() {
                break;
            }
            lines.push(line.strip_suffix('\n').unwrap_or(line));
            cursor = self.next_line_start(cursor, line);
        }

        lines
    }

    // Rendering and navigation share the same sequence of display-line starts.
    fn next_line_start(&self, cursor: usize, line: &str) -> usize {
        let end = cursor + line.len();
        if !line.ends_with('\n') && self.text[end..].starts_with('\n') {
            end + 1
        } else {
            end
        }
    }

    fn get_line(&self, styles: &Stylesheet, cursor: usize) -> &str {
        if cursor == self.text.len() {
            return "";
        }
        // Consume a blank line's newline so every nonempty suffix makes progress.
        if self.text[cursor..].starts_with('\n') {
            return &self.text[cursor..cursor + 1];
        }
        if let Some(theme) = self
            .res
            .get::<Option<ThemeHandle>>()
            .as_ref()
            .filter(|t| t.pack().ink())
        {
            if let Some(c) = theme
                .pack()
                .screens
                .get(&ScreenId::TextReader)
                .and_then(|s| {
                    s.components
                        .iter()
                        .find(|c| c.binding == Some(Binding::GuideText))
                })
            {
                let layout = common::theme::render::ink_text_layout(theme, c, false);
                let rest = self.text.get(cursor..).unwrap_or("");
                return rest[..layout.guide_line_len(rest, c.rect.2)].trim_end_matches('\n');
            }
        }
        let (line_width, text_style, line_height) = self.legacy_style(styles);
        let first_char_len = self.text[cursor..].chars().next().unwrap().len_utf8();
        let (mut offset, visited) = text_style.fitting_line_prefix(
            &self.text[cursor..],
            common::theme::render::InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT,
            line_width,
            line_height,
        );
        #[cfg(test)]
        {
            LEGACY_MEASURE_CALLS.with(|n| n.set(n.get() + 1));
            LEGACY_MEASURE_CHAR_VISITS.with(|n| n.set(n.get() + visited));
        }
        #[cfg(not(test))]
        let _ = visited;

        // An oversized glyph must still be consumed, including during word wrapping.
        if offset == first_char_len {
            return &self.text[cursor..cursor + offset];
        }
        let offset_without_word_wrap = offset;

        // If not linebreak, we try to break at the start of the word
        if offset > 0
            && self.text[cursor + offset..]
                .chars()
                .next()
                .unwrap_or_default()
                .is_alphanumeric()
        {
            offset -= 1;
            while !self.text.is_char_boundary(cursor + offset) {
                offset -= 1;
            }
            if &self.text[cursor + offset..cursor + offset] != "\n" {
                while self.text[cursor + offset..]
                    .chars()
                    .next()
                    .unwrap_or_default()
                    .is_alphanumeric()
                {
                    if offset == first_char_len {
                        offset = offset_without_word_wrap;
                        break;
                    }
                    offset -= 1;
                    while !self.text.is_char_boundary(cursor + offset) {
                        offset -= 1;
                    }
                }
                offset += 1;
                while !self.text.is_char_boundary(cursor + offset) {
                    offset += 1;
                }
            }
        }

        &self.text[cursor..cursor + offset.max(first_char_len)]
    }

    fn legacy_style(&self, styles: &Stylesheet) -> (u32, FontTextStyle, u32) {
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        if let Some(theme) = theme {
            let component = theme.pack().screens[&ScreenId::TextReader]
                .components
                .iter()
                .find(|component| component.binding == Some(Binding::GuideText))
                .expect("validated text reader component");
            let style =
                theme.text_style(component.role.unwrap_or(Role::Body), styles.ui.text_color);
            (
                component.rect.2,
                style,
                theme.pack().tokens.typography[&component.role.unwrap_or(Role::Body)].line_height,
            )
        } else {
            (
                self.rect.w - styles.ui.margin_x as u32 * 2 - styles.ui.margin_y as u32 * 2,
                FontTextStyleBuilder::new(styles.menu.guide_font.font())
                    .font_fallback(styles.cjk_font.font())
                    .font_size(styles.menu.guide_font.size)
                    .background_color(styles.ui.background_color)
                    .text_color(styles.ui.text_color)
                    .build(),
                styles.menu.guide_font.size,
            )
        }
    }

    #[cfg(test)]
    fn replace_text(&mut self, text: String) {
        self.legacy_paragraph = None;
        self.pagination_index = None;
        self.text = text;
    }

    fn legacy_back_lines(&mut self, styles: &Stylesheet, lines: usize) {
        let theme = self
            .res
            .get::<Option<ThemeHandle>>()
            .as_ref()
            .map(|theme| (theme.digest().to_owned(), theme.generation()));
        let (width, style, height) = self.legacy_style(styles);
        let mut remaining = lines;
        while remaining > 0 && self.cursor > 0 {
            let reusable = self.legacy_paragraph.as_ref().is_some_and(|cache| {
                self.cursor > cache.start
                    && self.cursor <= cache.end
                    && cache.theme == theme
                    && cache.width == width
                    && cache.height == height
                    && cache.style.measurement_identity() == style.measurement_identity()
            });
            if !reusable {
                let end = self.cursor - usize::from(self.text.as_bytes()[self.cursor - 1] == b'\n');
                let start = self.text[..end].rfind('\n').map_or(0, |at| at + 1);
                let end = self.text[start..]
                    .find('\n')
                    .map_or(self.text.len(), |at| start + at + 1);
                let mut starts = Vec::new();
                let mut at = start;
                while at < end {
                    starts.push(at);
                    let line = self.get_line(styles, at);
                    let next = self.next_line_start(at, line);
                    if next <= at {
                        break;
                    }
                    at = next;
                }
                self.legacy_paragraph = Some(LegacyParagraph {
                    start,
                    end,
                    starts,
                    theme: theme.clone(),
                    style: style.clone(),
                    width,
                    height,
                });
            }
            let cache = self.legacy_paragraph.as_ref().unwrap();
            let before = cache.starts.partition_point(|&at| at < self.cursor);
            if before == 0 {
                break;
            }
            let count = remaining.min(before);
            self.cursor = cache.starts[before - count];
            remaining -= count;
        }
    }

    #[cfg(test)]
    fn themes10_reference_line(&self, styles: &Stylesheet, cursor: usize) -> &str {
        if cursor == self.text.len() {
            return "";
        }
        // Consume a blank line's newline so every nonempty suffix makes progress.
        if self.text[cursor..].starts_with('\n') {
            return &self.text[cursor..cursor + 1];
        }
        if let Some(theme) = self
            .res
            .get::<Option<ThemeHandle>>()
            .as_ref()
            .filter(|t| t.pack().ink())
        {
            if let Some(c) = theme
                .pack()
                .screens
                .get(&ScreenId::TextReader)
                .and_then(|s| {
                    s.components
                        .iter()
                        .find(|c| c.binding == Some(Binding::GuideText))
                })
            {
                let layout = common::theme::render::ink_text_layout(theme, c, false);
                let rest = self.text.get(cursor..).unwrap_or("");
                return rest[..layout.guide_line_len(rest, c.rect.2)].trim_end_matches('\n');
            }
        }
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let (line_width, text_style, line_height) = if let Some(theme) = theme {
            let component = theme.pack().screens[&ScreenId::TextReader]
                .components
                .iter()
                .find(|component| component.binding == Some(Binding::GuideText))
                .expect("validated text reader component");
            let style =
                theme.text_style(component.role.unwrap_or(Role::Body), styles.ui.text_color);
            (
                component.rect.2,
                style,
                theme.pack().tokens.typography[&component.role.unwrap_or(Role::Body)].line_height,
            )
        } else {
            (
                self.rect.w - styles.ui.margin_x as u32 * 2 - styles.ui.margin_y as u32 * 2,
                FontTextStyleBuilder::new(styles.menu.guide_font.font())
                    .font_fallback(styles.cjk_font.font())
                    .font_size(styles.menu.guide_font.size)
                    .background_color(styles.ui.background_color)
                    .text_color(styles.ui.text_color)
                    .build(),
                styles.menu.guide_font.size,
            )
        };
        let first_char_len = self.text[cursor..].chars().next().unwrap().len_utf8();
        let mut offset = self.text[cursor..]
            .chars()
            .take(common::theme::render::InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT)
            .take_while(|&c| c != '\n')
            .map(char::len_utf8)
            .sum::<usize>()
            .max(first_char_len);

        let measure = |slice: &str| text_style.measure(slice);

        // Measure text to find line break point
        let mut text_slice = &self.text[cursor..cursor + offset];
        let mut text_size = measure(text_slice);

        while offset > first_char_len && (text_size.w > line_width || text_size.h > line_height) {
            offset -= 1;
            while !self.text.is_char_boundary(cursor + offset) {
                offset -= 1;
            }
            text_slice = &self.text[cursor..cursor + offset];
            text_size = measure(text_slice);
        }

        // An oversized glyph must still be consumed, including during word wrapping.
        if offset == first_char_len {
            return &self.text[cursor..cursor + offset];
        }
        let offset_without_word_wrap = offset;

        // If not linebreak, we try to break at the start of the word
        if offset > 0
            && self.text[cursor + offset..]
                .chars()
                .next()
                .unwrap_or_default()
                .is_alphanumeric()
        {
            offset -= 1;
            while !self.text.is_char_boundary(cursor + offset) {
                offset -= 1;
            }
            if &self.text[cursor + offset..cursor + offset] != "\n" {
                while self.text[cursor + offset..]
                    .chars()
                    .next()
                    .unwrap_or_default()
                    .is_alphanumeric()
                {
                    if offset == first_char_len {
                        offset = offset_without_word_wrap;
                        break;
                    }
                    offset -= 1;
                    while !self.text.is_char_boundary(cursor + offset) {
                        offset -= 1;
                    }
                }
                offset += 1;
                while !self.text.is_char_boundary(cursor + offset) {
                    offset += 1;
                }
            }
        }

        &self.text[cursor..cursor + offset.max(first_char_len)]
    }

    #[cfg(test)]
    fn reference_legacy_line(&self, styles: &Stylesheet, cursor: usize) -> &str {
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let (line_width, text_style, line_height) = if let Some(theme) = theme {
            let component = theme.pack().screens[&ScreenId::TextReader]
                .components
                .iter()
                .find(|component| component.binding == Some(Binding::GuideText))
                .expect("validated text reader component");
            let style =
                theme.text_style(component.role.unwrap_or(Role::Body), styles.ui.text_color);
            (
                component.rect.2,
                style,
                theme.pack().tokens.typography[&component.role.unwrap_or(Role::Body)].line_height,
            )
        } else {
            (
                self.rect.w - styles.ui.margin_x as u32 * 2 - styles.ui.margin_y as u32 * 2,
                FontTextStyleBuilder::new(styles.menu.guide_font.font())
                    .font_fallback(styles.cjk_font.font())
                    .font_size(styles.menu.guide_font.size)
                    .background_color(styles.ui.background_color)
                    .text_color(styles.ui.text_color)
                    .build(),
                styles.menu.guide_font.size,
            )
        };
        let mut offset = self.text[cursor..]
            .find('\n')
            .or_else(|| self.text[..cursor].rfind('\n'))
            .unwrap_or_default();

        if cursor + offset >= self.text.len() {
            return &self.text[cursor..];
        }

        // Measure text to find line break point
        let mut text_slice = &self.text[cursor..cursor + offset];
        let mut text_size = text_style.measure(text_slice);

        while text_size.w > line_width || text_size.h > line_height {
            offset -= 1;
            while !self.text.is_char_boundary(cursor + offset) {
                offset -= 1;
            }
            text_slice = &self.text[cursor..cursor + offset];
            text_size = text_style.measure(text_slice);
        }

        let offset_without_word_wrap = offset;

        // If not linebreak, we try to break at the start of the word
        if offset > 0
            && self.text[cursor + offset..]
                .chars()
                .next()
                .unwrap_or_default()
                .is_alphanumeric()
        {
            offset -= 1;
            while !self.text.is_char_boundary(cursor + offset) {
                offset -= 1;
            }
            if &self.text[cursor + offset..cursor + offset] != "\n" {
                while self.text[cursor + offset..]
                    .chars()
                    .next()
                    .unwrap_or_default()
                    .is_alphanumeric()
                {
                    offset -= 1;
                    while !self.text.is_char_boundary(cursor + offset) {
                        offset -= 1;
                    }

                    if offset == 0 {
                        offset = offset_without_word_wrap;
                        break;
                    }
                }
                offset += 1;
                while !self.text.is_char_boundary(cursor + offset) {
                    offset += 1;
                }
            }
        }

        &self.text[cursor..cursor + offset]
    }

    pub fn refresh_theme(&mut self, status: Status) -> Result<()> {
        self.normalize_cursor();
        self.status = status;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_theme_prepare) {
            anyhow::bail!("injected nested theme preparation failure");
        }
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let Some(theme) = theme else {
            self.theme_frame = None;
            self.theme_dirty = false;
            return Ok(());
        };
        let locale = self.res.get::<Locale>();
        let mut input = if let Some(keyboard) = self.keyboard.as_ref() {
            let mut keyboard = keyboard.theme_inputs();
            // Frame models are bounded, while the controller value intentionally remains
            // unbounded. Capture only the visible prefix without changing search text.
            keyboard.display_value = keyboard.display_value.chars().take(512).collect();
            let mut input = FrameInputs::new(ScreenId::Keyboard);
            input
                .text
                .insert(Binding::Title, locale.t("guide-button-search"));
            input
                .text
                .insert(Binding::KeyboardValue, keyboard.display_value);
            input.keyboard = keyboard.labels;
            input.keyboard_cursor = keyboard.cursor;
            input.hints = vec![
                Hint {
                    button: "START".into(),
                    label: locale.t("button-confirm"),
                },
                Hint {
                    button: "B".into(),
                    label: locale.t("button-back"),
                },
                Hint {
                    button: "R".into(),
                    label: locale.t("keyboard-button-backspace"),
                },
            ];
            input
        } else {
            let mut input = FrameInputs::new(ScreenId::TextReader);
            let styles = self.res.get::<Stylesheet>();
            let visible = if theme.pack().ink() {
                let (layout, rect, max_lines) = Self::ink_guide_layout(&theme)
                    .ok_or_else(|| anyhow::anyhow!("Missing guide text component"))?;
                let rest = self.text.get(self.cursor..).unwrap_or("");
                let used = layout.guide_page_len(rest, rect.2, rect.3, max_lines);
                rest[..used].to_owned()
            } else {
                self.visible_text(&styles, u32::MAX).join("\n")
            };
            input.text.insert(
                Binding::Title,
                format!(
                    "{} · {:.0}%",
                    self.path.file_name().unwrap_or_default().to_string_lossy(),
                    self.cursor as f32 / self.text.len().max(1) as f32 * 100.0
                ),
            );
            if let Some((layout, rect, max_lines)) = Self::ink_guide_layout(&theme) {
                let valid = self.pagination_index.as_ref().is_some_and(|index| {
                    index.digest == theme.digest()
                        && index.generation == theme.generation()
                        && index.text_len == self.text.len()
                });
                if !valid {
                    self.pagination_index = Some(PaginationIndex {
                        digest: theme.digest().into(),
                        generation: theme.generation(),
                        text_len: self.text.len(),
                        starts: vec![0],
                        resume: 0,
                        complete: self.text.is_empty(),
                    });
                }
                let index = self.pagination_index.as_mut().unwrap();
                for _ in 0..PAGINATION_PAGE_BUDGET {
                    if index.complete {
                        break;
                    }
                    let used = layout.guide_page_len(
                        &self.text[index.resume..], rect.2, rect.3, max_lines,
                    );
                    if used == 0 {
                        break;
                    }
                    index.resume += used;
                    index.complete = index.resume == self.text.len();
                    if !index.complete {
                        index.starts.push(index.resume);
                    }
                }
                input.pagination = index.pagination(self.cursor);
            }
            input.text.insert(Binding::GuideText, visible);
            input.hints = vec![
                Hint {
                    button: "MENU".into(),
                    label: locale.t("ingame-menu-continue"),
                },
                Hint {
                    button: "X".into(),
                    label: locale.t("guide-button-search"),
                },
                Hint {
                    button: "B".into(),
                    label: locale.t("button-back"),
                },
                Hint {
                    button: "L".into(),
                    label: locale.t("guide-button-prev"),
                },
                Hint {
                    button: "R".into(),
                    label: locale.t("guide-button-next"),
                },
            ];
            input
        };
        super::guide_shell(&mut input, &locale);
        input.status = self.status.clone();
        drop(locale);
        self.theme_frame = Some(PreparedFrame::prepare(theme, &input)?);
        self.theme_dirty = true;
        Ok(())
    }

    pub(crate) fn disable_theme(&mut self) {
        self.theme_frame = None;
        self.theme_dirty = false;
        self.dirty = true;
        self.button_hints.set_should_draw();
        if let Some(keyboard) = &mut self.keyboard {
            keyboard.set_should_draw();
        }
    }

    #[cfg(test)]
    pub(crate) fn theme_is_disabled(&self) -> bool {
        self.theme_frame.is_none()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_theme_prepare(&mut self) {
        self.fail_next_theme_prepare = true;
    }

    fn search_forward_wrapping(&mut self, needle: String) -> bool {
        if self.search_forward(&needle) {
            self.last_searched = needle;
            return true;
        }
        let cursor = self.cursor;
        self.cursor = 0;
        if self.search_forward(&needle) {
            self.last_searched = needle;
            true
        } else {
            self.cursor = cursor;
            false
        }
    }

    fn search_backward_wrapping(&mut self, needle: String) -> bool {
        if self.search_backward(&needle) {
            self.last_searched = needle;
            return true;
        }
        let cursor = self.cursor;
        self.cursor = self.text.len();
        if self.search_backward(&needle) {
            self.last_searched = needle;
            true
        } else {
            self.cursor = cursor;
            false
        }
    }

    fn lowercase_cursor(&self, cursor: usize) -> usize {
        self.text[..cursor]
            .chars()
            .flat_map(char::to_lowercase)
            .map(char::len_utf8)
            .sum()
    }

    fn search_line_start(&self, lowercase_cursor: usize) -> usize {
        // Lowercasing can change byte lengths, so search offsets need mapping back.
        let mut lowercase_at = 0;
        let mut cursor = self.text.len();
        for (at, ch) in self.text.char_indices() {
            lowercase_at += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            if lowercase_at > lowercase_cursor {
                cursor = at;
                break;
            }
        }
        self.text[..cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn search_forward(&mut self, needle: &str) -> bool {
        self.ink_page_history = VecDeque::new();
        self.normalize_cursor();
        let mut cursor = self.cursor;
        // Skip the current line
        cursor += self.text[cursor..].find('\n').unwrap_or_default();
        let lowercase_cursor = self.lowercase_cursor(cursor);

        if let Some(location) = self.lowercase_text[lowercase_cursor..].find(needle) {
            cursor = self.search_line_start(lowercase_cursor + location);
        } else {
            return false;
        }
        self.cursor = cursor;

        if self.button_hints.right().len() <= 2 {
            let locale = self.res.get::<Locale>();
            self.button_hints.right_mut().push(ButtonHint::new(
                self.res.clone(),
                Point::zero(),
                Key::L2,
                locale.t("guide-button-next"),
                Alignment::Right,
            ));
            self.button_hints.right_mut().push(ButtonHint::new(
                self.res.clone(),
                Point::zero(),
                Key::R2,
                locale.t("guide-button-prev"),
                Alignment::Right,
            ));
        }
        true
    }

    fn search_backward(&mut self, needle: &str) -> bool {
        self.ink_page_history = VecDeque::new();
        self.normalize_cursor();
        let cursor;
        if let Some(location) =
            self.lowercase_text[..self.lowercase_cursor(self.cursor)].rfind(needle)
        {
            cursor = self.search_line_start(location);
            self.last_searched = needle.to_owned();
        } else {
            return false;
        }
        self.cursor = cursor;

        if self.button_hints.right().len() <= 2 {
            let locale = self.res.get::<Locale>();
            self.button_hints.right_mut().push(ButtonHint::new(
                self.res.clone(),
                Point::zero(),
                Key::L,
                locale.t("guide-button-next"),
                Alignment::Right,
            ));
            self.button_hints.right_mut().push(ButtonHint::new(
                self.res.clone(),
                Point::zero(),
                Key::R,
                locale.t("guide-button-prev"),
                Alignment::Right,
            ));
        }
        true
    }

    fn ink_guide_layout(
        theme: &ThemeHandle,
    ) -> Option<(
        common::theme::render::InkTextLayout,
        common::theme::pack::Rect,
        u32,
    )> {
        if !theme.pack().ink() {
            return None;
        }
        let c = theme
            .pack()
            .screens
            .get(&ScreenId::TextReader)?
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::GuideText))?;
        Some((
            common::theme::render::ink_text_layout(theme, c, false),
            c.rect,
            c.max_lines.unwrap_or(1),
        ))
    }

    fn move_page(&mut self, forward: bool) {
        self.normalize_cursor();
        let theme = self.res.get::<Option<ThemeHandle>>().clone();
        let resolved = theme.as_ref().and_then(Self::ink_guide_layout);
        let Some((layout, rect, max_lines)) = resolved else {
            if forward {
                self.move_forward_lines(10);
            } else {
                self.move_back_lines(10);
            }
            return;
        };
        let theme = theme.unwrap();
        let key = (theme.digest().to_owned(), theme.generation());
        if self.ink_page_theme.as_ref() != Some(&key) {
            self.ink_page_history = VecDeque::new();
            self.ink_page_theme = Some(key);
        }
        if forward {
            let next = self.cursor
                + layout.guide_page_len(&self.text[self.cursor..], rect.2, rect.3, max_lines);
            if next > self.cursor {
                if self.ink_page_history.len() == INK_PAGE_HISTORY_LIMIT {
                    self.ink_page_history.pop_front();
                }
                self.ink_page_history.push_back((self.cursor, next));
                self.cursor = next;
            }
        } else if let Some((start, _)) = self
            .ink_page_history
            .back()
            .copied()
            .filter(|(_, end)| *end == self.cursor)
        {
            self.ink_page_history.pop_back();
            if self.ink_page_history.is_empty() {
                self.ink_page_history = VecDeque::new();
            }
            self.cursor = start;
        } else {
            self.ink_page_history = VecDeque::new();
            let count = layout.guide_line_count(rect.3, max_lines) as usize;
            let budget = count.max(1)
                * common::theme::render::InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT
                * 2;
            for _ in 0..count {
                if self.cursor == 0 {
                    break;
                }
                let mut start = self.cursor.saturating_sub(budget);
                while !self.text.is_char_boundary(start) {
                    start += 1;
                }
                // At a paragraph boundary, the previous line belongs to the paragraph
                // ending in that newline. Limit both the search and walk on huge paragraphs.
                let end = if self.text.as_bytes()[self.cursor - 1] == b'\n' {
                    self.cursor - 1
                } else {
                    self.cursor
                };
                start += self.text[start..end].rfind('\n').map_or(0, |at| at + 1);
                let mut at = start;
                let mut previous = start;
                while at < self.cursor {
                    previous = at;
                    #[cfg(test)]
                    INK_BACKWARD_LINE_CALLS.with(|n| n.set(n.get() + 1));
                    // Use the full suffix: truncation changes greedy wrapping at the cursor.
                    let len = layout.guide_line_len(&self.text[at..], rect.2);
                    if len == 0 {
                        break;
                    }
                    at += len;
                }
                self.cursor = previous;
            }
        }
        self.set_should_draw();
    }

    fn move_back_lines(&mut self, lines: usize) {
        self.normalize_cursor();
        self.ink_page_history = VecDeque::new();
        let res = self.res.clone();
        let styles = res.get::<Stylesheet>();
        if !self
            .res
            .get::<Option<ThemeHandle>>()
            .as_ref()
            .is_some_and(|theme| theme.pack().ink())
        {
            self.legacy_back_lines(&styles, lines);
            drop(styles);
            self.set_should_draw();
            return;
        }
        let mut remaining = lines;
        let mut starts = VecDeque::new();
        while remaining > 0 && self.cursor > 0 {
            // Exclude a newline immediately before the cursor: it belongs to the
            // paragraph being walked back, including a paragraph containing only it.
            let end = if self.text.as_bytes()[self.cursor - 1] == b'\n' {
                self.cursor - 1
            } else {
                self.cursor
            };
            let mut at = self.text[..end].rfind('\n').map_or(0, |i| i + 1);
            starts.clear();
            while at < self.cursor {
                let line = self.get_line(&styles, at);
                if line.is_empty() {
                    break;
                }
                let next = self.next_line_start(at, line);
                if next <= at {
                    break;
                }
                starts.push_back(at);
                if starts.len() > remaining {
                    starts.pop_front();
                }
                at = next;
            }
            let Some(previous) = starts.front().copied() else {
                break;
            };
            self.cursor = previous;
            remaining -= starts.len();
        }
        drop(styles);
        self.set_should_draw();
    }

    fn move_forward_lines(&mut self, lines: usize) {
        self.normalize_cursor();
        self.ink_page_history = VecDeque::new();
        let styles = self.res.get::<Stylesheet>();
        for _ in 0..lines {
            if self.cursor == self.text.len() {
                break;
            }
            let text = self.get_line(&styles, self.cursor);
            if text.is_empty() {
                break;
            }
            let next = self.next_line_start(self.cursor, text);
            if next <= self.cursor {
                break;
            }
            self.cursor = next;
        }
        drop(styles);

        self.set_should_draw();
    }
}

fn load_cursor(database: &Database, path: &Path) -> usize {
    database
        .get_guide_cursor(path)
        .map_err(|e| error!("failed to load guide cursor from database: {}", e))
        .ok()
        .and_then(|cursor| usize::try_from(cursor).ok())
        .unwrap_or_default()
}

#[async_trait(?Send)]
impl View for TextReader {
    fn draw(
        &mut self,
        display: &mut <DefaultPlatform as Platform>::Display,
        styles: &Stylesheet,
    ) -> Result<bool> {
        self.normalize_cursor();
        if let Some(frame) = &self.theme_frame {
            if !self.theme_dirty {
                return Ok(false);
            }
            frame.paint(display);
            self.theme_dirty = false;
            return Ok(true);
        }
        let mut drawn = false;

        if self.dirty {
            let button_hints_rect = self.button_hints.bounding_box(styles);
            display.load(button_hints_rect)?;
            let content_top = self.rect.y;
            let content_height = (button_hints_rect.y - content_top) as u32;

            display.load(Rect::new(
                self.rect.x,
                content_top,
                self.rect.w,
                content_height,
            ))?;

            // Draw content background
            let content_rect = Rect::new(
                self.rect.x + styles.ui.margin_x,
                content_top,
                self.rect.w - styles.ui.margin_x as u32 * 2,
                content_height,
            );
            common::display::fill_rounded_rect(
                &mut display.pixmap_mut(),
                content_rect,
                8,
                styles.ui.background_color,
            );

            let text_style = FontTextStyleBuilder::new(styles.menu.guide_font.font())
                .font_fallback(styles.cjk_font.font())
                .font_size(styles.menu.guide_font.size)
                .background_color(styles.ui.background_color)
                .text_color(styles.ui.text_color)
                .build();

            // Draw visible text lines
            let visible_lines: Vec<&str> = self.visible_text(styles, content_height);
            let mut y = content_top;
            for line in visible_lines {
                let line_pos = Point::new(self.rect.x + styles.ui.margin_x + 12, y);
                text_style.draw(&mut display.pixmap_mut(), line, line_pos);
                y += styles.menu.guide_font.size as i32;
            }

            // Draw percentage indicator
            let percentage_text = format!(
                "{:.0}%",
                self.cursor as f32 / self.text.len().max(1) as f32 * 100.0
            );
            let percentage_size = text_style.measure(&percentage_text);
            let percentage_pos = Point::new(
                self.rect.x + self.rect.w as i32 - 16 - percentage_size.w as i32,
                content_top + content_height as i32
                    - styles.menu.guide_font.size as i32
                    - styles.ui.margin_y / 2,
            );
            text_style.draw(&mut display.pixmap_mut(), &percentage_text, percentage_pos);

            self.dirty = false;

            trace!("drawing text reader");

            drawn = true;
        }

        drawn |= self.button_hints.draw(display, styles)?;

        if let Some(keyboard) = self.keyboard.as_mut() {
            drawn |= keyboard.draw(display, styles)?;
        }

        Ok(drawn)
    }

    fn should_draw(&self) -> bool {
        if self.theme_frame.is_some() {
            return self.theme_dirty;
        }
        self.dirty
            || self.button_hints.should_draw()
            || self
                .keyboard
                .as_ref()
                .is_some_and(common::view::View::should_draw)
    }

    fn set_should_draw(&mut self) {
        if self.theme_frame.is_some() {
            self.theme_dirty = true;
            return;
        }
        self.dirty = true;
        self.button_hints.set_should_draw();
        if let Some(keyboard) = self.keyboard.as_mut() {
            keyboard.set_should_draw();
        }
    }

    async fn handle_key_event(
        &mut self,
        event: KeyEvent,
        commands: Sender<Command>,
        bubble: &mut VecDeque<Command>,
    ) -> Result<bool> {
        if let Some(keyboard) = self.keyboard.as_mut() {
            if keyboard.handle_key_event(event, commands, bubble).await? {
                bubble.retain_mut(|cmd| match cmd {
                    Command::CloseView => {
                        self.keyboard = None;
                        false
                    }
                    Command::ValueChanged(_, value) => {
                        self.search_forward(&std::mem::take(value).as_string().unwrap());
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
                KeyEvent::Pressed(Key::Up) | KeyEvent::Autorepeat(Key::Up) => {
                    self.move_back_lines(1);
                }
                KeyEvent::Pressed(Key::Down) | KeyEvent::Autorepeat(Key::Down) => {
                    self.move_forward_lines(1);
                }
                KeyEvent::Pressed(Key::L) | KeyEvent::Autorepeat(Key::L) => {
                    self.move_page(false);
                }
                KeyEvent::Pressed(Key::R) | KeyEvent::Autorepeat(Key::R) => {
                    self.move_page(true);
                }
                KeyEvent::Pressed(Key::L2) => {
                    let last_searched = mem::take(&mut self.last_searched);
                    self.search_backward_wrapping(last_searched);
                    self.dirty = true;
                }
                KeyEvent::Pressed(Key::R2) => {
                    let last_searched = mem::take(&mut self.last_searched);
                    self.search_forward_wrapping(last_searched);
                    self.dirty = true;
                }
                KeyEvent::Pressed(Key::B) => {
                    self.save_cursor();
                    bubble.push_back(Command::CloseView);
                }
                KeyEvent::Pressed(Key::X) => {
                    self.keyboard = Some(Keyboard::new(
                        self.res.clone(),
                        mem::take(&mut self.last_searched),
                        false,
                    ));
                }
                _ => return Ok(false),
            }
            Ok(true)
        }
    }

    fn children(&self) -> Vec<&dyn View> {
        let mut children: Vec<&dyn View> = vec![&self.button_hints];
        if let Some(ref keyboard) = self.keyboard {
            children.push(keyboard);
        }
        children
    }

    fn children_mut(&mut self) -> Vec<&mut dyn View> {
        let mut children: Vec<&mut dyn View> = vec![&mut self.button_hints];
        if let Some(ref mut keyboard) = self.keyboard {
            children.push(keyboard);
        }
        children
    }

    fn set_position(&mut self, _point: Point) {
        unimplemented!()
    }
}

#[cfg(test)]
mod theme_adapter_tests {
    use super::*;
    use common::{constants::ALLIUM_FONTS_DIR, geom::Size, theme::ThemeRuntime};
    use type_map::TypeMap;

    pub(super) fn themed_resources(root: &Path) -> Resources {
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        let mut runtime =
            ThemeRuntime::bind(&state, &root.join("missing-themes"), &ALLIUM_FONTS_DIR).unwrap();
        let theme = runtime.choose("orbit").unwrap().snapshot.unwrap();
        let mut map = TypeMap::new();
        map.insert(Stylesheet::load().unwrap());
        map.insert(Locale::new("en-US"));
        map.insert(Size { w: 640, h: 480 });
        map.insert(Database::in_memory().unwrap());
        map.insert(Some(theme));
        Resources::new(map)
    }

    #[tokio::test]
    async fn themed_reader_keeps_utf8_cursor_search_confirm_and_b_save() {
        let root = std::env::current_dir().unwrap().join("out").join(format!(
            "allium-text-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("unicode-guide.txt");
        fs::write(&path, "start é🦊\n密碼 target\nfinish").unwrap();
        let res = themed_resources(&root);
        let mut reader = TextReader::new(Rect::new(0, 0, 640, 480), res.clone(), path.clone());
        reader.refresh_theme(Status::default()).unwrap();
        assert!(reader.theme_frame.is_some());
        if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
            fs::create_dir_all(&proof).unwrap();
            reader
                .theme_frame
                .as_ref()
                .unwrap()
                .pixels()
                .save_png(PathBuf::from(&proof).join("native-orbit-text-reader.png"))
                .unwrap();
        }

        reader.keyboard = Some(Keyboard::new(res.clone(), "密碼".into(), false));
        reader.refresh_theme(Status::default()).unwrap();
        if let Ok(proof) = std::env::var("ALLIUM_ORBIT_PROOF_DIR") {
            reader
                .theme_frame
                .as_ref()
                .unwrap()
                .pixels()
                .save_png(PathBuf::from(proof).join("native-orbit-keyboard.png"))
                .unwrap();
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut bubble = VecDeque::new();
        assert!(
            reader
                .handle_key_event(KeyEvent::Pressed(Key::Start), tx.clone(), &mut bubble)
                .await
                .unwrap()
        );
        assert!(reader.keyboard.is_none());
        assert!(reader.text.is_char_boundary(reader.cursor));
        assert_eq!(&reader.text[reader.cursor..][.."密碼".len()], "密碼");

        reader
            .handle_key_event(KeyEvent::Pressed(Key::B), tx, &mut bubble)
            .await
            .unwrap();
        assert_eq!(
            res.get::<Database>().get_guide_cursor(&path).unwrap(),
            reader.cursor as i64
        );
        assert!(
            bubble
                .iter()
                .any(|command| matches!(command, Command::CloseView))
        );

        let masked = Keyboard::new(res, "密碼é🦊".into(), true).theme_inputs();
        assert_eq!(masked.display_value, "****");
        assert!(!format!("{masked:?}").contains("密碼"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn themes4_shoulder_events_page_eight_packs_and_preserve_legacy_bytes() {
        if std::env::var_os("ALLIUM_SD_ROOT").is_none() {
            eprintln!("Skipping mock-platform reader test: ALLIUM_SD_ROOT is unset");
            return;
        }
        use common::theme::{FontBank, runtime::ThemeSnapshot};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root =
            std::env::temp_dir().canonicalize().unwrap().join(format!("allium-themes4-guide-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("guide.txt");
        let text = (0..12).map(|n| format!("{n}: A guide with wide words WWW, narrow letters iii, é🦊 日本語 and Straße. Keep every source character, including  spaces.\n\n"))
            .collect::<String>();
        fs::write(&path, &text).unwrap();
        assert_eq!(bounded_guide_line_count(u32::MAX, u32::MAX), 40);
        assert_eq!(bounded_guide_line_count(u32::MAX, 120), 10);
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let mut snapshots = vec![
            ThemeSnapshot::builtin("orbit").unwrap(),
            ThemeSnapshot::builtin("compact").unwrap(),
        ];
        for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            for suffix in ["", "-tall"] {
                snapshots.push(
                    ThemeSnapshot::load_with_settings(
                        &repo.join(format!("themes/{base}")),
                        &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty() { "square" } else { "tall" }.into())]),
                    ).unwrap(),
                );
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut bubble = VecDeque::new();
        for snapshot in snapshots {
            let theme = ThemeHandle::prepare(snapshot, &bank, 1).unwrap();
            let text = if theme.pack().ink() {
                text.clone()
            } else {
                (0..60)
                    .map(|n| format!("{n}: short é 日本語\n"))
                    .collect::<String>()
            };
            fs::write(&path, &text).unwrap();
            let res = themed_resources(&root);
            res.insert(Some(theme.clone()));
            let mut reader = TextReader::new(Rect::new(0, 0, 640, 480), res.clone(), path.clone());
            let c = theme.pack().screens[&ScreenId::TextReader]
                .components
                .iter()
                .find(|c| c.binding == Some(Binding::GuideText))
                .unwrap();
            let count = if theme.pack().ink() {
                common::theme::render::ink_text_layout(&theme, c, false)
                    .guide_line_count(c.rect.3, c.max_lines.unwrap())
            } else {
                c.max_lines.unwrap()
            };
            assert!(count > 0);
            reader
                .handle_key_event(KeyEvent::Pressed(Key::L), tx.clone(), &mut bubble)
                .await
                .unwrap();
            assert_eq!(reader.cursor, 0);
            reader
                .handle_key_event(KeyEvent::Autorepeat(Key::L), tx.clone(), &mut bubble)
                .await
                .unwrap();
            assert_eq!(reader.cursor, 0);
            let mut legacy = TextReader::new(Rect::new(0, 0, 640, 480), res.clone(), path.clone());
            let mut starts = Vec::new();
            let mut consumed = String::new();
            let mut pages = 0;
            while reader.cursor < text.len() {
                let start = reader.cursor;
                let styles = res.get::<Stylesheet>();
                if !theme.pack().ink() {
                    let mut at = start;
                    let mut expected = Vec::new();
                    for _ in 0..count {
                        let line = reader.reference_legacy_line(&styles, at);
                        expected.push(line);
                        at += line.len();
                        if text[at..].starts_with('\n') {
                            at += 1;
                        }
                    }
                    assert_eq!(
                        reader.visible_text(&styles, u32::MAX).join("\n"),
                        expected.join("\n"),
                        "{} page {pages}",
                        theme.pack().id
                    );
                }
                drop(styles);
                reader.refresh_theme(Status::default()).unwrap();
                let event = if pages % 2 == 0 {
                    KeyEvent::Pressed(Key::R)
                } else {
                    KeyEvent::Autorepeat(Key::R)
                };
                reader
                    .handle_key_event(event, tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                let end = reader.cursor;
                if !theme.pack().ink() {
                    legacy.move_forward_lines(10);
                    assert_eq!(end, legacy.cursor, "{} legacy R bytes", theme.pack().id);
                }
                reader
                    .handle_key_event(KeyEvent::Pressed(Key::L), tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                if theme.pack().ink() {
                    assert_eq!(reader.cursor, start, "{} R then L", theme.pack().id);
                } else {
                    legacy.move_back_lines(10);
                    assert_eq!(
                        reader.cursor,
                        legacy.cursor,
                        "{} legacy L bytes",
                        theme.pack().id
                    );
                }
                // Continue from the recorded page start, including legacy's asymmetric wrapping.
                reader.cursor = start;
                legacy.cursor = start;
                reader
                    .handle_key_event(event, tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                if !theme.pack().ink() {
                    legacy.move_forward_lines(10);
                }
                assert_eq!(reader.cursor, end);
                starts.push(start);
                assert!(reader.cursor > start, "{} stalled", theme.pack().id);
                let page = &text[start..reader.cursor];
                consumed.push_str(page);
                if theme.pack().ink() {
                    let layout = common::theme::render::ink_text_layout(&theme, c, false);
                    assert_eq!(
                        layout.guide_page_len(&text[start..], c.rect.2, c.rect.3, count),
                        page.len(),
                        "{} source offsets differ",
                        theme.pack().id
                    );
                    assert_eq!(
                        layout.guide_page_len(page, c.rect.2, c.rect.3, count),
                        page.len(),
                        "{} page wraps beyond max_lines",
                        theme.pack().id
                    );
                }
                pages += 1;
            }
            assert!(pages >= 3, "{} did not paginate", theme.pack().id);
            assert_eq!(
                consumed,
                text,
                "{} skipped or duplicated source",
                theme.pack().id
            );
            let end = reader.cursor;
            for event in [KeyEvent::Pressed(Key::R), KeyEvent::Autorepeat(Key::R)] {
                reader
                    .handle_key_event(event, tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                assert_eq!(reader.cursor, end);
            }
            if theme.pack().ink() {
                for start in starts.into_iter().rev() {
                    reader
                        .handle_key_event(KeyEvent::Autorepeat(Key::L), tx.clone(), &mut bubble)
                        .await
                        .unwrap();
                    assert_eq!(
                        reader.cursor,
                        start,
                        "{} reverse page history",
                        theme.pack().id
                    );
                }
                reader
                    .handle_key_event(KeyEvent::Pressed(Key::L), tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                assert_eq!(reader.cursor, 0);
                reader.cursor = text.len();
                reader.ink_page_history.clear();
                reader
                    .handle_key_event(KeyEvent::Pressed(Key::L), tx.clone(), &mut bubble)
                    .await
                    .unwrap();
                assert!(reader.cursor < text.len());
            }
            // Allocation remains bounded independently of manifest validation.
            assert!(
                reader
                    .visible_text(&res.get::<Stylesheet>(), u32::MAX)
                    .len()
                    <= 40
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn themes5_reader(case: &str, id: &str, text: &str) -> (PathBuf, TextReader) {
        use common::theme::{FontBank, runtime::ThemeSnapshot};
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root =
            std::env::temp_dir().join(format!("allium-themes5-{case}-{id}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("guide.txt");
        fs::write(&path, text).unwrap();
        let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(
            if matches!(id, "orbit" | "compact") {
                ThemeSnapshot::builtin(id).unwrap()
            } else {
                {
                    let (base, settings) = common::theme::settings::migrate(id, &Default::default());
                    ThemeSnapshot::load_with_settings(&repo.join("themes").join(base), &settings).unwrap()
                }
            },
            &bank,
            1,
        )
        .unwrap();
        let res = themed_resources(&root);
        res.insert(Some(theme));
        let reader = TextReader::new(Rect::new(0, 0, 640, 480), res, path);
        (root, reader)
    }

    async fn themes5_event(reader: &mut TextReader, event: KeyEvent) {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        assert!(
            reader
                .handle_key_event(event, tx, &mut VecDeque::new())
                .await
                .unwrap()
        );
    }

    const THEMES5_PACKS: [&str; 6] = [
        "orbit-hero",
        "orbit-hero-tall",
        "big-picture",
        "big-picture-tall",
        "compact-plus",
        "compact-plus-tall",
    ];

    const THEMES6_PACKS: [&str; 10] = [
        "orbit-hero",
        "orbit-hero-tall",
        "orbit-row",
        "orbit-row-tall",
        "big-picture",
        "big-picture-tall",
        "compact-plus",
        "compact-plus-tall",
        "orbit",
        "compact",
    ];

    #[test]
    fn guide_pagination_indexes_bounded_pages_per_refresh_and_resumes() {
        let text = "é 日本語 line\n".repeat(240);
        let (root, mut reader) = themes5_reader("pagination-budget", "orbit-hero", &text);
        let theme = reader.res.get::<Option<ThemeHandle>>().clone().unwrap();
        let (layout, rect, max_lines) = TextReader::ink_guide_layout(&theme).unwrap();
        let mut expected = vec![0];
        let mut at = 0;
        while at < text.len() {
            at += layout.guide_page_len(&text[at..], rect.2, rect.3, max_lines);
            if at < text.len() {
                expected.push(at);
            }
        }
        assert!(expected.len() > PAGINATION_PAGE_BUDGET * 2);
        reader.refresh_theme(Status::default()).unwrap();
        let index = reader.pagination_index.as_ref().unwrap();
        assert!(!index.complete);
        assert_eq!(index.starts, expected[..=PAGINATION_PAGE_BUDGET]);
        let pagination = index.pagination(0).unwrap();
        assert_eq!(
            (pagination.page, pagination.total),
            (1, PAGINATION_PAGE_BUDGET + 1)
        );
        // Deep restored/search cursors must not cause an unbounded synchronous catch-up.
        reader.cursor = expected[expected.len() - 2] + "é".len();
        assert!(index.pagination(reader.cursor).is_none());
        let mut previous = index.starts.len();
        for _ in 0..expected.len() {
            reader.refresh_theme(Status::default()).unwrap();
            let index = reader.pagination_index.as_ref().unwrap();
            assert!(index.starts.len() - previous <= PAGINATION_PAGE_BUDGET);
            assert_eq!(index.starts, expected[..index.starts.len()]);
            previous = index.starts.len();
            if index.complete {
                break;
            }
        }
        let index = reader.pagination_index.as_ref().unwrap();
        assert!(index.complete);
        assert_eq!(index.starts, expected);
        assert_eq!(
            index.pagination(reader.cursor).unwrap().page,
            expected.len() - 1
        );
        for (page, cursor) in expected.iter().enumerate() {
            assert_eq!(index.pagination(*cursor).unwrap().page, page + 1);
        }
        assert_eq!(index.pagination(text.len()).unwrap().page, expected.len());
        // Each cache identity component must invalidate a completed index.
        for identity in 0..3 {
            let index = reader.pagination_index.as_mut().unwrap();
            match identity {
                0 => index.digest.push('x'),
                1 => index.generation += 1,
                _ => index.text_len += 1,
            }
            reader.refresh_theme(Status::default()).unwrap();
            let index = reader.pagination_index.as_ref().unwrap();
            assert_eq!(index.starts.len(), PAGINATION_PAGE_BUDGET + 1);
            assert!(!index.complete);
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn themes6_render(reader: &mut TextReader) {
        reader.refresh_theme(Status::default()).unwrap();
        assert!(reader.theme_frame.is_some());
        assert!(reader.text.is_char_boundary(reader.cursor));
        let res = reader.res.clone();
        let styles = res.get::<Stylesheet>();
        reader.visible_text(&styles, 480);
    }

    async fn themes9_legacy_bounded_navigation(reader: &mut TextReader) {
        LEGACY_MEASURE_CALLS.with(|n| n.set(0));
        let res = reader.res.clone();
        let styles = res.get::<Stylesheet>();
        let visible = reader.visible_text(&styles, 480);
        assert!(!visible.is_empty());
        assert!(visible.iter().all(|line| !line.is_empty()));
        drop(styles);
        let mut starts = vec![reader.cursor];
        for _ in 0..5 {
            themes5_event(reader, KeyEvent::Pressed(Key::Down)).await;
            assert!(reader.cursor > *starts.last().unwrap());
            assert!(reader.text.is_char_boundary(reader.cursor));
            starts.push(reader.cursor);
        }
        for expected in starts[..5].iter().rev() {
            themes5_event(reader, KeyEvent::Pressed(Key::Up)).await;
            assert_eq!(reader.cursor, *expected);
        }
        themes5_event(reader, KeyEvent::Pressed(Key::R)).await;
        assert!(reader.cursor > starts[5]);
        assert!(reader.text.is_char_boundary(reader.cursor));
        themes5_event(reader, KeyEvent::Pressed(Key::L)).await;
        assert_eq!(reader.cursor, starts[0]);
        // At most 40 visible lines, 5 Down, 15 rescanned by Up, 10 R and 10 L.
        let calls = LEGACY_MEASURE_CALLS.with(|n| n.get());
        assert!(calls > 0 && calls <= 80 * 513, "{calls} measure calls");
    }

    #[tokio::test]
    async fn themes9_legacy_long_paragraph_measurement_is_bounded() {
        for id in ["orbit", "compact"] {
            for ending in ["", "\n"] {
                let text = "W".repeat(200_000) + ending;
                let (root, mut reader) = themes5_reader("themes9-long", id, &text);
                let res = reader.res.clone();
                let styles = res.get::<Stylesheet>();
                reader.replace_text("W".repeat(1024) + "\n");
                let expected = reader.reference_legacy_line(&styles, 0).to_owned();
                reader.replace_text(text);
                assert_eq!(reader.get_line(&styles, 0), expected);
                drop(styles);
                themes9_legacy_bounded_navigation(&mut reader).await;
                fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn themes9_legacy_zero_width_paragraph_progresses_at_source_cap() {
        let limit = common::theme::render::InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT;
        for id in ["orbit", "compact"] {
            let text = "\u{200b}".repeat(200_000);
            let (root, mut reader) = themes5_reader("themes9-zero-width", id, &text);
            let res = reader.res.clone();
            let styles = res.get::<Stylesheet>();
            assert_eq!(reader.get_line(&styles, 0).chars().count(), limit);
            drop(styles);
            themes9_legacy_bounded_navigation(&mut reader).await;
            LEGACY_MEASURE_CALLS.with(|n| n.set(0));
            reader.move_forward_lines(usize::MAX);
            assert_eq!(reader.cursor, text.len());
            assert_eq!(
                LEGACY_MEASURE_CALLS.with(|n| n.get()),
                200_000_usize.div_ceil(limit)
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes8_legacy_blank_line_up_down_and_shoulders_are_reversible() {
        for id in ["orbit", "compact"] {
            let (root, mut reader) = themes5_reader("themes8-events", id, &"a\n\n".repeat(20));
            for repeat in [false, true] {
                let event = |key| {
                    if repeat {
                        KeyEvent::Autorepeat(key)
                    } else {
                        KeyEvent::Pressed(key)
                    }
                };
                reader.cursor = 3;
                themes5_event(&mut reader, event(Key::Up)).await;
                assert_eq!(reader.cursor, 2, "{id} Up must count the blank line");
                themes5_event(&mut reader, event(Key::Down)).await;
                assert_eq!(reader.cursor, 3);
                for (key, expected) in [
                    (Key::Up, 2),
                    (Key::Up, 0),
                    (Key::Down, 2),
                    (Key::Down, 3),
                    (Key::Down, 5),
                    (Key::Down, 6),
                    (Key::Up, 5),
                    (Key::Up, 3),
                ] {
                    themes5_event(&mut reader, event(key)).await;
                    assert_eq!(reader.cursor, expected, "{id} {key:?}");
                }
                reader.cursor = 30;
                for (key, expected) in [(Key::L, 15), (Key::R, 30), (Key::R, 45), (Key::L, 30)] {
                    themes5_event(&mut reader, event(key)).await;
                    assert_eq!(reader.cursor, expected, "{id} {key:?}");
                }
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn themes8_property_corpus() -> Vec<String> {
        let mut seed = 0x7468_656d_6573_3801_u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let mut guides = vec![
            String::new(),
            "\n".into(),
            "\n\n\n\n".into(),
            "\r\n\r\n密é\r\n\r\nZ".into(),
            "W密é".into(),
            "a\n\n".repeat(20),
        ];
        for case in 0..300 {
            let mut guide = String::new();
            for _ in 0..2 {
                match random() % 5 {
                    0 => guide.push_str(["a", "two words", "éclair café"][random() % 3]),
                    1 => guide.push_str(
                        &"Alpha beta café WWW iii 日本語 ".repeat(if case % 20 == 0 {
                            3
                        } else {
                            2
                        }),
                    ),
                    2 => guide.push_str(&"\n".repeat(1 + random() % 4)),
                    3 => guide.push_str("é密\r\n\r\nshort\r\n"),
                    _ => guide.push_str("密碼日本語長文章 café été"),
                }
                guide.push_str(if random() % 2 == 0 { "\n" } else { "\r\n" });
            }
            if case % 2 == 0 {
                guide.truncate(guide.trim_end_matches(['\r', '\n']).len());
            }
            guides.push(guide);
        }
        guides
    }

    #[test]
    fn themes10_legacy_break_offsets_match_previous_algorithm() {
        let mut guides = themes8_property_corpus();
        guides.extend([
            "ab\nabc密ZZ".into(),
            format!(
                "ab\n{}",
                "Alpha beta WWW iii words without a final newline. ".repeat(5)
            ),
            format!("ab\n{}", "密碼日本語長文章".repeat(8)),
            "W".repeat(1024),
            "W".repeat(1024) + "\n",
            "\u{200b}".repeat(1024),
            "\u{200b}".repeat(1024) + "\n",
            "AVATAR To fi é密😀\r\n".repeat(40),
            "\u{0301}\u{200b}W\t\r".repeat(140),
        ]);
        for id in ["orbit", "compact"] {
            let (root, mut reader) = themes5_reader("themes10-equivalence", id, "");
            let res = reader.res.clone();
            let styles = res.get::<Stylesheet>();
            for (case, text) in guides.iter().enumerate() {
                reader.replace_text(text.clone());
                let mut at = 0;
                loop {
                    let actual = reader.get_line(&styles, at);
                    assert_eq!(
                        actual,
                        reader.themes10_reference_line(&styles, at),
                        "{id} case={case} byte={at}"
                    );
                    if at == reader.text.len() {
                        break;
                    }
                    let next = reader.next_line_start(at, actual);
                    assert!(next > at);
                    at = next;
                }
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn themes10_legacy_cache_invalidates_text_theme_and_style() {
        let (root, mut reader) =
            themes5_reader("themes10-invalidation", "orbit", &"W".repeat(2048));
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        let res = reader.res.clone();
        let original = res.get::<Option<ThemeHandle>>().clone();
        assert!(reader.legacy_paragraph.as_ref().unwrap().theme.is_some());
        res.insert(None::<ThemeHandle>);
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        assert!(reader.legacy_paragraph.as_ref().unwrap().theme.is_none());
        let mut styles = res.get::<Stylesheet>().clone();
        styles.menu.guide_font.size += 5;
        res.insert(styles.clone());
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        assert_eq!(
            reader.legacy_paragraph.as_ref().unwrap().height,
            styles.menu.guide_font.size
        );
        reader.rect.w -= 20;
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        let cache = reader.legacy_paragraph.as_ref().unwrap();
        assert_eq!(cache.width, reader.legacy_style(&styles).0);
        res.insert(original);
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        assert!(reader.legacy_paragraph.as_ref().unwrap().theme.is_some());
        reader.replace_text("a\n\n".repeat(30));
        assert!(reader.legacy_paragraph.is_none());
        reader.cursor = reader.text.len();
        reader.move_back_lines(1);
        assert_eq!(reader.cursor, reader.text.len() - 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn themes10_legacy_cold_and_cached_navigation_char_visits() {
        for id in ["orbit", "compact"] {
            for (case, text) in [
                ("unterminated", "W".repeat(200_000)),
                ("terminated", "W".repeat(200_000) + "\n"),
                ("zero-width", "\u{200b}".repeat(200_000)),
            ] {
                let (root, mut reader) = themes5_reader("themes10-visits", id, &text);
                let styles = reader.res.get::<Stylesheet>();
                let mut at = 0;
                while at < reader.text.len() {
                    reader.cursor = at;
                    at = reader.next_line_start(at, reader.get_line(&styles, at));
                    assert!(at > reader.cursor);
                }
                drop(styles);
                assert!(reader.legacy_paragraph.is_none());
                LEGACY_MEASURE_CHAR_VISITS.with(|n| n.set(0));
                themes5_event(&mut reader, KeyEvent::Pressed(Key::Up)).await;
                let first = LEGACY_MEASURE_CHAR_VISITS.with(|n| n.get());
                assert!(first <= 900_000, "{id} {case} first Up {first}");
                let mut up = Vec::new();
                for _ in 0..20 {
                    let previous = reader.cursor;
                    LEGACY_MEASURE_CHAR_VISITS.with(|n| n.set(0));
                    themes5_event(&mut reader, KeyEvent::Pressed(Key::Up)).await;
                    assert!(reader.cursor < previous);
                    let visits = LEGACY_MEASURE_CHAR_VISITS.with(|n| n.get());
                    assert!(visits <= 50_000, "{id} {case} cached Up {visits}");
                    up.push(visits);
                }
                LEGACY_MEASURE_CHAR_VISITS.with(|n| n.set(0));
                themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
                let left = LEGACY_MEASURE_CHAR_VISITS.with(|n| n.get());
                assert!(left <= 50_000, "{id} {case} L {left}");
                LEGACY_MEASURE_CHAR_VISITS.with(|n| n.set(0));
                let styles = reader.res.get::<Stylesheet>();
                reader.visible_text(&styles, 480);
                let frame = LEGACY_MEASURE_CHAR_VISITS.with(|n| n.get());
                assert!(frame <= 100_000, "{id} {case} frame {frame}");
                println!(
                    "VISITS {id} {case}: first Up={first}; next 20 Up={up:?}; L={left}; frame={frame}"
                );
                assert!(reader.legacy_paragraph.as_ref().unwrap().starts.len() <= 200_000);
                fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn themes8_legacy_generated_display_line_starts_reverse_forward_movement() {
        let guides = themes8_property_corpus();
        for id in ["orbit", "compact"] {
            let (root, mut reader) = themes5_reader("themes8-generated", id, "");
            let mut wrapped_lines = 0;
            for (case, text) in guides.iter().enumerate() {
                reader.replace_text(text.clone());
                reader.lowercase_text = text.to_lowercase();
                reader.cursor = 0;
                let mut starts = vec![0];
                for _ in 0..text.chars().count() {
                    if reader.cursor == text.len() {
                        break;
                    }
                    let previous = reader.cursor;
                    reader.move_forward_lines(1);
                    assert!(reader.cursor > previous, "{id} case {case} stalled");
                    assert!(text.is_char_boundary(reader.cursor));
                    let paragraph_end = text[previous..]
                        .find('\n')
                        .map_or(text.len(), |n| previous + n);
                    wrapped_lines += usize::from(reader.cursor < paragraph_end);
                    starts.push(reader.cursor);
                }
                assert_eq!(reader.cursor, text.len(), "{id} case {case} missed EOF");
                reader.cursor = 0;
                let styles = reader.res.get::<Stylesheet>();
                let visible = reader.visible_text(&styles, 480);
                for (n, line) in visible.iter().enumerate() {
                    let at = starts.get(n).copied().unwrap_or(text.len());
                    let expected = reader.get_line(&styles, at);
                    assert_eq!(*line, expected.strip_suffix('\n').unwrap_or(expected));
                }
                drop(styles);
                for (index, &start) in starts.iter().enumerate() {
                    if index > 0 {
                        reader.cursor = start;
                        reader.move_back_lines(1);
                        assert_eq!(
                            reader.cursor,
                            starts[index - 1],
                            "{id} case {case} Up at {start}: {text:?}"
                        );
                    }
                    for k in 0..=index {
                        reader.cursor = start;
                        reader.move_back_lines(k);
                        assert_eq!(
                            reader.cursor,
                            starts[index - k],
                            "{id} case {case} back {k} at {start}: {text:?}"
                        );
                        reader.move_forward_lines(k);
                        assert_eq!(reader.cursor, start, "{id} case {case} round trip {k}");
                    }
                    if index >= 10 {
                        reader.cursor = start;
                        reader.move_page(false);
                        assert_eq!(reader.cursor, starts[index - 10]);
                        reader.move_page(true);
                        assert_eq!(reader.cursor, start, "{id} case {case} L/R");
                    }
                    if index + 10 < starts.len() {
                        reader.cursor = start;
                        reader.move_page(true);
                        assert_eq!(reader.cursor, starts[index + 10]);
                        reader.move_page(false);
                        assert_eq!(reader.cursor, start, "{id} case {case} R/L");
                    }
                }
                reader.cursor = text.len();
                reader.move_back_lines(usize::MAX);
                assert_eq!(reader.cursor, 0);
                reader.move_forward_lines(usize::MAX);
                assert_eq!(reader.cursor, text.len());
            }
            assert!(
                wrapped_lines > 0,
                "{id} generated corpus must exercise wrapping"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes7_legacy_restored_unterminated_utf8_events_make_progress() {
        for id in ["orbit", "compact"] {
            let (root, reader) = themes5_reader("unterminated-eof", id, "ab\nabc密ZZ");
            assert_eq!(reader.text.len(), 11);
            reader
                .res
                .get::<Database>()
                .update_guide_cursor(&reader.path, 11)
                .unwrap();
            for repeat in [false, true] {
                let event = |key| {
                    if repeat {
                        KeyEvent::Autorepeat(key)
                    } else {
                        KeyEvent::Pressed(key)
                    }
                };
                for back in [Key::Up, Key::L] {
                    let mut restored =
                        TextReader::new(reader.rect, reader.res.clone(), reader.path.clone());
                    assert_eq!(restored.cursor, 11);
                    themes5_event(&mut restored, event(back)).await;
                    assert!(restored.cursor < 11, "{id} {back:?}");
                    assert!(restored.text.is_char_boundary(restored.cursor));
                    themes6_render(&mut restored);
                }
                let mut restored =
                    TextReader::new(reader.rect, reader.res.clone(), reader.path.clone());
                for key in [Key::Up, Key::L, Key::Down, Key::R] {
                    let previous = restored.cursor;
                    themes5_event(&mut restored, event(key)).await;
                    assert!(
                        restored.text.is_char_boundary(restored.cursor),
                        "{id} {key:?}"
                    );
                    if matches!(key, Key::Up | Key::L) {
                        assert!(restored.cursor < previous, "{id} {key:?}");
                    } else {
                        assert!(restored.cursor > previous, "{id} {key:?}");
                    }
                    themes6_render(&mut restored);
                }
                assert_eq!(restored.cursor, 11);
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes7_legacy_unterminated_paragraph_wraps_like_terminated_copy() {
        for id in ["orbit", "compact"] {
            for (case, paragraph) in [
                (
                    "ascii",
                    "Alpha beta WWW iii words without a final newline. ".repeat(5),
                ),
                ("cjk", "密碼日本語長文章".repeat(8)),
            ] {
                let text = format!("ab\n{paragraph}");
                let (root, mut reader) = themes5_reader(case, id, &text);
                let styles = reader.res.get::<Stylesheet>();
                let mut at = 3;
                let mut consumed = String::new();
                let mut count = 0;
                // Bound the test walk so a zero-length regression fails rather than spinning.
                for _ in 0..paragraph.chars().count() {
                    if at == text.len() {
                        break;
                    }
                    let line = reader.get_line(&styles, at).to_owned();
                    assert!(!line.is_empty(), "{id} {case} at {at}");
                    if at == 3 {
                        assert!(line.len() < paragraph.len(), "{id} {case} must wrap");
                    }
                    reader.text.push('\n');
                    assert_eq!(
                        line,
                        reader.get_line(&styles, at),
                        "{id} {case} terminated at {at}"
                    );
                    assert_eq!(
                        line,
                        reader.reference_legacy_line(&styles, at),
                        "{id} {case} legacy wrap at {at}"
                    );
                    reader.text.pop();
                    at += line.len();
                    assert!(reader.text.is_char_boundary(at));
                    consumed.push_str(&line);
                    count += 1;
                }
                assert_eq!(at, text.len(), "{id} {case} stalled");
                assert_eq!(consumed, paragraph);
                assert!(count > 1);
                assert_eq!(reader.get_line(&styles, at), "");
                drop(styles);
                reader.cursor = text.len();
                themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
                let previous = reader.cursor;
                assert!(previous < text.len(), "{id} {case} L from EOF");
                themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
                assert!(reader.cursor > previous, "{id} {case} R after L");
                assert!(reader.text.is_char_boundary(reader.cursor));
                fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn themes7_legacy_zero_width_consumes_one_complete_glyph() {
        let (root, mut reader) = themes5_reader("zero-width", "orbit", "W密Z");
        reader.res.insert(None::<ThemeHandle>);
        let res = reader.res.clone();
        let styles = res.get::<Stylesheet>();
        reader.rect.w = styles.ui.margin_x as u32 * 2 + styles.ui.margin_y as u32 * 2;
        let mut at = 0;
        for glyph in ['W', '密', 'Z'] {
            let line = reader.get_line(&styles, at);
            assert_eq!(line, glyph.to_string());
            at += line.len();
        }
        assert_eq!(at, reader.text.len());
        assert_eq!(reader.get_line(&styles, at), "");
        drop(styles);
        for expected in [1, 4, 5] {
            themes5_event(&mut reader, KeyEvent::Pressed(Key::Down)).await;
            assert_eq!(reader.cursor, expected);
        }
        themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
        assert_eq!(reader.cursor, 0);
        themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
        assert_eq!(reader.cursor, reader.text.len());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn themes7_blank_lines_consume_newlines_without_skipping() {
        for id in THEMES6_PACKS {
            let (root, mut reader) = themes5_reader("blank-lines", id, "\n\n密\n\nZ");
            let res = reader.res.clone();
            let styles = res.get::<Stylesheet>();
            assert_eq!(reader.get_line(&styles, 0), "\n");
            assert_eq!(reader.get_line(&styles, 1), "\n");
            assert_eq!(
                &reader.visible_text(&styles, 480)[..5],
                &["", "", "密", "", "Z"]
            );
            drop(styles);
            for expected in [1, 2, 6, 7, 8] {
                themes5_event(&mut reader, KeyEvent::Pressed(Key::Down)).await;
                assert_eq!(reader.cursor, expected, "{id}");
            }
            themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
            assert_eq!(reader.cursor, 0);
            themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
            assert_eq!(reader.cursor, reader.text.len());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes6_first_line_search_and_shoulders_keep_utf8_boundaries() {
        for id in THEMES6_PACKS {
            let (root, mut reader) = themes5_reader("utf8-search", id, "éclair");
            assert!(reader.search_forward("c"), "{id}");
            assert_eq!(reader.cursor, 0, "{id} first line starts at zero");
            for event in [
                KeyEvent::Pressed(Key::L),
                KeyEvent::Pressed(Key::R),
                KeyEvent::Autorepeat(Key::L),
                KeyEvent::Autorepeat(Key::R),
            ] {
                themes5_event(&mut reader, event).await;
                themes6_render(&mut reader);
            }
            reader.cursor = reader.text.len();
            assert!(reader.search_backward("c"));
            assert_eq!(reader.cursor, 0, "{id} backward first-line search");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes6_restored_and_invalid_cursors_normalize_before_paging_and_render() {
        for id in THEMES6_PACKS {
            let (root, reader) = themes5_reader("utf8-restore", id, "密éclair\nsecond\n");
            let res = reader.res.clone();
            let path = reader.path.clone();
            for saved in [1, 4, reader.text.len() + 20] {
                res.get::<Database>()
                    .update_guide_cursor(&path, saved as i64)
                    .unwrap();
                let mut restored = TextReader::new(reader.rect, res.clone(), path.clone());
                assert_eq!(restored.cursor, guide_cursor(&reader.text, saved), "{id}");
                themes6_render(&mut restored);
                for key in [Key::L, Key::R, Key::Up, Key::Down] {
                    // Exercise the paging and line entry guards independently of restore.
                    restored.cursor = saved;
                    restored.ink_page_history.push_back((0, saved));
                    themes5_event(&mut restored, KeyEvent::Pressed(key)).await;
                    assert!(restored.text.is_char_boundary(restored.cursor), "{id}");
                    themes6_render(&mut restored);
                }
                restored.cursor = saved;
                themes6_render(&mut restored);
                assert_eq!(restored.cursor, guide_cursor(&reader.text, saved), "{id}");
                assert!(restored.ink_page_history.is_empty());
            }
            res.get::<Database>()
                .update_guide_cursor(&path, -1)
                .unwrap();
            assert_eq!(TextReader::new(reader.rect, res, path).cursor, 0);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes6_empty_guides_page_and_search_without_underflow() {
        for id in THEMES6_PACKS {
            let (root, mut reader) = themes5_reader("empty", id, "");
            for event in [
                KeyEvent::Pressed(Key::L),
                KeyEvent::Pressed(Key::R),
                KeyEvent::Autorepeat(Key::L),
                KeyEvent::Autorepeat(Key::R),
            ] {
                reader.cursor = usize::MAX;
                themes5_event(&mut reader, event).await;
                assert_eq!(reader.cursor, 0, "{id}");
                themes6_render(&mut reader);
            }
            assert!(!reader.search_forward("c"));
            assert!(!reader.search_backward("c"));
            assert!(reader.search_forward(""));
            assert!(reader.search_backward(""));
            assert_eq!(reader.cursor, 0);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn themes6_search_maps_lowercase_byte_offsets_to_original_lines() {
        let (root, mut reader) =
            themes5_reader("lowercase", "orbit-hero", "İéclair\n密 TARGET\nfinish");
        assert!(reader.search_forward("target"));
        let expected = "İéclair\n".len();
        assert_eq!(reader.cursor, expected);
        reader.cursor = reader.text.len();
        assert!(reader.search_backward("target"));
        assert_eq!(reader.cursor, expected);
        reader.cursor = 4;
        assert!(!reader.search_forward_wrapping("missing".into()));
        assert!(reader.text.is_char_boundary(reader.cursor));
        reader.cursor = usize::MAX;
        assert!(!reader.search_backward_wrapping("missing".into()));
        assert_eq!(reader.cursor, reader.text.len());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn themes5_restored_short_lines_use_canonical_previous_page() {
        for id in THEMES5_PACKS {
            let text = "AB\n".repeat(2000);
            let (root, mut reader) = themes5_reader("restore", id, &text);
            let theme = reader.res.get::<Option<ThemeHandle>>().clone().unwrap();
            let (layout, rect, max_lines) = TextReader::ink_guide_layout(&theme).unwrap();
            let count = layout.guide_line_count(rect.3, max_lines) as usize;
            assert_eq!(
                count,
                if id.starts_with("orbit") {
                    9
                } else if id.starts_with("big") {
                    7
                } else {
                    12
                }
            );
            let mut starts = vec![0];
            while reader.cursor + count * 3 <= 4860 {
                themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
                starts.push(reader.cursor);
            }
            let original = *starts.last().unwrap();
            if id.starts_with("orbit") {
                assert_eq!(original, 4860);
                assert_eq!(starts[starts.len() - 2], 4833);
            }
            reader.save_cursor();
            reader = TextReader::new(reader.rect, reader.res.clone(), reader.path.clone());
            assert_eq!(reader.cursor, original);
            assert_eq!(reader.ink_page_history.capacity(), 0);
            themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
            assert_eq!(reader.cursor, starts[starts.len() - 2], "{id}");
            themes5_event(&mut reader, KeyEvent::Autorepeat(Key::R)).await;
            assert_eq!(reader.cursor, original, "{id}");

            // A display-line boundary need not also be a canonical page boundary.
            reader.cursor = original + 3;
            reader.ink_page_history = VecDeque::new();
            themes5_event(&mut reader, KeyEvent::Autorepeat(Key::L)).await;
            assert_eq!(reader.cursor, original + 3 - count * 3, "{id}");
            themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
            assert_eq!(reader.cursor, original + 3, "{id}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes5_mixed_guide_fallback_reverses_recorded_page_starts() {
        let paragraph = "Wide WWW narrow iii, éclair Straße 日本語 密碼 and  spaces. ".repeat(12);
        let text = format!("\n{paragraph}\n\nAB\n{paragraph}\n\n").repeat(12);
        for id in THEMES5_PACKS {
            let (root, mut reader) = themes5_reader("mixed", id, &text);
            let mut starts = vec![0];
            loop {
                let previous = reader.cursor;
                themes5_event(&mut reader, KeyEvent::Autorepeat(Key::R)).await;
                assert!(reader.cursor > previous, "{id} stalled");
                if reader.cursor == text.len() {
                    break;
                }
                starts.push(reader.cursor);
            }
            assert!(starts.len() > 20, "{id}");
            // The final, potentially partial page has no full-page inverse at EOF.
            reader.cursor = starts.pop().unwrap();
            reader.ink_page_history = VecDeque::new();
            for expected in starts.into_iter().rev() {
                let original = reader.cursor;
                themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
                assert_eq!(reader.cursor, expected, "{id} previous page of {original}");
                themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
                assert_eq!(reader.cursor, original, "{id} fallback L then R");
                reader.ink_page_history = VecDeque::new();
                themes5_event(&mut reader, KeyEvent::Autorepeat(Key::L)).await;
                assert_eq!(reader.cursor, expected, "{id} repeated fallback L");
            }
            themes5_event(&mut reader, KeyEvent::Autorepeat(Key::L)).await;
            assert_eq!(reader.cursor, 0);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes5_history_caps_ten_thousand_pages_and_falls_back_after_eviction() {
        for id in THEMES5_PACKS {
            let (root, mut reader) = themes5_reader("cap", id, &"AB\n".repeat(120_012));
            let mut starts = vec![0];
            for _ in 0..10_000 {
                themes5_event(&mut reader, KeyEvent::Autorepeat(Key::R)).await;
                assert!(reader.cursor > *starts.last().unwrap());
                starts.push(reader.cursor);
                assert!(
                    reader.ink_page_history.len() <= INK_PAGE_HISTORY_LIMIT,
                    "{id}"
                );
                assert!(
                    reader.ink_page_history.capacity() <= INK_PAGE_HISTORY_LIMIT,
                    "{id}"
                );
            }
            assert_eq!(reader.ink_page_history.len(), INK_PAGE_HISTORY_LIMIT);
            starts.pop();
            for expected in starts.into_iter().rev().take(INK_PAGE_HISTORY_LIMIT + 32) {
                themes5_event(&mut reader, KeyEvent::Autorepeat(Key::L)).await;
                assert_eq!(reader.cursor, expected, "{id} after history eviction");
            }
            assert_eq!(reader.ink_page_history.capacity(), 0);
            themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
            themes5_event(&mut reader, KeyEvent::Pressed(Key::Down)).await;
            assert_eq!(
                reader.ink_page_history.capacity(),
                0,
                "{id} line jump frees history"
            );
            themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
            assert!(reader.search_forward("ab"));
            assert_eq!(
                reader.ink_page_history.capacity(),
                0,
                "{id} search frees history"
            );
            themes5_event(&mut reader, KeyEvent::Pressed(Key::R)).await;
            reader.ink_page_theme = None;
            themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
            assert_eq!(
                reader.ink_page_history.capacity(),
                0,
                "{id} theme change frees history"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn themes5_degenerate_paragraph_has_bounded_backward_line_calls() {
        for value in ["W", "密", "\u{200b}"] {
            for id in THEMES5_PACKS {
                let text = value.repeat(200_000);
                let (root, mut reader) = themes5_reader("degenerate", id, &text);
                let theme = reader.res.get::<Option<ThemeHandle>>().clone().unwrap();
                let (layout, rect, max_lines) = TextReader::ink_guide_layout(&theme).unwrap();
                let count = layout.guide_line_count(rect.3, max_lines) as usize;
                let budget =
                    count * common::theme::render::InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT * 2;
                reader.cursor = text.len() - value.len();
                let original = reader.cursor;
                INK_BACKWARD_LINE_CALLS.with(|n| n.set(0));
                themes5_event(&mut reader, KeyEvent::Pressed(Key::L)).await;
                assert!(reader.cursor < original, "{id} {value:?} did not move");
                assert!(text.is_char_boundary(reader.cursor));
                let calls = INK_BACKWARD_LINE_CALLS.with(|n| n.get());
                assert!(
                    calls > 0 && calls <= count * budget,
                    "{id} {value:?}: {calls} calls"
                );
                fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn themed_keyboard_capture_bounds_model_without_changing_controller_text() {
        let root = std::env::current_dir().unwrap().join("out").join(format!(
            "allium-keyboard-bound-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("guide.txt");
        fs::write(&path, "guide").unwrap();
        let res = themed_resources(&root);
        let mut reader = TextReader::new(Rect::new(0, 0, 640, 480), res.clone(), path);
        let query = "密".repeat(513);
        reader.keyboard = Some(Keyboard::new(res, query.clone(), false));
        reader.refresh_theme(Status::default()).unwrap();
        assert!(reader.theme_frame.is_some());
        assert_eq!(reader.keyboard.as_ref().unwrap().value(), query);
        assert_eq!(
            reader
                .keyboard
                .as_ref()
                .unwrap()
                .theme_inputs()
                .display_value
                .chars()
                .count(),
            513
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod parity_b_tests {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/parity-b-support.rs"
    ));
    #[test]
    fn parity_b_reader_and_keyboard() {
        let temp =
            std::env::temp_dir().join(format!("allium-parity-b-reader-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("Real Racing Roots.txt");
        fs::write(
            &path,
            "Your saved notes for this game appear here.\nA readable guide page.",
        )
        .unwrap();
        for (variant, theme) in parity_variants() {
            let res = super::theme_adapter_tests::themed_resources(&temp);
            res.insert(Some(theme));
            let mut reader = TextReader::new(Rect::new(0, 0, 640, 480), res.clone(), path.clone());
            let status = Status {
                clock: Some("12:34".into()),
                battery: Some(82),
                network: Some("online".into()),
                ..Default::default()
            };
            reader.refresh_theme(status.clone()).unwrap();
            parity_save(
                &variant,
                "text_reader",
                reader.theme_frame.as_ref().unwrap().pixels(),
            );
            reader.keyboard = Some(Keyboard::new(res, "Search".into(), false));
            reader.refresh_theme(status).unwrap();
            parity_save(
                &variant,
                "keyboard",
                reader.theme_frame.as_ref().unwrap().pixels(),
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }
}
