//! Settled-frame renderer for explicitly ink-aligned external packs.
use super::*;
use crate::theme::ink::{Run, Surface, merge_run};
use crate::theme::pack::FontId;
use rusttype::{Font, Scale, point};
use std::{cell::RefCell, collections::VecDeque, sync::Arc};

#[derive(Clone)]
pub struct InkTextLayout {
    font: Font<'static>,
    fallback: Font<'static>,
    size: f32,
    line: u32,
    tracking: f32,
    upper: bool,
    color: ColorRole,
    weight: u32,
    family: FontId,
}
type Type = InkTextLayout;
#[path = "render_ink_parity.rs"]
mod parity;
pub use parity::ink_detail_pages;
use parity::symbol;
#[path = "render_ink_retained.rs"]
pub(super) mod retained;
impl InkTextLayout {
    // Sixteen lines fit the frame model's 8,192 source-character limit.
    pub const GUIDE_LINE_SOURCE_CHAR_LIMIT: usize = 512;

    /// Source bytes consumed by one guide line, including its newline when present.
    pub fn guide_line_len(&self, value: &str, width: u32) -> usize {
        let mut fit = 0;
        let mut word = None;
        let mut x = 0.0;
        let mut prev = None;
        for (at, ch) in value
            .char_indices()
            .take(Self::GUIDE_LINE_SOURCE_CHAR_LIMIT)
        {
            #[cfg(test)]
            GUIDE_SOURCE_VISITS.with(|n| n.set(n.get() + 1));
            let next = at + ch.len_utf8();
            if ch == '\n' {
                return next;
            }
            // Preserve metrics' floating-point addition order, including uppercase expansions.
            let mut advance = |ch| {
                #[cfg(test)]
                GUIDE_GLYPH_VISITS.with(|n| n.set(n.get() + 1));
                let font = self.face(ch);
                let glyph = font.glyph(ch).scaled(Scale::uniform(self.size as f32));
                if let Some(id) = prev {
                    x += font.pair_kerning(Scale::uniform(self.size as f32), id, glyph.id());
                }
                prev = Some(glyph.id());
                x += glyph.h_metrics().advance_width + self.tracking;
            };
            if self.upper {
                for ch in ch.to_uppercase() {
                    advance(ch);
                }
            } else {
                advance(ch);
            }
            if x > width as f32 && fit > 0 {
                return word.filter(|n| *n > 0).unwrap_or(fit);
            }
            fit = next;
            if ch.is_whitespace() {
                word = Some(next);
            }
        }
        fit
    }
    pub fn guide_line_count(&self, height: u32, max_lines: u32) -> u32 {
        max_lines.min(height.min(480) / self.line.max(12)).min(16)
    }
    pub fn guide_page_len(&self, value: &str, width: u32, height: u32, max_lines: u32) -> usize {
        let mut used = 0;
        for _ in 0..self.guide_line_count(height, max_lines) {
            used += self.guide_line_len(&value[used..], width);
            if used == value.len() {
                break;
            }
        }
        used
    }
    fn guide_lines(&self, value: &str, width: u32, count: u32) -> Vec<String> {
        let mut rest = value;
        let mut lines = Vec::new();
        for _ in 0..count {
            if rest.is_empty() {
                break;
            }
            let used = self.guide_line_len(rest, width);
            lines.push(self.value(rest[..used].trim_end_matches('\n')));
            rest = &rest[used..];
        }
        lines
    }
    fn value(&self, value: &str) -> String {
        if self.upper {
            value.to_uppercase()
        } else {
            value.into()
        }
    }
    fn face(&self, ch: char) -> &Font<'static> {
        if self.font.glyph(ch).id().0 == 0 {
            &self.fallback
        } else {
            &self.font
        }
    }
    fn metrics(&self, value: &str) -> (f32, f32, f32) {
        let mut x = 0.0;
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut prev = None;
        for ch in value.chars() {
            #[cfg(test)]
            GUIDE_GLYPH_VISITS.with(|n| n.set(n.get() + 1));
            let font = self.face(ch);
            let glyph = font.glyph(ch).scaled(Scale::uniform(self.size as f32));
            if let Some(id) = prev {
                x += font.pair_kerning(Scale::uniform(self.size as f32), id, glyph.id());
            }
            let advance = glyph.h_metrics().advance_width;
            prev = Some(glyph.id());
            if let Some(bb) = glyph.exact_bounding_box() {
                left = left.min(x + bb.min.x);
                right = right.max(x + bb.max.x);
            }
            x += advance + self.tracking;
        }
        if !left.is_finite() {
            left = 0.0;
            right = 0.0;
        }
        (x, left, right)
    }
    fn cap(&self) -> f32 {
        self.font
            .glyph('H')
            .scaled(Scale::uniform(self.size as f32))
            .exact_bounding_box()
            .map_or(self.size * 0.7, |b| -b.min.y)
    }
    fn baseline(&self) -> f32 {
        let v = self.font.v_metrics(Scale::uniform(self.size as f32));
        (self.line as f32 - v.ascent + v.descent) / 2.0 + v.ascent
    }
    fn lines(&self, value: &str, width: u32, count: u32, overflow: Overflow) -> Vec<String> {
        #[cfg(test)]
        LAYOUT_CALLS.with(|n| n.set(n.get() + 1));
        if value.is_empty() {
            return vec![];
        }
        let count = count.max(1) as usize;
        if overflow == Overflow::Ellipsis || count == 1 {
            let mut value = value.replace('\n', " ");
            if self.metrics(&value).0 > width as f32 {
                while !value.is_empty() && self.metrics(&(value.clone() + "…")).0 > width as f32 {
                    value.pop();
                }
                value = value.trim_end().to_owned() + "…";
            }
            return vec![value];
        }
        let mut lines = vec![];
        let mut line = String::new();
        let mut truncated = false;
        for ch in value.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut line));
            } else {
                let mut next = line.clone();
                next.push(ch);
                if !line.is_empty() && self.metrics(&next).0 > width as f32 {
                    if let Some(at) = line.rfind(char::is_whitespace).filter(|n| *n > 0) {
                        let rest = line[at..].trim_start().to_owned();
                        lines.push(line[..at].trim_end().into());
                        line = rest;
                    } else {
                        lines.push(std::mem::take(&mut line));
                    }
                }
                line.push(ch);
            }
            if lines.len() >= count {
                truncated = true;
                break;
            }
        }
        if lines.len() < count && !line.is_empty() {
            lines.push(line);
        }
        if truncated {
            if let Some(last) = lines.last_mut() {
                while !last.is_empty() && self.metrics(&(last.clone() + "…")).0 > width as f32 {
                    last.pop();
                }
                *last = last.trim_end().to_owned() + "…";
            }
        }
        lines
    }
}
struct CachedShelf {
    theme: std::sync::Weak<crate::theme::fonts::PreparedTheme>,
    source: Option<std::sync::Weak<Pixmap>>,
    key: String,
    pixels: Arc<Pixmap>,
}
thread_local! { static SHELVES: RefCell<VecDeque<CachedShelf>> = const { RefCell::new(VecDeque::new()) }; }
struct CachedText {
    theme: std::sync::Weak<crate::theme::fonts::PreparedTheme>,
    key: String,
    pixels: Arc<Pixmap>,
}
thread_local! {static SURFACES: RefCell<VecDeque<CachedText>>=const {RefCell::new(VecDeque::new())};static TEXT: RefCell<VecDeque<CachedText>>=const {RefCell::new(VecDeque::new())};}
fn opacity_table(opacity: f32) -> Arc<[u8]> {
    thread_local! { static TABLES: RefCell<VecDeque<(u32, Arc<[u8]>)>> = const { RefCell::new(VecDeque::new()) }; }
    TABLES.with(|tables| {
        let mut tables = tables.borrow_mut();
        let key = opacity.to_bits();
        let index = if let Some(index) = tables.iter().position(|(bits, _)| *bits == key) {
            index
        } else {
            let mut table = vec![0; 256 * 256].into_boxed_slice();
            for old in 0..256 {
                for value in 0..256 {
                    table[old * 256 + value] =
                        (old as f32 + (value as f32 - old as f32) * opacity).round() as u8;
                }
            }
            if tables.len() == 8 {
                tables.pop_back();
            }
            tables.push_front((key, table.into()));
            0
        };
        tables[index].1.clone()
    })
}
fn check_ink_dimensions(width: u32, height: u32) -> Result<()> {
    anyhow::ensure!(
        width > 0
            && width <= 640
            && height > 0
            && height <= 480
            && width
                .checked_mul(height)
                .is_some_and(|area| area <= 640 * 480),
        "Ink pixmap exceeds canvas"
    );
    Ok(())
}
fn ink_pixmap(width: u32, height: u32) -> Result<Pixmap> {
    check_ink_dimensions(width, height)?;
    Pixmap::new(width, height).ok_or_else(|| anyhow::anyhow!("Invalid ink box"))
}
fn inside(a: Rect, b: Rect) -> bool {
    b.0 >= a.0 && b.1 >= a.1 && b.0 + b.2 <= a.0 + a.2 && b.1 + b.3 <= a.1 + a.3
}
fn shift(r: &mut Rect, dy: i32) {
    r.1 = (r.1 as i32 + dy).clamp(0, 480u32.saturating_sub(r.3) as i32) as u32;
}
/// Resolves the same merged typography used by ink rendering.
pub fn ink_text_layout(theme: &ThemeHandle, c: &Component, focused: bool) -> InkTextLayout {
    let run = theme.pack().ink_run(c, focused);
    let family = run.font.unwrap();
    let weight = run.weight.unwrap_or(if family == FontId::BarlowCondensed {
        600
    } else {
        0
    });
    Type {
        font: theme.ink_font(family, weight),
        fallback: theme.fallback_font(),
        size: run.size.unwrap() as f32,
        line: run.line_height.unwrap(),
        tracking: run.letter_spacing.unwrap_or(0.0),
        upper: run.transform.as_deref() == Some("uppercase"),
        color: run.color.or(c.color).unwrap_or(ColorRole::Text),
        weight,
        family,
    }
}
impl PreparedFrame {
    fn ink_type(&self, c: &Component, focused: bool) -> Type {
        ink_text_layout(&self.theme, c, focused)
    }
    fn ink_subtype(&self, c: &Component, r: &Run) -> Type {
        let mut c = c.clone();
        if let Some(role) = r.role {
            c.role = Some(role);
            c.paint.as_mut().unwrap().run = None;
        }
        let p = c.paint.as_mut().unwrap();
        let mut run = p.run.clone().unwrap_or_default();
        merge_run(&mut run, r);
        p.run = Some(run);
        self.ink_type(&c, false)
    }
    fn ink_text(
        &mut self,
        id: &str,
        rect: Rect,
        ty: &Type,
        value: &str,
        count: u32,
        align: Align,
        overflow: Overflow,
        center: bool,
    ) -> Result<()> {
        self.ink_text_impl(id, rect, ty, value, count, align, overflow, center, false)
    }
    fn ink_text_impl(
        &mut self,
        id: &str,
        rect: Rect,
        ty: &Type,
        value: &str,
        count: u32,
        align: Align,
        overflow: Overflow,
        center: bool,
        guide: bool,
    ) -> Result<()> {
        if rect.2 == 0 || rect.3 == 0 || value.is_empty() {
            return Ok(());
        }
        check_ink_dimensions(rect.2, rect.3)?;
        let value = if guide {
            value.to_owned()
        } else {
            ty.value(value)
        };
        let key = format!(
            "{:?}:{:?}:{}:{}:{}:{}:{}:{:?}:{:?}:{}:{}",
            ty.family,
            ty.color,
            ty.size,
            ty.line,
            ty.weight,
            ty.tracking,
            ty.upper,
            align,
            overflow,
            center,
            value
        ) + &format!(":{}:{}:{}", rect.2, rect.3, count);
        let key = if guide { key + ":guide" } else { key };
        let weak = self.theme.weak();
        let cached = TEXT.with(|cache| {
            let mut cache = cache.borrow_mut();
            let index = cache.iter().position(|e| {
                e.theme.ptr_eq(&weak) && e.theme.strong_count() > 0 && e.key == key
            })?;
            let e = cache.remove(index).unwrap();
            let pixels = e.pixels.clone();
            cache.push_front(e);
            Some(pixels)
        });
        let pixels = if let Some(p) = cached {
            p
        } else {
            let lines = if guide {
                ty.guide_lines(&value, rect.2, ty.guide_line_count(rect.3, count))
            } else {
                ty.lines(&value, rect.2, count, overflow)
            };
            let mut pix = ink_pixmap(rect.2, rect.3)?;
            let color = self.color(ty.color);
            let cap = ty.cap();
            let block = cap + (lines.len().saturating_sub(1) as u32).saturating_mul(ty.line) as f32;
            for (i, line) in lines.iter().enumerate() {
                let (_, left, right) = ty.metrics(line);
                let x = match align {
                    Align::Left => (-left).ceil(),
                    Align::Right => (rect.2 as f32 - right).floor(),
                    Align::Center => ((rect.2 as f32 - left - right) / 2.0).round(),
                };
                let baseline = if center {
                    (rect.3 as f32 - block) / 2.0 + cap
                } else {
                    ty.baseline()
                } + i as f32 * ty.line as f32;
                raster(&mut pix, ty, line, x, baseline.round(), color);
            }
            let pix = Arc::new(pix);
            if pix.data().len() <= 512 * 1024 {
                TEXT.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    while !cache.is_empty()
                        && (cache.len() >= 64
                            || cache
                                .iter()
                                .map(|e| e.pixels.data().len() + e.key.len())
                                .sum::<usize>()
                                + pix.data().len()
                                > 512 * 1024)
                    {
                        cache.pop_back();
                    }
                    cache.push_front(CachedText {
                        theme: weak,
                        key,
                        pixels: pix.clone(),
                    });
                });
            }
            pix
        };
        super::ink_blit::bitmap(&mut self.pixels, rect.0 as i32, rect.1 as i32, pixels.as_ref().as_ref());
        self.text_boxes.push((id.into(), rect));
        Ok(())
    }
    fn ink_surface(
        &mut self,
        rect: Rect,
        c: &Component,
        fill: Option<ColorRole>,
        focused: bool,
        style: Option<&Surface>,
    ) {
        let p = c.paint.as_ref().unwrap();
        if rect.2 == 0 || rect.3 == 0 {
            return;
        }
        if p.gradient.is_none() && style.is_none_or(|s| s.gradient.is_none()) {
            self.ink_surface_raw(rect, c, fill, focused, style);
            return;
        }
        let key = format!(
            "{}:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{}",
            rect.2, rect.3, fill, c.radius, p.gradient, p.border, style, focused
        );
        let weak = self.theme.weak();
        let cached = SURFACES.with(|cache| {
            let mut cache = cache.borrow_mut();
            let index = cache.iter().position(|e| {
                e.theme.ptr_eq(&weak) && e.theme.strong_count() > 0 && e.key == key
            })?;
            let e = cache.remove(index).unwrap();
            let pixels = e.pixels.clone();
            cache.push_front(e);
            Some(pixels)
        });
        let pixels = if let Some(p) = cached {
            p
        } else {
            let Ok(local) = ink_pixmap(rect.2, rect.3) else {
                return;
            };
            let framebuffer = std::mem::replace(&mut self.pixels, local);
            self.ink_surface_raw(Rect(0, 0, rect.2, rect.3), c, fill, focused, style);
            let pixels = Arc::new(std::mem::replace(&mut self.pixels, framebuffer));
            SURFACES.with(|cache| {
                let mut cache = cache.borrow_mut();
                while !cache.is_empty()
                    && (cache.len() >= 64
                        || cache
                            .iter()
                            .map(|e| e.pixels.data().len() + e.key.len())
                            .sum::<usize>()
                            + pixels.data().len()
                            > if super::ink_blit::reference() { 2 * 1024 * 1024 } else { 6 * 1024 * 1024 })
                {
                    cache.pop_back();
                }
                cache.push_front(CachedText {
                    theme: weak,
                    key,
                    pixels: pixels.clone(),
                });
            });
            pixels
        };
        super::ink_blit::bitmap(&mut self.pixels, rect.0 as i32, rect.1 as i32, pixels.as_ref().as_ref());
    }
    fn ink_surface_raw(
        &mut self,
        rect: Rect,
        c: &Component,
        fill: Option<ColorRole>,
        focused: bool,
        style: Option<&Surface>,
    ) {
        if rect.2 == 0 || rect.3 == 0 {
            return;
        }
        let p = c.paint.as_ref().unwrap();
        let radius = style
            .and_then(|s| s.radius)
            .unwrap_or(c.radius.unwrap_or(0))
            .min(rect.2 / 2)
            .min(rect.3 / 2);
        let gradient = style
            .and_then(|s| s.gradient.as_ref())
            .or(p.gradient.as_ref());
        let fill = style.and_then(|s| s.fill).or(fill);
        let path = crate::display::build_rounded_rect_path(draw_rect(rect), radius);
        if let Some(path) = path {
            if let Some(g) = gradient {
                let angle = g.angle.to_radians();
                let dx = angle.sin();
                let dy = -angle.cos();
                let len = (rect.2 as f32 * dx).abs() + (rect.3 as f32 * dy).abs();
                let cx = rect.0 as f32 + rect.2 as f32 / 2.0;
                let cy = rect.1 as f32 + rect.3 as f32 / 2.0;
                let stops = g
                    .stops
                    .iter()
                    .map(|s| {
                        let c = self.color(s.token);
                        tiny_skia::GradientStop::new(
                            s.at,
                            tiny_skia::Color::from_rgba8(
                                c.r(),
                                c.g(),
                                c.b(),
                                (c.a() as f32 * s.alpha).round() as u8,
                            ),
                        )
                    })
                    .collect();
                if let Some(shader) = tiny_skia::LinearGradient::new(
                    tiny_skia::Point::from_xy(cx - dx * len / 2.0, cy - dy * len / 2.0),
                    tiny_skia::Point::from_xy(cx + dx * len / 2.0, cy + dy * len / 2.0),
                    stops,
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                ) {
                    self.pixels.fill_path(
                        &path,
                        &tiny_skia::Paint {
                            shader,
                            anti_alias: true,
                            ..Default::default()
                        },
                        tiny_skia::FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            } else if let Some(fill) = fill {
                self.panel(rect, fill, radius);
            }
        }
        let border = style.and_then(|s| s.border.as_ref()).or(p.border.as_ref());
        if let Some(b) = border {
            let color = if focused {
                b.focus_color.unwrap_or(b.color)
            } else {
                b.color
            };
            self.ink_border(rect, color, b.width, radius, b.alpha);
        }
    }
    fn ink_border(&mut self, r: Rect, color: ColorRole, width: u32, radius: u32, alpha: f32) {
        let width = width.min(r.2 / 2).min(r.3 / 2);
        if width == 0 {
            return;
        }
        let inset = (width + 1) / 2;
        let rr = Rect(
            r.0 + inset,
            r.1 + inset,
            r.2.saturating_sub(2 * inset).max(1),
            r.3.saturating_sub(2 * inset).max(1),
        );
        if let Some(path) =
            crate::display::build_rounded_rect_path(draw_rect(rr), radius.saturating_sub(inset))
        {
            let color = self.color(color);
            let paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                    color.r(),
                    color.g(),
                    color.b(),
                    (color.a() as f32 * alpha).round() as u8,
                )),
                anti_alias: true,
                ..Default::default()
            };
            self.pixels.stroke_path(
                &path,
                &paint,
                &tiny_skia::Stroke {
                    width: width as f32,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
    fn ink_focus(&mut self, c: &Component, r: Rect, focused: bool) {
        if !focused {
            return;
        }
        let color = c.focus_color.unwrap_or(ColorRole::Focus);
        let width = c.focus_width.unwrap_or(2);
        let radius = c.radius.unwrap_or(0);
        match c.focus_style.unwrap_or(FocusStyle::Outline) {
            FocusStyle::Underline => self.panel(
                Rect(r.0, r.1 + r.3 - width.min(r.3), r.2, width.min(r.3)),
                color,
                0,
            ),
            FocusStyle::Outline | FocusStyle::Glow => self.ink_border(r, color, width, radius, 1.0),
            FocusStyle::Fill => {}
        }
    }
    pub(super) fn render_ink(&mut self, input: &FrameInputs) -> Result<()> {
        let mut components = self.theme.pack().screens[&input.screen].components.clone();
        let state = primary_binding(input.screen)
            .and_then(|b| input.rows.get(&b))
            .is_some_and(|r| r.iter().any(|r| !r.label.is_empty()));
        components.retain(|c| {
            c.paint
                .as_ref()
                .unwrap()
                .when
                .as_deref()
                .is_none_or(|w| (w == "populated") == state)
        });
        self.ink_empty_stack(&mut components);
        self.ink_fit_dialog(&mut components, input);
        #[cfg(test)]
        if std::env::var_os("ALLIUM_INK_REFERENCE").is_some() {
            return self.render_ink_uncached(input, &components);
        }
        self.render_ink_retained(input, &components)
    }
    fn render_ink_uncached(&mut self, input: &FrameInputs, components: &[Component]) -> Result<()> {
        self.pixels.data_mut().fill(0);
        self.targets.clear();
        self.text_boxes.clear();
        self.damage = vec![Rect(0, 0, 640, 480)];
        crate::perf_trace::render_reason("ink_uncached");
        for c in components {
            self.ink_paint_component(c, input)?;
        }
        Ok(())
    }
    fn ink_paint_component(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let opacity = c.paint.as_ref().unwrap().opacity.unwrap_or(1.0);
        if opacity == 0.0 {
            return Ok(());
        }
        let before = if opacity < 1.0 {
            let mut v = vec![];
            for y in c.rect.1..c.rect.1 + c.rect.3 {
                let at = ((y * 640 + c.rect.0) * 4) as usize;
                v.extend_from_slice(&self.pixels.data()[at..at + c.rect.2 as usize * 4]);
            }
            Some(v)
        } else {
            None
        };
        self.ink_component(c, input)?;
        if let Some(before) = before {
            let table = (!super::ink_blit::reference()).then(|| opacity_table(opacity));
            for (row, y) in (c.rect.1..c.rect.1 + c.rect.3).enumerate() {
                let at = ((y * 640 + c.rect.0) * 4) as usize;
                for (channel, value) in self.pixels.data_mut()[at..at + c.rect.2 as usize * 4]
                    .iter_mut()
                    .enumerate()
                {
                    let old = before[row * c.rect.2 as usize * 4 + channel];
                    *value = if let Some(table) = &table {
                        table[old as usize * 256 + *value as usize]
                    } else {
                        (old as f32 + (*value as f32 - old as f32) * opacity).round() as u8
                    };
                }
            }
        }
        Ok(())
    }
    fn ink_fit_dialog(&self, components: &mut [Component], input: &FrameInputs) {
        if let Some(empty) = components
            .iter()
            .find(|c| {
                c.paint.as_ref().unwrap().collapse_empty
                    && input
                        .rows
                        .get(&c.binding.unwrap())
                        .is_none_or(|r| r.is_empty())
            })
            .map(|c| c.rect)
        {
            for c in components
                .iter_mut()
                .filter(|c| c.kind == Kind::DialogActions && c.rect.1 >= empty.1 + empty.3)
            {
                shift(&mut c.rect, -(empty.3 as i32));
            }
        }
        let panels: Vec<_> = components
            .iter()
            .filter(|c| c.kind == Kind::Panel && c.paint.as_ref().unwrap().fit_content)
            .cloned()
            .collect();
        for panel in panels {
            let Some(title) = components
                .iter()
                .find(|c| c.binding == Some(Binding::Title) && inside(panel.rect, c.rect))
            else {
                continue;
            };
            let ty = self.ink_type(title, false);
            let top = title.rect.1 as f32 + ty.baseline() - ty.cap() - panel.rect.1 as f32;
            let mut bottom = title.rect.1 + title.rect.3;
            for c in components
                .iter()
                .filter(|c| inside(panel.rect, c.rect) && c.kind != Kind::Panel)
            {
                let count = c
                    .binding
                    .and_then(|b| input.rows.get(&b))
                    .map_or(0, Vec::len) as u32;
                let end = if matches!(c.kind, Kind::Entries | Kind::SettingsRows) {
                    if count == 0 {
                        continue;
                    }
                    c.rect.1
                        + (count * c.item_extent.unwrap_or(0)
                            + count.saturating_sub(1) * c.gap.unwrap_or(0))
                        .min(c.rect.3)
                } else if c.kind == Kind::Text
                    && input
                        .text
                        .get(&c.binding.unwrap())
                        .is_none_or(|s| s.is_empty())
                {
                    continue;
                } else {
                    c.rect.1 + c.rect.3
                };
                bottom = bottom.max(end);
            }
            let height = ((bottom - panel.rect.1) as f32 + top)
                .round()
                .clamp(1.0, 480.0) as u32;
            let y = (480 - height) / 2;
            let delta = y as i32 - panel.rect.1 as i32;
            for c in components.iter_mut() {
                if c.id == panel.id {
                    c.rect.1 = y;
                    c.rect.3 = height;
                } else if inside(panel.rect, c.rect) {
                    if c.kind == Kind::Panel && c.rect.2 <= 4 && c.rect.3 > panel.rect.3 / 2 {
                        c.rect.3 =
                            (c.rect.3 as i32 + height as i32 - panel.rect.3 as i32).max(1) as u32;
                    }
                    shift(&mut c.rect, delta);
                }
            }
        }
    }
    fn ink_component(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let p = c.paint.as_ref().unwrap();
        match c.kind {
            Kind::Background | Kind::Panel | Kind::Overlay => {
                self.ink_surface(c.rect, c, c.color, false, None)
            }
            Kind::Text => {
                if c.logo && self.logo(c, input)? { return Ok(()); }
                let mut value = match c.binding.unwrap() {
                    Binding::ThemeName => self.theme.pack().name.trim_end_matches(" Tall").into(),
                    Binding::SelectedEyebrow => input
                        .text
                        .get(&Binding::SelectedMetadata)
                        .cloned()
                        .unwrap_or_default(),
                    Binding::EmptyTitle => if input.screen == ScreenId::Recents {
                        "Nothing played yet"
                    } else if input.screen == ScreenId::SearchResults {
                        "No results"
                    } else {
                        "No items"
                    }
                    .into(),
                    Binding::EmptyHint => if input.screen == ScreenId::Recents {
                        "Open a game from the library"
                    } else if input.screen == ScreenId::SearchResults {
                        "Try another search"
                    } else {
                        "Choose another collection"
                    }
                    .into(),
                    Binding::OsdValue => input
                        .progress
                        .map(|v| {
                            format!(
                                "{}{}",
                                (v * 100.0).round() as u32,
                                p.suffix.as_deref().unwrap_or("")
                            )
                        })
                        .unwrap_or_default(),
                    Binding::Pagination => {
                        let pagination = input.pagination.or_else(|| {
                            let text = input.text.get(&Binding::Pagination)?;
                            let (page, total) = text.split_once('/')?;
                            Some(crate::theme::frame::Pagination { page: page.trim().parse().ok()?, total: total.trim().parse().ok()? })
                        });
                        pagination.filter(|page| !p.hide_single || page.total > 1).map(|page| {
                            format!("{} / {}", page.page, page.total)
                        }).unwrap_or_default()
                    }
                    b => input.text.get(&b).cloned().unwrap_or_default(),
                };
                if c.binding == Some(Binding::SelectedBlurb) && value.starts_with("Developer:") {
                    if let Some(at) = value.find("\n\n") {
                        value = value[at + 2..].into();
                    }
                }
                if c.binding == Some(Binding::SelectedBlurb) && !input.detail_lines.is_empty() {
                    self.ink_details(c, &input.detail_lines)?;
                } else if matches!(
                    c.binding,
                    Some(Binding::SelectedMetadata | Binding::SelectedFacts)
                ) && (p.label.is_some()
                    || p.value.is_some()
                    || c.binding == Some(Binding::SelectedFacts))
                {
                    self.ink_metadata(c, input, &value)?;
                } else {
                    let ty = self.ink_type(c, false);
                    self.ink_text_impl(
                        &c.id,
                        c.rect,
                        &ty,
                        &value,
                        c.max_lines.unwrap_or(1),
                        c.align.unwrap_or(Align::Left),
                        c.overflow.unwrap_or(Overflow::Wrap),
                        false,
                        c.binding == Some(Binding::GuideText),
                    )?;
                }
            }
            Kind::Image => {
                if c.binding == Some(Binding::SelectedLogo) {
                    if self.logo(c, input)? { self.ink_image_action(c, input); return Ok(()); }
                    let title = input
                        .text
                        .get(&Binding::SelectedTitle)
                        .map(String::as_str)
                        .unwrap_or("");
                    let ty = self.ink_type(c, false);
                    self.ink_text(
                        &c.id,
                        c.rect,
                        &ty,
                        title,
                        1,
                        p.anchor.unwrap_or(Align::Center),
                        Overflow::Ellipsis,
                        true,
                    )?;
                } else {
                    let src = match c.binding {
                        Some(Binding::SelectedHero) => input.art.selected_hero.as_ref(),
                        Some(Binding::NextCover) => input.art.next_cover.as_ref(),
                        _ => input.art.selected_cover.as_ref(),
                    };
                    if let Some(source) = src {
                        let mut image_c = c.clone();
                        if c.binding != Some(Binding::SelectedHero) {
                            image_c.fit = Some(if self.theme.pack().alignment.as_ref().unwrap().cover_shape == "square" {crate::theme::pack::Fit::Cover} else {crate::theme::pack::Fit::Contain});
                        }
                        let image = self.image_pixels(c.rect, source, &image_c)?;
                        self.draw_image(c.rect, &image);
                        if let Some(b) = &p.border {
                            self.ink_border(
                                c.rect,
                                b.color,
                                b.width,
                                c.radius.unwrap_or(0),
                                b.alpha,
                            );
                        }
                    }
                }
                self.ink_image_action(c, input);
            }
            Kind::AssetImage => {
                if let Some(action) = &p.action_content {
                    self.ink_surface(c.rect, c, Some(action.fill), false, None);
                    let mut copy = c.clone();
                    copy.role = Some(action.role);
                    copy.paint.as_mut().unwrap().run = None;
                    let mut ty = self.ink_type(&copy, false);
                    ty.color = action.color;
                    let label = action
                        .key
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .map_or(action.label.clone(), |k| format!("{k}  {}", action.label));
                    self.ink_text(
                        &c.id,
                        c.rect,
                        &ty,
                        &label,
                        1,
                        Align::Center,
                        Overflow::Ellipsis,
                        true,
                    )?;
                } else if let Some(image) = self
                    .theme
                    .base()
                    .0
                    .prepared_assets
                    .get(&input.screen)
                    .and_then(|a| a.get(&c.id))
                {
                    super::ink_blit::bitmap(&mut self.pixels, c.rect.0 as i32, c.rect.1 as i32, image.as_ref());
                }
                self.ink_image_action(c, input);
            }
            Kind::Entries | Kind::SettingsRows => {
                let rows = input
                    .rows
                    .get(&c.binding.unwrap())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if rows.is_empty() && p.collapse_empty {
                    return Ok(());
                }
                for (i, row) in rows
                    .iter()
                    .take(c.visible_count.unwrap_or(16) as usize)
                    .enumerate()
                {
                    if c.layout == Some(Arrangement::Shelf) {
                        self.ink_shelf(c, row, i, input, 1.0)?;
                    } else {
                        let r = Rect(
                            c.rect.0,
                            c.rect.1 + i as u32 * (c.item_extent.unwrap() + c.gap.unwrap()),
                            c.rect.2,
                            c.item_extent.unwrap(),
                        );
                        self.ink_row(c, row, r, input, false)?;
                    }
                }
                if c.layout == Some(Arrangement::Shelf) && p.peek.is_some() && !rows.is_empty() {
                    let source = input
                        .art
                        .next_cover
                        .as_ref()
                        .or_else(|| input.art.entries.get(&rows.last().unwrap().id));
                    if let Some(source) = source {
                        let row = Row {
                            subtitle: String::new(),
                            id: "ink-preview".into(),
                            label: String::new(),
                            value: String::new(),
                            enabled: false,
                        };
                        let mut model = input.clone();
                        let last = rows.last().and_then(|r| input.art.entries.get(&r.id));
                        let end = last.is_some_and(|last| Arc::ptr_eq(last, source));
                        model.art.entries.insert(row.id.clone(), source.clone());
                        self.ink_shelf(
                            c,
                            &row,
                            rows.len(),
                            &model,
                            p.peek.as_ref().unwrap().opacity.unwrap_or(0.62)
                                * if end { 0.35 } else { 1.0 },
                        )?;
                    }
                }
            }
            Kind::DialogActions => {
                let rows = input
                    .rows
                    .get(&Binding::Actions)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let n = rows.len() as u32;
                if n == 0 {
                    return Ok(());
                }
                ensure!(n <= 4, "Too many ink actions");
                let gap = p.action_gap.unwrap_or(8);
                let vertical = c.layout == Some(Arrangement::Vertical);
                let axis = if vertical { c.rect.3 } else { c.rect.2 };
                let extent = axis.saturating_sub(gap * (n - 1)) / n;
                ensure!(extent > 0, "Ink action cells exceed component");
                let slack = axis.saturating_sub(extent * n + gap * (n - 1));
                let offset = match c.align.unwrap_or(Align::Center) {
                    Align::Left => 0,
                    Align::Center => slack / 2,
                    Align::Right => slack,
                };
                for (i, row) in rows.iter().enumerate() {
                    let d = offset + i as u32 * (extent + gap);
                    let rect = if vertical {
                        Rect(c.rect.0, c.rect.1 + d, c.rect.2, extent)
                    } else {
                        Rect(c.rect.0 + d, c.rect.1, extent, c.rect.3)
                    };
                    self.ink_row(c, row, rect, input, true)?;
                }
            }
            Kind::Navigation => {
                if c.layout == Some(Arrangement::Hidden) {
                    return Ok(());
                }
                if let Some(rows) = input.navigation.get(&c.group.unwrap()) {
                    let rows: Vec<_> = if c.layout == Some(Arrangement::Current) {
                        rows.iter()
                            .filter(|r| input.selected.get(&Binding::Navigation) == Some(&r.id))
                            .collect()
                    } else {
                        rows.iter().collect()
                    };
                    for (i, row) in rows
                        .into_iter()
                        .take(c.max_items.unwrap() as usize)
                        .enumerate()
                    {
                        let d = i as u32 * (c.item_extent.unwrap() + c.gap.unwrap());
                        let r = if c.layout == Some(Arrangement::Rail) {
                            Rect(c.rect.0, c.rect.1 + d, c.rect.2, c.item_extent.unwrap())
                        } else {
                            Rect(c.rect.0 + d, c.rect.1, c.item_extent.unwrap(), c.rect.3)
                        };
                        self.ink_row(c, row, r, input, false)?;
                    }
                }
            }
            Kind::Keyboard => self.ink_keyboard(c, input)?,
            Kind::Status => self.ink_status(c, input)?,
            Kind::Hints => self.ink_hints(c, input)?,
            Kind::Progress => {
                self.ink_surface(c.rect, c, Some(ColorRole::Border), false, None);
                if let Some(progress) = input.progress {
                    let width = (c.rect.2 as f32 * progress).round() as u32;
                    if width > 0 {
                        self.panel(
                            Rect(c.rect.0, c.rect.1, width, c.rect.3),
                            c.color.unwrap_or(ColorRole::Accent),
                            c.radius.unwrap_or(0),
                        );
                    }
                }
            }
        }
        Ok(())
    }
    fn ink_row(
        &mut self,
        c: &Component,
        row: &Row,
        r: Rect,
        input: &FrameInputs,
        action: bool,
    ) -> Result<()> {
        let p = c.paint.as_ref().unwrap();
        let focused = input.focus.component == c.id && input.focus.item.as_ref() == Some(&row.id);
        let active =
            c.kind == Kind::Navigation && input.selected.get(&Binding::Navigation) == Some(&row.id);
        let fill = if focused && c.focus_style == Some(FocusStyle::Fill) {
            c.focus_color
        } else if active {
            p.active_fill
        } else {
            c.frame_color.or(if c.kind == Kind::Navigation || action && p.action_style.is_none() && p.primary_style.is_none() {
                Some(ColorRole::Surface)
            } else {
                None
            })
        };
        let mut surface = if action {
            if row.id == "play" || row.id == "launch" {
                p.primary_style
                    .clone()
                    .or(p.action_style.clone())
                    .unwrap_or_default()
            } else {
                p.action_style.clone().unwrap_or_default()
            }
        } else {
            Surface::default()
        };
        if focused && action {
            if let Some(s) = &p.action_focus_style {
                if s.fill.is_some() {
                    surface.fill = s.fill;
                    surface.gradient = None;
                }
                surface.text = s.text.or(surface.text);
                surface.border = s.border.clone().or(surface.border);
                surface.radius = s.radius.or(surface.radius);
                surface.gradient = s.gradient.clone().or(surface.gradient);
            }
        }
        let mut chrome = c.clone();
        if focused && c.focus_style == Some(FocusStyle::Fill) {
            chrome.paint.as_mut().unwrap().gradient = None;
        }
        self.ink_surface(
            r,
            &chrome,
            fill,
            focused,
            if action { Some(&surface) } else { None },
        );
        self.ink_focus(c, r, focused);
        if active {
            let color = p
                .active_marker_color
                .or(p.active_fill)
                .unwrap_or(ColorRole::Focus);
            match p.active_marker.as_deref() {
                Some("underline") => self.panel(
                    Rect(r.0 + 8, r.1 + r.3 - 2, r.2.saturating_sub(16), 2),
                    color,
                    0,
                ),
                Some("dot") => self.panel(Rect(r.0 + r.2 / 2 - 2, r.1 + r.3 - 6, 4, 4), color, 2),
                Some("pill") => self.panel(r, color, r.3 / 2),
                _ => {}
            }
        }
        let mut action_component = c.clone();
        if action {
            if let Some(role) = p.action_font_role {
                action_component.role = Some(role);
                action_component.paint.as_mut().unwrap().run = None;
            }
        }
        let mut ty = self.ink_type(&action_component, focused);
        if action {
            if let Some(color) = surface.text {
                ty.color = color;
            }
        } else if active {
            if let Some(color) = p.active_text {
                ty.color = color;
            }
        }
        if c.kind == Kind::Navigation && !p.icons.is_empty() {
            if let Some(index) = input
                .navigation
                .get(&c.group.unwrap())
                .and_then(|rows| rows.iter().position(|r| r.id == row.id))
            {
                if let Some(icon) = p.icons.get(index) {
                    self.ink_nav_icon(r, icon, p.icon_ink_height.unwrap_or(18), ty.color);
                }
            }
            if row.enabled {
                self.targets.push(Target {
                    component: c.id.clone(),
                    item: row.id.clone(),
                    rect: r,
                });
            }
            return Ok(());
        }
        let inset = c
            .padding
            .unwrap_or(0)
            .max(self.theme.pack().alignment.as_ref().unwrap().row_inset)
            .min(r.2 / 2);
        let align = if action || c.kind == Kind::Navigation && c.layout != Some(Arrangement::Rail) {
            Align::Center
        } else {
            p.text_align.unwrap_or(Align::Left)
        };
        let rect = Rect(r.0 + inset, r.1, r.2.saturating_sub(2 * inset).max(1), r.3);
        let sub = p.subtitle.as_ref().filter(|_| !row.subtitle.is_empty()).map(|run| self.ink_subtype(c, run));
        let mut lines = vec![(&row.label, ty.clone())];
        if let Some(mut st) = sub {
            if focused { st.color = p.subtitle.as_ref().and_then(|r| r.focus_color).or(p.focus_text).unwrap_or(if c.focus_style == Some(FocusStyle::Fill) { ColorRole::FocusText } else { st.color }); }
            lines.push((&row.subtitle, st));
        }
        let mut relative = Vec::new();
        let mut y = 0.0;
        for (text, style) in &lines { relative.push(y + style.ink_bounds(text).0); y += style.line as f32; }
        let top = lines.iter().enumerate().map(|(i,(text,t))| relative[i] - if symbol(text) { t.ink_bounds(text).0 } else { t.cap() }).fold(f32::INFINITY, f32::min);
        let bottom = lines.iter().enumerate().map(|(i,(text,t))| relative[i] + if symbol(text) { t.ink_bounds(text).1 } else { 0.0 }).fold(f32::NEG_INFINITY, f32::max);
        let shift = r.1 as f32 + (r.3 as f32 - (bottom-top))/2.0 - top;
        let baselines: Vec<_> = relative.iter().map(|b| (b+shift).round()).collect();
        let mut vt = self.ink_type(&action_component, false); vt.color = ty.color; vt.size *= 0.82;
        let value_width = if row.value.is_empty() { 0.0 } else { vt.metrics(&vt.value(&row.value)).0.ceil().min(r.2 as f32 * 0.38) };
        let copy_width = (rect.2 as f32 - if value_width > 0.0 { value_width + 7.0 } else { 0.0 }).max(1.0);
        let title_ink = ty.metrics(&ty.value(&row.label));
        let title_width = copy_width.min(title_ink.2-title_ink.1);
        let vi = vt.metrics(&vt.value(&row.value));
        let value_extent = value_width.min(vi.2-vi.1);
        let centered = align == Align::Center && value_width > 0.0;
        let start = r.0 as f32 + (r.2 as f32-title_width-value_extent-7.0)/2.0;
        for (i,(text,style)) in lines.iter().enumerate() {
            let (x,width) = if centered && i==0 { (start,title_width) } else { (rect.0 as f32,if align == Align::Center { rect.2 as f32 } else { copy_width }) };
            self.ink_baseline(&c.id, Rect(x.round().max(0.0) as u32,r.1,width.max(1.0).floor() as u32,r.3),style,text,align,baselines[i])?;
        }
        if value_width > 0.0 {
            let x = if centered { start+title_width+7.0 } else { (rect.0+rect.2) as f32-value_width };
            self.ink_baseline(&c.id,Rect(x.round().max(0.0) as u32,r.1,if centered {value_extent} else {value_width}.max(1.0).floor() as u32,r.3),&vt,&row.value,if centered {Align::Center} else {Align::Right},baselines[0])?;
        }
        if row.enabled {
            self.targets.push(Target {
                component: c.id.clone(),
                item: row.id.clone(),
                rect: r,
            });
        }
        Ok(())
    }
    fn ink_shelf_tile(
        &mut self,
        c: &Component,
        row: &Row,
        input: &FrameInputs,
        focused: bool,
        preview: bool,
        aw: u32,
        ah: u32,
        pad: u32,
        scale: f32,
    ) -> Result<Arc<Pixmap>> {
        let p = c.paint.as_ref().unwrap();
        let weak = self.theme.weak();
        let source = input.art.entries.get(&row.id);
        let key = format!("{:?}:{}:{}:{}:{}:{}", self.screen, c.id, c.rect.2, c.rect.3, focused, preview);
        if !super::ink_blit::reference() {
            let cached = SHELVES.with(|cache| {
                let mut cache = cache.borrow_mut();
                let index = cache.iter().position(|entry| {
                    entry.theme.ptr_eq(&weak)
                        && entry.theme.strong_count() > 0
                        && entry.key == key
                        && match (&entry.source, source) {
                            (Some(a), Some(b)) => a.upgrade().is_some_and(|a| Arc::ptr_eq(&a, b)),
                            (None, None) => true,
                            _ => false,
                        }
                })?;
                let entry = cache.remove(index).unwrap();
                let pixels = entry.pixels.clone();
                cache.push_front(entry);
                Some(pixels)
            });
            if let Some(pixels) = cached {
                return Ok(pixels);
            }
        }
        let tile_height = ah.saturating_add(pad.saturating_mul(2));
        let mut tile = ink_pixmap(c.item_extent.unwrap(), tile_height)?;
        let old = std::mem::replace(&mut self.pixels, tile);
        let case = Rect(0, 0, c.item_extent.unwrap(), ah + 2 * pad);
        let mut copy = c.clone();
        copy.rect = case;
        self.ink_surface(case, &copy, c.frame_color, focused, None);
        if let Some(source) = input.art.entries.get(&row.id) {
            let rect = Rect(pad, pad, aw, ah);
            let mut image_c = c.clone();
            if preview {
                image_c.blur_radius = p.peek.as_ref().and_then(|p| p.blur);
            }
            image_c.fit = Some(
                if self.theme.pack().alignment.as_ref().unwrap().cover_shape == "square" {
                    crate::theme::pack::Fit::Cover
                } else {
                    crate::theme::pack::Fit::Contain
                },
            );
            let image = self.image_pixels_with_radius(
                rect,
                source,
                &image_c,
                c.radius.unwrap_or(0).saturating_sub(pad),
            )?;
            self.draw_image(rect, &image);
        }
        self.ink_focus(c, case, focused);
        tile = std::mem::replace(&mut self.pixels, old);
        let dw = (tile.width() as f32 * scale).round() as u32;
        let dh = (tile.height() as f32 * scale).round() as u32;
        if scale != 1.0 {
            check_ink_dimensions(dw, dh)?;
            tile =
                crate::theme::runtime::fit_image(&tile, dw, dh, crate::theme::pack::Fit::Contain)?;
        }

        let pixels = Arc::new(tile);
        if !super::ink_blit::reference() {
            SHELVES.with(|cache| {
                let mut cache = cache.borrow_mut();
                while !cache.is_empty()
                    && (cache.len() >= 32
                        || cache.iter().map(|e| e.pixels.data().len()).sum::<usize>()
                            + pixels.data().len()
                            > 2 * 1024 * 1024)
                {
                    cache.pop_back();
                }
                cache.push_front(CachedShelf {
                    theme: weak,
                    source: source.map(Arc::downgrade),
                    key,
                    pixels: pixels.clone(),
                });
            });
        }
        Ok(pixels)
    }
    fn ink_shelf(
        &mut self,
        c: &Component,
        row: &Row,
        index: usize,
        input: &FrameInputs,
        opacity: f32,
    ) -> Result<()> {
        let p = c.paint.as_ref().unwrap();
        let pad = c.padding.unwrap();
        let (aw, ah) = p
            .art
            .as_ref()
            .map(|a| (a.width, a.height))
            .unwrap_or((c.art_size.unwrap(), c.art_size.unwrap()));
        let x = c.rect.0 + index as u32 * (c.item_extent.unwrap() + c.gap.unwrap());
        let right = (c.rect.0 + c.rect.2).min(640);
        if x >= right {
            return Ok(());
        }
        let preview = opacity < 1.0;
        let width = if preview {
            p.peek
                .as_ref()
                .map_or(right - x, |p| p.width)
                .min(right - x)
        } else {
            c.item_extent.unwrap()
        };
        let focused =
            !preview && input.focus.component == c.id && input.focus.item.as_ref() == Some(&row.id);
        let scale = if focused {
            p.focus_scale.unwrap_or(1.0)
        } else {
            1.0
        };
        let tile = self.ink_shelf_tile(c, row, input, focused, preview, aw, ah, pad, scale)?;
        let dw = tile.width();
        let dh = tile.height();
        let atx = x as i32 - (dw as i32 - c.item_extent.unwrap() as i32) / 2;
        let aty = c.rect.1 as i32 - (dh as i32 - (ah + 2 * pad) as i32) / 2;
        if opacity == 1.0 && !super::ink_blit::reference() {
            super::ink_blit::draw_clipped(
                &mut self.pixels.as_mut(),
                atx,
                aty,
                tile.as_ref().as_ref(),
                (c.rect.0, c.rect.1, c.rect.2, c.rect.3),
            );
        } else {
            let mut mask = tiny_skia::Mask::new(640, 480).unwrap();
            let path = tiny_skia::PathBuilder::from_rect(
                tiny_skia::Rect::from_xywh(
                    c.rect.0 as f32,
                    c.rect.1 as f32,
                    c.rect.2 as f32,
                    c.rect.3 as f32,
                )
                .unwrap(),
            );
            mask.fill_path(
                &path,
                tiny_skia::FillRule::Winding,
                false,
                Transform::identity(),
            );
            if preview {
                for y in 0..480usize {
                    let a = y * 640 + (x + width) as usize;
                    let b = (y + 1) * 640;
                    mask.data_mut()[a..b].fill(0);
                }
            }
            self.pixels.draw_pixmap(
                atx,
                aty,
                tile.as_ref().as_ref(),
                &PixmapPaint {
                    opacity,
                    ..Default::default()
                },
                Transform::identity(),
                Some(&mask),
            );
        }
        let show = !preview
            && p.caption_mode.as_deref() != Some("none")
            && (p.caption_mode.as_deref() != Some("focused") || focused);
        if show {
            let ty = self.ink_type(c, focused);
            let cy = c.rect.1 + pad + ah;
            let h = c.caption_height.unwrap_or(ty.line);
            let base_width = aw;
            let full = p.caption_mode.as_deref() == Some("focused") && focused;
            let width = if full {
                (ty.metrics(&ty.value(&row.label)).0.ceil() as u32 + 2)
                    .max(base_width)
                    .min(c.rect.2)
            } else {
                (base_width as f32 * scale).round() as u32
            };
            let width = width.min(right.saturating_sub(c.rect.0));
            let cx = (x + pad + aw / 2)
                .saturating_sub(width / 2)
                .max(c.rect.0)
                .min(right.saturating_sub(width).max(c.rect.0));
            let line_y = cy + h.saturating_sub(c.caption_lines.unwrap_or(1) * ty.line);
            self.ink_text(
                &c.id,
                Rect(cx, line_y, width, h.min(c.rect.1 + c.rect.3 - line_y)),
                &ty,
                &row.label,
                c.caption_lines.unwrap_or(1),
                Align::Center,
                if c.caption_lines == Some(1) {
                    Overflow::Ellipsis
                } else {
                    Overflow::Wrap
                },
                false,
            )?;
        }
        if !preview && row.enabled {
            self.targets.push(Target {
                component: c.id.clone(),
                item: row.id.clone(),
                rect: Rect(x, c.rect.1, c.item_extent.unwrap(), c.rect.3),
            });
        }
        Ok(())
    }
    fn ink_nav_icon(&mut self, r: Rect, icon: &str, height: u32, color: ColorRole) {
        let mut b = tiny_skia::PathBuilder::new();
        let bounds = match icon {
            "apps" => {
                for (x,y) in [(4.0,4.0),(14.0,4.0),(4.0,14.0),(14.0,14.0)] { b.push_rect(tiny_skia::Rect::from_xywh(x,y,6.0,6.0).unwrap()); }
                (3.0,3.0,18.0,18.0)
            }
            "library" => {
                for x in [4.0,10.0] {b.push_rect(tiny_skia::Rect::from_xywh(x,5.0,5.0,15.0).unwrap());}
                b.move_to(16.0,5.0);b.line_to(20.0,4.0);b.line_to(23.0,19.0);b.line_to(19.0,20.0);b.close();
                (3.0,3.0,21.0,18.0)
            }
            "recents" => {
                b.move_to(12.0,4.0);
                b.cubic_to(16.418278,4.0,20.0,7.581722,20.0,12.0);
                b.cubic_to(20.0,16.418278,16.418278,20.0,12.0,20.0);
                b.cubic_to(7.581722,20.0,4.0,16.418278,4.0,12.0);
                b.move_to(4.0,4.0);b.line_to(4.0,10.0);b.line_to(10.0,10.0);
                b.move_to(12.0,7.0);b.line_to(12.0,12.0);b.line_to(16.0,14.0);
                (3.0,3.0,18.0,18.0)
            }
            "settings" => {
                b.move_to(12.0,3.0);
                for (x,y) in [(15.0,6.0),(19.0,5.0),(20.0,9.0),(23.0,12.0),(20.0,15.0),(19.0,19.0),(15.0,18.0),(12.0,21.0),(9.0,18.0),(5.0,19.0),(4.0,15.0),(1.0,12.0),(4.0,9.0),(5.0,5.0),(9.0,6.0)] {b.line_to(x,y);}b.close();
                b.push_circle(12.0,12.0,4.0);
                (0.0,2.0,24.0,20.0)
            }
            _ => {
                b.push_circle(10.0,10.0,7.0);b.move_to(15.0,15.0);b.line_to(22.0,22.0);
                (2.0,2.0,21.0,21.0)
            }
        };
        if let Some(path) = b.finish() {
            let scale = height.min(48) as f32 / bounds.3;
            let width = (bounds.2 * scale).round();
            let transform = Transform::from_scale(width/bounds.2, scale).post_translate(
                (r.0 as f32 + (r.2 as f32-width)/2.0).round() - bounds.0*width/bounds.2,
                (r.1 as f32 + (r.3 as f32-height as f32)/2.0).round() - bounds.1*scale,
            );
            self.pixels.stroke_path(
                &path,
                &tiny_skia::Paint {
                    shader: tiny_skia::Shader::SolidColor(self.color(color).into()),
                    anti_alias: true,
                    ..Default::default()
                },
                &tiny_skia::Stroke {
                    width: 2.0,
                    line_cap: tiny_skia::LineCap::Round,
                    line_join: tiny_skia::LineJoin::Round,
                    ..Default::default()
                },
                transform,
                None,
            );
        }
    }
    fn ink_keyboard(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        anyhow::ensure!(
            input.keyboard.len() == 45,
            "Fixed ink keyboard model required"
        );
        let ty = self.ink_type(c, false);
        let gap = c.gap.unwrap();
        let cell = ty.line.saturating_add(c.padding.unwrap().saturating_mul(2));
        let width = cell
            .saturating_mul(11)
            .saturating_add(gap.saturating_mul(10));
        let height = cell.saturating_mul(5).saturating_add(gap.saturating_mul(4));
        anyhow::ensure!(
            width <= c.rect.2 && height <= c.rect.3,
            "Ink keyboard exceeds bounds"
        );
        let x = c.rect.0 + (c.rect.2 - width) / 2;
        let y = c.rect.1 + (c.rect.3 - height) / 2;
        for row in 0..5u32 {
            for col in 0..if row == 4 { 1 } else { 11 } {
                let index = if row == 4 { 44 } else { row * 11 + col };
                let rect = Rect(
                    x + col * (cell + gap),
                    y + row * (cell + gap),
                    if row == 4 { width } else { cell },
                    cell,
                );
                let focused = input.keyboard_cursor == (col as usize, row as usize);
                self.ink_surface(
                    rect,
                    c,
                    Some(if focused {
                        ColorRole::Focus
                    } else {
                        ColorRole::Surface
                    }),
                    focused,
                    None,
                );
                if focused {
                    self.ink_border(rect, ColorRole::Focus, 2, c.radius.unwrap_or(0), 1.0);
                }
                let mut ty = ty.clone();
                if focused {
                    ty.color = ColorRole::FocusText;
                }
                self.ink_text(
                    &c.id,
                    rect,
                    &ty,
                    input
                        .keyboard
                        .get(index as usize)
                        .ok_or_else(|| anyhow::anyhow!("Missing ink keyboard key {index}"))?,
                    1,
                    Align::Center,
                    Overflow::Ellipsis,
                    true,
                )?;
                self.targets.push(Target {
                    component: c.id.clone(),
                    item: format!("key-{row}-{col}"),
                    rect,
                });
            }
        }
        Ok(())
    }
    fn ink_status(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        // Fixed-width status icons may exceed a valid component's available space.
        // Keep absolute coordinates to preserve path rasterization, and copy back only retained bounds.
        let mut local = ink_pixmap(c.rect.0 + c.rect.2, c.rect.1 + c.rect.3)?;
        let stride = self.pixels.width() as usize * 4;
        let local_stride = local.width() as usize * 4;
        let row_bytes = c.rect.2 as usize * 4;
        for y in c.rect.1..c.rect.1 + c.rect.3 {
            let at = y as usize * stride + c.rect.0 as usize * 4;
            let to = y as usize * local_stride + c.rect.0 as usize * 4;
            local.data_mut()[to..to + row_bytes]
                .copy_from_slice(&self.pixels.data()[at..at + row_bytes]);
        }
        let framebuffer = std::mem::replace(&mut self.pixels, local);
        let result = self.ink_status_contents(c, input);
        let local = std::mem::replace(&mut self.pixels, framebuffer);
        for y in c.rect.1..c.rect.1 + c.rect.3 {
            let at = y as usize * stride + c.rect.0 as usize * 4;
            let from = y as usize * local_stride + c.rect.0 as usize * 4;
            self.pixels.data_mut()[at..at + row_bytes]
                .copy_from_slice(&local.data()[from..from + row_bytes]);
        }
        result
    }
    fn ink_status_contents(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let ty = self.ink_type(c, false);
        let gap = self.theme.pack().alignment.as_ref().unwrap().status_gap;
        let color = ty.color;
        let clock = input.status.clock.as_deref().unwrap_or("");
        let clock_w = if clock.is_empty() {
            0
        } else {
            ty.metrics(&ty.value(clock)).0.ceil().max(0.0) as u32 + 2
        };
        if c.layout == Some(Arrangement::Horizontal) {
            let wifi = u32::from(input.status.network.is_some()) * 24;
            let bat = u32::from(input.status.battery.is_some()) * 26;
            let percentage = if c.battery_style == Some(BatteryStyle::Icon) {
                String::new()
            } else {
                input
                    .status
                    .battery
                    .map(|v| format!("{v}%"))
                    .unwrap_or_default()
            };
            let pw = ty.metrics(&percentage).0.ceil() as u32;
            let widths = [wifi, bat, pw, clock_w];
            let count = widths.iter().filter(|w| **w > 0).count() as u32;
            let used = widths.iter().sum::<u32>() + count.saturating_sub(1) * gap;
            let mut x = if c.align == Some(Align::Right) {
                c.rect.0 + c.rect.2 - used.min(c.rect.2)
            } else {
                c.rect.0 + c.rect.2.saturating_sub(used) / 2
            };
            let cy = c.rect.1 + c.rect.3 / 2;
            if wifi > 0 {
                self.wifi_arcs(x, cy.saturating_sub(8), color);
                x += wifi + gap;
            }
            if bat > 0 {
                self.ink_battery(
                    Rect(x, cy.saturating_sub(6), 26, 12),
                    input.status.battery.unwrap(),
                    color,
                );
                x += bat + gap;
            }
            if pw > 0 {
                self.ink_text(
                    &c.id,
                    Rect(x, c.rect.1, pw, c.rect.3),
                    &ty,
                    &percentage,
                    1,
                    Align::Center,
                    Overflow::Ellipsis,
                    true,
                )?;
                x += pw + gap;
            }
            if !clock.is_empty() {
                let w = (c.rect.0 + c.rect.2).saturating_sub(x).max(1);
                self.ink_text(
                    &c.id,
                    Rect(x, c.rect.1, w, c.rect.3),
                    &ty,
                    clock,
                    1,
                    if c.align == Some(Align::Right) {
                        Align::Right
                    } else {
                        Align::Center
                    },
                    Overflow::Ellipsis,
                    true,
                )?;
            }
        } else {
            let mut row = 0;
            for value in [
                input.status.clock.clone(),
                input
                    .status
                    .battery
                    .map(|v| format!("{}{}%", if input.status.charging { "+" } else { "" }, v)),
                input.status.network.as_ref().map(|n| {
                    if n == "online" {
                        "Wi-Fi".into()
                    } else {
                        n.clone()
                    }
                }),
            ]
            .into_iter()
            .flatten()
            {
                let rect = Rect(c.rect.0, c.rect.1 + row * ty.line, c.rect.2, ty.line);
                self.ink_text(
                    &c.id,
                    rect,
                    &ty,
                    &value,
                    1,
                    Align::Right,
                    Overflow::Ellipsis,
                    true,
                )?;
                row += 1;
                if row >= c.max_lines.unwrap() {
                    break;
                }
            }
        }
        Ok(())
    }
    fn ink_battery(&mut self, r: Rect, percent: u8, color: ColorRole) {
        self.ink_border(Rect(r.0, r.1, 24, r.3), color, 1, 2, 1.0);
        self.panel(Rect(r.0 + 24, r.1 + 3, 2, 6), color, 0);
        let width = 20 * u32::from(percent) / 100;
        if width > 0 {
            self.panel(Rect(r.0 + 2, r.1 + 2, width, 8), color, 0);
        }
    }
    fn ink_hints(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let ty = self.ink_type(c, false);
        let mut widths = vec![];
        for hint in &input.hints {
            let badge = if matches!(hint.button.as_str(), "A" | "B" | "X" | "Y") {
                20
            } else {
                ty.metrics(&ty.value(&hint.button)).0.ceil() as u32 + 8
            };
            widths.push((badge, ty.metrics(&ty.value(&hint.label)).0.ceil() as u32));
        }
        let fixed = widths.iter().map(|(b, _)| b + 6).sum::<u32>()
            + 14 * (widths.len().saturating_sub(1) as u32);
        let wanted = widths.iter().map(|(_, w)| *w).sum::<u32>();
        let ratio = if wanted > 0 {
            c.rect.2.saturating_sub(fixed) as f32 / wanted as f32
        } else {
            1.0
        };
        let mut x = c.rect.0;
        let y = c.rect.1 + c.rect.3.saturating_sub(20) / 2;
        for (hint, (badge, label)) in input.hints.iter().zip(widths) {
            if x + badge >= c.rect.0 + c.rect.2 {
                break;
            }
            let rect = Rect(x, y, badge, 20);
            self.ink_surface(rect, c, Some(ColorRole::Surface), false, None);
            self.ink_text(
                &c.id,
                rect,
                &ty,
                &hint.button,
                1,
                Align::Center,
                Overflow::Ellipsis,
                true,
            )?;
            let width = ((label as f32 * ratio.min(1.0)).floor() as u32)
                .max(1)
                .min((c.rect.0 + c.rect.2).saturating_sub(x + badge + 6).max(1));
            self.ink_text(
                &c.id,
                Rect(x + badge + 6, c.rect.1, width, c.rect.3),
                &ty,
                &hint.label,
                1,
                Align::Left,
                Overflow::Ellipsis,
                true,
            )?;
            x += badge + 6 + width + 14;
        }
        Ok(())
    }
    fn ink_metadata(&mut self, c: &Component, input: &FrameInputs, value: &str) -> Result<()> {
        let p = c.paint.as_ref().unwrap();
        let records: Vec<(String, String)> = if c.binding == Some(Binding::SelectedFacts) {
            input.facts.clone()
        } else if !input.metadata.is_empty() {
            input.metadata.clone()
        } else {
            value
                .split(" · ")
                .take(3)
                .enumerate()
                .map(|(i, v)| {
                    (
                        if i == 0 {
                            "Platform"
                        } else if i == 1 {
                            "Released"
                        } else {
                            "Info"
                        }
                        .into(),
                        v.trim_start_matches("Released ").into(),
                    )
                })
                .collect()
        };
        let label = self.ink_subtype(
            c,
            &p.label.clone().unwrap_or(Run {
                color: Some(ColorRole::Muted),
                ..Default::default()
            }),
        );
        let val = self.ink_subtype(c, &p.value.clone().unwrap_or_default());
        let gap = c.gap.unwrap_or(0);
        let stacked = c.layout == Some(Arrangement::Stacked);
        let reserve = records
            .iter()
            .map(|(l, _)| label.metrics(&label.value(l)).0.ceil() as u32)
            .max()
            .unwrap_or(0)
            + p.value_gap.unwrap_or(if gap > 0 { gap } else { 12 });
        let mut y = c.rect.1;
        for (l, v) in records {
            if y + val.line > c.rect.1 + c.rect.3 {
                break;
            }
            if stacked {
                self.ink_text(
                    &c.id,
                    Rect(c.rect.0, y, c.rect.2, label.line),
                    &label,
                    &l,
                    1,
                    Align::Left,
                    Overflow::Ellipsis,
                    false,
                )?;
                y += label.line + gap;
                if y + val.line > c.rect.1 + c.rect.3 {
                    break;
                }
                self.ink_text(
                    &c.id,
                    Rect(c.rect.0, y, c.rect.2, val.line),
                    &val,
                    &v,
                    1,
                    Align::Left,
                    Overflow::Ellipsis,
                    false,
                )?;
                y += val.line + gap;
            } else {
                let reserve = reserve.min(c.rect.2.saturating_sub(20));
                self.ink_text(
                    &c.id,
                    Rect(c.rect.0, y, reserve.max(1), label.line),
                    &label,
                    &l,
                    1,
                    Align::Left,
                    Overflow::Ellipsis,
                    false,
                )?;
                self.ink_text(
                    &c.id,
                    Rect(c.rect.0 + reserve, y, (c.rect.2 - reserve).max(1), val.line),
                    &val,
                    &v,
                    1,
                    Align::Left,
                    Overflow::Ellipsis,
                    false,
                )?;
                y += val.line.max(label.line) + gap;
            }
        }
        Ok(())
    }
}
fn primary_binding(screen: ScreenId) -> Option<Binding> {
    Some(match screen {
        ScreenId::SettingsRoot | ScreenId::SettingsPage => Binding::SettingsRows,
        ScreenId::DiscPicker => Binding::Discs,
        ScreenId::NetworkDialog => Binding::StorageEntries,
        ScreenId::GuideSelector => Binding::Guides,
        ScreenId::Library
        | ScreenId::Recents
        | ScreenId::SearchResults
        | ScreenId::Apps
        | ScreenId::ContextMenu
        | ScreenId::IngameMenu
        | ScreenId::GameDetail => Binding::Entries,
        _ => return None,
    })
}
fn raster(pix: &mut Pixmap, ty: &Type, text: &str, x: f32, baseline: f32, color: Color) {
    let mut x = x;
    let mut prev = None;
    for ch in text.chars() {
        let font = ty.face(ch);
        let glyph = font.glyph(ch).scaled(Scale::uniform(ty.size as f32));
        if let Some(id) = prev {
            x += font.pair_kerning(Scale::uniform(ty.size as f32), id, glyph.id());
        }
        prev = Some(glyph.id());
        let advance = glyph.h_metrics().advance_width;
        let glyph = glyph.positioned(point(x, baseline));
        if let Some(bb) = glyph.pixel_bounding_box() {
            glyph.draw(|gx, gy, coverage| {
                let xx = bb.min.x + gx as i32;
                let yy = bb.min.y + gy as i32;
                blend(pix, xx, yy, color, coverage);
            });
        }
        x += advance + ty.tracking;
    }
}
fn blend(pix: &mut Pixmap, x: i32, y: i32, color: Color, coverage: f32) {
    if x < 0 || y < 0 || x >= pix.width() as i32 || y >= pix.height() as i32 {
        return;
    }
    let a = coverage * color.a() as f32 / 255.0;
    let at = ((y as u32 * pix.width() + x as u32) * 4) as usize;
    let data = &mut pix.data_mut()[at..at + 4];
    for (i, c) in [color.r(), color.g(), color.b()].into_iter().enumerate() {
        data[i] = (c as f32 * a + data[i] as f32 * (1.0 - a)).round() as u8;
    }
    data[3] = (255.0 * a + data[3] as f32 * (1.0 - a)).round() as u8;
}

#[cfg(test)]
thread_local! { static GUIDE_GLYPH_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; static GUIDE_SOURCE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; static LAYOUT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
mod tests {
    use super::*;
    fn previous_guide_line_len(layout: &InkTextLayout, value: &str, width: u32) -> usize {
        let end = value.find('\n').unwrap_or(value.len());
        let paragraph = &value[..end];
        let mut fit = 0;
        let mut word = None;
        for (at, ch) in paragraph.char_indices() {
            let next = at + ch.len_utf8();
            if layout.metrics(&layout.value(&paragraph[..next])).0 > width as f32 && fit > 0 {
                return word.filter(|n| *n > 0).unwrap_or(fit);
            }
            fit = next;
            if ch.is_whitespace() {
                word = Some(next);
            }
        }
        end + usize::from(end < value.len())
    }

    #[test]
    fn themes4_incremental_guide_breaks_match_prefix_metrics_in_eight_packs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let sample =
            "AV To WWW iii é è Á ß Straße ﬃ Ελληνικά 日本語 中國  two spaces.\n\nlast line";
        for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            for suffix in ["", "-tall"] {
                let id = format!("{base}{suffix}");
                let theme = ThemeHandle::prepare(
                    ThemeSnapshot::load_with_settings(
                        &root.join(format!("themes/{base}")),
                        &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty() { "square" } else { "tall" }.into())]),
                    ).unwrap(),
                    &bank,
                    1,
                )
                .unwrap();
                let c = theme.pack().screens[&ScreenId::TextReader]
                    .components
                    .iter()
                    .find(|c| c.binding == Some(Binding::GuideText))
                    .unwrap();
                let original = ink_text_layout(&theme, c, false);
                for (upper, tracking) in [
                    (original.upper, original.tracking),
                    (true, 1.25),
                    (false, -0.25),
                ] {
                    let mut layout = original.clone();
                    layout.upper = upper;
                    layout.tracking = tracking;
                    for width in [0, 1, 37, c.rect.2] {
                        let mut rest = sample;
                        while !rest.is_empty() {
                            let old = previous_guide_line_len(&layout, rest, width);
                            let new = layout.guide_line_len(rest, width);
                            assert_eq!(
                                new, old,
                                "{id}, width {width}, upper {upper}, tracking {tracking}: {rest:?}"
                            );
                            assert!(new > 0);
                            rest = &rest[new..];
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn themes4_zero_advance_guide_has_linear_work_and_bounded_progressing_pages() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(
            ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(),
            &bank,
            1,
        )
        .unwrap();
        let c = theme.pack().screens[&ScreenId::TextReader]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::GuideText))
            .unwrap();
        let mut layout = ink_text_layout(&theme, c, false);
        layout.font = theme.ink_font(FontId::Roboto, 400);
        layout.tracking = 0.0;
        assert_eq!(layout.metrics("\u{200b}").0, 0.0);
        let source = "\u{200b}".repeat(100_000);
        GUIDE_SOURCE_VISITS.with(|n| n.set(0));
        GUIDE_GLYPH_VISITS.with(|n| n.set(0));
        let mut at = 0;
        let mut pages = 0;
        while at < source.len() {
            let before = GUIDE_SOURCE_VISITS.with(|n| n.get());
            let used =
                layout.guide_page_len(&source[at..], c.rect.2, c.rect.3, c.max_lines.unwrap());
            assert!(used > 0);
            let budget = layout.guide_line_count(c.rect.3, c.max_lines.unwrap()) as usize
                * InkTextLayout::GUIDE_LINE_SOURCE_CHAR_LIMIT;
            assert!(GUIDE_SOURCE_VISITS.with(|n| n.get()) - before <= budget);
            assert!(source[at..at + used].chars().count() <= budget);
            at += used;
            pages += 1;
        }
        assert_eq!(at, source.len());
        assert!(pages > 1);
        assert_eq!(GUIDE_SOURCE_VISITS.with(|n| n.get()), 100_000);
        assert_eq!(GUIDE_GLYPH_VISITS.with(|n| n.get()), 100_000);
        assert_eq!(layout.guide_line_len("密", 0), "密".len());
        assert_eq!(layout.guide_line_len("\n", 0), 1);
        assert_eq!(layout.guide_line_len("", 0), 0);
    }

    #[test]
    fn theme_port_text_cache_skips_layout_and_keys_line_count() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = crate::theme::fonts::ThemeHandle::prepare(
            crate::theme::runtime::ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(),
            &bank,
            1,
        )
        .unwrap();
        let input = FrameInputs::new(ScreenId::Apps);
        let mut frame = PreparedFrame::prepare(theme, &input).unwrap();
        let c = frame.theme.pack().screens[&ScreenId::Apps]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::Entries))
            .unwrap()
            .clone();
        let ty = frame.ink_type(&c, false);
        let rect = Rect(0, 0, 120, 100);
        let value = "A long unchanged label that wraps onto several lines";
        TEXT.with(|c| c.borrow_mut().clear());
        LAYOUT_CALLS.with(|n| n.set(0));
        let mut draw = |count| {
            frame.pixels.fill(tiny_skia::Color::TRANSPARENT);
            frame
                .ink_text(
                    "cache-proof",
                    rect,
                    &ty,
                    value,
                    count,
                    Align::Left,
                    Overflow::Wrap,
                    false,
                )
                .unwrap();
            frame.pixels.data().to_vec()
        };
        let first = draw(2);
        assert_eq!(LAYOUT_CALLS.with(|n| n.get()), 1);
        assert_eq!(first, draw(2));
        assert_eq!(
            LAYOUT_CALLS.with(|n| n.get()),
            1,
            "cache hit performed layout"
        );
        assert_ne!(first, draw(1));
        assert_eq!(
            LAYOUT_CALLS.with(|n| n.get()),
            2,
            "line count absent from cache key"
        );
    }
    #[test]
    fn theme_port_focused_caption_fits_narrow_shelf_and_keyboard_errors() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let theme = ThemeHandle::prepare(
            ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(),
            &bank,
            1,
        )
        .unwrap();
        let mut input = FrameInputs::new(ScreenId::Apps);
        let mut frame = PreparedFrame::prepare(theme, &input).unwrap();
        let mut c = frame.theme.pack().screens[&ScreenId::Library]
            .components
            .iter()
            .find(|c| c.binding == Some(Binding::Entries))
            .unwrap()
            .clone();
        c.rect = Rect(10, 100, 100, 200);
        c.item_extent = Some(100);
        c.visible_count = Some(1);
        c.padding = Some(0);
        c.gap = Some(0);
        c.art_size = Some(100);
        c.caption_height = Some(48);
        c.caption_lines = Some(1);
        c.glow_radius = Some(0);
        let p = c.paint.as_mut().unwrap();
        p.art = Some(crate::theme::ink::ArtSize {
            width: 100,
            height: 100,
        });
        p.focus_scale = Some(1.12);
        p.caption_mode = Some("all".into());
        let mut pack = frame.theme.pack().clone();
        let slot = pack
            .screens
            .get_mut(&ScreenId::Library)
            .unwrap()
            .components
            .iter_mut()
            .find(|c| c.binding == Some(Binding::Entries))
            .unwrap();
        *slot = c.clone();
        pack.validate().unwrap();
        let row = Row {
            subtitle: String::new(),
            id: "focused".into(),
            label: "Caption".into(),
            value: String::new(),
            enabled: true,
        };
        input.focus = crate::theme::frame::Focus {
            component: c.id.clone(),
            item: Some(row.id.clone()),
        };
        frame.text_boxes.clear();
        frame.ink_shelf(&c, &row, 0, &input, 1.0).unwrap();
        let caption = frame
            .text_boxes
            .iter()
            .find(|(id, _)| id == &c.id)
            .unwrap()
            .1;
        assert_eq!(caption.0, 10);
        assert_eq!(caption.2, 100);
        assert!(inside(c.rect, caption));
        assert!(frame.pixels.pixels().iter().any(|p| p.alpha() > 0));
        let keyboard = frame.theme.pack().screens[&ScreenId::Keyboard]
            .components
            .iter()
            .find(|c| c.kind == Kind::Keyboard)
            .unwrap()
            .clone();
        assert!(
            frame
                .ink_keyboard(&keyboard, &input)
                .unwrap_err()
                .to_string()
                .contains("Fixed ink keyboard model required")
        );
        let mut too_tall = Rect(0, 0, 20, 481);
        shift(&mut too_tall, -100);
        assert_eq!(too_tall.1, 0);
    }
    #[test]
    fn theme_port_real_guide_pages_fit_resolved_render_lines() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let bank = crate::theme::fonts::FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
        let source = "Wide WWW guide, narrow iii letters, é🦊 日本語 Straße and  spaces.\n\n"
            .repeat(55)
            + "last line without newline";
        for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
            for suffix in ["", "-tall"] {
                let id = format!("{base}{suffix}");
                let theme = ThemeHandle::prepare(
                    ThemeSnapshot::load_with_settings(
                        &root.join(format!("themes/{base}")),
                        &std::collections::BTreeMap::from([("cover_shape".into(), if suffix.is_empty() { "square" } else { "tall" }.into())]),
                    ).unwrap(),
                    &bank,
                    1,
                )
                .unwrap();
                let c = theme.pack().screens[&ScreenId::TextReader]
                    .components
                    .iter()
                    .find(|c| c.binding == Some(Binding::GuideText))
                    .unwrap();
                let layout = ink_text_layout(&theme, c, false);
                let count = layout.guide_line_count(c.rect.3, c.max_lines.unwrap());
                let mut at = 0;
                while at < source.len() {
                    let used = layout.guide_page_len(&source[at..], c.rect.2, c.rect.3, count);
                    assert!(used > 0);
                    let page = &source[at..at + used];
                    let lines = layout.guide_lines(page, c.rect.2, count);
                    assert!(lines.len() <= count as usize);
                    assert!(
                        lines.iter().all(|s| layout.metrics(s).0 <= c.rect.2 as f32),
                        "{id}: rendered line rewraps"
                    );
                    assert_eq!(
                        layout.guide_lines(page, c.rect.2, 16),
                        lines,
                        "{id}: page exceeds max_lines"
                    );
                    at += used;
                }
                assert_eq!(at, source.len());
            }
        }
    }
    #[test]
    fn theme_port_pixmaps_cannot_exceed_canvas() {
        for (w, h) in [(640, 481), (641, 480), (10016, 10016), (u32::MAX, u32::MAX)] {
            assert!(ink_pixmap(w, h).is_err());
        }
        assert_eq!(ink_pixmap(640, 480).unwrap().data().len(), 640 * 480 * 4);
    }
}
