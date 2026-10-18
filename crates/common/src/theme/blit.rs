use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, Weak},
};
use tiny_skia::{Mask, Pixmap, PixmapMut, PixmapPaint, PixmapRef, Transform};

pub(super) const MAP_LIMIT: usize = 64 * 1024;
const CACHE_LIMIT: usize = 96 * 1024;
const ENTRY_LIMIT: usize = 32;
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Opaque,
    Constant,
    Mixed,
}

struct ConstantScrim {
    source: [u8; 4],
}
impl ConstantScrim {
    fn new(source: [u8; 4]) -> Self {
        Self { source }
    }

    fn draw(&self, dest: &mut PixmapMut<'_>, x: i32, y: i32, source: &[u8]) {
        let start = (y as usize * dest.width() as usize + x as usize) * 4;
        super::ink_blit_neon::constant_source_over(
            &mut dest.data_mut()[start..start + source.len()],
            self.source,
        );
    }
}

fn draw_mixed(dest: &mut PixmapMut<'_>, x: i32, y: i32, source: &[u8]) {
    let start = (y as usize * dest.width() as usize + x as usize) * 4;
    super::ink_blit_neon::source_over(&mut dest.data_mut()[start..start + source.len()], source);
}

#[derive(Clone, Copy)]
struct Run {
    row: u32,
    start: u32,
    end: u32,
    kind: RunKind,
}
#[derive(Default)]
pub(super) struct BlitMap {
    runs: Vec<Run>,
    fallback: bool,
    constant: Option<Box<ConstantScrim>>,
}
impl BlitMap {
    pub fn new(image: PixmapRef<'_>) -> Self {
        Self::build(image, None)
    }
    pub fn with_constant_scrim(image: PixmapRef<'_>) -> Self {
        let source: [u8; 4] = image.data()[..4].try_into().unwrap();
        let constant =
            (source[3] > 0 && source[3] < 255).then(|| Box::new(ConstantScrim::new(source)));
        Self::build(image, constant)
    }
    fn build(image: PixmapRef<'_>, constant: Option<Box<ConstantScrim>>) -> Self {
        crate::perf_trace::count(crate::perf_trace::Count::MapBuild, 1);
        let mut result = Self {
            constant,
            ..Self::default()
        };
        let width = image.width() as usize;
        for (row, pixels) in image.data().chunks_exact(width * 4).enumerate() {
            let classify = |x: usize| {
                let p = &pixels[x * 4..x * 4 + 4];
                if p[3] == 0 {
                    None
                } else if p[3] == 255 {
                    Some(RunKind::Opaque)
                } else if result.constant.as_ref().is_some_and(|c| p == c.source) {
                    Some(RunKind::Constant)
                } else {
                    Some(RunKind::Mixed)
                }
            };
            let mut start = 0;
            while start < width {
                let kind = classify(start);
                let mut end = start + 1;
                if kind != Some(RunKind::Mixed) {
                    while end < width && classify(end) == kind {
                        end += 1;
                    }
                }
                let kind = if kind.is_some() && end - start < 8 {
                    Some(RunKind::Mixed)
                } else {
                    kind
                };
                if kind == Some(RunKind::Mixed) {
                    while end < width {
                        let next = classify(end);
                        if next != Some(RunKind::Mixed)
                            && (end..(end + 8).min(width)).all(|x| classify(x) == next)
                        {
                            break;
                        }
                        end += 1;
                    }
                }
                if let Some(kind) = kind {
                    if (result.runs.len() + 1) * std::mem::size_of::<Run>() > MAP_LIMIT {
                        return Self {
                            fallback: true,
                            ..Self::default()
                        };
                    }
                    result.runs.push(Run {
                        row: row as u32,
                        start: start as u32,
                        end: end as u32,
                        kind,
                    });
                }
                start = end;
            }
        }
        result.runs.shrink_to_fit();
        result
    }
    pub fn bytes(&self) -> usize {
        self.runs.capacity() * std::mem::size_of::<Run>()
            + std::mem::size_of::<Self>()
            + self
                .constant
                .as_ref()
                .map_or(0, |_| std::mem::size_of::<ConstantScrim>())
    }
    pub fn draw(
        &self,
        dest: &mut PixmapMut<'_>,
        x: i32,
        y: i32,
        source: PixmapRef<'_>,
        opacity: f32,
        mask: Option<&Mask>,
    ) {
        let _phase = crate::perf_trace::phase(crate::perf_trace::Phase::Blit);
        let w = (x as i64 + source.width() as i64)
            .min(dest.width() as i64)
            .saturating_sub((x as i64).max(0))
            .max(0) as u64;
        let h = (y as i64 + source.height() as i64)
            .min(dest.height() as i64)
            .saturating_sub((y as i64).max(0))
            .max(0) as u64;
        crate::perf_trace::count(crate::perf_trace::Count::ComposedPixels, w * h);
        if self.fallback || mask.is_some() || opacity != 1.0 {
            dest.draw_pixmap(
                x,
                y,
                source,
                &PixmapPaint {
                    opacity,
                    ..Default::default()
                },
                Transform::identity(),
                mask,
            );
            return;
        }
        let dw = dest.width() as i64;
        let dh = dest.height() as i64;
        let sw = source.width() as usize;
        for run in &self.runs {
            let dy = y as i64 + run.row as i64;
            if dy < 0 || dy >= dh {
                continue;
            }
            let left = (x as i64 + run.start as i64).max(0);
            let right = (x as i64 + run.end as i64).min(dw);
            if left >= right {
                continue;
            }
            let sx = (left - x as i64) as usize;
            let len = (right - left) as usize;
            let at = (run.row as usize * sw + sx) * 4;
            let data = &source.data()[at..at + len * 4];
            if run.kind == RunKind::Opaque {
                let at = (dy * dw + left) as usize * 4;
                super::ink_blit_neon::opaque_copy(&mut dest.data_mut()[at..at + len * 4], data);
            } else if run.kind == RunKind::Constant {
                self.constant
                    .as_ref()
                    .unwrap()
                    .draw(dest, left as i32, dy as i32, data);
            } else {
                draw_mixed(dest, left as i32, dy as i32, data);
            }
        }
    }
}
struct Entry {
    source: Weak<Pixmap>,
    map: Arc<BlitMap>,
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    bytes: usize,
}
pub(super) fn clear() { CACHE.with(|c| *c.borrow_mut() = Cache::default()); }
thread_local! { static CACHE: RefCell<Cache> = RefCell::new(Cache::default()); }
pub(super) fn draw_cached(
    dest: &mut PixmapMut<'_>,
    x: i32,
    y: i32,
    source: &Arc<Pixmap>,
    opacity: f32,
) {
    let map = CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some(at) = cache
            .entries
            .iter()
            .position(|e| e.source.upgrade().is_some_and(|s| Arc::ptr_eq(&s, source)))
        {
            let entry = cache.entries.remove(at).unwrap();
            let map = entry.map.clone();
            cache.entries.push_front(entry);
            return map;
        }
        let map = Arc::new(BlitMap::new(source.as_ref().as_ref()));
        let bytes = map.bytes() + std::mem::size_of::<Entry>();
        while cache.bytes + bytes > CACHE_LIMIT || cache.entries.len() >= ENTRY_LIMIT {
            let old = cache.entries.pop_back().unwrap();
            cache.bytes -= old.map.bytes() + std::mem::size_of::<Entry>();
        }
        cache.bytes += bytes;
        cache.entries.push_front(Entry {
            source: Arc::downgrade(source),
            map: map.clone(),
        });
        map
    });
    map.draw(dest, x, y, source.as_ref().as_ref(), opacity, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scrim_frame(width: u32, height: u32, panel: crate::geom::Rect, radius: u32) -> Pixmap {
        let mut frame = Pixmap::new(width, height).unwrap();
        frame.fill(tiny_skia::Color::from_rgba8(24, 35, 42, 153));
        crate::display::fill_rounded_rect(
            &mut frame.as_mut(),
            panel,
            radius,
            crate::display::color::Color::rgba(245, 247, 249, 255),
        );
        frame
    }

    #[test]
    fn blit_constant_scrim_all_alpha_nonopaque_destinations_match_pattern() {
        for a in 1..255u8 {
            let mut source = Pixmap::new(256, 1).unwrap();
            for p in source.data_mut().chunks_exact_mut(4) {
                p.copy_from_slice(&[a, a / 2, a / 3, a]);
            }
            let map = BlitMap::with_constant_scrim(source.as_ref());
            let mut expected = Pixmap::new(256, 1).unwrap();
            for (alpha, p) in expected.data_mut().chunks_exact_mut(4).enumerate() {
                let alpha = alpha as u8;
                p.copy_from_slice(&[alpha / 3, alpha, alpha / 2, alpha]);
            }
            let mut actual = expected.clone();
            expected.draw_pixmap(
                0,
                0,
                source.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            map.draw(&mut actual.as_mut(), 0, 0, source.as_ref(), 1.0, None);
            assert_eq!(actual.data(), expected.data(), "constant source alpha={a}");
        }
    }

    #[test]
    fn perf9_randomized_scrim_lut_matches_reference_and_repeated_draws() {
        let mut seed = 881937u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u32
        };
        for case in 0..180 {
            let (w, h) = if case < 4 {
                (640, 480)
            } else {
                (40 + random() % 90, 25 + random() % 50)
            };
            let panel = crate::geom::Rect::new(
                (random() % w) as i32 - 15,
                (random() % h) as i32 - 10,
                1 + random() % w,
                1 + random() % h,
            );
            let radius = random() % 30;
            let mut source = scrim_frame(w, h, panel, radius);
            if case % 3 == 0 {
                crate::display::fill_rounded_rect(
                    &mut source.as_mut(),
                    crate::geom::Rect::new(13, 9, 21, 17),
                    6,
                    crate::display::color::Color::rgba(19, 87, 120, 97),
                );
            }
            let map = BlitMap::with_constant_scrim(source.as_ref());
            for opaque in [true, false] {
                let mut expected = Pixmap::new(w + 3, h + 2).unwrap();
                for p in expected.data_mut().chunks_exact_mut(4) {
                    let a = if opaque { 255 } else { random() as u8 };
                    for ch in &mut p[..3] {
                        *ch = (random() % (a as u32 + 1)) as u8;
                    }
                    p[3] = a;
                }
                let mut actual = expected.clone();
                for (x, y) in [(0, 0), (-9, -3), (2, 1), (0, 0)] {
                    expected.draw_pixmap(
                        x,
                        y,
                        source.as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                    map.draw(&mut actual.as_mut(), x, y, source.as_ref(), 1.0, None);
                    assert_eq!(
                        actual.data(),
                        expected.data(),
                        "case={case} opaque={opaque} offset={x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn perf9_all_channel_values_and_fresh_backgrounds_match_reference() {
        for color in [[14, 21, 25, 153], [0, 0, 0, 1], [120, 87, 3, 254]] {
            let mut source = Pixmap::new(256, 3).unwrap();
            for p in source.data_mut().chunks_exact_mut(4) {
                p.copy_from_slice(&color);
            }
            let map = BlitMap::with_constant_scrim(source.as_ref());
            assert!(map.constant.is_some());
            assert_eq!(std::mem::size_of::<ConstantScrim>(), 4);
            assert!(map.runs.iter().all(|r| r.kind == RunKind::Constant));
            for capture in 0..4 {
                let mut expected = Pixmap::new(256, 3).unwrap();
                for (i, p) in expected.data_mut().chunks_exact_mut(4).enumerate() {
                    p.copy_from_slice(&[
                        (i + capture) as u8,
                        (i * 37 + capture) as u8,
                        (i * 151 + capture) as u8,
                        255,
                    ]);
                }
                let mut actual = expected.clone();
                expected.draw_pixmap(
                    0,
                    0,
                    source.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                map.draw(&mut actual.as_mut(), 0, 0, source.as_ref(), 1.0, None);
                assert_eq!(actual.data(), expected.data());
            }
        }
    }

    #[test]
    #[ignore]
    fn perf9_host_menu_compose_benchmark() {
        use std::{hint::black_box, time::Instant};
        let source = scrim_frame(640, 480, crate::geom::Rect::new(90, 64, 460, 352), 16);
        let before = BlitMap::new(source.as_ref());
        let after = BlitMap::with_constant_scrim(source.as_ref());
        let mut background = Pixmap::new(640, 480).unwrap();
        for (i, p) in background.data_mut().chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[i as u8, (i * 37) as u8, (i * 151) as u8, 255]);
        }
        let mut dest = background.clone();
        for (label, map) in [("before", &before), ("after", &after)] {
            let mut samples = Vec::new();
            for _ in 0..101 {
                dest.data_mut().copy_from_slice(background.data());
                let start = Instant::now();
                map.draw(&mut dest.as_mut(), 0, 0, source.as_ref(), 1.0, None);
                samples.push(start.elapsed().as_nanos());
                black_box(dest.data());
            }
            samples.sort_unstable();
            println!(
                "PERF9_HOST_MENU_COMPOSE {}",
                serde_json::json!({"variant":label,"median_ns":samples[50],"samples":samples.len(),"release":!cfg!(debug_assertions),"width":640,"height":480,"synthetic_menu":true})
            );
        }
    }

    #[test]
    fn dl12_randomized_identity_blits_match_tiny_skia() {
        let mut seed = 7182u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u32
        };
        for case in 0..400 {
            let w = 1 + random() % 87;
            let h = 1 + random() % 31;
            let mut source = Pixmap::new(w, h).unwrap();
            let mut background = Pixmap::new(83, 39).unwrap();
            for image in [&mut source, &mut background] {
                for p in image.data_mut().chunks_exact_mut(4) {
                    let a = match random() % 5 {
                        0 => 0,
                        1 | 2 => 255,
                        _ => random() as u8,
                    };
                    for ch in &mut p[..3] {
                        *ch = (random() % (a as u32 + 1)) as u8;
                    }
                    p[3] = a;
                }
            }
            if case % 3 == 0 {
                for row in source.data_mut().chunks_exact_mut(w as usize * 4) {
                    for p in row.chunks_exact_mut(4).take((w / 2) as usize) {
                        p.copy_from_slice(&[19, 28, 37, 255]);
                    }
                }
            }
            let map = BlitMap::new(source.as_ref());
            let x = (random() % 130) as i32 - 70;
            let y = (random() % 60) as i32 - 25;
            for opacity in [0.0, 0.62, 1.0] {
                for masked in [false, true] {
                    let mut mask = Mask::new(83, 39).unwrap();
                    for b in mask.data_mut() {
                        *b = random() as u8;
                    }
                    let mask = masked.then_some(&mask);
                    let mut expected = background.clone();
                    let mut actual = background.clone();
                    expected.draw_pixmap(
                        x,
                        y,
                        source.as_ref(),
                        &PixmapPaint {
                            opacity,
                            ..Default::default()
                        },
                        Transform::identity(),
                        mask,
                    );
                    map.draw(&mut actual.as_mut(), x, y, source.as_ref(), opacity, mask);
                    assert_eq!(
                        actual.data(),
                        expected.data(),
                        "case {case}, {x},{y}, opacity {opacity}, mask {masked}"
                    );
                }
            }
        }
    }
    #[test]
    fn dl12_fragmented_metadata_falls_back_with_exact_pixels() {
        let mut source = Pixmap::new(640, 480).unwrap();
        for (i, pixel) in source.data_mut().chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(if i / 8 % 2 == 0 {
                &[12, 23, 34, 255]
            } else {
                &[3, 4, 5, 8]
            });
        }
        let map = BlitMap::new(source.as_ref());
        assert!(map.fallback);
        assert!(map.bytes() <= MAP_LIMIT);
        let mut expected = Pixmap::new(73, 51).unwrap();
        expected.fill(tiny_skia::Color::WHITE);
        let mut actual = expected.clone();
        expected.draw_pixmap(
            -19,
            -31,
            source.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        map.draw(&mut actual.as_mut(), -19, -31, source.as_ref(), 1.0, None);
        assert_eq!(expected.data(), actual.data());
    }
    #[test]
    fn dl12_metadata_cache_is_bounded_and_identity_checked() {
        let mut dest = Pixmap::new(30, 30).unwrap();
        for _ in 0..100 {
            let mut source = Pixmap::new(120, 120).unwrap();
            source.fill(tiny_skia::Color::WHITE);
            let source = Arc::new(source);
            draw_cached(&mut dest.as_mut(), 0, 0, &source, 1.0);
            assert_eq!(Arc::strong_count(&source), 1);
            CACHE.with(|c| {
                let c = c.borrow();
                assert!(c.bytes <= CACHE_LIMIT);
                assert!(c.entries.len() <= ENTRY_LIMIT);
            });
        }
    }
}
