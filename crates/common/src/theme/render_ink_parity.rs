use super::*;
use crate::theme::frame::DetailLine;

pub(super) fn symbol(text: &str) -> bool {
    !text.trim().is_empty() && !text.chars().any(char::is_alphanumeric)
}
impl InkTextLayout {
    fn detail_lines(&self, text: &str, width: u32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        let measured = |s: &str| self.metrics(&self.value(s)).0.ceil();
        for ch in text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            if !line.is_empty() && measured(&format!("{line}{ch}")) > width as f32 {
                if let Some(space) = line
                    .char_indices()
                    .filter(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| i)
                    .last()
                    .filter(|i| *i > 0)
                {
                    lines.push(line[..space].trim_end().into());
                    line = line[space..].trim_start().into();
                } else {
                    lines.push(std::mem::take(&mut line));
                }
                if !line.is_empty() && measured(&format!("{line}{ch}")) > width as f32 {
                    lines.push(std::mem::take(&mut line));
                }
            }
            line.push(ch);
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }
    pub(super) fn ink_bounds(&self, text: &str) -> (f32, f32) {
        let mut ascent = 0.0_f32;
        let mut descent = 0.0_f32;
        for ch in self.value(text).chars() {
            if let Some(b) = self
                .face(ch)
                .glyph(ch)
                .scaled(Scale::uniform(self.size))
                .exact_bounding_box()
            {
                ascent = ascent.max(-b.min.y);
                descent = descent.max(b.max.y);
            }
        }
        (ascent, descent)
    }
}

