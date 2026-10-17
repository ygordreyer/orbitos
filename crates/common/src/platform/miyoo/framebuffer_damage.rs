use super::{Framebuffer, Rect};
use std::cell::RefCell;

#[derive(PartialEq, Eq)]
struct Layout([u64; 10]);
impl Layout {
    fn of(iface: &Framebuffer) -> Self {
        let v = &iface.var_screen_info;
        let f = &iface.fix_screen_info;
        Self([
            v.xres as u64,
            v.yres as u64,
            v.xres_virtual as u64,
            v.yres_virtual as u64,
            v.bits_per_pixel as u64,
            v.xoffset as u64,
            f.line_length as u64,
            f.smem_start as u64,
            f.smem_len as u64,
            iface.frame.len() as u64,
        ])
    }
}
struct Page {
    valid: bool,
    rows: Vec<u64>,
}
#[derive(Clone, Copy)]
struct RowRun {
    end: usize,
    y: usize,
    area: usize,
}

pub(super) struct PageHistory {
    layout: Layout,
    width: u32,
    height: u32,
    groups: usize,
    words: usize,
    pages: Vec<Page>,
    runs: RefCell<Vec<RowRun>>,
}
impl PageHistory {
    pub fn new(iface: &Framebuffer) -> Option<Self> {
        let v = &iface.var_screen_info;
        let count = v.yres_virtual.checked_div(v.yres)? as usize;
        if !(1..=3).contains(&count) {
            return None;
        }
        let groups = v.xres.div_ceil(8) as usize;
        let words = groups.div_ceil(64);
        Some(Self {
            layout: Layout::of(iface),
            width: v.xres,
            height: v.yres,
            groups,
            words,
            runs: RefCell::new(vec![
                RowRun {
                    end: 0,
                    y: usize::MAX,
                    area: 0
                };
                groups
            ]),
            pages: (0..count)
                .map(|_| Page {
                    valid: false,
                    rows: vec![0; v.yres as usize * words],
                })
                .collect(),
        })
    }
    pub fn matches(&self, iface: &Framebuffer) -> bool {
        self.layout == Layout::of(iface)
    }
    pub fn page(&self, yoffset: u32) -> Option<usize> {
        let page = (yoffset / self.height) as usize;
        (yoffset % self.height == 0 && page < self.pages.len()).then_some(page)
    }
    pub fn is_valid(&self, page: usize) -> bool {
        self.pages[page].valid
    }
    pub fn needs_repair(&self, page: usize) -> bool {
        !self.pages[page].valid || self.pages[page].rows.iter().any(|word| *word != 0)
    }
    pub fn add(&mut self, areas: &[Rect]) {
        for area in areas {
            let x0 = area.x.max(0).min(self.width as i32) as u32;
            let x1 = area.right().max(0).min(self.width as i32) as u32;
            let y0 = area.y.max(0).min(self.height as i32) as usize;
            let y1 = area.bottom().max(0).min(self.height as i32) as usize;
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            let start = (x0 / 8) as usize;
            let end = x1.div_ceil(8) as usize;
            for index in start / 64..(end - 1) / 64 + 1 {
                let left = start.saturating_sub(index * 64);
                let right = (end - index * 64).min(64);
                let mask = (u64::MAX << left) & (u64::MAX >> (64 - right));
                for page in &mut self.pages {
                    for row in page.rows[y0 * self.words..y1 * self.words]
                        .chunks_exact_mut(self.words)
                    {
                        row[index] |= mask;
                    }
                }
            }
        }
    }
    pub fn damage(&self, page: usize) -> Vec<Rect> {
        if !self.pages[page].valid {
            return vec![Rect::new(0, 0, self.width, self.height)];
        }
        let mut areas: Vec<Rect> = vec![];
        let mut runs = self.runs.borrow_mut();
        // A run can continue only from the immediately preceding row in this call.
        runs.fill(RowRun {
            end: 0,
            y: usize::MAX,
            area: 0,
        });
        for (y, row) in self.pages[page].rows.chunks_exact(self.words).enumerate() {
            let mut x = 0;
            while x < self.groups {
                let word = row[x / 64] >> (x % 64);
                if word == 0 {
                    x = (x / 64 + 1) * 64;
                    continue;
                }
                x += word.trailing_zeros() as usize;
                if x >= self.groups {
                    break;
                }
                let first = x;
                loop {
                    let remaining = 64 - x % 64;
                    let count = (row[x / 64] >> (x % 64)).trailing_ones() as usize;
                    x += count.min(self.groups - x);
                    if x == self.groups || count < remaining {
                        break;
                    }
                }
                let previous = runs[first];
                let index = if previous.y.checked_add(1) == Some(y) && previous.end == x {
                    areas[previous.area].h += 1;
                    previous.area
                } else {
                    areas.push(Rect::new(
                        (first * 8) as i32,
                        y as i32,
                        (x as u32 * 8).min(self.width) - first as u32 * 8,
                        1,
                    ));
                    if areas.len() > 4096 {
                        return vec![Rect::new(0, 0, self.width, self.height)];
                    }
                    areas.len() - 1
                };
                runs[first] = RowRun {
                    end: x,
                    y,
                    area: index,
                };
            }
        }
        areas
    }
    pub fn presented(&mut self, page: usize) {
        self.pages[page].valid = true;
        self.pages[page].rows.fill(0);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn history(width: u32, height: u32) -> PageHistory {
        let groups = width.div_ceil(8) as usize;
        let words = groups.div_ceil(64);
        PageHistory {
            layout: Layout([0; 10]),
            width,
            height,
            groups,
            words,
            pages: (0..3)
                .map(|_| Page {
                    valid: false,
                    rows: vec![0; height as usize * words],
                })
                .collect(),
            runs: RefCell::new(vec![
                RowRun {
                    end: 0,
                    y: usize::MAX,
                    area: 0
                };
                groups
            ]),
        }
    }

    fn original_add(history: &mut PageHistory, areas: &[Rect]) {
        for area in areas {
            let x0 = area.x.max(0).min(history.width as i32) as u32;
            let x1 = area.right().max(0).min(history.width as i32) as u32;
            let y0 = area.y.max(0).min(history.height as i32) as usize;
            let y1 = area.bottom().max(0).min(history.height as i32) as usize;
            if x0 >= x1 || y0 >= y1 { continue; }
            for page in &mut history.pages {
                for y in y0..y1 {
                    let words = &mut page.rows[y * history.words..(y + 1) * history.words];
                    let start = (x0 / 8) as usize;
                    let end = x1.div_ceil(8) as usize;
                    for index in start / 64..(end - 1) / 64 + 1 {
                        let left = start.saturating_sub(index * 64);
                        let right = (end - index * 64).min(64);
                        words[index] |= (u64::MAX << left) & (u64::MAX >> (64 - right));
                    }
                }
            }
        }
    }

    fn bitwise_damage(history: &PageHistory, page: usize) -> Vec<Rect> {
        if !history.pages[page].valid {
            return vec![Rect::new(0, 0, history.width, history.height)];
        }
        let mut areas: Vec<Rect> = vec![];
        let mut runs = history.runs.borrow_mut();
        // A run can continue only from the immediately preceding row in this call.
        runs.fill(RowRun {
            end: 0,
            y: usize::MAX,
            area: 0,
        });
        for (y, row) in history.pages[page].rows.chunks_exact(history.words).enumerate() {
            let mut x = 0;
            while x < history.groups {
                if row[x / 64] & (1u64 << (x % 64)) == 0 {
                    x += 1;
                    continue;
                }
                let first = x;
                while x < history.groups && row[x / 64] & (1u64 << (x % 64)) != 0 {
                    x += 1;
                }
                let previous = runs[first];
                let index = if previous.y.checked_add(1) == Some(y) && previous.end == x {
                    areas[previous.area].h += 1;
                    previous.area
                } else {
                    areas.push(Rect::new(
                        (first * 8) as i32,
                        y as i32,
                        (x as u32 * 8).min(history.width) - first as u32 * 8,
                        1,
                    ));
                    if areas.len() > 4096 {
                        return vec![Rect::new(0, 0, history.width, history.height)];
                    }
                    areas.len() - 1
                };
                runs[first] = RowRun {
                    end: x,
                    y,
                    area: index,
                };
            }
        }
        areas
    }

    fn reference(history: &PageHistory, page: usize) -> Vec<Rect> {
        if !history.pages[page].valid {
            return vec![Rect::new(0, 0, history.width, history.height)];
        }
        let mut areas: Vec<Rect> = vec![];
        let mut previous: Vec<(usize, usize, usize)> = vec![];
        for (y, row) in history.pages[page]
            .rows
            .chunks_exact(history.words)
            .enumerate()
        {
            let mut current = vec![];
            let mut x = 0;
            while x < history.groups {
                if row[x / 64] & (1 << (x % 64)) == 0 {
                    x += 1;
                    continue;
                }
                let first = x;
                while x < history.groups && row[x / 64] & (1 << (x % 64)) != 0 {
                    x += 1;
                }
                let index = if let Some(&(_, _, index)) = previous
                    .iter()
                    .find(|&&(start, end, _)| start == first && end == x)
                {
                    areas[index].h += 1;
                    index
                } else {
                    areas.push(Rect::new(
                        (first * 8) as i32,
                        y as i32,
                        (x as u32 * 8).min(history.width) - first as u32 * 8,
                        1,
                    ));
                    if areas.len() > 4096 {
                        return vec![Rect::new(0, 0, history.width, history.height)];
                    }
                    areas.len() - 1
                };
                current.push((first, x, index));
            }
            previous = current;
        }
        areas
    }

    #[test]
    fn reusable_runs_match_original_for_pages_edges_and_fragmentation() {
        let mut seed = 0xabcd1234u32;
        for (width, height) in [(1, 1), (17, 19), (73, 51), (640, 480), (1031, 31)] {
            let mut h = history(width, height);
            let scratch = (h.runs.borrow().as_ptr(), h.runs.borrow().capacity());
            for iteration in 0..70 {
                if iteration == 1 {
                    for page in 0..3 {
                        h.presented(page);
                    }
                }
                let page = iteration % 3;
                let rects = match iteration % 5 {
                    0 => vec![],
                    1 => vec![Rect::new(0, 0, width, height)],
                    2 => (0..width.div_ceil(16))
                        .map(|x| Rect::new((x * 16) as i32, 0, 1, height))
                        .collect(),
                    _ => (0..11)
                        .map(|_| {
                            seed ^= seed << 13;
                            seed ^= seed >> 17;
                            seed ^= seed << 5;
                            Rect::new(
                                (seed % (width * 2)) as i32 - width as i32 / 2,
                                (seed.rotate_left(7) % (height * 2)) as i32 - height as i32 / 2,
                                seed.rotate_left(13) % (width + 1),
                                seed.rotate_left(19) % (height + 1),
                            )
                        })
                        .collect(),
                };
                h.add(&rects);
                for observed in 0..3 {
                    assert_eq!(
                        h.damage(observed),
                        reference(&h, observed),
                        "{width}x{height} iteration={iteration} page={observed}"
                    );
                    // Repeating the query must reset row stamps without growing scratch.
                    assert_eq!(h.damage(observed), reference(&h, observed));
                    assert_eq!(
                        (h.runs.borrow().as_ptr(), h.runs.borrow().capacity()),
                        scratch
                    );
                }
                h.presented(page);
            }
            // Force the bounded-output fallback with alternating runs in adjacent rows.
            for page in &mut h.pages {
                page.valid = true;
                for (y, row) in page.rows.chunks_exact_mut(h.words).enumerate() {
                    row.fill(if y % 2 == 0 {
                        0x5555555555555555
                    } else {
                        0xaaaaaaaaaaaaaaaa
                    });
                }
            }
            assert_eq!(h.damage(0), reference(&h, 0));
            if width == 640 {
                assert_eq!(h.damage(0), vec![Rect::new(0, 0, width, height)]);
            }
        }
    }

    #[test]
    fn wordwise_damage_matches_bitwise_for_randomized_rects_and_words() {
        fn next(seed: &mut u32) -> u32 {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 17;
            *seed ^= *seed << 5;
            *seed
        }
        for initial_seed in 1..=96u32 {
            for width in [1, 17, 511, 512, 513, 640, 641, 1031, 1537] {
                let height = 19 + initial_seed % 29;
                let mut h = history(width, height);
                let mut original = history(width, height);
                let mut seed = initial_seed.wrapping_mul(0x9e3779b9) ^ width;
                for iteration in 0..18 {
                    let rects = match iteration % 6 {
                        0 => vec![Rect::new(0, 0, width, height)],
                        1 => vec![Rect::new(width as i32 - 1, height as i32 - 1, 1, 1)],
                        2 => vec![Rect::new(0, 1, width, 1), Rect::new(3, 0, 17, height)],
                        _ => (0..12)
                            .map(|_| Rect::new(
                                (next(&mut seed) % (width + 32)) as i32 - 16,
                                (next(&mut seed) % (height + 32)) as i32 - 16,
                                next(&mut seed) % (width + 1),
                                next(&mut seed) % (height + 1),
                            ))
                            .collect(),
                    };
                    h.add(&rects);
                    original_add(&mut original, &rects);
                    for page in 0..3 {
                        assert_eq!(h.pages[page].rows, original.pages[page].rows);
                        assert_eq!(h.needs_repair(page), original.needs_repair(page));
                        let expected = bitwise_damage(&original, page);
                        assert_eq!(h.damage(page), expected,
                            "seed={initial_seed} width={width} iteration={iteration} page={page}");
                        assert_eq!(h.damage(page), expected);
                    }
                    h.presented(iteration % 3);
                    original.presented(iteration % 3);
                }
                // Include arbitrary sparse/dense words and dirty padding beyond groups.
                for page in &mut h.pages {
                    page.valid = true;
                    for word in &mut page.rows {
                        *word = u64::from(next(&mut seed)) << 32 | u64::from(next(&mut seed));
                    }
                }
                for page in 0..3 {
                    assert_eq!(h.damage(page), bitwise_damage(&h, page));
                    h.presented(page);
                    assert_eq!(h.damage(page), bitwise_damage(&h, page));
                }
            }
        }
    }

    #[test]
    fn wordwise_damage_preserves_boundary_runs_clamp_and_fallback() {
        let mut h = history(1031, 480);
        for page in 0..3 { h.presented(page); }
        h.add(&[Rect::new(496, 0, 535, 2), Rect::new(512, 3, 512, 2)]);
        assert_eq!(h.damage(0), vec![
            Rect::new(496, 0, 535, 2), Rect::new(512, 3, 512, 2),
        ]);
        h.presented(0);
        let fragmented: Vec<_> = (0..480)
            .flat_map(|y| (0..h.groups).filter_map(move |x| {
                (x % 2 == y % 2).then_some(Rect::new((x * 8) as i32, y as i32, 1, 1))
            }))
            .collect();
        h.add(&fragmented);
        assert_eq!(h.damage(0), bitwise_damage(&h, 0));
        assert_eq!(h.damage(0), vec![Rect::new(0, 0, 1031, 480)]);
        // A bounded-output return must not contaminate the next query's run table.
        h.presented(0);
        h.add(&[Rect::new(0, 0, 8, 1)]);
        assert_eq!(h.damage(0), vec![Rect::new(0, 0, 8, 1)]);
        // Without the reset, the old row-zero stamp would extend an absent area.
        h.presented(0);
        h.add(&[Rect::new(0, 1, 8, 1)]);
        assert_eq!(h.damage(0), vec![Rect::new(0, 1, 8, 1)]);
        assert_eq!(h.damage(0), bitwise_damage(&h, 0));
    }

    #[cfg(feature = "perf-bench")]
    #[test]
    #[ignore = "microbenchmark; run with --release --features perf-bench --ignored --nocapture"]
    fn benchmark_wordwise_damage() {
        use std::{hint::black_box, time::Instant};
        let mut h = history(640, 480);
        let nav = vec![
            Rect::new(0, 0, 640, 64),
            Rect::new(0, 96, 376, 128),
            Rect::new(448, 256, 112, 200),
        ];
        assert_eq!(nav.iter().map(|r| r.w * r.h).sum::<u32>(), 111_488);
        for (name, areas) in [
            ("nav", nav),
            ("empty", vec![]),
            ("full", vec![Rect::new(0, 0, 640, 480)]),
        ] {
            h.presented(0);
            h.add(&areas);
            assert_eq!(h.damage(0), bitwise_damage(&h, 0));
            for (label, add) in [
                ("old", original_add as fn(&mut PageHistory, &[Rect])),
                ("new", PageHistory::add),
            ] {
                let iterations = 20_000;
                let start = Instant::now();
                for _ in 0..iterations { add(black_box(&mut h), black_box(&areas)); }
                println!("HISTSCAN_ADD case={name} marking={label} iterations={iterations} ns_per_call={:.1}",
                    start.elapsed().as_nanos() as f64 / iterations as f64);
            }
            for (label, plan) in [
                ("old", bitwise_damage as fn(&PageHistory, usize) -> Vec<Rect>),
                ("new", PageHistory::damage),
            ] {
                for _ in 0..128 { black_box(plan(black_box(&h), 0)); }
                let iterations = 20_000;
                let start = Instant::now();
                for _ in 0..iterations { black_box(plan(black_box(&h), 0)); }
                println!("HISTSCAN case={name} scan={label} iterations={iterations} ns_per_call={:.1}",
                    start.elapsed().as_nanos() as f64 / iterations as f64);
            }
        }
    }

    #[test]
    #[ignore = "microbenchmark; run with --release --ignored --nocapture"]
    fn benchmark_reusable_damage_runs() {
        use std::{hint::black_box, time::Instant};
        let mut h = history(640, 480);
        h.presented(0);
        for (name, areas) in [
            ("empty", vec![]),
            ("full_height", vec![Rect::new(80, 0, 176, 480)]),
            (
                "fragmented",
                (0..40).map(|x| Rect::new(x * 16, 0, 1, 480)).collect(),
            ),
        ] {
            h.presented(0);
            h.add(&areas);
            for (label, plan) in [
                ("before", reference as fn(&PageHistory, usize) -> Vec<Rect>),
                ("after", PageHistory::damage),
            ] {
                let start = Instant::now();
                for _ in 0..2000 {
                    black_box(plan(black_box(&h), 0));
                }
                println!(
                    "damage {name} {label}: {:.3} us/call",
                    start.elapsed().as_secs_f64() * 1e6 / 2000.0
                );
            }
        }
    }
}
