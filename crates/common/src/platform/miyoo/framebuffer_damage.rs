use super::{Framebuffer, Rect};

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
pub(super) struct PageHistory {
    layout: Layout,
    width: u32,
    height: u32,
    groups: usize,
    words: usize,
    pages: Vec<Page>,
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
            for page in &mut self.pages {
                for y in y0..y1 {
                    mark_groups(
                        &mut page.rows[y * self.words..(y + 1) * self.words],
                        (x0 / 8) as usize,
                        x1.div_ceil(8) as usize,
                    );
                }
            }
        }
    }
    pub fn damage(&self, page: usize) -> Vec<Rect> {
        if !self.pages[page].valid {
            return vec![Rect::new(0, 0, self.width, self.height)];
        }
        let mut areas: Vec<Rect> = vec![];
        let mut previous: Vec<(usize, usize, usize)> = vec![];
        for (y, row) in self.pages[page].rows.chunks_exact(self.words).enumerate() {
            let mut current = vec![];
            let mut x = 0;
            while x < self.groups {
                if row[x / 64] & (1u64 << (x % 64)) == 0 {
                    x += 1;
                    continue;
                }
                let first = x;
                while x < self.groups && row[x / 64] & (1u64 << (x % 64)) != 0 {
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
                        (x as u32 * 8).min(self.width) - first as u32 * 8,
                        1,
                    ));
                    if areas.len() > 4096 {
                        return vec![Rect::new(0, 0, self.width, self.height)];
                    }
                    areas.len() - 1
                };
                current.push((first, x, index));
            }
            previous = current;
        }
        areas
    }
    pub fn presented(&mut self, page: usize) {
        self.pages[page].valid = true;
        self.pages[page].rows.fill(0);
    }
}
fn mark_groups(words: &mut [u64], start: usize, end: usize) {
    for index in start / 64..(end - 1) / 64 + 1 {
        let left = start.saturating_sub(index * 64);
        let right = (end - index * 64).min(64);
        words[index] |= (u64::MAX << left) & (u64::MAX >> (64 - right));
    }
}