/// Section headings and body lines share one pagination and painting contract.
pub fn ink_detail_pages(
    theme: &ThemeHandle,
    c: &Component,
    sections: &[(Option<String>, String)],
    separate: bool,
) -> Vec<Vec<DetailLine>> {
    let body = ink_text_layout(theme, c, false);
    let heading = detail_heading(theme, c);
    let height = c
        .rect
        .3
        .saturating_sub(if separate { 0 } else { body.line });
    let limit = c
        .max_lines
        .unwrap_or(32)
        .saturating_sub(usize::from(!separate) as u32)
        .max(1) as usize;
    let fits = |lines: &[DetailLine]| {
        lines.len() <= limit && lines.iter().enumerate().map(|(i,l)| if l.heading { heading.line } else { body.line } + if i>0 {l.gap_before} else {0}).sum::<u32>() <= height && lines.iter().map(|l|l.text.chars().count()+1).sum::<usize>() <= if separate {480} else {460}
    };
    let mut pages = vec![Vec::new()];
    let mut add = |group: Vec<DetailLine>| {
        if !pages.last().unwrap().is_empty()
            && !fits(&[pages.last().unwrap().as_slice(), group.as_slice()].concat())
        {
            pages.push(Vec::new());
        }
        pages.last_mut().unwrap().extend(group);
    };
    for (title, text) in sections {
        let mut content = body
            .detail_lines(text, c.rect.2)
            .into_iter()
            .map(|text| DetailLine {
                text,
                heading: false,
                gap_before: 0,
            })
            .collect::<Vec<_>>();
        if let Some(title) = title {
            let mut group = heading
                .detail_lines(title, c.rect.2)
                .into_iter()
                .map(|text| DetailLine {
                    text,
                    heading: true,
                    gap_before: 0,
                })
                .collect::<Vec<_>>();
            if let Some(first) = group.first_mut() {
                first.gap_before = c
                    .paint
                    .as_ref()
                    .and_then(|p| p.section_style.as_ref())
                    .and_then(|s| s.gap_before)
                    .unwrap_or(0);
            }
            if !content.is_empty() {
                group.push(content.remove(0));
            }
            add(group);
        }
        for line in content {
            add(vec![line]);
        }
    }
    if pages.len() > 1 {
        let last = pages.len() - 1;
        if pages[last].len() == 1
            && pages[last][0].text.trim().chars().count() < 12
            && pages[last - 1].len() > 1
        {
            let prior = &pages[last - 1];
            let mut start = prior.len() - 1;
            while start > 0 && prior[start - 1].heading {
                start -= 1;
            }
            let moved = prior[start..].to_vec();
            if start > 0 && fits(&[moved.as_slice(), pages[last].as_slice()].concat()) {
                pages[last - 1].truncate(start);
                pages[last].splice(0..0, moved);
            }
        }
    }
    pages
}
fn detail_heading(theme: &ThemeHandle, c: &Component) -> Type {
    let mut copy = c.clone();
    let run = c
        .paint
        .as_ref()
        .and_then(|p| p.section_style.as_ref())
        .cloned()
        .unwrap_or_default();
    if let Some(role) = run.role {
        copy.role = Some(role);
        copy.paint.as_mut().unwrap().run = None;
    }
    let mut base = copy.paint.as_ref().unwrap().run.clone().unwrap_or_default();
    merge_run(&mut base, &run);
    copy.paint.as_mut().unwrap().run = Some(base);
    ink_text_layout(theme, &copy, false)
}
impl PreparedFrame {
    pub(super) fn ink_baseline(
        &mut self,
        id: &str,
        rect: Rect,
        ty: &Type,
        text: &str,
        align: Align,
        baseline: f32,
    ) -> Result<()> {
        let value = ty
            .lines(&ty.value(text), rect.2, 1, Overflow::Ellipsis)
            .pop()
            .unwrap_or_default();
        let (_, left, right) = ty.metrics(&value);
        let x = match align {
            Align::Left => -left,
            Align::Center => (rect.2 as f32 - left - right) / 2.0,
            Align::Right => rect.2 as f32 - right,
        };
        let mut pixels = ink_pixmap(rect.2, rect.3)?;
        raster(
            &mut pixels,
            ty,
            &value,
            x,
            baseline - rect.1 as f32,
            self.color(ty.color),
        );
        crate::theme::render::ink_blit::bitmap(
            &mut self.pixels,
            rect.0 as i32,
            rect.1 as i32,
            pixels.as_ref(),
        );
        self.text_boxes.push((id.into(), rect));
        Ok(())
    }
    pub(super) fn ink_details(&mut self, c: &Component, lines: &[DetailLine]) -> Result<()> {
        let body = self.ink_type(c, false);
        let heading = detail_heading(&self.theme, c);
        let mut y = c.rect.1;
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                y += line.gap_before;
            }
            let ty = if line.heading { &heading } else { &body };
            if y + ty.line > c.rect.1 + c.rect.3 {
                break;
            }
            self.ink_text(
                &c.id,
                Rect(c.rect.0, y, c.rect.2, ty.line),
                ty,
                &line.text,
                1,
                c.align.unwrap_or(Align::Left),
                Overflow::Ellipsis,
                false,
            )?;
            y += ty.line;
        }
        Ok(())
    }
    pub(super) fn ink_image_action(&mut self, c: &Component, input: &FrameInputs) {
        let p = c.paint.as_ref().unwrap();
        let Some(action) = &p.action else { return };
        let focused = input.focus.component == c.id;
        let border = if focused {
            p.action_focus_style
                .as_ref()
                .and_then(|s| s.border.as_ref())
        } else {
            None
        }
        .or_else(|| p.action_style.as_ref().and_then(|s| s.border.as_ref()))
        .or(p.border.as_ref());
        if let Some(b) = border {
            self.ink_border(c.rect, b.color, b.width, c.radius.unwrap_or(0), b.alpha);
        }
        self.targets.push(Target {
            component: c.id.clone(),
            item: action.clone(),
            rect: c.rect,
        });
    }
    pub(super) fn ink_empty_stack(&self, components: &mut [Component]) {
        let Some(title) = components
            .iter()
            .find(|c| c.binding == Some(Binding::EmptyTitle))
            .cloned()
        else {
            return;
        };
        let Some(hint) = components
            .iter()
            .find(|c| c.binding == Some(Binding::EmptyHint))
            .cloned()
        else {
            return;
        };
        let Some(panel) = components
            .iter()
            .filter(|c| {
                c.kind == Kind::Panel && inside(c.rect, title.rect) && inside(c.rect, hint.rect)
            })
            .min_by_key(|c| c.rect.2 * c.rect.3)
        else {
            return;
        };
        let Some(icon) = components
            .iter()
            .find(|c| {
                c.kind == Kind::AssetImage
                    && c.paint
                        .as_ref()
                        .is_some_and(|p| p.when.as_deref() == Some("empty") && p.action.is_none())
                    && inside(panel.rect, c.rect)
            })
            .cloned()
        else {
            return;
        };
        let Some(image) = self
            .theme
            .base()
            .0
            .prepared_assets
            .get(&self.screen)
            .and_then(|a| a.get(&icon.id))
        else {
            return;
        };
        let mut top = image.height();
        let mut bottom = 0;
        for y in 0..image.height() {
            for x in 0..image.width() {
                if image.pixels()[(y * image.width() + x) as usize].alpha() >= 32 {
                    top = top.min(y);
                    bottom = bottom.max(y + 1);
                }
            }
        }
        if bottom <= top {
            return;
        }
        let ink_top = top as f32;
        let ink_height = (bottom - top) as f32;
        let caps = [
            self.ink_type(&title, false).cap(),
            self.ink_type(&hint, false).cap(),
        ];
        let gaps = [
            (icon.rect.3 as f32 - ink_top - ink_height + 12.0).max(16.0),
            16.0,
        ];
        let height = ink_height + caps.iter().sum::<f32>() + gaps.iter().sum::<f32>();
        let mut icon_top = (panel.rect.1 as f32 + (panel.rect.3 as f32 - height) / 2.0).round();
        let mut bottom = icon_top + ink_height;
        let mut baselines = [0.0; 2];
        for i in 0..2 {
            baselines[i] = (bottom + gaps[i] + caps[i]).ceil();
            bottom = baselines[i];
        }
        let offset =
            (panel.rect.1 as f32 + panel.rect.3 as f32 / 2.0 - (icon_top + bottom) / 2.0).round();
        icon_top += offset;
        for c in components {
            if c.id == icon.id {
                c.rect.1 = (icon_top - ink_top).max(0.0).round() as u32;
            }
            for (i, text) in [&title, &hint].iter().enumerate() {
                if c.id == text.id {
                    let baseline = baselines[i] + offset;
                    c.rect.1 = (baseline - self.ink_type(text, false).baseline())
                        .max(0.0)
                        .round() as u32;
                }
            }
        }
    }
}
