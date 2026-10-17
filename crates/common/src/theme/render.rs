//! Prepared pack art primitive; hot draw performs no decode, resize or file I/O.
use super::{
    fonts::ThemeHandle,
    frame::{FrameInputs, Row},
    pack::{
        Align, Arrangement, BatteryStyle, Binding, ColorRole, Component, FadeEdge, FocusStyle,
        HintPresentation, Overflow, Rect, Role,
    },
};
use super::{
    pack::{Kind, ScreenId},
    runtime::ThemeSnapshot,
};
use crate::display::{Display, overlay::PreparedOverlay};
use crate::{
    display::{color::Color, fill_rounded_rect, font::FontTextStyle},
    geom::{Point, Rect as DrawRect},
};
use anyhow::{Result, ensure};
use tiny_skia::Pixmap;
use tiny_skia::{PixmapPaint, Transform};

#[path = "ink_blit.rs"]
mod ink_blit;
#[path = "render_retained.rs"]
mod retained;
#[path = "render_tiles.rs"]
mod tiles;
#[path = "render_details.rs"]
mod details;
pub use details::{DetailHeroWarm, DetailImageWarm, DetailTextWarm};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub component: String,
    pub item: String,
    pub rect: Rect,
}
/// One bounded precomposed frame. All glyph/art work occurs in prepare, not paint.
pub struct PreparedFrame {
    theme: ThemeHandle,
    screen: ScreenId,
    pixels: crate::display::SharedPixmap,
    opaque: bool,
    blit: super::blit::BlitMap,
    previous_input: Option<retained::Inputs>,
    component_metadata: Vec<retained::Metadata>,
    retained_layout: bool,
    tiles: tiles::Cache,
    ink_cache: ink_renderer::retained::Cache,
    damage: Vec<Rect>,
    navigation_epoch: std::sync::Arc<()>,
    ink_components: Option<(ScreenId, bool, std::sync::Arc<Vec<Component>>)>,
    pub targets: Vec<Target>,
    pub text_boxes: Vec<(String, Rect)>,
}
fn pixel_word(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[..4].try_into().unwrap())
}
pub(super) fn equal_words(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.chunks_exact(4).zip(b.chunks_exact(4))
        .all(|(a, b)| pixel_word(a) == pixel_word(b))
        && a[a.len() / 4 * 4..].iter().zip(&b[b.len() / 4 * 4..]).all(|(a, b)| a == b)
}
fn dense_damage(source: &[u8], target: &[u8], rect: Rect) -> bool {
    let area = rect.2 as usize * rect.3 as usize;
    if area < 1024 { return false; }
    let mut changed = 0;
    for sample in 0..32 {
        let x = (sample % 4 * 2 + 1) * rect.2 as usize / 8;
        let y = (sample / 4 * 2 + 1) * rect.3 as usize / 16;
        let at = ((rect.1 as usize + y) * 640 + rect.0 as usize + x) * 4;
        changed += usize::from(pixel_word(&source[at..]) != pixel_word(&target[at..]));
    }
    crate::perf_trace::count(crate::perf_trace::Count::ComparedPixels, 32);
    crate::perf_trace::count(crate::perf_trace::Count::ChangedPixels, changed as u64);
    changed >= 16
}
fn copy_damage(source: &[u8], target: &mut [u8], rect: Rect) {
    for y in rect.1..rect.1 + rect.3 {
        let at = (y as usize * 640 + rect.0 as usize) * 4;
        let end = at + rect.2 as usize * 4;
        target[at..end].copy_from_slice(&source[at..end]);
    }
    crate::perf_trace::count(crate::perf_trace::Count::CopiedPixels, rect.2 as u64 * rect.3 as u64);
}
fn changed_runs(source: &[u8], target: &[u8]) -> Vec<(usize, usize)> {
    let mut runs = vec![];
    let mut first = None;
    let mut last = 0;
    let mut changed = 0;
    for (x, (a, b)) in source
        .chunks_exact(4)
        .zip(target.chunks_exact(4))
        .enumerate()
    {
        if pixel_word(a) != pixel_word(b) {
            first.get_or_insert(x);
            last = x + 1;
            changed += 1;
        } else if x + 1 - last >= 8 {
            if let Some(first) = first.take() {
                runs.push((first, last));
            }
        }
    }
    if let Some(first) = first {
        runs.push((first, last));
    }
    crate::perf_trace::count(crate::perf_trace::Count::ChangedPixels, changed);
    runs
}

#[cfg(test)]
pub(super) fn navigation_reference(
    theme: ThemeHandle,
    input: &FrameInputs,
) -> Result<PreparedFrame> {
    ink_renderer::uncached(|| PreparedFrame::prepare(theme, input))
}

use super::render_navigation::NavigationRasterPart;
pub use ink_renderer::effects::DetailShadowWarm;
pub use super::render_navigation::{NavigationImageWarm, NavigationRasterWarm, NavigationStamp};
fn shelf_tile_rect(c: &Component, x: u32) -> Rect {
    Rect(
        x.saturating_sub(4),
        c.rect.1.saturating_sub(4),
        c.item_extent.unwrap() + 8,
        188,
    )
}

