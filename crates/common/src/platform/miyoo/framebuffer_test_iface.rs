use anyhow::{Result, bail};
use std::{cell::RefCell, fs::File};

#[derive(Debug, Clone, Default)]
pub struct Bitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

#[derive(Debug, Clone, Default)]
pub struct VarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub nonstd: u32,
    pub rotate: u32,
    pub vmode: u32,
    pub red: Bitfield,
    pub green: Bitfield,
    pub blue: Bitfield,
    pub transp: Bitfield,
}

#[derive(Debug, Clone, Default)]
pub struct FixScreeninfo {
    pub smem_start: usize,
    pub smem_len: u32,
    pub fb_type: u32,
    pub visual: u32,
    pub line_length: u32,
}

pub struct Framebuffer {
    pub device: File,
    pub frame: Vec<u8>,
    pub var_screen_info: VarScreeninfo,
    pub fix_screen_info: FixScreeninfo,
}

thread_local! {
    static OBSERVED: RefCell<(VarScreeninfo, FixScreeninfo)> = RefCell::default();
}

impl Framebuffer {
    pub fn new(_: &str) -> Result<Self> {
        bail!("host presenter tests must not open a framebuffer device")
    }
    pub fn get_var_screeninfo(_: &File) -> Result<VarScreeninfo> {
        Ok(OBSERVED.with(|state| state.borrow().0.clone()))
    }
    pub fn get_fix_screeninfo(_: &File) -> Result<FixScreeninfo> {
        Ok(OBSERVED.with(|state| state.borrow().1.clone()))
    }
    pub fn put_var_screeninfo(_: &File, var: &VarScreeninfo) -> Result<()> {
        OBSERVED.with(|state| state.borrow_mut().0 = var.clone());
        Ok(())
    }
    pub fn read_frame(&self) -> &[u8] {
        &self.frame
    }
}

#[test]
fn dl15b_framebuffer_display_sequential_partial_presents_match_full_render() {
    use super::*;
    let (w, h) = (32, 24);
    let page_bytes = (w * h * 4) as usize;
    let mut display = test_display(w, h);
    display.clear(Color::new(30, 40, 50)).unwrap();
    display.flush().unwrap();
    assert_visible(&display, 0, "initial full refresh");
    let damage = Rect::new(2, 3, 5, 4);
    for (step, page, full) in [
        (0, 0, false),
        (1, 1, false),
        (2, 1, false),
        (3, 1, true),
        (4, 1, false),
        (5, 0, false),
        (6, 0, false),
        (7, 1, false),
    ] {
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = page * h);
        if full {
            display.clear(Color::new(77, 88, 99)).unwrap();
            display.flush().unwrap();
        } else {
            crate::display::fill_rect(
                &mut display.pixmap_mut(),
                damage,
                Color::new(90 + step, 120, 170),
            );
            let other = 1 - page as usize;
            let untouched =
                display.iface.frame[other * page_bytes..(other + 1) * page_bytes].to_vec();
            display.flush_rects(&[damage]).unwrap();
            assert_eq!(
                &display.iface.frame[other * page_bytes..(other + 1) * page_bytes],
                untouched,
                "only the observed visible page may be written"
            );
        }
        assert_visible(
            &display,
            page as usize,
            &format!("step {step}, page {page}, full {full}"),
        );
        assert!(
            display.iface.frame[page_bytes * 2..]
                .iter()
                .all(|byte| *byte == 0xa5)
        );
    }
    OBSERVED.with(|state| state.borrow_mut().0.xres += 1);
    let before = display.iface.frame.clone();
    assert!(display.flush_rects(&[damage]).is_err());
    assert_eq!(
        display.iface.frame, before,
        "invalid dimensions fail before writing"
    );
}

fn assert_visible(display: &super::FramebufferDisplay, page: usize, stage: &str) {
    let w = display.pixmap.width() as usize;
    let h = display.pixmap.height() as usize;
    let bytes = w * h * 4;
    let visible = &display.iface.frame[page * bytes..(page + 1) * bytes];
    let mut expected = Vec::with_capacity(bytes);
    for pixel in display.pixmap.pixels().iter().rev() {
        expected.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
    }
    assert!(
        visible == expected,
        "{stage}: visible page differs from fresh full render"
    );
}

