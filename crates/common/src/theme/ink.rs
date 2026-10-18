//! Opt-in presentation fields emitted by the web theme exporter.
use super::pack::*;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alignment {
    pub mode: String,
    pub cover_shape: String,
    #[serde(default = "inset")]
    pub row_inset: u32,
    #[serde(default = "gap")]
    pub status_gap: u32,
    #[serde(default = "panel_padding")]
    pub panel_padding: u32,
}
fn inset() -> u32 {
    12
}
fn gap() -> u32 {
    8
}
fn panel_padding() -> u32 {
    16
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Run {
    pub font: Option<FontId>,
    pub size: Option<u32>,
    pub line_height: Option<u32>,
    pub weight: Option<u32>,
    pub letter_spacing: Option<f32>,
    pub transform: Option<String>,
    pub color: Option<ColorRole>,
    pub role: Option<Role>,
    pub gap_before: Option<u32>,
    pub focus_color: Option<ColorRole>,
}
impl Run {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.size.is_none_or(|v| (12..=48).contains(&v))
                && self.line_height.is_none_or(|v| (12..=56).contains(&v)),
            "Invalid ink typography size"
        );
        ensure!(
            self.weight.is_none_or(|v| (300..=900).contains(&v))
                && self
                    .letter_spacing
                    .is_none_or(|v| v.is_finite() && (-2.0..=6.0).contains(&v)),
            "Invalid ink typography weight/spacing"
        );
        ensure!(
            self.transform
                .as_deref()
                .is_none_or(|v| matches!(v, "none" | "uppercase")),
            "Invalid text transform"
        );
        if let (Some(size), Some(line)) = (self.size, self.line_height) {
            ensure!((size..=size + 8).contains(&line), "Invalid ink line height");
        }
        Ok(())
    }
}
pub(crate) fn merge_run(a: &mut Run, b: &Run) {
    if b.font.is_some() {
        a.font = b.font;
    }
    if b.size.is_some() {
        a.size = b.size;
    }
    if b.line_height.is_some() {
        a.line_height = b.line_height;
    }
    if b.weight.is_some() {
        a.weight = b.weight;
    }
    if b.letter_spacing.is_some() {
        a.letter_spacing = b.letter_spacing;
    }
    if b.transform.is_some() {
        a.transform = b.transform.clone();
    }
    if b.color.is_some() {
        a.color = b.color;
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Border {
    pub width: u32,
    pub color: ColorRole,
    pub alpha: f32,
    pub focus_color: Option<ColorRole>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shadow {
    pub x: i32,
    pub y: i32,
    pub blur: u32,
    pub color: ColorRole,
    pub alpha: f32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub token: ColorRole,
    pub alpha: f32,
    pub at: f32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gradient {
    pub angle: f32,
    pub stops: Vec<Stop>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Surface {
    pub fill: Option<ColorRole>,
    pub text: Option<ColorRole>,
    pub radius: Option<u32>,
    pub border: Option<Border>,
    pub shadow: Option<Shadow>,
    pub gradient: Option<Gradient>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtSize {
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peek {
    pub width: u32,
    pub opacity: Option<f32>,
    pub blur: Option<u32>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Empty {
    pub role: Option<Role>,
    pub color: Option<ColorRole>,
    pub align: Option<Align>,
    pub offset_y: u32,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Item {
    pub id: Option<String>,
    pub icon: Option<String>,
    pub button: Option<String>,
    pub label: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionContent {
    pub label: String,
    pub role: Role,
    pub fill: ColorRole,
    pub color: ColorRole,
    pub key: Option<String>,
    pub gap: Option<u32>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paint {
    pub hint_items: Option<Vec<Item>>,
    pub keyboard_layout: Option<String>,
    pub placeholder_color: Option<ColorRole>,
    pub icons: Vec<String>,
    pub run: Option<Run>,
    pub focus_run: Option<Run>,
    pub border: Option<Border>,
    pub shadow: Option<Shadow>,
    pub gradient: Option<Gradient>,
    pub opacity: Option<f32>,
    pub text_align: Option<Align>,
    pub content_right: Option<u32>,
    pub row_align: Option<String>,
    pub when: Option<String>,
    pub hide_single: bool,
    pub collapse_empty: bool,
    pub art: Option<ArtSize>,
    pub caption_mode: Option<String>,
    pub peek: Option<Peek>,
    pub focus_scale: Option<f32>,
    pub focus_text: Option<ColorRole>,
    pub active_fill: Option<ColorRole>,
    pub active_text: Option<ColorRole>,
    pub active_marker: Option<String>,
    pub active_marker_color: Option<ColorRole>,
    pub action_width: Option<String>,
    pub action_font_role: Option<Role>,
    pub action_gap: Option<u32>,
    pub action_content: Option<ActionContent>,
    pub action: Option<String>,
    pub action_style: Option<Surface>,
    pub primary_style: Option<Surface>,
    pub action_focus_style: Option<Surface>,
    pub fit_content: bool,
    pub icon_ink_height: Option<u32>,
    pub breadcrumb_icon: bool,
    pub anchor: Option<Align>,
    pub suffix: Option<String>,
    pub empty: Option<Empty>,
    pub subtitle: Option<Run>,
    pub label: Option<Run>,
    pub value: Option<Run>,
    pub value_gap: Option<u32>,
    pub value_width_percent: Option<u32>,
    pub section_style: Option<Run>,
}
impl Paint {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.value_width_percent.is_none_or(|value| (20..=90).contains(&value)), "Invalid value column width");
        ensure!(
            self.icons.len() <= 4
                && self
                    .icons
                    .iter()
                    .all(|v| matches!(v.as_str(), "recents" | "library" | "apps" | "settings")),
            "Invalid navigation glyph"
        );
        ensure!(self.keyboard_layout.as_deref().is_none_or(|v| v == "qwerty"), "Invalid keyboard layout");
        if let Some(items) = &self.hint_items {
            ensure!(items.len() <= 8, "Too many descriptor hints");
            for item in items {
                ensure!(item.id.is_none() && item.icon.is_none()
                    && item.button.as_deref().is_some_and(super::frame::semantic_button)
                    && item.label.as_ref().is_some_and(|v| v.chars().count() <= 64), "Invalid descriptor hint");
            }
        }
        let alpha = |a: f32| a.is_finite() && (0.0..=1.0).contains(&a);
        ensure!(self.opacity.is_none_or(alpha), "Invalid component opacity");
        ensure!(
            self.focus_scale
                .is_none_or(|v| v.is_finite() && (1.0..=1.12).contains(&v)),
            "Invalid focus scale"
        );
        for run in [
            &self.run,
            &self.focus_run,
            &self.subtitle,
            &self.label,
            &self.value,
            &self.section_style,
        ]
        .into_iter()
        .flatten()
        {
            run.validate()?;
        }
        for surface in [
            Some(Surface {
                border: self.border.clone(),
                shadow: self.shadow.clone(),
                gradient: self.gradient.clone(),
                ..Default::default()
            }),
            self.action_style.clone(),
            self.primary_style.clone(),
            self.action_focus_style.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(b) = surface.border {
                ensure!(
                    (1..=4).contains(&b.width) && alpha(b.alpha),
                    "Invalid border"
                );
            }
            if let Some(s) = surface.shadow {
                ensure!(
                    s.x.unsigned_abs() <= 24 && s.y.unsigned_abs() <= 24 && s.blur <= 12 && alpha(s.alpha),
                    "Invalid shadow"
                );
            }
            if let Some(g) = surface.gradient {
                ensure!(
                    g.angle.is_finite()
                        && (0.0..=360.0).contains(&g.angle)
                        && (2..=3).contains(&g.stops.len()),
                    "Invalid gradient"
                );
                for (i, s) in g.stops.iter().enumerate() {
                    ensure!(
                        alpha(s.alpha) && alpha(s.at) && (i == 0 || s.at > g.stops[i - 1].at),
                        "Invalid gradient stops"
                    );
                }
            }
        }
        ensure!(
            self.when
                .as_deref()
                .is_none_or(|v| matches!(v, "empty" | "populated")),
            "Invalid condition"
        );
        ensure!(
            self.peek.as_ref().is_none_or(|p| p.width > 0
                && p.width <= 640
                && p.opacity.is_none_or(alpha)
                && p.blur.unwrap_or(0) <= 4),
            "Invalid shelf preview"
        );
        ensure!(
            self.art
                .as_ref()
                .is_none_or(|v| v.width > 0 && v.width <= 640 && v.height > 0 && v.height <= 480),
            "Invalid portrait art"
        );
        ensure!(
            self.content_right.is_none_or(|v| (1..=640).contains(&v))
                && self.icon_ink_height.is_none_or(|v| (1..=48).contains(&v)),
            "Invalid ink anchor"
        );
        ensure!(
            self.action_gap.unwrap_or(8) <= 32 && self.value_gap.unwrap_or(12) <= 32,
            "Invalid ink gap"
        );
        Ok(())
    }
}