impl PreparedFrame {
    pub fn screen(&self) -> ScreenId {
        self.screen
    }
    pub fn can_prepare_neighbor(&self) -> bool {
        self.screen == ScreenId::Recents && self.opaque && self.can_prewarm_navigation()
            && self.theme.pack().motion == "none"
            && self.previous_input.as_ref().is_some_and(|saved| Self::neighbor_input(&saved.model))
    }
    pub fn neighbor_ineligible_reason(&self) -> &'static str {
        if self.screen != ScreenId::Recents { "not_eligible:screen" }
        else if !self.opaque { "not_eligible:opacity" }
        else if self.theme.pack().motion != "none" { "not_eligible:motion" }
        else if self.previous_input.is_none() { "not_eligible:no_retained_inputs" }
        else if !self.can_prewarm_navigation() { "not_eligible:retained_layout" }
        else if self.previous_input.as_ref().is_some_and(|saved| !saved.model.keyboard.is_empty()) { "not_eligible:keyboard" }
        else if self.previous_input.as_ref().is_some_and(|saved| saved.model.progress.is_some()) { "not_eligible:progress" }
        else { "not_eligible:inputs" }
    }
    fn neighbor_input(input: &FrameInputs) -> bool {
        // Wi-Fi status is retained input; the controller excludes active network overlays/jobs.
        input.screen == ScreenId::Recents && retained::eligible(input)
            && input.keyboard.is_empty() && input.progress.is_none()
    }
    /// Upper bound reserved before the fork: pixels, retained copies, and transient metadata.
    pub fn neighbor_charge(&self, input: &FrameInputs) -> Option<usize> {
        if !self.can_prepare_neighbor() || !Self::neighbor_input(input) { return None; }
        let saved = self.previous_input.as_ref()?;
        let model_bytes = retained::allocation_bytes(input).max(retained::allocation_bytes(&saved.model));
        if model_bytes > 16 * 1024 { return None; }
        let (snapshots, metadata) = if self.theme.pack().ink() { self.ink_cache.navigation_charge(input)? }
            else { (0, model_bytes * 6 + self.component_metadata.iter().map(retained::metadata_bytes).sum::<usize>() * 2) };
        // Includes the temporary row/dependency models and retained interaction output.
        if metadata + model_bytes * 4 > 128 * 1024 { return None; }
        Some(640 * 480 * 4 + snapshots + 128 * 1024)
    }
    pub fn prepare_neighbor(&self, input: &FrameInputs, available: usize) -> Result<Option<(Self, usize)>> {
        let Some(charge) = self.neighbor_charge(input).filter(|charge| *charge <= available) else {
            if crate::perf_trace::recording() { crate::perf_trace::neighbor_build("admission_rejected"); }
            return Ok(None);
        };
        let _task = crate::perf_trace::task("neighbor_build", crate::perf_trace::generation());
        let fork = Self {
            theme: self.theme.clone(), screen: self.screen,
            pixels: self.pixels.as_ref().to_owned().into(), opaque: self.opaque,
            blit: Default::default(), previous_input: self.previous_input.clone(),
            component_metadata: self.component_metadata.clone(), retained_layout: self.retained_layout,
            tiles: Default::default(), ink_cache: self.ink_cache.fork_navigation(input),
            damage: self.damage.clone(), navigation_epoch: std::sync::Arc::new(()),
            ink_components: self.ink_components.clone(), targets: vec![],
            text_boxes: vec![],
        };
        let frame = Self::prepare_reusing(self.theme.clone(), input, Some(fork)).inspect_err(|_| {
            if crate::perf_trace::recording() { crate::perf_trace::neighbor_build("build_failed"); }
        })?;
        crate::perf_trace::count(crate::perf_trace::Count::NeighborBuilt, 1);
        if crate::perf_trace::recording() { crate::perf_trace::neighbor_build("built"); }
        Ok(Some((frame, charge)))
    }
    pub fn neighbor_matches(&self, theme: &ThemeHandle, input: &FrameInputs) -> bool {
        self.navigation_theme_matches(theme) && self.theme.digest() == theme.digest()
            && Self::neighbor_input(input) && self.input_matches(input)
            && self.previous_input.as_ref().is_some_and(|saved|
                saved.model.art.completed_misses == input.art.completed_misses)
    }
    /// Diagnostic only; acceptance still uses neighbor_matches and its live artwork proof.
    pub fn neighbor_rejection_reason(&self, theme: &ThemeHandle, input: &FrameInputs) -> &'static str {
        if !self.navigation_theme_matches(theme) || self.theme.digest() != theme.digest() { return "theme_mismatch"; }
        if !Self::neighbor_input(input) { return "inputs_mismatch:eligibility"; }
        let Some(saved) = &self.previous_input else { return "inputs_mismatch:no_retained_inputs"; };
        macro_rules! fields {
            ($($field:ident),*) => { $(if saved.model.$field != input.$field {
                return concat!("inputs_mismatch:", stringify!($field));
            })* };
        }
        fields!(screen, raster_revision, text, rows, selected, pagination, detail_lines,
            metadata, facts, navigation, focus, hints, suppress_hints, status, progress,
            keyboard, keyboard_cursor);
        if saved.model.art.next_cover_expected != input.art.next_cover_expected { return "inputs_mismatch:next_cover_expected"; }
        if saved.model.art.completed_misses != input.art.completed_misses { return "inputs_mismatch:completed_misses"; }
        "inputs_mismatch:artwork"
    }
    pub fn can_prewarm_navigation(&self) -> bool {
        (self.retained_layout || self.theme.pack().ink())
            && matches!(self.screen, ScreenId::Library | ScreenId::Recents)
            && self
                .previous_input
                .as_ref()
                .is_some_and(|saved| retained::eligible(&saved.model))
    }
    pub fn has_retained_inputs(&self) -> bool {
        self.previous_input.is_some()
    }
    pub fn input_matches(&self, input: &FrameInputs) -> bool {
        self.previous_input
            .as_ref()
            .is_some_and(|saved| retained::matches(saved, input))
    }
    pub fn navigation_theme_matches(&self, theme: &ThemeHandle) -> bool {
        self.theme.weak().ptr_eq(&theme.weak()) && self.theme.generation() == theme.generation()
    }
    pub fn navigation_stamp(&self) -> NavigationStamp {
        NavigationStamp(std::sync::Arc::downgrade(&self.navigation_epoch))
    }
    pub fn navigation_is_current(&self, stamp: &NavigationStamp) -> bool {
        stamp
            .0
            .ptr_eq(&std::sync::Arc::downgrade(&self.navigation_epoch))
    }
    pub fn navigation_model(&self) -> Option<FrameInputs> {
        self.can_prewarm_navigation()
            .then(|| self.previous_input.as_ref().map(retained::restore_model))
            .flatten()
    }
    pub fn navigation_images(
        &self,
        input: &FrameInputs,
    ) -> std::collections::VecDeque<NavigationImageWarm> {
        let mut work = std::collections::VecDeque::new();
        // Ink transforms depend on shape, focus scale, logo aspect ratio and preview blur.
        // Warm them through the actual ink paint operations instead of guessing geometry.
        if self.theme.pack().ink() {
            return work;
        }
        for c in &self.theme.pack().screens[&self.screen].components {
            match c.kind {
                Kind::Image if c.binding != Some(Binding::NextCover) => {
                    let source = match c.binding {
                        Some(Binding::SelectedHero) => input.art.selected_hero.as_ref(),
                        Some(Binding::SelectedLogo) => input.art.selected_logo.as_ref(),
                        _ => input.art.selected_cover.as_ref(),
                    };
                    if let Some(source) = source {
                        work.push_back(NavigationImageWarm {
                            stamp: self.navigation_stamp(),
                            component: c.clone(),
                            source: source.clone(),
                            rect: c.rect,
                            radius: c.radius.unwrap_or(0),
                        });
                    }
                }
                Kind::Entries if c.art_size.unwrap_or(0) > 0 => {
                    let art = c.art_size.unwrap();
                    let radius = if c.layout == Some(Arrangement::Shelf) {
                        c.radius.unwrap().saturating_sub(c.padding.unwrap())
                    } else {
                        c.radius.unwrap_or(0)
                    };
                    for source in input
                        .art
                        .entries
                        .values()
                        .chain(input.art.next_cover.iter())
                    {
                        work.push_back(NavigationImageWarm {
                            stamp: self.navigation_stamp(),
                            component: c.clone(),
                            source: source.clone(),
                            rect: Rect(0, 0, art, art),
                            radius,
                        });
                    }
                }
                _ => {}
            }
        }
        work
    }
    pub fn prewarm_navigation_image(&self, work: NavigationImageWarm) -> Result<()> {
        if !self.navigation_is_current(&work.stamp) {
            return Ok(());
        }
        self.image_pixels_with_radius(work.rect, &work.source, &work.component, work.radius)?;
        Ok(())
    }
    pub fn navigation_rasters(
        &self,
        input: &FrameInputs,
    ) -> std::collections::VecDeque<NavigationRasterWarm> {
        let mut work = std::collections::VecDeque::new();
        let input = std::sync::Arc::new(input.clone());
        let components = self
            .ink_components
            .as_ref()
            .map(|(_, _, plan)| plan.as_slice())
            .unwrap_or(&self.theme.pack().screens[&self.screen].components);
        for c in components {
            let mut add = |part| {
                work.push_back(NavigationRasterWarm {
                    stamp: self.navigation_stamp(),
                    component: c.clone(),
                    input: input.clone(),
                    part,
                })
            };
            if self.theme.pack().ink() {
                match c.kind {
                    Kind::Image if c.binding == Some(Binding::SelectedHero) => {
                        add(NavigationRasterPart::Hero)
                    }
                    Kind::Text | Kind::Image => add(NavigationRasterPart::InkComponent),
                    Kind::Entries if c.layout == Some(Arrangement::Shelf) => {
                        for index in 0..input
                            .rows
                            .get(&c.binding.unwrap())
                            .map_or(0, Vec::len)
                            .min(c.visible_count.unwrap_or(16) as usize)
                        {
                            add(NavigationRasterPart::InkShelfRow(index));
                        }
                        add(NavigationRasterPart::InkPeek);
                    }
                    Kind::Entries if c.layout == Some(Arrangement::List) => {
                        for index in 0..input
                            .rows
                            .get(&c.binding.unwrap())
                            .map_or(0, Vec::len)
                            .min(c.visible_count.unwrap_or(16) as usize)
                        {
                            add(NavigationRasterPart::InkListRow(index));
                        }
                    }
                    _ => {}
                }
            } else {
                if c.kind == Kind::Text && (c.binding == Some(Binding::SelectedTitle) || c.logo) {
                    add(NavigationRasterPart::Title);
                }
                if c.kind == Kind::Entries && c.layout == Some(Arrangement::Shelf) {
                    for index in 0..input.rows.get(&c.binding.unwrap()).map_or(0, Vec::len) {
                        add(NavigationRasterPart::ShelfRow(index));
                    }
                    add(NavigationRasterPart::Peek);
                }
            }
        }
        work
    }
    /// Runs one cache operation on the calling (UI) thread, preserving presentation state.
    pub fn prewarm_navigation_raster(&mut self, work: NavigationRasterWarm) -> Result<()> {
        if !self.navigation_is_current(&work.stamp) {
            return Ok(());
        }
        let c = &work.component;
        if matches!(work.part, NavigationRasterPart::Hero) {
            return self.prewarm_ink_hero(&work.input, c);
        }
        if matches!(
            work.part,
            NavigationRasterPart::InkComponent
                | NavigationRasterPart::InkShelfRow(_)
                | NavigationRasterPart::InkListRow(_)
                | NavigationRasterPart::InkPeek
        ) {
            return self.prewarm_ink_navigation(c, &work.input, &work.part);
        }
        let x = |index: usize| c.rect.0 + index as u32 * (c.item_extent.unwrap() + c.gap.unwrap());
        let rect = match work.part {
            NavigationRasterPart::Title => c.rect,
            NavigationRasterPart::ShelfRow(index) => shelf_tile_rect(c, x(index)),
            NavigationRasterPart::Peek => {
                let x = x(c.visible_count.unwrap() as usize);
                Rect(
                    x,
                    c.rect.1,
                    640u32.saturating_sub(x),
                    c.item_extent.unwrap(),
                )
            }
            _ => unreachable!(),
        };
        let stride = self.pixels.width() as usize * 4;
        let row_bytes = rect.2 as usize * 4;
        let mut saved = Vec::with_capacity(row_bytes * rect.3 as usize);
        for y in rect.1..rect.1 + rect.3 {
            let at = y as usize * stride + rect.0 as usize * 4;
            saved.extend_from_slice(&self.pixels.data()[at..at + row_bytes]);
        }
        let targets = self.targets.len();
        let text_boxes = self.text_boxes.len();
        let result = match work.part {
            NavigationRasterPart::Title => self.component(c, &work.input),
            NavigationRasterPart::ShelfRow(index) => self.shelf_entry(
                c,
                &work.input.rows[&c.binding.unwrap()][index],
                x(index),
                &work.input,
            ),
            NavigationRasterPart::Peek => self.cover_peek(c, &work.input),
            _ => unreachable!(),
        };
        let pixels = self.pixels.data_mut();
        for (row, y) in (rect.1..rect.1 + rect.3).enumerate() {
            let at = y as usize * stride + rect.0 as usize * 4;
            pixels[at..at + row_bytes]
                .copy_from_slice(&saved[row * row_bytes..(row + 1) * row_bytes]);
        }
        self.targets.truncate(targets);
        self.text_boxes.truncate(text_boxes);
        result
    }
    fn unpainted(theme: ThemeHandle, screen: ScreenId) -> Self {
        Self {
            theme: theme.clone(),
            screen,
            pixels: Pixmap::new(640, 480).unwrap().into(),
            opaque: false,
            blit: Default::default(),
            previous_input: None,
            component_metadata: vec![],
            retained_layout: false,
            tiles: Default::default(),
            ink_cache: Default::default(),
            damage: vec![],
            navigation_epoch: std::sync::Arc::new(()),
            ink_components: None,
            targets: vec![],
            text_boxes: vec![],
        }
    }
    pub fn prepare(theme: ThemeHandle, input: &FrameInputs) -> Result<Self> {
        Self::prepare_reusing(theme, input, None)
    }
    pub fn prepare_reusing(
        theme: ThemeHandle,
        input: &FrameInputs,
        previous: Option<Self>,
    ) -> Result<Self> {
        let _stage = crate::perf_trace::stage(crate::perf_trace::Stage::Raster);
        input.validate(&theme)?;
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Compose);
        #[cfg(feature = "perf-bench")]
        crate::perf::hit(crate::perf::Op::Frame);
        if previous.is_none() {
            crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
        }
        let mut result = previous.unwrap_or_else(|| Self::unpainted(theme.clone(), input.screen));
        // Reuse the token allocation when no speculative work holds a weak stamp.
        // make_mut dissociates outstanding weak stamps before this model can be displayed.
        std::sync::Arc::make_mut(&mut result.navigation_epoch);
        let same_theme = result.theme.weak().ptr_eq(&theme.weak())
            && result.theme.generation() == theme.generation();
        if !same_theme {
            result.tiles = Default::default();
            result.ink_cache = Default::default();
            result.ink_components = None;
        }
        if !same_theme || result.previous_input.is_none() || result.screen != input.screen {
            result.retained_layout = retained::supported_screen(&theme, input.screen);
        }
        let eligible = result.retained_layout && retained::eligible(input);
        let incremental = eligible
            && same_theme
            && result
                .previous_input
                .as_ref()
                .is_some_and(|saved| retained::eligible(&saved.model));
        result.theme = theme;
        result.screen = input.screen;
        crate::perf_trace::render_reason(if !result.retained_layout {
            "unsupported_layout"
        } else if !eligible {
            "ineligible_model"
        } else if incremental {
            "incremental"
        } else {
            "initial_or_theme_change"
        });
        if result.theme.pack().ink() {
            result.render_ink(input)?;
        } else {
            result.render_components(input, incremental)?;
        }
        let retained_opaque_ink = result.theme.pack().ink() && result.ink_cache.opaque_base;
        if !eligible && !retained_opaque_ink {
            crate::perf_trace::count(crate::perf_trace::Count::ScannedPixels, 640 * 480);
        }
        result.previous_input =
            (eligible || result.theme.pack().ink()).then(|| retained::capture(input));
        result.opaque = retained_opaque_ink
            || eligible
            || result
                .pixels
                .pixels()
                .iter()
                .all(|pixel| pixel.alpha() == 255);
        result.blit = if result.opaque {
            Default::default()
        } else if input.screen == ScreenId::IngameMenu {
            super::blit::BlitMap::with_constant_scrim(result.pixels.as_ref())
        } else {
            super::blit::BlitMap::new(result.pixels.as_ref())
        };
        Ok(result)
    }
    /// The destination must contain the preceding frame, without external overlays.
    pub fn paint_damage(&self, display: &mut impl Display) -> Vec<DrawRect> {
        if !self.opaque || display.width() != 640 || display.height() != 480 {
            self.paint(display);
            return vec![display.bounding_box()];
        }
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Compose);
        if self.damage.is_empty() {
            return vec![];
        }
        let mut dest = display.pixmap_mut();
        let mut damage: Vec<DrawRect> = Vec::new();
        for rect in &self.damage {
            if dense_damage(self.pixels.data(), dest.data_mut(), *rect) {
                copy_damage(self.pixels.data(), dest.data_mut(), *rect);
                damage.push(DrawRect::new(rect.0 as i32, rect.1 as i32, rect.2, rect.3));
                continue;
            }
            let mut previous: Vec<(usize, usize, usize)> = vec![];
            for y in rect.1..rect.1 + rect.3 {
                let at = (y as usize * 640 + rect.0 as usize) * 4;
                let end = at + rect.2 as usize * 4;
                let source = &self.pixels.data()[at..end];
                let target = &mut dest.data_mut()[at..end];
                crate::perf_trace::count(crate::perf_trace::Count::ComparedPixels, rect.2 as u64);
                let mut current = vec![];
                for (first, last) in changed_runs(source, target) {
                    target[first * 4..last * 4].copy_from_slice(&source[first * 4..last * 4]);
                    crate::perf_trace::count(
                        crate::perf_trace::Count::CopiedPixels,
                        (last - first) as u64,
                    );
                    let index = if let Some(&(_, _, index)) = previous
                        .iter()
                        .find(|&&(x, end, _)| x == first && end == last)
                    {
                        damage[index].h += 1;
                        index
                    } else {
                        damage.push(DrawRect::new(
                            (rect.0 as usize + first) as i32,
                            y as i32,
                            (last - first) as u32,
                            1,
                        ));
                        if damage.len() > 4096 {
                            drop(dest);
                            self.paint(display);
                            return vec![display.bounding_box()];
                        }
                        damage.len() - 1
                    };
                    current.push((first, last, index));
                }
                previous = current;
            }
        }
        damage
    }
    pub fn paint(&self, display: &mut impl Display) {
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Compose);
        if self.opaque
            && display.width() == self.pixels.width()
            && display.height() == self.pixels.height()
            && display.set_shared_pixmap(&self.pixels.0)
        {
            return;
        }
        crate::perf_trace::count(
            crate::perf_trace::Count::CopiedPixels,
            self.pixels.width() as u64 * self.pixels.height() as u64,
        );
        let mut dest = display.pixmap_mut();
        if self.opaque
            && dest.width() == self.pixels.width()
            && dest.height() == self.pixels.height()
        {
            dest.data_mut().copy_from_slice(self.pixels.data());
        } else if self.opaque {
            dest.draw_pixmap(
                0,
                0,
                self.pixels.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        } else {
            self.blit
                .draw(&mut dest, 0, 0, self.pixels.as_ref(), 1.0, None);
        }
    }
    pub fn is_opaque(&self) -> bool {
        self.opaque
    }
    pub fn pixels(&self) -> &Pixmap {
        &self.pixels
    }
    pub fn snapshot(&self) -> &ThemeHandle {
        &self.theme
    }
    pub fn pixel_bytes(&self) -> usize {
        self.pixels.data().len()
    }
    pub fn into_overlay(self) -> Result<PreparedOverlay> {
        PreparedOverlay::from_frame(self.theme, self.screen, self.pixels.into_pixmap())
    }
    fn color(&self, role: ColorRole) -> Color {
        let s = &self.theme.pack().tokens.colors[&role];
        Color::rgba(
            u8::from_str_radix(&s[1..3], 16).unwrap(),
            u8::from_str_radix(&s[3..5], 16).unwrap(),
            u8::from_str_radix(&s[5..7], 16).unwrap(),
            u8::from_str_radix(&s[7..9], 16).unwrap(),
        )
    }
    fn panel(&mut self, rect: Rect, role: ColorRole, radius: u32) {
        let color = self.color(role);
        if self.theme.pack().ink() && radius == 0 && color.a() == 255 && !ink_blit::reference() {
            crate::display::fill_rect(&mut self.pixels.as_mut(), draw_rect(rect), color);
        } else {
            fill_rounded_rect(&mut self.pixels.as_mut(), draw_rect(rect), radius, color);
        }
    }
    fn text(
        &mut self,
        id: &str,
        rect: Rect,
        role: Role,
        color: ColorRole,
        value: &str,
        lines: u32,
    ) -> Result<()> {
        self.text_options(
            id,
            rect,
            role,
            color,
            value,
            lines,
            Align::Left,
            Overflow::Wrap,
        )
    }
    fn text_options(
        &mut self,
        id: &str,
        rect: Rect,
        role: Role,
        color: ColorRole,
        value: &str,
        lines: u32,
        align: Align,
        overflow: Overflow,
    ) -> Result<()> {
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Text);
        let cache_key = super::text_cache::Key::new(
            &self.theme,
            role,
            self.color(color),
            value,
            rect,
            lines,
            align,
            overflow,
        );
        if let Some((scratch, map)) = super::text_cache::get(&self.theme, &cache_key) {
            map.draw(
                &mut self.pixels.as_mut(),
                rect.0 as i32,
                rect.1 as i32,
                scratch.as_ref().as_ref(),
                1.0,
                None,
            );
            self.text_boxes.push((id.into(), rect));
            return Ok(());
        }
        let style = self.theme.text_style(role, self.color(color));
        let single;
        let (value, lines) = if overflow == Overflow::Ellipsis {
            single = value.replace('\n', " ");
            (single.as_str(), 1)
        } else {
            (value, lines)
        };
        let wrapped = wrap(&style, value, rect.2, lines as usize);
        let scratch_bytes = rect.2 as usize * rect.3 as usize * 4;
        ensure!(
            scratch_bytes + self.pixels.data().len() <= 2 * 1024 * 1024,
            "Prepared text pixel workspace exceeds budget"
        );
        super::text_cache::reserve(rect.2, rect.3, &cache_key.text_value());
        crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
        let mut scratch =
            Pixmap::new(rect.2, rect.3).ok_or_else(|| anyhow::anyhow!("Invalid text box"))?;
        for (i, line) in wrapped.iter().enumerate() {
            style.draw(
                &mut scratch.as_mut(),
                line,
                Point::new(
                    match align {
                        Align::Left => 0,
                        Align::Center => rect.2.saturating_sub(style.measure(line).w) as i32 / 2,
                        Align::Right => rect.2.saturating_sub(style.measure(line).w) as i32,
                    },
                    i as i32 * self.theme.pack().tokens.typography[&role].line_height as i32,
                ),
            );
        }
        let scratch = std::sync::Arc::new(scratch);
        let map = super::text_cache::insert(&self.theme, cache_key, scratch.clone());
        map.draw(
            &mut self.pixels.as_mut(),
            rect.0 as i32,
            rect.1 as i32,
            scratch.as_ref().as_ref(),
            1.0,
            None,
        );
        self.text_boxes.push((id.into(), rect));
        Ok(())
    }
    fn image_key(&self, screen: ScreenId, rect: Rect, c: &Component, radius: u32) -> super::image_cache::ImageKey {
        let mask = self
            .theme
            .base()
            .0
            .image_masks
            .get(&screen)
            .and_then(|m| m.get(&c.id));
        let background = (self.retained_layout
            && screen == ScreenId::Library
            && c.id == "hero")
            .then(|| {
                let color = self.color(ColorRole::Background);
                [color.r(), color.g(), color.b(), color.a()]
            });
        super::image_cache::ImageKey {
            generation: self.theme.generation(),
            screen,
            component: c.id.clone(),
            width: rect.2,
            height: rect.3,
            fit: c.fit.unwrap_or(super::pack::Fit::Contain),
            radius,
            blur: c.blur_radius.unwrap_or(0),
            fade_edge: c.fade_edge,
            fade_width: c.fade_width,
            mask: mask.map_or(0, |pixels| pixels as *const Pixmap as usize),
            background,
        }
    }
    fn transform_image(theme: &ThemeHandle, key: &super::image_cache::ImageKey, source: &std::sync::Arc<Pixmap>) -> Result<std::sync::Arc<Pixmap>> {
        let mut image = super::runtime::fit_image(
            source,
            key.width,
            key.height,
            key.fit,
        )?;
        super::runtime::blur(&mut image, key.blur)?;
        super::runtime::rounded(&mut image, key.radius);
        if key.fade_edge == Some(FadeEdge::Left) {
            let width = key.fade_width.unwrap();
            for y in 0..key.height {
                for x in 0..width {
                    let at = ((y * key.width + x) * 4) as usize;
                    let fraction = x as f32 / width as f32;
                    for channel in &mut image.data_mut()[at..at + 4] {
                        *channel = (*channel as f32 * fraction).round() as u8;
                    }
                }
            }
        }
        if let Some(mask) = theme
            .base()
            .0
            .image_masks
            .get(&key.screen)
            .and_then(|m| m.get(&key.component))
        {
            super::runtime::alpha_mask(&mut image, mask)?;
        }
        if let Some(background) = key.background {
            crate::perf_trace::count(crate::perf_trace::Count::Allocations, 1);
            let mut opaque = Pixmap::new(key.width, key.height).unwrap();
            opaque.fill(tiny_skia::Color::from_rgba8(
                background[0],
                background[1],
                background[2],
                background[3],
            ));
            super::blit::BlitMap::new(image.as_ref()).draw(
                &mut opaque.as_mut(),
                0,
                0,
                image.as_ref(),
                1.0,
                None,
            );
            image = opaque;
        }
        Ok(std::sync::Arc::new(image))
    }
    fn image_pixels_with_radius(
        &self,
        rect: Rect,
        source: &std::sync::Arc<Pixmap>,
        c: &Component,
        radius: u32,
    ) -> Result<std::sync::Arc<Pixmap>> {
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::ComponentPrep);
        let key = self.image_key(self.screen, rect, c, radius);
        if let Some(pixels) = self
            .theme
            .base()
            .0
            .image_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Image cache unavailable"))?
            .get(&key, source)
        {
            crate::perf_trace::count(crate::perf_trace::Count::ImageHit, 1);
            return Ok(pixels);
        }
        crate::perf_trace::count(crate::perf_trace::Count::ImageMiss, 1);
        if ink_renderer::HERO_SCRATCH.get() {
            return Self::transform_image(&self.theme, &key, source);
        }
        self.theme
            .base()
            .0
            .image_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Image cache unavailable"))?
            .reserve(
                rect.2 as usize * rect.3 as usize * 4 * if key.background.is_some() { 2 } else { 1 },
            );
        let image = Self::transform_image(&self.theme, &key, source)?;
        self.theme
            .base()
            .0
            .image_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Image cache unavailable"))?
            .insert(key, source, image.clone());
        Ok(image)
    }
    fn image_pixels(
        &self,
        rect: Rect,
        source: &std::sync::Arc<Pixmap>,
        c: &Component,
    ) -> Result<std::sync::Arc<Pixmap>> {
        self.image_pixels_with_radius(rect, source, c, c.radius.unwrap_or(0))
    }
    fn image(&mut self, rect: Rect, source: &std::sync::Arc<Pixmap>, c: &Component) -> Result<()> {
        let image = self.image_pixels(rect, source, c)?;
        self.draw_image(rect, &image);
        Ok(())
    }
    fn rounded_image(
        &mut self,
        rect: Rect,
        source: &std::sync::Arc<Pixmap>,
        c: &Component,
        radius: u32,
    ) -> Result<()> {
        let image = self.image_pixels_with_radius(rect, source, c, radius)?;
        self.draw_image(rect, &image);
        Ok(())
    }
    fn draw_image(&mut self, rect: Rect, image: &std::sync::Arc<Pixmap>) {
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Artwork);
        if self.theme.pack().ink() && !ink_blit::reference() {
            ink_renderer::bitmap(
                &mut self.pixels,
                rect.0 as i32,
                rect.1 as i32,
                image.as_ref().as_ref(),
                false,
            );
            return;
        }
        super::blit::draw_cached(
            &mut self.pixels.as_mut(),
            rect.0 as i32,
            rect.1 as i32,
            image,
            1.0,
        );
    }
    fn logo_rect(c: &Component, source: &Pixmap) -> Rect {
        let target = super::art::Target { width: c.rect.2, height: c.rect.3, fit: super::pack::Fit::Contain, logo_box: true }
            .logo(source.width(), source.height());
        let (width, height) = (target.width, target.height);
        let align = c.align.or_else(|| c.paint.as_ref().and_then(|p| p.anchor)).unwrap_or(Align::Left);
        let x = c.rect.0 + match align {
            Align::Left => 0,
            Align::Center => (c.rect.2 - width) / 2,
            Align::Right => c.rect.2 - width,
        };
        Rect(x, c.rect.1 + (c.rect.3 - height) / 2, width, height)
    }
    fn ink_image_component(&self, c: &Component) -> Component {
        let mut image = c.clone();
        if c.binding != Some(Binding::SelectedHero) {
            image.fit = Some(if self.theme.pack().alignment.as_ref().unwrap().cover_shape == "square" {
                super::pack::Fit::Cover
            } else { super::pack::Fit::Contain });
        }
        image
    }
    fn logo(&mut self, c: &Component, input: &FrameInputs) -> Result<bool> {
        if super::logo_suppressed_by_cover(c, input) {
            return Ok(false);
        }
        let Some(source) = input.art.selected_logo.as_ref() else {
            return Ok(false);
        };
        if ink_renderer::collecting_text() { return Ok(true); }
        let rect = Self::logo_rect(c, source);
        let mut slot = c.clone();
        slot.fit = Some(super::pack::Fit::Contain);
        slot.radius = Some(0);
        let pixels = self.image_pixels(rect, source, &slot)?;
        self.draw_image(rect, &pixels);
        Ok(true)
    }
    fn component(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        match c.kind {
            Kind::Background | Kind::Panel | Kind::Overlay => {
                self.panel(c.rect, c.color.unwrap(), c.radius.unwrap())
            }
            Kind::Text => {
                if c.logo && self.logo(c, input)? {
                    return Ok(());
                }
                let value = if c.binding == Some(Binding::ThemeName) {
                    self.theme.pack().name.clone()
                } else {
                    input
                        .text
                        .get(&c.binding.unwrap())
                        .cloned()
                        .unwrap_or_default()
                };
                self.text_options(
                    &c.id,
                    c.rect,
                    c.role.unwrap(),
                    c.color.unwrap(),
                    &value,
                    c.max_lines.unwrap(),
                    c.align.unwrap(),
                    c.overflow.unwrap(),
                )?;
            }
            Kind::Image => {
                if c.binding == Some(Binding::NextCover) {
                    return Ok(());
                }
                let source = match c.binding {
                    Some(Binding::SelectedHero) => input.art.selected_hero.as_ref(),
                    _ => input.art.selected_cover.as_ref(),
                };
                if let Some(source) = source {
                    self.image(c.rect, source, c)?;
                } else {
                    self.panel(c.rect, ColorRole::Surface, c.radius.unwrap());
                }
            }
            Kind::AssetImage => {
                if let Some(image) = self
                    .theme
                    .base()
                    .0
                    .prepared_assets
                    .get(&input.screen)
                    .and_then(|m| m.get(&c.id))
                {
                    self.pixels.as_mut().draw_pixmap(
                        c.rect.0 as i32,
                        c.rect.1 as i32,
                        image.as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                }
            }
            Kind::Entries | Kind::SettingsRows => self.entries(c, input)?,
            Kind::Navigation => {
                if c.layout == Some(Arrangement::Hidden) {
                    return Ok(());
                }
                if let Some(rows) = input.navigation.get(&c.group.unwrap()) {
                    let rows: Vec<_> = if c.layout == Some(Arrangement::Current) {
                        let selected = c.binding.and_then(|binding| input.selected.get(&binding));
                        rows.iter()
                            .filter(|row| selected == Some(&row.id))
                            .collect()
                    } else {
                        rows.iter().collect()
                    };
                    for (i, row) in rows.into_iter().enumerate() {
                        let offset = i as u32 * (c.item_extent.unwrap() + c.gap.unwrap());
                        let Rect(x, y, w, h) = c.rect;
                        let rect = if c.layout == Some(Arrangement::Rail) {
                            Rect(x, y + offset, w, c.item_extent.unwrap())
                        } else {
                            Rect(x + offset, y, c.item_extent.unwrap(), h)
                        };
                        self.row(c, row, rect, input)?;
                    }
                }
            }
            Kind::DialogActions => {
                if let Some(rows) = input.rows.get(&Binding::Actions) {
                    let n = rows.len().max(1) as u32;
                    ensure!(n <= 4, "Too many dialog actions");
                    let Rect(x, y, w, h) = c.rect;
                    for (i, row) in rows.iter().enumerate() {
                        let rect = if c.layout == Some(Arrangement::Vertical) {
                            Rect(x, y + i as u32 * (h / n), w, h / n)
                        } else {
                            Rect(x + i as u32 * (w / n), y, w / n, h)
                        };
                        self.row(c, row, rect, input)?;
                    }
                }
            }
            Kind::Progress => {
                self.panel(c.rect, ColorRole::Border, c.radius.unwrap());
                if let Some(p) = input.progress {
                    let mut rect = c.rect;
                    rect.2 = (rect.2 as f32 * p) as u32;
                    if rect.2 > 0 {
                        self.panel(rect, c.color.unwrap(), c.radius.unwrap());
                    }
                }
            }
            Kind::Hints => {
                if c.presentation == Some(HintPresentation::Keycaps) {
                    return self.keycaps(c, input);
                }
                let value = input
                    .hints
                    .iter()
                    .map(|h| format!("{} {}", h.button, h.label))
                    .collect::<Vec<_>>()
                    .join("   ");
                self.text(&c.id, c.rect, c.role.unwrap(), c.color.unwrap(), &value, 1)?;
            }
            Kind::Status => {
                if c.layout == Some(Arrangement::Horizontal) {
                    return self.inline_status(c, input);
                }
                let role = c.role.unwrap();
                let color = c.color.unwrap();
                let line = self.theme.pack().tokens.typography[&role].line_height;
                let battery_row = u32::from(input.status.clock.is_some());
                let mut clock = input.status.clock.clone().unwrap_or_default();
                if c.max_lines == Some(2) && input.status.network.is_some() {
                    // A compact four-bar network icon leaves the clock readable.
                    let x = c.rect.0 + c.rect.2 - 22;
                    let y = c.rect.1 + line.saturating_sub(12) / 2;
                    for i in 0..4 {
                        self.panel(
                            Rect(x + i * 5, y + 9 - i * 3, 3, 3 + i * 3),
                            if input.status.network.as_deref() == Some("online") {
                                color
                            } else {
                                ColorRole::Muted
                            },
                            0,
                        );
                    }
                    if !clock.is_empty() {
                        clock.push_str(" ");
                    }
                    self.text(
                        &c.id,
                        Rect(c.rect.0, c.rect.1, c.rect.2 - 26, line),
                        role,
                        color,
                        &clock,
                        1,
                    )?;
                } else if !clock.is_empty() {
                    self.text(
                        &c.id,
                        Rect(c.rect.0, c.rect.1, c.rect.2, line),
                        role,
                        color,
                        &clock,
                        1,
                    )?;
                }
                if let Some(percent) = input.status.battery {
                    let y = c.rect.1 + battery_row * line;
                    let icon = c.battery_style == Some(BatteryStyle::IconPercentage);
                    let offset = if icon { 30 } else { 0 };
                    if icon {
                        let icon_h = line.saturating_sub(2).min(12);
                        let top = y + (line - icon_h) / 2;
                        let r = Rect(c.rect.0, top, 24, icon_h);
                        self.outline(r, color, 1);
                        self.panel(
                            Rect(c.rect.0 + 24, top + icon_h / 4, 2, icon_h / 2),
                            color,
                            0,
                        );
                        let width = 20 * u32::from(percent) / 100;
                        if width > 0 {
                            self.panel(
                                Rect(c.rect.0 + 2, top + 2, width, icon_h.saturating_sub(4)),
                                color,
                                0,
                            );
                        }
                    }
                    self.text(
                        &c.id,
                        Rect(c.rect.0 + offset, y, c.rect.2 - offset, line),
                        role,
                        color,
                        &format!(
                            "{}{}%",
                            if input.status.charging { "+ " } else { "" },
                            percent
                        ),
                        1,
                    )?;
                }
                if c.max_lines == Some(3) {
                    if let Some(network) = &input.status.network {
                        let row = battery_row + u32::from(input.status.battery.is_some());
                        self.text(
                            &c.id,
                            Rect(c.rect.0, c.rect.1 + row * line, c.rect.2, line),
                            role,
                            color,
                            if network == "online" {
                                "Wi-Fi"
                            } else {
                                network
                            },
                            1,
                        )?;
                    }
                }
            }
            Kind::Keyboard => self.keyboard(c, input)?,
        }
        Ok(())
    }
    fn keycaps(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        if input.hints.is_empty() {
            return Ok(());
        }
        if c.max_lines == Some(1) {
            return self.keycaps_one_row(c, input);
        }
        let role = c.role.unwrap();
        let style = self.theme.text_style(role, self.color(ColorRole::Text));
        let widths: Vec<_> = input
            .hints
            .iter()
            .map(|hint| {
                let badge = if matches!(hint.button.as_str(), "A" | "B" | "X" | "Y") {
                    20
                } else {
                    style.measure(&hint.button).w + 10
                };
                let label = style.measure(&hint.label).w;
                (badge, label, badge + 6 + label)
            })
            .collect();
        let mut row_width = 0;
        let mut rows = 1;
        for (_, _, width) in &widths {
            if row_width > 0 && row_width + width > c.rect.2 {
                rows += 1;
                row_width = 0;
            }
            row_width += width + 14;
        }
        ensure!(rows <= 2, "Keycap hints exceed two-row bounds");
        let minimum_height = rows * 20;
        ensure!(
            minimum_height <= c.rect.3,
            "Keycap hints exceed component height"
        );
        let row_stride = if rows > 1 {
            ((c.rect.3 - 20) / (rows - 1)).clamp(20, 24)
        } else {
            0
        };
        let block_height = 20 + (rows - 1) * row_stride;
        let top = c
            .rect
            .3
            .checked_sub(block_height)
            .and_then(|slack| c.rect.1.checked_add(slack))
            .ok_or_else(|| anyhow::anyhow!("Keycap hint geometry overflow"))?;
        let mut x = c.rect.0;
        let mut y = top;
        for (hint, (badge, label, width)) in input.hints.iter().zip(widths) {
            if x > c.rect.0 && x + width > c.rect.0 + c.rect.2 {
                x = c.rect.0;
                y += row_stride;
            }
            ensure!(
                width <= c.rect.2 && y + 20 <= c.rect.1 + c.rect.3,
                "Keycap hints exceed two-row bounds"
            );
            let rect = Rect(x, y, badge, 20);
            self.panel(rect, ColorRole::Surface, 10);
            self.focus_outline(rect, ColorRole::Border, 1, 10);
            self.text_options(
                &c.id,
                rect,
                role,
                ColorRole::Text,
                &hint.button,
                1,
                Align::Center,
                Overflow::Ellipsis,
            )?;
            self.text(
                &c.id,
                Rect(x + badge + 6, y, label.max(1), 20),
                role,
                c.color.unwrap(),
                &hint.label,
                1,
            )?;
            x += width + 14;
        }
        Ok(())
    }
    fn keycaps_one_row(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let role = c.role.unwrap();
        let style = self.theme.text_style(role, self.color(ColorRole::Text));
        let mut x = c.rect.0;
        let y = c.rect.1 + (c.rect.3 - 20) / 2;
        let badges: Vec<_> = input
            .hints
            .iter()
            .map(|hint| {
                if matches!(hint.button.as_str(), "A" | "B" | "X" | "Y") {
                    20
                } else {
                    style.measure(&hint.button).w + 10
                }
            })
            .collect();
        let desired: Vec<_> = input
            .hints
            .iter()
            .map(|hint| style.measure(&hint.label).w)
            .collect();
        let count = input.hints.len() as u32;
        let fixed = badges.iter().sum::<u32>() + count * 4 + count.saturating_sub(1) * 8;
        ensure!(
            fixed + count * 12 <= c.rect.2,
            "One-row keycap badges exceed bounds"
        );
        let mut labels: Vec<_> = desired.iter().map(|width| (*width).min(12)).collect();
        let mut remaining = c.rect.2 - fixed - labels.iter().sum::<u32>();
        while remaining > 0 {
            let mut added = false;
            for (width, wanted) in labels.iter_mut().zip(&desired) {
                if *width < *wanted {
                    *width += 1;
                    remaining -= 1;
                    added = true;
                    if remaining == 0 {
                        break;
                    }
                }
            }
            if !added {
                break;
            }
        }
        for (index, hint) in input.hints.iter().enumerate() {
            let button = hint.button.as_str();
            let label = hint.label.as_str();
            let badge = badges[index];
            let label_width = labels[index];
            let width = badge + 4 + label_width;
            let rect = Rect(x, y, badge, 20);
            self.panel(rect, ColorRole::Surface, 10);
            self.focus_outline(rect, ColorRole::Border, 1, 10);
            self.text_options(
                &c.id,
                rect,
                role,
                ColorRole::Text,
                button,
                1,
                Align::Center,
                Overflow::Ellipsis,
            )?;
            self.text_options(
                &c.id,
                Rect(x + badge + 4, y, label_width.max(1), 20),
                role,
                c.color.unwrap(),
                label,
                1,
                Align::Left,
                Overflow::Ellipsis,
            )?;
            x += width + 8;
        }
        Ok(())
    }
    fn inline_status(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let role = c.role.unwrap();
        let color = c.color.unwrap();
        let line = self.theme.pack().tokens.typography[&role].line_height;
        let x = c.rect.0;
        let y = c.rect.1;
        if c.battery_style == Some(BatteryStyle::Icon) {
            if let Some(network) = &input.status.network {
                self.wifi_arcs(
                    x + 2,
                    y + 1,
                    if network == "online" {
                        color
                    } else {
                        ColorRole::Border
                    },
                );
            }
            if let Some(percent) = input.status.battery {
                let top = y + (line - 12) / 2;
                let bx = x + 38;
                self.outline(Rect(bx, top, 24, 12), color, 1);
                self.panel(Rect(bx + 24, top + 3, 2, 6), color, 0);
                let fill = 20 * u32::from(percent) / 100;
                if fill > 0 {
                    self.panel(Rect(bx + 2, top + 2, fill, 8), color, 0);
                }
            }
            if let Some(clock) = &input.status.clock {
                self.text_options(
                    &c.id,
                    Rect(x + 96, y, c.rect.2 - 104, line),
                    role,
                    color,
                    clock,
                    1,
                    Align::Right,
                    Overflow::Ellipsis,
                )?;
            }
            return Ok(());
        }
        if input.status.network.is_some() {
            for i in 0..4 {
                self.panel(
                    Rect(
                        x + 10 + i * 5,
                        y + (line - 12) / 2 + 9 - i * 3,
                        3,
                        3 + i * 3,
                    ),
                    if input.status.network.as_deref() == Some("online") {
                        color
                    } else {
                        ColorRole::Muted
                    },
                    0,
                );
            }
        }
        if let Some(percent) = input.status.battery {
            let bx = x + 38;
            let top = y + (line - 12) / 2;
            if c.battery_style == Some(BatteryStyle::IconPercentage) {
                self.outline(Rect(bx, top, 24, 12), color, 1);
                self.panel(Rect(bx + 24, top + 3, 2, 6), color, 0);
                let width = 20 * u32::from(percent) / 100;
                if width > 0 {
                    self.panel(Rect(bx + 2, top + 2, width, 8), color, 0);
                }
            }
            self.text(
                &c.id,
                Rect(bx + 30, y, 48, line),
                role,
                color,
                &format!(
                    "{}{}%",
                    if input.status.charging { "+" } else { "" },
                    percent
                ),
                1,
            )?;
        }
        if let Some(clock) = &input.status.clock {
            self.text(
                &c.id,
                Rect(x + c.rect.2.saturating_sub(54), y, 54, line),
                role,
                color,
                clock,
                1,
            )?;
        }
        Ok(())
    }
    fn wifi_arcs(&mut self, x: u32, y: u32, role: ColorRole) {
        let mut path = tiny_skia::PathBuilder::new();
        let x = x as f32;
        let y = y as f32;
        path.move_to(x + 1.0, y + 6.0);
        path.cubic_to(x + 7.0, y - 1.0, x + 17.0, y - 1.0, x + 23.0, y + 6.0);
        path.move_to(x + 6.0, y + 9.0);
        path.cubic_to(x + 10.0, y + 5.0, x + 14.0, y + 5.0, x + 18.0, y + 9.0);
        if let Some(path) = path.finish() {
            let paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(self.color(role).into()),
                anti_alias: true,
                ..Default::default()
            };
            self.pixels.stroke_path(
                &path,
                &paint,
                &tiny_skia::Stroke {
                    width: 1.7,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
        self.panel(Rect(x as u32 + 11, y as u32 + 11, 3, 3), role, 2);
    }
    fn row(&mut self, c: &Component, row: &Row, rect: Rect, input: &FrameInputs) -> Result<()> {
        let focused = input.focus.component == c.id && input.focus.item.as_ref() == Some(&row.id);
        let selected = c
            .binding
            .and_then(|b| input.selected.get(&b))
            .is_some_and(|id| id == &row.id);
        self.panel(
            rect,
            if focused {
                ColorRole::Focus
            } else {
                ColorRole::Surface
            },
            c.radius.unwrap_or(6),
        );
        if focused {
            self.focus_outline(
                rect,
                c.focus_color.unwrap_or(ColorRole::Accent),
                c.focus_width.unwrap_or(2),
                c.radius.unwrap_or(6),
            );
        }
        let font = self.theme.pack().tokens.typography[&c.role.unwrap()].line_height;
        let pad = c.padding.unwrap_or(6);
        ensure!(
            rect.2 > 2 * pad && rect.3 >= font,
            "Row text does not fit actual model"
        );
        let value = if row.value.is_empty() {
            row.label.clone()
        } else {
            format!("{}  {}", row.label, row.value)
        };
        let art_size = c.art_size.unwrap_or(0);
        let art_offset = if art_size > 0 {
            if let Some(image) = input.art.entries.get(&row.id) {
                self.image(
                    Rect(
                        rect.0 + pad,
                        rect.1 + (rect.3 - art_size) / 2,
                        art_size,
                        art_size,
                    ),
                    image,
                    c,
                )?;
            }
            art_size + pad
        } else {
            0
        };
        self.text(
            &c.id,
            Rect(
                rect.0 + pad + art_offset,
                rect.1 + (rect.3 - font) / 2,
                rect.2 - 2 * pad - art_offset,
                font,
            ),
            c.role.unwrap(),
            if !row.enabled {
                ColorRole::Muted
            } else if focused || selected {
                ColorRole::FocusText
            } else {
                ColorRole::Text
            },
            &value,
            1,
        )?;
        self.targets.push(Target {
            component: c.id.clone(),
            item: row.id.clone(),
            rect,
        });
        Ok(())
    }
    fn focus_outline(&mut self, r: Rect, color: ColorRole, width: u32, radius: u32) {
        let inset = width.div_ceil(2);
        if r.2 <= inset * 2 || r.3 <= inset * 2 {
            return;
        }
        if let Some(path) = crate::display::build_rounded_rect_path(
            draw_rect(Rect(
                r.0 + inset,
                r.1 + inset,
                r.2 - 2 * inset,
                r.3 - 2 * inset,
            )),
            radius.saturating_sub(inset),
        ) {
            let paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(self.color(color).into()),
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
    fn outline(&mut self, r: Rect, color: ColorRole, width: u32) {
        let width = width.min(r.2 / 2).min(r.3 / 2);
        if width == 0 {
            return;
        }
        for rect in [
            Rect(r.0, r.1, r.2, width),
            Rect(r.0, r.1 + r.3 - width, r.2, width),
            Rect(r.0, r.1, width, r.3),
            Rect(r.0 + r.2 - width, r.1, width, r.3),
        ] {
            self.panel(rect, color, 0);
        }
    }
    fn cover_case(&self, c: &Component, source: &std::sync::Arc<Pixmap>) -> Result<Pixmap> {
        let pad = c.padding.unwrap();
        let art = c.art_size.unwrap();
        let mut pixels = Pixmap::new(c.item_extent.unwrap(), art + 2 * pad)
            .ok_or_else(|| anyhow::anyhow!("Invalid cover case"))?;
        let case = Rect(0, 0, pixels.width(), pixels.height());
        fill_rounded_rect(
            &mut pixels.as_mut(),
            draw_rect(case),
            c.radius.unwrap(),
            self.color(c.frame_color.unwrap()),
        );
        let image = self.image_pixels_with_radius(
            Rect(0, 0, art, art),
            source,
            c,
            c.radius.unwrap().saturating_sub(pad),
        )?;
        super::blit::draw_cached(&mut pixels.as_mut(), pad as i32, pad as i32, &image, 1.0);
        Ok(pixels)
    }
    fn cover_peek(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let x = c.rect.0 + c.visible_count.unwrap() * (c.item_extent.unwrap() + c.gap.unwrap());
        let rect = Rect(
            x,
            c.rect.1,
            640u32.saturating_sub(x),
            c.item_extent.unwrap(),
        );
        let cache_tile =
            self.retained_layout && retained::eligible(input) && input.art.next_cover.is_some();
        let key = tiles::Key {
            label: "",
            source: input.art.next_cover.as_ref(),
            focused: false,
            preview: true,
        };
        if cache_tile && self.tiles.paint(&key, &mut self.pixels, rect) {
            return Ok(());
        }
        if self.retained_layout && retained::eligible(input) {
            let color = self.color(ColorRole::Background);
            crate::display::fill_rect(&mut self.pixels.as_mut(), draw_rect(rect), color);
        }
        let base = self.theme.base().clone();
        let mut cache = base
            .0
            .preview_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Preview cache unavailable"))?;
        let Some(source) = input.art.next_cover.as_ref() else {
            *cache = None;
            return Ok(());
        };
        let cached = cache
            .as_ref()
            .filter(|v| {
                v.generation == self.theme.generation()
                    && v.screen == input.screen
                    && v.component == c.id
                    && std::sync::Arc::ptr_eq(&v.source, source)
            })
            .map(|v| v.pixels.clone());
        let pixels = match cached {
            Some(pixels) => pixels,
            None => {
                let pixels = std::sync::Arc::new(self.cover_case(c, source)?);
                *cache = Some(super::runtime::PreviewCache {
                    generation: self.theme.generation(),
                    screen: input.screen,
                    component: c.id.clone(),
                    source: source.clone(),
                    pixels: pixels.clone(),
                });
                pixels
            }
        };
        let x = c.rect.0 + c.visible_count.unwrap() * (c.item_extent.unwrap() + c.gap.unwrap());
        crate::perf_trace::count(
            crate::perf_trace::Count::ComposedPixels,
            rect.2 as u64 * rect.3 as u64,
        );
        crate::perf_trace::count(
            crate::perf_trace::Count::OpacityPixels,
            pixels.width().min(self.pixels.width().saturating_sub(x)) as u64
                * pixels.height().min(self.pixels.height().saturating_sub(c.rect.1)) as u64,
        );
        self.pixels.as_mut().draw_pixmap(
            x as i32,
            c.rect.1 as i32,
            pixels.as_ref().as_ref(),
            &PixmapPaint {
                opacity: 0.62,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        if cache_tile {
            self.tiles.capture(key, &self.pixels, rect);
        }
        Ok(())
    }
    fn shelf_glow(&mut self, c: &Component, case: Rect) {
        if c.focus_style != Some(FocusStyle::Glow) {
            return;
        }
        for ring in (1..=c.glow_radius.unwrap()).rev() {
            let color = self
                .color(c.focus_color.unwrap())
                .with_a((110 / (ring + 1)) as u8);
            fill_rounded_rect(
                &mut self.pixels.as_mut(),
                draw_rect(Rect(
                    case.0.saturating_sub(ring),
                    case.1.saturating_sub(ring),
                    case.2 + ring * 2,
                    case.3 + ring * 2,
                )),
                c.radius.unwrap() + ring,
                color,
            );
        }
    }
    fn shelf_art(
        &mut self,
        c: &Component,
        case: Rect,
        row: &Row,
        input: &FrameInputs,
        focused: bool,
    ) -> Result<()> {
        if focused {
            self.shelf_glow(c, case);
        }
        self.panel(case, c.frame_color.unwrap(), c.radius.unwrap());
        let pad = c.padding.unwrap();
        let art = c.art_size.unwrap();
        if let Some(image) = input.art.entries.get(&row.id) {
            self.rounded_image(
                Rect(case.0 + pad, case.1 + pad, art, art),
                image,
                c,
                c.radius.unwrap().saturating_sub(pad),
            )?;
        }
        if focused {
            self.focus_outline(
                case,
                c.focus_color.unwrap(),
                c.focus_width.unwrap(),
                c.radius.unwrap(),
            );
        }
        Ok(())
    }
    fn shelf_caption(&self, c: &Component, x: u32) -> Rect {
        let line = self.theme.pack().tokens.typography[&c.role.unwrap()].line_height;
        let pad = c.padding.unwrap();
        let height = line * c.caption_lines.unwrap();
        Rect(
            x + pad,
            c.rect.1 + pad + c.art_size.unwrap() + c.caption_height.unwrap() - height,
            c.item_extent.unwrap() - 2 * pad,
            height,
        )
    }
    fn shelf_entry(&mut self, c: &Component, row: &Row, x: u32, input: &FrameInputs) -> Result<()> {
        let focused = input.focus.component == c.id && input.focus.item.as_ref() == Some(&row.id);
        let tile_rect = shelf_tile_rect(c, x);
        let caption = self.shelf_caption(c, x);
        let key = tiles::Key {
            label: &row.label,
            source: input.art.entries.get(&row.id),
            focused,
            preview: false,
        };
        let cache_tile = self.retained_layout && retained::eligible(input);
        if cache_tile && self.tiles.paint(&key, &mut self.pixels, tile_rect) {
            self.text_boxes.push((c.id.clone(), caption));
        } else {
            if cache_tile {
                let color = self.color(ColorRole::Background);
                crate::display::fill_rect(&mut self.pixels.as_mut(), draw_rect(tile_rect), color);
            }
            let case = Rect(
                x,
                c.rect.1,
                c.item_extent.unwrap(),
                c.art_size.unwrap() + 2 * c.padding.unwrap(),
            );
            self.shelf_art(c, case, row, input, focused)?;
            self.text(
                &c.id,
                caption,
                c.role.unwrap(),
                ColorRole::Text,
                &row.label,
                c.caption_lines.unwrap(),
            )?;
            if cache_tile {
                self.tiles.capture(key, &self.pixels, tile_rect);
            }
        }
        self.targets.push(Target {
            component: c.id.clone(),
            item: row.id.clone(),
            rect: Rect(x, c.rect.1, c.item_extent.unwrap(), c.rect.3),
        });
        Ok(())
    }
    fn entries(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        let rows = input
            .rows
            .get(&c.binding.unwrap())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        ensure!(
            rows.len() <= c.visible_count.unwrap() as usize,
            "Adapter must provide only declared visible window"
        );
        for (i, row) in rows.iter().enumerate() {
            let offset = i as u32 * (c.item_extent.unwrap() + c.gap.unwrap());
            if c.layout == Some(Arrangement::Shelf) {
                self.shelf_entry(c, row, c.rect.0 + offset, input)?;
            } else {
                self.row(
                    c,
                    row,
                    Rect(
                        c.rect.0,
                        c.rect.1 + offset,
                        c.rect.2,
                        c.item_extent.unwrap(),
                    ),
                    input,
                )?;
            }
        }
        if c.layout == Some(Arrangement::Shelf) {
            self.cover_peek(c, input)?;
        }
        Ok(())
    }
    fn keyboard(&mut self, c: &Component, input: &FrameInputs) -> Result<()> {
        if input.keyboard.is_empty() {
            return Ok(());
        }
        let pad = c.padding.unwrap();
        let gap = c.gap.unwrap();
        let font = self.theme.pack().tokens.typography[&c.role.unwrap()].line_height;
        let cell = font + 2 * pad;
        let x = c.rect.0 + (c.rect.2 - (11 * cell + 10 * gap)) / 2;
        let y = c.rect.1 + (c.rect.3 - (5 * cell + 4 * gap)) / 2;
        for (i, label) in input.keyboard.iter().enumerate() {
            let (col, row) = if i == 44 { (0, 4) } else { (i % 11, i / 11) };
            let rect = Rect(
                x + col as u32 * (cell + gap),
                y + row as u32 * (cell + gap),
                if i == 44 { 11 * cell + 10 * gap } else { cell },
                cell,
            );
            self.panel(
                rect,
                if input.keyboard_cursor.1 == row && (row == 4 || input.keyboard_cursor.0 == col) {
                    ColorRole::Focus
                } else {
                    ColorRole::Surface
                },
                c.radius.unwrap(),
            );
            self.text(
                &c.id,
                Rect(rect.0 + pad, rect.1 + pad, rect.2 - 2 * pad, font),
                c.role.unwrap(),
                ColorRole::Text,
                label,
                1,
            )?;
        }
        Ok(())
    }
}
fn draw_rect(r: Rect) -> DrawRect {
    DrawRect::new(r.0 as i32, r.1 as i32, r.2, r.3)
}
pub fn wrap(style: &FontTextStyle, value: &str, width: u32, max_lines: usize) -> Vec<String> {
    let mut lines = vec![];
    let mut line = String::new();
    let mut truncated = false;
    let mut measure = style.prefix_measure();
    for ch in value.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            measure.reset("");
            if lines.len() == max_lines {
                truncated = true;
                break;
            }
            continue;
        }
        let candidate_width = measure.push(ch);
        if !line.is_empty() && candidate_width > width {
            if let Some(at) = line.rfind(char::is_whitespace).filter(|at| *at > 0) {
                let remainder = line[at..].trim_start().to_owned();
                lines.push(line[..at].trim_end().to_owned());
                line = remainder;
            } else {
                lines.push(std::mem::take(&mut line));
            }
            if lines.len() == max_lines {
                truncated = true;
                break;
            }
            measure.reset(&line);
            let width_with_char = measure.push(ch);
            if !line.is_empty() && width_with_char > width {
                lines.push(std::mem::take(&mut line));
                measure.reset("");
                measure.push(ch);
                if lines.len() == max_lines {
                    truncated = true;
                    break;
                }
            }
        }
        line.push(ch);
    }
    if lines.len() < max_lines && !line.is_empty() {
        lines.push(line);
    }
    if truncated {
        if let Some(last) = lines.last_mut() {
            while !last.is_empty() && style.measure(&format!("{last}…")).w > width {
                last.pop();
            }
            last.push('…');
        }
    }
    for line in &mut lines {
        while !line.is_empty() && style.measure(line).w > width {
            line.pop();
        }
    }
    lines
}

pub fn asset_images(display: &mut impl Display, snapshot: &ThemeSnapshot, screen: ScreenId) {
    for c in &snapshot.0.pack.screens[&screen].components {
        if c.kind == Kind::AssetImage {
            if let Some(image) = snapshot
                .0
                .prepared_assets
                .get(&screen)
                .and_then(|images| images.get(&c.id))
            {
                display.pixmap_mut().draw_pixmap(
                    c.rect.0 as i32,
                    c.rect.1 as i32,
                    image.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("render_dl12_bench.rs");
    include!("render_dl15_tests.rs");
    pub(super) struct TestDisplay(pub(super) Pixmap);
    impl Display for TestDisplay {
        fn width(&self) -> u32 {
            self.0.width()
        }
        fn height(&self) -> u32 {
            self.0.height()
        }
        fn pixmap(&self) -> tiny_skia::PixmapRef<'_> {
            self.0.as_ref()
        }
        fn pixmap_mut(&mut self) -> tiny_skia::PixmapMut<'_> {
            self.0.as_mut()
        }
        fn map_pixels<F>(&mut self, mut f: F) -> Result<()>
        where
            F: FnMut(Color) -> Color,
        {
            for pixel in self.0.pixels_mut() {
                let color: Color = (*pixel).into();
                *pixel = f(color).into();
            }
            Ok(())
        }
        fn save(&mut self) -> Result<()> {
            Ok(())
        }
        fn load(&mut self, _: DrawRect) -> Result<()> {
            Ok(())
        }
        fn pop(&mut self) -> bool {
            false
        }
    }
    fn dl13_assert_opaque_frame_matches_draw_pixmap(width: u32, height: u32) {
        let bank =
            super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
        let theme =
            ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
        let mut pixels = Pixmap::new(640, 480).unwrap();
        for (index, pixel) in pixels.pixels_mut().iter_mut().enumerate() {
            *pixel = tiny_skia::PremultipliedColorU8::from_rgba(
                index as u8,
                (index / 640) as u8,
                91,
                255,
            )
            .unwrap();
        }
        let prepared = PreparedFrame {
            theme,
            screen: ScreenId::Library,
            pixels: pixels.into(),
            opaque: true,
            blit: Default::default(),
            previous_input: None,
            component_metadata: vec![],
            retained_layout: false,
            tiles: Default::default(),
            ink_cache: Default::default(),
            damage: vec![],
            navigation_epoch: std::sync::Arc::new(()),
            ink_components: None,
            targets: vec![],
            text_boxes: vec![],
        };
        let mut actual = TestDisplay(Pixmap::new(width, height).unwrap());
        actual.0.fill(tiny_skia::Color::from_rgba8(21, 45, 78, 123));
        let mut expected = actual.0.clone();
        expected.draw_pixmap(
            0,
            0,
            prepared.pixels.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        prepared.paint(&mut actual);
        assert!(
            actual.0.data() == expected.data(),
            "opaque frame differs at {width}x{height}"
        );
    }
    #[test]
    fn dl13_opaque_frame_paints_smaller_destination() {
        dl13_assert_opaque_frame_matches_draw_pixmap(317, 239);
    }
    #[test]
    fn dl13_opaque_frame_paints_larger_destination() {
        dl13_assert_opaque_frame_matches_draw_pixmap(701, 517);
    }
    #[test]
    fn dl12_incremental_wrap_matches_prefix_reference() {
        let bank =
            super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
        let theme =
            ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
        let chars: Vec<char> = "AV To fj 日本中文
  café…	"
            .chars()
            .collect();
        let mut seed = 57u64;
        for role in [Role::Title, Role::Body, Role::Caption] {
            let style = theme.text_style(role, Color::new(25, 36, 47));
            for _ in 0..300 {
                let mut value = String::new();
                for _ in 0..80 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    value.push(chars[(seed >> 32) as usize % chars.len()]);
                }
                let width = 1 + (seed % 250) as u32;
                let lines = 1 + (seed % 5) as usize;
                assert_eq!(
                    wrap(&style, &value, width, lines),
                    wrap_reference(&style, &value, width, lines),
                    "{value:?}"
                );
            }
        }
    }
    fn wrap_reference(
        style: &FontTextStyle,
        value: &str,
        width: u32,
        max_lines: usize,
    ) -> Vec<String> {
        let mut lines = vec![];
        let mut line = String::new();
        let mut truncated = false;
        for ch in value.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut line));
                if lines.len() == max_lines {
                    truncated = true;
                    break;
                }
                continue;
            }
            let mut candidate = line.clone();
            candidate.push(ch);
            if !line.is_empty() && style.measure(&candidate).w > width {
                if let Some(at) = line.rfind(char::is_whitespace).filter(|at| *at > 0) {
                    let remainder = line[at..].trim_start().to_owned();
                    lines.push(line[..at].trim_end().to_owned());
                    line = remainder;
                } else {
                    lines.push(std::mem::take(&mut line));
                }
                if lines.len() == max_lines {
                    truncated = true;
                    break;
                }
                if !line.is_empty() && style.measure(&format!("{line}{ch}")).w > width {
                    lines.push(std::mem::take(&mut line));
                    if lines.len() == max_lines {
                        truncated = true;
                        break;
                    }
                }
            }
            line.push(ch);
        }
        if lines.len() < max_lines && !line.is_empty() {
            lines.push(line);
        }
        if truncated {
            if let Some(last) = lines.last_mut() {
                while !last.is_empty() && style.measure(&format!("{last}…")).w > width {
                    last.pop();
                }
                last.push('…');
            }
        }
        for line in &mut lines {
            while !line.is_empty() && style.measure(line).w > width {
                line.pop();
            }
        }
        lines
    }
    #[test]
    fn dl12_text_cache_reuses_exact_pixels_and_separates_theme_generation_and_layout() {
        let bank =
            super::super::fonts::FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
        let theme =
            ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 41).unwrap();
        let color = ColorRole::Text;
        let rect = Rect(0, 0, 200, 60);
        let key = super::super::text_cache::Key::new(
            &theme,
            Role::Title,
            Color::new(1, 2, 3),
            "Cache text",
            rect,
            2,
            Align::Left,
            Overflow::Wrap,
        );
        // The prepared handle owns both primary and fallback font identity.
        let mut first =
            PreparedFrame::prepare(theme.clone(), &FrameInputs::new(ScreenId::Library)).unwrap();
        first.pixels.data_mut().fill(0);
        first
            .text_options(
                "cache-test",
                rect,
                Role::Title,
                color,
                "Cache text",
                2,
                Align::Left,
                Overflow::Wrap,
            )
            .unwrap();
        let actual_key = super::super::text_cache::Key::new(
            &theme,
            Role::Title,
            first.color(color),
            "Cache text",
            rect,
            2,
            Align::Left,
            Overflow::Wrap,
        );
        let (scratch, _) = super::super::text_cache::get(&theme, &actual_key).unwrap();
        first.pixels.data_mut().fill(0);
        first
            .text_options(
                "cache-test",
                rect,
                Role::Title,
                color,
                "Cache text",
                2,
                Align::Left,
                Overflow::Wrap,
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &scratch,
            &super::super::text_cache::get(&theme, &actual_key)
                .unwrap()
                .0
        ));
        assert!(super::super::text_cache::get(&theme.generation_copy(42), &actual_key).is_none());
        let other =
            ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 41).unwrap();
        assert!(super::super::text_cache::get(&other, &actual_key).is_none());
        assert!(super::super::text_cache::get(&theme, &key).is_none());
        let mut expected = Pixmap::new(640, 480).unwrap();
        expected.draw_pixmap(
            0,
            0,
            scratch.as_ref().as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        assert_eq!(first.pixels.data(), expected.data());
    }
    #[test]
    fn reused_frame_and_opaque_copy_match_fresh_source_over() {
        use super::super::frame::FrameInputs;
        use crate::theme::fonts::FontBank;
        let bank = FontBank::bind(&crate::constants::ALLIUM_FONTS_DIR).unwrap();
        let theme =
            ThemeHandle::prepare(ThemeSnapshot::builtin("orbit").unwrap(), &bank, 1).unwrap();
        let input = FrameInputs::new(ScreenId::Library);
        let fresh = PreparedFrame::prepare(theme.clone(), &input).unwrap();
        let address = fresh.pixels.data().as_ptr();
        let mut changed = FrameInputs::new(ScreenId::Library);
        changed
            .text
            .insert(Binding::SelectedTitle, "A different selection".into());
        let expected_frame = PreparedFrame::prepare(theme.clone(), &changed).unwrap();
        let mut reused =
            PreparedFrame::prepare_reusing(theme.clone(), &changed, Some(fresh)).unwrap();
        assert_eq!(address, reused.pixels.data().as_ptr());
        assert_eq!(expected_frame.pixels.data(), reused.pixels.data());
        for transparent in [false, true] {
            if transparent {
                reused.pixels.data_mut()[..4].fill(0);
                reused.opaque = false;
                reused.blit = super::super::blit::BlitMap::new(reused.pixels.as_ref());
            }
            let mut expected = TestDisplay(Pixmap::new(640, 480).unwrap());
            expected
                .0
                .fill(tiny_skia::Color::from_rgba8(31, 45, 91, 255));
            let mut actual = TestDisplay(expected.0.clone());
            expected.0.draw_pixmap(
                0,
                0,
                reused.pixels.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            reused.paint(&mut actual);
            assert_eq!(expected.0.data(), actual.0.data());
        }
    }
    #[test]
    fn theme_render_real_asset_pixels_and_dual_geometry() {
        for id in ["orbit", "compact"] {
            let snapshot = ThemeSnapshot::builtin(id).unwrap();
            let mut display = TestDisplay(Pixmap::new(640, 480).unwrap());
            asset_images(&mut display, &snapshot, ScreenId::Library);
            if id == "orbit" {
                assert!(
                    snapshot
                        .0
                        .pack
                        .component(ScreenId::Library, "accent-band")
                        .is_none()
                );
                assert!(
                    display
                        .pixmap()
                        .pixels()
                        .iter()
                        .all(|pixel| pixel.alpha() == 0)
                );
                continue;
            }
            let c = snapshot
                .0
                .pack
                .component(ScreenId::Library, "accent-band")
                .unwrap();
            let pixel = display.pixmap().pixel(c.rect.0, c.rect.1).unwrap();
            assert_eq!(pixel.alpha(), 255);
            assert!(pixel.blue() > 100);
            assert_eq!(display.pixmap().pixel(639, 479).unwrap().alpha(), 0);
            if let Ok(root) = std::env::var("ALLIUM_THEME_PROOF_DIR") {
                display
                    .pixmap()
                    .to_owned()
                    .save_png(
                        std::path::Path::new(&root).join(format!("theme-{id}-asset-primitive.png")),
                    )
                    .unwrap();
            }
        }
    }
}

#[path = "render_ink.rs"]
mod ink_renderer;

pub use ink_renderer::{InkTextLayout, ink_detail_pages, ink_text_layout};

#[cfg(all(test, feature = "perf-bench"))]
#[path = "render_selection_bench.rs"]
mod selection_bench;

#[cfg(feature = "perf-bench")]
#[doc(hidden)]
pub fn benchmark_blit(dest: &mut tiny_skia::PixmapMut<'_>, x: i32, y: i32, source: tiny_skia::PixmapRef<'_>) {
    ink_blit::draw(dest, x, y, source);
}

#[cfg(test)]
mod dense_damage_tests {
    use super::*;
    #[test]
    fn dense_copy_and_exact_scanner_produce_identical_pixels() {
        let rect = Rect(425, 16, 206, 408);
        let source: Vec<u8> = (0..640 * 480 * 4).map(|i| (i * 17 + i / 257) as u8).collect();
        for density in [0, 1, 25, 50, 77, 100] {
            let mut initial = source.clone();
            for y in rect.1..rect.1 + rect.3 {
                for x in rect.0..rect.0 + rect.2 {
                    if ((x * 73 + y * 37) % 100) < density {
                        initial[((y * 640 + x) * 4) as usize] ^= 123;
                    }
                }
            }
            let mut exact = initial.clone();
            for y in rect.1..rect.1 + rect.3 {
                let at = ((y * 640 + rect.0) * 4) as usize;
                let end = at + rect.2 as usize * 4;
                for (first, last) in changed_runs(&source[at..end], &exact[at..end]) {
                    exact[at + first * 4..at + last * 4].copy_from_slice(&source[at + first * 4..at + last * 4]);
                }
            }
            let mut dense = initial.clone();
            copy_damage(&source, &mut dense, rect);
            assert_eq!(dense, exact, "density {density}");
            assert_eq!(dense, source);
            if density == 0 || density == 100 {
                assert_eq!(dense_damage(&source, &initial, rect), density == 100);
            }
        }
        assert!(!dense_damage(&source, &vec![0; source.len()], Rect(0, 0, 3, 3)));
    }
    #[test]
    fn word_comparison_checks_unaligned_words_tails_lengths_and_run_gaps() {
        let bytes = vec![42; 67];
        for len in 0..65 {
            assert!(equal_words(&bytes[1..1 + len], &bytes[2..2 + len]));
            for at in 0..len {
                let mut changed = bytes[1..1 + len].to_vec();
                changed[at] ^= 1;
                assert!(!equal_words(&changed, &bytes[2..2 + len]));
            }
        }
        assert!(!equal_words(&bytes[..1], &bytes[..2]));
        let source = [0; 80];
        for positions in [vec![0, 7], vec![0, 8], vec![0, 9], vec![19], vec![]] {
            let mut target = source;
            for x in &positions { target[x * 4] = 1; }
            let runs = changed_runs(&source, &target);
            let expected = match positions.as_slice() {
                [0, 7] => vec![(0, 8)],
                [0, 8] => vec![(0, 9)],
                [0, 9] => vec![(0, 1), (9, 10)],
                [19] => vec![(19, 20)],
                _ => vec![],
            };
            assert_eq!(runs, expected);
        }
    }
}

#[cfg(test)]
#[path = "render_neighbor_tests.rs"]
mod neighbor_tests;