fn test_display(w: u32, h: u32) -> super::FramebufferDisplay {
    use super::*;
    let page_bytes = (w * h * 4) as usize;
    let var = VarScreeninfo {
        xres: w,
        yres: h,
        xres_virtual: w,
        yres_virtual: h * 2,
        bits_per_pixel: 32,
        ..VarScreeninfo::default()
    };
    let fix = FixScreeninfo {
        line_length: w * 4,
        ..FixScreeninfo::default()
    };
    OBSERVED.with(|state| *state.borrow_mut() = (var.clone(), fix.clone()));
    FramebufferDisplay {
        page_history: None,
        pixmap: Pixmap::new(w, h).unwrap(),
        iface: Framebuffer {
            device: File::open("/dev/null").unwrap(),
            frame: vec![0xa5; page_bytes * 2 + 64],
            var_screen_info: var,
            fix_screen_info: fix,
        },
        scratch: Vec::new(),
        capture_pread: false,
        saved: Vec::new(),
        reusable_snapshots: Vec::new(),
        overlay_instance: 1,
        overlay_mapping: None,
    }
}

#[test]
fn dl16_observed_page_flips_repair_only_missed_damage() {
    use super::*;
    let (w, h) = (32, 24);
    let mut display = test_display(w, h);
    display.clear(Color::new(40, 50, 60)).unwrap();
    display.flush().unwrap();
    let trace = std::env::temp_dir().join(format!("dl16-presenter-{}.jsonl", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    for step in 0..96 {
        let page = (step % 2) as usize;
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = page as u32 * h);
        let area = Rect::new(1 + (step * 7 % 25) as i32, 1 + (step * 3 % 17) as i32, 4, 3);
        crate::display::fill_rect(
            &mut display.pixmap_mut(),
            area,
            Color::new(step as u8, 100, 190),
        );
        crate::perf_trace::begin_render();
        display.flush_rects(&[area]).unwrap();
        assert_visible(
            &display,
            page,
            &format!("96-event page history step {step}"),
        );
        if step >= 2 {
            assert!(
                crate::perf_trace::probe_counts()["flushed_pixels"]
                    .as_u64()
                    .unwrap()
                    < w as u64 * h as u64,
                "initialized page flip must repair damage rather than refresh full page at step {step}"
            );
        }
    }
    std::fs::remove_file(trace).unwrap();
}

#[cfg(feature = "perf-bench")]
pub struct PresenterProbe(super::FramebufferDisplay);
#[cfg(feature = "perf-bench")]
impl PresenterProbe {
    pub fn new(width: u32, height: u32) -> Self {
        Self(test_display(width, height))
    }
    pub fn present(
        &mut self,
        pixels: tiny_skia::PixmapRef<'_>,
        areas: &[crate::geom::Rect],
        page: u32,
    ) -> Result<()> {
        use crate::display::Display;
        self.0.pixmap.data_mut().copy_from_slice(pixels.data());
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = page * self.0.pixmap.height());
        self.0.flush_rects(areas)?;
        assert_visible(
            &self.0,
            page as usize,
            "actual Library move through production presenter",
        );
        Ok(())
    }
}

#[test]
fn dl16_full_flush_observes_new_page_and_invalidates_other_pages() {
    use super::*;
    let mut display = test_display(32, 24);
    display.clear(Color::new(17, 33, 99)).unwrap();
    display.flush().unwrap();
    OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
    display.clear(Color::new(52, 66, 120)).unwrap();
    display.flush().unwrap();
    assert_visible(&display, 1, "full refresh after external pan");
    OBSERVED.with(|state| state.borrow_mut().0.yoffset = 0);
    let area = Rect::new(2, 4, 3, 5);
    crate::display::fill_rect(&mut display.pixmap_mut(), area, Color::new(200, 100, 10));
    display.flush_rects(&[area]).unwrap();
    assert_visible(&display, 0, "old page after intervening full refresh");
    OBSERVED.with(|state| state.borrow_mut().1.smem_start += 4096);
    display.iface.frame[..32 * 24 * 4].fill(0xa5);
    display.flush_rects(&[]).unwrap();
    assert_visible(
        &display,
        0,
        "new backing requires full initialization even with empty damage",
    );
}

#[test]
fn dl16_fragmented_page_history_bounds_damage_memory() {
    use super::*;
    let mut display = test_display(640, 480);
    display.flush().unwrap();
    let areas: Vec<Rect> = (0..480)
        .flat_map(|y| (0..40).map(move |x| Rect::new(x * 16 + (y % 2) * 8, y, 8, 1)))
        .collect();
    let history = display.page_history.as_mut().unwrap();
    history.add(&areas);
    assert_eq!(history.damage(0), vec![Rect::new(0, 0, 640, 480)]);
    display.flush_rects(&areas).unwrap();
    assert_visible(&display, 0, "fragmented history full fallback");
}
