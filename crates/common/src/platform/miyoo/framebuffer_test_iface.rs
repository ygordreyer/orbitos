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
    static NEW_BACKING: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

impl Framebuffer {
    pub fn new(_: &str) -> Result<Self> {
        let frame = NEW_BACKING.with(|backing| backing.borrow_mut().take());
        let Some(frame) = frame else {
            bail!("host presenter tests must not open a framebuffer device");
        };
        Ok(Self {
            device: File::open("/dev/null")?,
            frame,
            var_screen_info: OBSERVED.with(|state| state.borrow().0.clone()),
            fix_screen_info: OBSERVED.with(|state| state.borrow().1.clone()),
        })
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
    assert!(
        NEW_BACKING.with(|backing| backing.borrow().is_none()),
        "new backing was never mapped; writes still target the old backing"
    );
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
        red: Bitfield { offset: 16, length: 8, msb_right: 0 },
        green: Bitfield { offset: 8, length: 8, msb_right: 0 },
        blue: Bitfield { offset: 0, length: 8, msb_right: 0 },
        transp: Bitfield { offset: 24, length: 8, msb_right: 0 },
        ..VarScreeninfo::default()
    };
    let fix = FixScreeninfo {
        line_length: w * 4,
        ..FixScreeninfo::default()
    };
    OBSERVED.with(|state| *state.borrow_mut() = (var.clone(), fix.clone()));
    NEW_BACKING.with(|backing| *backing.borrow_mut() = None);
    FramebufferDisplay {
        page_history: None,
        present_damage: Vec::new(),
        composition_revision: 0,
        pixmap: Pixmap::new(w, h).unwrap().into(),
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
fn shared_prepared_transitions_preserve_pixels_reuse_and_page_history() {
    use super::*;
    use crate::theme::{
        FontBank, ThemeHandle, frame::FrameInputs, pack::ScreenId,
        render::PreparedFrame, runtime::ThemeSnapshot,
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let theme = ThemeHandle::prepare(
        ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(), &bank, 1,
    ).unwrap();
    let mut input = FrameInputs::new(ScreenId::Recents);
    input.status.clock = Some("12:34".into());
    let recents = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let details = PreparedFrame::prepare(
        theme.clone(), &FrameInputs::new(ScreenId::GameDetail),
    ).unwrap();
    assert!(recents.is_opaque() && details.is_opaque());
    let original = recents.pixels().data().to_vec();
    let targets = recents.targets.clone();
    let mut display = test_display(640, 480);
    for frame in [&details, &recents, &details, &recents] {
        frame.paint(&mut display);
        assert_eq!(display.pixmap().data().as_ptr(), frame.pixels().data().as_ptr());
        assert_eq!(display.pixmap().data(), frame.pixels().data());
        display.flush().unwrap();
        assert_visible(&display, 0, "shared transition");
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = 480);
        assert!(display.needs_present().unwrap());
        display.flush_rects(&[]).unwrap();
        assert_visible(&display, 1, "repair other page from shared frame");
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = 0);
    }
    // Restoring a saved region after adoption must detach once and preserve the prepared frame.
    display.clear(Color::new(17, 27, 37)).unwrap();
    let saved = display.pixmap().data().to_vec();
    display.save().unwrap();
    recents.paint(&mut display);
    display.load(Rect::new(3, 5, 0, 9)).unwrap();
    assert_eq!(display.pixmap().data().as_ptr(), recents.pixels().data().as_ptr());
    for area in [Rect::new(3, 5, 7, 9), Rect::new(636, 477, 8, 6), display.bounding_box()] {
        recents.paint(&mut display);
        display.load(area).unwrap();
        let mut expected = original.clone();
        for y in area.y as usize..(area.y as usize + area.h as usize).min(480) {
            let at = (y * 640 + area.x as usize) * 4;
            let end = (y * 640 + (area.x as usize + area.w as usize).min(640)) * 4;
            expected[at..end].copy_from_slice(&saved[at..end]);
        }
        assert_eq!(display.pixmap().data(), expected);
        assert_eq!(recents.pixels().data(), original);
        assert_ne!(display.pixmap().data().as_ptr(), recents.pixels().data().as_ptr());
        display.flush().unwrap();
        assert_visible(&display, 0, "snapshot restore after shared adoption");
    }
    assert!(!display.pop());
    assert!(display.saved.is_empty());
    recents.paint(&mut display);
    display.flush().unwrap();
    // A partial display write must not alter the reusable prepared frame.
    let area = Rect::new(3, 5, 7, 9);
    crate::display::fill_rect(&mut display.pixmap_mut(), area, Color::new(9, 19, 29));
    display.flush_rects(&[area]).unwrap();
    assert_visible(&display, 0, "partial write detaches shared frame");
    assert_eq!(recents.pixels().data(), original);
    assert_eq!(recents.targets, targets);
    recents.paint(&mut display);
    display.flush().unwrap();
    assert_eq!(display.pixmap().data(), original);

    // Incremental preparation must leave the displayed frame intact until paint.
    input.status.clock = Some("12:35".into());
    let next = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(recents)).unwrap();
    let cold = PreparedFrame::prepare(theme, &input).unwrap();
    assert_eq!(next.pixels().data(), cold.pixels().data());
    assert_ne!(next.pixels().data(), original);
    assert_eq!(display.pixmap().data(), original);
    let damage = next.paint_damage(&mut display).unwrap();
    display.flush_rects(&damage).unwrap();
    assert_eq!(display.pixmap().data(), cold.pixels().data());
    assert_visible(&display, 0, "partial redraw after shared transition");
    OBSERVED.with(|state| state.borrow_mut().0.yoffset = 480);
    display.flush_rects(&[]).unwrap();
    assert_visible(&display, 1, "old page after partial redraw");
}

#[test]
fn shared_full_redraw_clears_without_copying_and_parks_the_presented_arc() {
    use super::*;
    use crate::theme::{
        FontBank, ThemeHandle, frame::FrameInputs, pack::ScreenId,
        render::PreparedFrame, runtime::ThemeSnapshot,
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&root.join("static/.allium/fonts")).unwrap();
    let theme = ThemeHandle::prepare(
        ThemeSnapshot::load(&root.join("themes/orbit-hero")).unwrap(), &bank, 1,
    ).unwrap();
    let input = FrameInputs::new(ScreenId::Recents);
    let frame = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let original = frame.pixels().data().to_vec();
    let mut display = test_display(640, 480);
    frame.paint(&mut display);
    let presented = Arc::clone(&display.pixmap.0);
    let trace = std::env::temp_dir().join(format!("shared-redraw-{}.jsonl", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    let details = FrameInputs::new(ScreenId::GameDetail);
    let frame = PreparedFrame::prepare_reusing(theme.clone(), &details, Some(frame)).unwrap();
    let copied = crate::perf_trace::probe_counts()["copied_pixels"].as_u64().unwrap();
    crate::perf_trace::stop_probe();
    std::fs::remove_file(trace).unwrap();
    assert_eq!(copied, 0, "a full clear must not copy the displayed buffer");
    assert_eq!(display.pixmap().data(), original);
    assert_eq!(frame.pixels().data(), PreparedFrame::prepare(theme.clone(), &details).unwrap().pixels().data());
    let frame = PreparedFrame::prepare_reusing(theme, &input, Some(frame)).unwrap();
    assert_eq!(frame.pixels().data(), original);
    assert_eq!(frame.pixels().data().as_ptr(), presented.data().as_ptr(),
        "parking and restoring an unchanged screen must preserve its Arc");
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
    pub fn display(&mut self) -> &mut impl crate::display::Display { &mut self.0 }
    pub fn set_page(&mut self, page: u32) {
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = page * self.0.pixmap.height());
    }
    pub fn replace_backing(&mut self) {
        OBSERVED.with(|state| state.borrow_mut().1.smem_start += 4096);
        NEW_BACKING.with(|backing| {
            *backing.borrow_mut() = Some(vec![0xa5; self.0.iface.frame.len()]);
        });
    }
    pub fn composition_revision(&mut self) -> Result<u64> {
        use crate::display::Display;
        self.0.composition_revision()
    }
    pub fn needs_present(&mut self) -> Result<bool> {
        use crate::display::Display;
        self.0.needs_present()
    }
    pub fn assert_visible(&self) {
        let page = OBSERVED.with(|state| state.borrow().0.yoffset / self.0.pixmap.height());
        assert_visible(&self.0, page as usize, "launcher display repair");
    }
    pub fn flush(
        &mut self,
        pixels: tiny_skia::PixmapRef<'_>,
        areas: &[crate::geom::Rect],
    ) -> Result<()> {
        use crate::display::Display;
        self.0.pixmap.data_mut().copy_from_slice(pixels.data());
        self.0.flush_rects(areas)
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
    NEW_BACKING.with(|backing| {
        *backing.borrow_mut() = Some(vec![0xa5; display.iface.frame.len()]);
    });
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

#[test]
fn framebuffer_backing_identity_and_mapping_size_remap_before_repair() {
    use super::*;
    for identity_change in [true, false] {
        let mut display = test_display(32, 24);
        display.clear(Color::new(17, 33, 99)).unwrap();
        display.flush().unwrap();
        assert!(!display.needs_present().unwrap());
        let old_mapping = display.iface.frame.as_ptr();
        let new_len = OBSERVED.with(|state| {
            let mut state = state.borrow_mut();
            if identity_change {
                state.1.smem_start += 4096;
            } else {
                state.0.yres_virtual += 24;
                state.1.smem_len = state.1.line_length * state.0.yres_virtual;
            }
            (state.1.line_length * state.0.yres_virtual) as usize + 64
        });
        NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; new_len]));
        assert!(display.needs_present().unwrap());
        display.flush_rects(&[]).unwrap();
        assert_visible(&display, 0, "replacement backing full repaint");
        assert_ne!(
            display.iface.frame.as_ptr(),
            old_mapping,
            "old mapping was reused"
        );
        assert_eq!(display.iface.frame.len(), new_len);
        assert!(!display.needs_present().unwrap());
        OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
        assert!(
            display.needs_present().unwrap(),
            "old page validity survived remapping"
        );
        display.flush_rects(&[]).unwrap();
        assert_visible(&display, 1, "other replacement page requires full repaint");
    }
}

#[test]
fn fpdiag_composition_revision_ignores_page_repair_but_tracks_external_reads_and_remap() {
    use super::*;
    let mut display = test_display(32, 24);
    display.clear(Color::new(17, 33, 99)).unwrap();
    let initial = display.composition_revision().unwrap();
    display.flush_rects(&[display.bounding_box()]).unwrap();
    OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
    assert!(display.needs_present().unwrap());
    assert_eq!(display.composition_revision().unwrap(), initial);
    display.flush_rects(&[]).unwrap();
    assert_visible(&display, 1, "repair preserves logical base");
    assert_eq!(display.composition_revision().unwrap(), initial);
    display.flush_rect(display.bounding_box()).unwrap();
    assert!(display.needs_present().unwrap());
    assert_eq!(display.composition_revision().unwrap(), initial);
    display.read_rect(display.bounding_box()).unwrap();
    let read = display.composition_revision().unwrap();
    assert_ne!(read, initial);
    OBSERVED.with(|state| state.borrow_mut().1.smem_start += 4096);
    NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; display.iface.frame.len()]));
    assert_ne!(display.composition_revision().unwrap(), read);
    display.flush_rects(&[]).unwrap();
    assert_visible(&display, 1, "remap conservatively invalidates base");
}

#[test]
fn compose2_swap_rejects_size_without_mutation_and_preserves_shared_owners() {
    use super::*;
    let mut display = test_display(32, 24);
    display.clear(Color::new(17, 53, 91)).unwrap();
    display.flush().unwrap();
    let revision = display.composition_revision().unwrap();
    let original = display.pixmap.0.clone();
    let mut wrong = Arc::new(Pixmap::new(31, 24).unwrap());
    let wrong_pointer = Arc::as_ptr(&wrong);
    assert!(!display.swap_shared_pixmap(&mut wrong));
    assert_eq!(Arc::as_ptr(&wrong), wrong_pointer);
    assert!(Arc::ptr_eq(&original, &display.pixmap.0));
    let mut next = Pixmap::new(32, 24).unwrap();
    next.fill(tiny_skia::Color::from_rgba8(93, 71, 39, 255));
    let shared = Arc::new(next);
    let mut incoming = shared.clone();
    assert!(display.swap_shared_pixmap(&mut incoming));
    assert!(Arc::ptr_eq(&incoming, &original));
    assert!(Arc::ptr_eq(&shared, &display.pixmap.0));
    assert_eq!(display.composition_revision().unwrap(), revision);
    display.flush().unwrap();
    assert_visible(&display, 0, "swap pixels");
    display.clear(Color::new(1, 2, 3)).unwrap();
    assert_eq!(shared.pixel(0, 0).unwrap().red(), 93);
    assert_eq!(original.pixel(0, 0).unwrap().red(), 17);
}

#[test]
fn compose2_neighbor_defers_unique_outgoing_release_until_idle() {
    use super::*;
    use crate::theme::{render::{PreparedFrame, neighbor_test_arts as arts,
        neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let theme = theme("compact-plus", "square");
    let arts = arts();
    let mut base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
    let mut display = test_display(640, 480);
    base.paint(&mut display);
    display.flush().unwrap();
    for (step, selected) in [1, 0, 7, 0].into_iter().enumerate() {
        base.release_retired_pixels();
        base.restore_pixel_storage();
        let next = model(&theme, ScreenId::Recents, selected, &arts);
        let reference = crate::theme::render::navigation_reference(theme.clone(), &next).unwrap();
        let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
        let packed = candidate.pack_neighbor(&mut display, 4 * 1024 * 1024, &mut Default::default()).unwrap().unwrap();
        let outgoing = Arc::downgrade(&display.pixmap.0);
        let retired = candidate.recycle_neighbor_base(base);
        let damage = candidate.adopt_neighbor_damage(&mut display).unwrap();
        assert!(outgoing.upgrade().is_some(), "outgoing allocation released on press");
        assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
        assert_eq!(display.pixmap().data(), reference.pixels().data());
        assert_eq!(candidate.targets, reference.targets);
        assert_eq!(candidate.text_boxes, reference.text_boxes);
        display.flush_packed_rects(&damage, &packed).unwrap();
        assert_visible(&display, 0, "deferred outgoing");
        drop(retired);
        let outgoing = (step != 0).then_some(outgoing);
        candidate.release_retired_pixels();
        candidate.restore_pixel_storage();
        if let Some(outgoing) = outgoing { assert_eq!(outgoing.strong_count(), 0); }
        assert_eq!(candidate.pixels().data(), reference.pixels().data());
        assert_eq!(display.pixmap().data(), reference.pixels().data());
        base = candidate;
    }
}

#[test]
fn compose2_neighbor_hit_after_full_paint_then_miss_repairs_in_place() {
    use super::*;
    use crate::theme::{render::{PreparedFrame, neighbor_test_arts as arts,
        neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let theme = theme("compact-plus", "square");
    let arts = arts();
    let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
    let storage = base.pixels().data().as_ptr();
    let mut display = test_display(640, 480);
    base.paint(&mut display);
    display.flush().unwrap();
    assert_eq!(display.pixmap().data().as_ptr(), storage);
    assert_eq!(Arc::strong_count(&display.pixmap.0), 2);
    let next = model(&theme, ScreenId::Recents, 1, &arts);
    let (mut hit, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
    let retired = hit.recycle_neighbor_base(base);
    let damage = hit.adopt_neighbor_damage(&mut display).unwrap();
    display.flush_rects(&damage).unwrap();
    drop(retired);
    let reference = crate::theme::render::navigation_reference(theme.clone(), &next).unwrap();
    assert_eq!(display.pixmap().data(), reference.pixels().data());
    let displayed = display.pixmap().data().to_vec();
    let mut missed = model(&theme, ScreenId::Recents, 2, &arts);
    missed.status.clock = Some("15:21".into());
    assert!(!hit.neighbor_matches(&theme, &missed));
    let frame = PreparedFrame::prepare_reusing(theme.clone(), &missed, Some(hit)).unwrap();
    assert_eq!(frame.pixels().data().as_ptr(), storage, "immediate miss rebuilt aliased storage");
    assert_eq!(display.pixmap().data(), displayed);
    let reference = crate::theme::render::navigation_reference(theme.clone(), &missed).unwrap();
    assert_eq!(frame.pixels().data(), reference.pixels().data());
    assert_eq!(frame.targets, reference.targets);
    assert_eq!(frame.text_boxes, reference.text_boxes);
    let damage = frame.paint_damage(&mut display).unwrap();
    assert_eq!(display.pixmap().data(), reference.pixels().data());
    display.flush_rects(&damage).unwrap();
    assert_visible(&display, 0, "immediate miss after aliased outgoing hit");
}

#[test]
fn compose2_neighbor_hit_then_miss_preserves_outgoing_until_idle_or_retire() {
    use super::*;
    use crate::theme::{render::{PreparedFrame, neighbor_test_arts as arts,
        neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let theme = theme("compact-plus", "square");
    let arts = arts();
    for release in ["idle", "next_hit", "reused_candidate"] {
        let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
        let mut display = test_display(640, 480);
        base.paint(&mut display);
        // Detach without changing bytes so the outgoing display allocation has one owner.
        drop(display.pixmap_mut());
        assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
        let outgoing = Arc::downgrade(&display.pixmap.0);
        let next = model(&theme, ScreenId::Recents, 1, &arts);
        let (mut hit, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
        let retired = hit.recycle_neighbor_base(base);
        hit.adopt_neighbor_damage(&mut display).unwrap();
        drop(retired);
        assert_eq!(outgoing.strong_count(), 1);
        let displayed = display.pixmap().data().to_vec();
        let mut missed = model(&theme, ScreenId::Recents, 2, &arts);
        missed.status.clock = Some("15:21".into());
        assert!(!hit.neighbor_matches(&theme, &missed));
        let mut frame = PreparedFrame::prepare_reusing(theme.clone(), &missed, Some(hit)).unwrap();
        assert_eq!(outgoing.strong_count(), 1, "miss released outgoing storage: {release}");
        assert_eq!(display.pixmap().data(), displayed);
        let reference = crate::theme::render::navigation_reference(theme.clone(), &missed).unwrap();
        assert_eq!(frame.pixels().data(), reference.pixels().data());
        assert_eq!(frame.targets, reference.targets);
        assert_eq!(frame.text_boxes, reference.text_boxes);
        frame.paint_damage(&mut display).unwrap();
        assert_eq!(display.pixmap().data(), reference.pixels().data());
        if release == "next_hit" {
            let mut next = model(&theme, ScreenId::Recents, 3, &arts);
            next.status = missed.status.clone();
            let reference = crate::theme::render::navigation_reference(theme.clone(), &next).unwrap();
            let (mut candidate, _) = frame.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
            let retired = candidate.recycle_neighbor_base(frame);
            let newer = Arc::downgrade(&display.pixmap.0);
            candidate.adopt_neighbor_damage(&mut display).unwrap();
            assert_eq!(outgoing.strong_count(), 1, "later hit released older storage");
            assert_eq!(newer.strong_count(), 1);
            assert_eq!(display.pixmap().data(), reference.pixels().data());
            drop(retired);
            assert_eq!(outgoing.strong_count(), 0, "graveyard did not release older storage");
            candidate.release_retired_pixels();
            assert_eq!(newer.strong_count(), 0);
            candidate.restore_pixel_storage();
            assert_eq!(candidate.pixels().data(), reference.pixels().data());
        } else {
            if release == "reused_candidate" {
                let base = PreparedFrame::prepare(theme.clone(), &missed).unwrap();
                let retired = frame.recycle_neighbor_base(base);
                frame.adopt_neighbor_damage(&mut display).unwrap();
                drop(retired);
                assert_eq!(outgoing.strong_count(), 1, "reused candidate overwrote retained storage");
                assert_eq!(display.pixmap().data(), reference.pixels().data());
            }
            frame.restore_pixel_storage();
            assert_eq!(outgoing.strong_count(), 1, "pixel repair released outgoing storage");
            frame.release_retired_pixels();
            assert_eq!(outgoing.strong_count(), 0, "idle did not release outgoing storage");
            assert_eq!(frame.pixels().data(), reference.pixels().data());
        }
    }
}

#[test]
fn frameptr_neighbor_hits_transfer_unique_pixels_and_misses_do_not_detach() {
    use super::*;
    use crate::theme::{render::PreparedFrame, render::{neighbor_test_arts as arts, neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let arts = arts();
    for name in ["compact", "compact-plus", "orbit-hero", "orbit-row", "big-picture"] {
        let mut theme = theme(name, "square");
        let mut current = model(&theme, ScreenId::Recents, 0, &arts);
        let mut base = PreparedFrame::prepare(theme.clone(), &current).unwrap();
        let mut display = test_display(640, 480);
        base.paint(&mut display);
        display.flush().unwrap();
        let revision = display.composition_revision().unwrap();
        let mut graveyard = Vec::new();
        // Include direction reversal, a viewport shift, and hits without intervening idle work.
        for (step, selected) in [1, 0, 7, 8, 3, 12, 1].into_iter().enumerate() {
            let next = model(&theme, ScreenId::Recents, selected, &arts);
            let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
            let reference = crate::theme::render::navigation_reference(theme.clone(), &next).unwrap();
            let prepared_pointer = candidate.pixels().data().as_ptr();
            let packed = candidate.pack_neighbor(&mut display, 4 * 1024 * 1024, &mut Default::default()).unwrap().unwrap();
            graveyard.push(candidate.recycle_neighbor_base(base));
            // Pan after preparation. Repair must still use the complete logical candidate base.
            let page = (step % 2) as u32;
            OBSERVED.with(|state| state.borrow_mut().0.yoffset = page * 480);
            let trace = std::env::temp_dir().join(format!("frameptr-hit-{}-{name}-{step}.jsonl", std::process::id()));
            crate::perf_trace::start_probe(trace.to_str().unwrap());
            let damage = candidate.adopt_neighbor_damage(&mut display).unwrap();
            let counts = crate::perf_trace::probe_counts();
            crate::perf_trace::stop_probe();
            std::fs::remove_file(trace).unwrap();
            assert_eq!(counts["neighbor_pointer_adopts"], 1);
            assert_eq!(counts["neighbor_damage_copies"], 0);
            assert_eq!(counts["copied_pixels"], 0, "{name} hit {step}");
            assert_eq!(counts["full_frame_detaches"], 0);
            assert_eq!(Arc::strong_count(&display.pixmap.0), 1, "candidate retained displayed storage");
            assert_eq!(display.pixmap().data().as_ptr(), prepared_pointer);
            assert_eq!(display.pixmap().data().len(), 1_228_800);
            assert_eq!(display.pixmap().data(), reference.pixels().data());
            assert_eq!(candidate.targets, reference.targets);
            assert_eq!(candidate.text_boxes, reference.text_boxes);
            display.flush_packed_rects(&damage, &packed).unwrap();
            assert_visible(&display, page as usize, "adopted page");
            OBSERVED.with(|state| state.borrow_mut().0.yoffset = (1 - page) * 480);
            display.flush_rects(&[]).unwrap();
            assert_visible(&display, (1 - page) as usize, "stale page after adoption");
            assert_eq!(display.composition_revision().unwrap(), revision);
            base = candidate;
        }
        for change in ["status", "artwork", "theme"] {
            base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
            base.paint(&mut display);
            display.flush().unwrap();
            let next = model(&theme, ScreenId::Recents, 2, &arts);
            let (mut hit, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap()
                .unwrap_or_else(|| panic!("{name} {change}: {:?}, {}", base.neighbor_charge(&next), base.neighbor_ineligible_reason()));
            let packed = hit.pack_neighbor(&mut display, 4 * 1024 * 1024, &mut Default::default()).unwrap().unwrap();
            let _retired = hit.recycle_neighbor_base(base);
            let damage = hit.adopt_neighbor_damage(&mut display).unwrap();
            display.flush_packed_rects(&damage, &packed).unwrap();
            current = next;
            match change {
                "status" => current.status.clock = Some("15:21".into()),
                "artwork" => current.art.selected_cover = Some(Arc::new((*arts[3]).clone())),
                "theme" => theme = crate::theme::render::neighbor_test_theme(name, "square"),
                _ => unreachable!(),
            }
            assert!(!hit.neighbor_matches(&theme, &current));
            let before = display.pixmap().data().to_vec();
            let trace = std::env::temp_dir().join(format!("frameptr-miss-{}-{name}-{change}.jsonl", std::process::id()));
            crate::perf_trace::start_probe(trace.to_str().unwrap());
            base = PreparedFrame::prepare_reusing(theme.clone(), &current, Some(hit)).unwrap();
            assert_eq!(display.pixmap().data(), before, "prepare mutated the display");
            let damage = base.paint_damage(&mut display).unwrap();
            assert_eq!(crate::perf_trace::probe_counts()["full_frame_detaches"], 0, "{name} {change}");
            crate::perf_trace::stop_probe();
            std::fs::remove_file(trace).unwrap();
            let reference = crate::theme::render::navigation_reference(theme.clone(), &current).unwrap();
            assert_eq!(display.pixmap().data(), reference.pixels().data());
            assert_eq!(base.targets, reference.targets);
            assert_eq!(base.text_boxes, reference.text_boxes);
            display.flush_rects(&damage).unwrap();
        }
    }
}

#[test]
fn frameptr_idle_repair_reuses_retired_buffer_and_preserves_retained_data() {
    use super::*;
    use crate::theme::{render::PreparedFrame, render::{neighbor_test_arts as arts, neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let arts = arts();
    let theme = theme("compact-plus", "square");
    let input = model(&theme, ScreenId::Recents, 0, &arts);
    let base = PreparedFrame::prepare(theme.clone(), &input).unwrap();
    let retired = base.pixels().data().as_ptr();
    let next = model(&theme, ScreenId::Recents, 1, &arts);
    let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
    let (retained, _) = base.prepare_neighbor(&model(&theme, ScreenId::Recents, 11, &arts), 4 * 1024 * 1024).unwrap().unwrap();
    let retained_bytes = retained.pixels().data().to_vec();
    let expected = candidate.pixels().data().to_vec();
    let mut display = test_display(640, 480);
    base.paint(&mut display);
    candidate.recycle_neighbor_base(base);
    candidate.adopt_neighbor_damage(&mut display);
    assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
    candidate.release_retired_pixels();
    candidate.restore_pixel_storage();
    assert_eq!(candidate.pixels().data().as_ptr(), retired);
    assert_eq!(candidate.pixels().data(), expected);
    let displayed = display.pixmap().data().to_vec();
    let (future, _) = candidate.prepare_neighbor(&input, 4 * 1024 * 1024).unwrap().unwrap();
    assert_ne!(future.pixels().data().as_ptr(), display.pixmap().data().as_ptr());
    assert_eq!(display.pixmap().data(), displayed, "idle build changed displayed frame");
    display.clear(Color::new(9, 19, 29)).unwrap();
    assert_eq!(candidate.pixels().data(), expected);
    assert_eq!(retained.pixels().data(), retained_bytes);
}

#[test]
fn frameptr_menu_and_details_round_trips_after_adoption_match_reference() {
    use super::*;
    use crate::theme::{frame::FrameInputs, pack::ScreenId, render::{PreparedFrame,
        neighbor_test_arts as arts, neighbor_test_model as model, neighbor_test_theme as theme}};
    let arts = arts();
    for name in ["compact", "compact-plus"] {
        let theme = theme(name, "square");
        let mut display = test_display(640, 480);
        let mut reference_display = test_display(640, 480);
        let mut base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
        base.paint(&mut display);
        for overlay_screen in [ScreenId::ContextMenu, ScreenId::GameDetail, ScreenId::ContextMenu, ScreenId::GameDetail] {
            let input = model(&theme, ScreenId::Recents, 1, &arts);
            let (mut candidate, _) = base.prepare_neighbor(&input, 4 * 1024 * 1024).unwrap().unwrap();
            let packed = candidate.pack_neighbor(&mut display, 4 * 1024 * 1024, &mut Default::default()).unwrap();
            candidate.recycle_neighbor_base(base);
            let damage = candidate.adopt_neighbor_damage(&mut display).unwrap();
            if let Some(packed) = &packed { display.flush_packed_rects(&damage, packed).unwrap(); }
            else { display.flush_rects(&damage).unwrap(); }
            assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
            let pixels = display.pixmap().data().to_vec();
            base = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(candidate)).unwrap();
            assert_eq!(display.pixmap().data(), pixels);
            let overlay_input = FrameInputs::new(overlay_screen);
            let overlay = PreparedFrame::prepare(theme.clone(), &overlay_input).unwrap();
            let reference = crate::theme::render::navigation_reference(theme.clone(), &input).unwrap();
            if !overlay.is_opaque() { base.paint(&mut display); }
            overlay.paint(&mut display);
            reference.paint(&mut reference_display);
            overlay.paint(&mut reference_display);
            assert_eq!(display.pixmap().data(), reference_display.pixmap().data(), "{name} {overlay_screen:?} open");
            display.flush().unwrap();
            assert_visible(&display, 0, "overlay after adoption");
            base = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(base)).unwrap();
            base.paint(&mut display);
            assert_eq!(display.pixmap().data(), reference.pixels().data(), "{name} {overlay_screen:?} close");
            display.flush().unwrap();
            assert_visible(&display, 0, "close overlay after adoption");
        }
    }
}

#[test]
fn frameptr_full_frame_detach_counter_detects_retained_strong_owner() {
    use super::*;
    let mut display = test_display(640, 480);
    let retained = Arc::clone(&display.pixmap.0);
    let before = retained.data().to_vec();
    let trace = std::env::temp_dir().join(format!("frameptr-detach-{}.jsonl", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    display.clear(Color::new(1, 2, 3)).unwrap();
    assert_eq!(crate::perf_trace::probe_counts()["full_frame_detaches"], 1);
    crate::perf_trace::stop_probe();
    std::fs::remove_file(trace).unwrap();
    assert_eq!(retained.data(), before);
}

#[test]
fn frameptr_mutation_before_idle_preserves_retained_pixels_and_rebuilds_lost_source() {
    use super::*;
    use crate::theme::{pack::ScreenId, render::{PreparedFrame,
        neighbor_test_arts as arts, neighbor_test_model as model, neighbor_test_theme as theme}};
    let arts = arts();
    let theme = theme("compact-plus", "square");
    for retain_read in [false, true] {
        let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
        let mut input = model(&theme, ScreenId::Recents, 1, &arts);
        let (mut candidate, _) = base.prepare_neighbor(&input, 4 * 1024 * 1024).unwrap().unwrap();
        let original = candidate.pixels().data().to_vec();
        let mut display = test_display(640, 480);
        base.paint(&mut display);
        candidate.recycle_neighbor_base(base);
        candidate.adopt_neighbor_damage(&mut display);
        assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
        if retain_read { assert_eq!(candidate.pixels().data(), original); }
        let trace = std::env::temp_dir().join(format!("frameptr-write-{}-{retain_read}.jsonl", std::process::id()));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        display.clear(Color::new(9, 19, 29)).unwrap();
        assert_eq!(candidate.try_pixels().unwrap().data(), original);
        let damage = candidate.paint_damage(&mut display).unwrap();
        assert_eq!(damage, vec![display.bounding_box()]);
        assert_eq!(display.pixmap().data(), original);
        input.status.clock = Some("15:23".into());
        let candidate = PreparedFrame::prepare_reusing(theme.clone(), &input, Some(candidate)).unwrap();
        assert_eq!(crate::perf_trace::probe_counts()["full_frame_detaches"], 0);
        crate::perf_trace::stop_probe();
        std::fs::remove_file(trace).unwrap();
        let reference = crate::theme::render::navigation_reference(theme.clone(), &input).unwrap();
        assert_eq!(candidate.pixels().data(), reference.pixels().data());
        assert_eq!(candidate.targets, reference.targets);
        assert_eq!(candidate.text_boxes, reference.text_boxes);
        // External mutation invalidates the partial base; presentation must be complete.
        candidate.paint(&mut display);
        assert_eq!(display.pixmap().data(), reference.pixels().data());
    }
}

#[test]
fn frameptr_expired_art_before_first_retained_read_requires_full_render() {
    use super::*;
    use crate::theme::{pack::ScreenId, render::{PreparedFrame,
        neighbor_test_arts as arts, neighbor_test_model as model, neighbor_test_theme as theme}};
    for name in ["compact", "compact-plus"] {
        let arts = arts();
        let theme = theme(name, "square");
        let mut input = model(&theme, ScreenId::Recents, 1, &arts);
        let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
        let (mut candidate, _) = base.prepare_neighbor(&input, 4 * 1024 * 1024).unwrap().unwrap();
        let original = candidate.pixels().data().to_vec();
        let mut display = test_display(640, 480);
        base.paint(&mut display);
        candidate.recycle_neighbor_base(base);
        candidate.adopt_neighbor_damage(&mut display).unwrap();
        assert_eq!(Arc::strong_count(&display.pixmap.0), 1);
        display.clear(Color::new(9, 19, 29)).unwrap();
        input.art = Default::default();
        drop(arts);
        // This is the first pixel read after adoption, with both weak sources expired.
        assert!(candidate.try_pixels().is_none(), "{name}");
        assert!(!candidate.pixels_usable());
        assert!(candidate.navigation_model().is_none());
        assert!(candidate.prepare_neighbor(&input, 4 * 1024 * 1024).unwrap().is_none());
        let unchanged = display.pixmap().data().to_vec();
        assert!(!candidate.paint(&mut display));
        assert!(candidate.paint_damage(&mut display).is_none());
        assert!(candidate.adopt_neighbor_damage(&mut display).is_none());
        assert_eq!(display.pixmap().data(), unchanged, "invalid frame painted");
        candidate.restore_pixel_storage();
        assert!(candidate.try_pixels().is_none(), "retired buffer is not valid frame storage");
        assert!(!candidate.paint(&mut display));
        let reference = crate::theme::render::navigation_reference(theme.clone(), &input).unwrap();
        if name == "compact-plus" { assert!(reference.pixels().data() != original); }
        let fallback = PreparedFrame::prepare_reusing(theme, &input, Some(candidate)).unwrap();
        let damage = fallback.paint_damage(&mut display).unwrap();
        assert_eq!(damage, vec![display.bounding_box()]);
        assert_eq!(display.pixmap().data().len(), 1_228_800);
        assert_eq!(display.pixmap().data(), reference.pixels().data());
        display.flush_rects(&damage).unwrap();
        assert_visible(&display, 0, "expired retained frame full render");
    }
}

#[test]
fn packed_row_copy_preserves_bytes_and_guards_at_unaligned_lengths() {
    for len in [0, 4, 60, 64, 68, 832, 1024, 1028] {
        for source_offset in [0, 1, 3, 7, 15] {
            for target_offset in [0, 1, 2, 5, 15] {
                let source: Vec<_> = (0..len + source_offset + 16)
                    .map(|i| (i * 37 + 19) as u8).collect();
                let mut target = vec![0xa5; len + target_offset + 16];
                let mut expected = target.clone();
                expected[target_offset..target_offset + len]
                    .copy_from_slice(&source[source_offset..source_offset + len]);
                super::copy_packed_row(&mut target[target_offset..target_offset + len],
                    &source[source_offset..source_offset + len]);
                assert_eq!(target, expected, "len={len} source={source_offset} target={target_offset}");
            }
        }
    }
}

#[test]
fn packed_reverse_source_order_matches_pixel_rotation_in_direct_and_repair_paths() {
    use super::*;
    let (w, h) = (96, 24);
    let areas = [Rect::new(-3, 2, 12, 9), Rect::new(33, 3, 9, 8),
        Rect::new(65, 1, 27, 13), Rect::new(41, 6, 9, 2), Rect::new(90, 22, 12, 4)];
    for page in [0, 1] {
        for repair in [false, true] {
            let mut display = test_display(w, h);
            display.clear(Color::new(17, 53, 91)).unwrap();
            display.flush().unwrap();
            OBSERVED.with(|s| s.borrow_mut().0.yoffset = page * h);
            if !repair { display.flush_rects(&[]).unwrap(); }
            let mut candidate = display.pixmap.as_ref().to_owned();
            for (y, row) in candidate.data_mut().chunks_exact_mut(w as usize * 4).enumerate() {
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    if areas.iter().any(|r| r.x <= x as i32 && r.right() > x as i32
                        && r.y <= y as i32 && r.bottom() > y as i32) {
                        pixel.copy_from_slice(&[(x * 13 + y * 7) as u8, (y * 17) as u8,
                            (x * 3 + y * 11) as u8, 255]);
                    }
                }
            }
            let candidate = Arc::new(candidate);
            let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
            assert!(packed.rows.windows(2).any(|pair| pair[0].y == pair[1].y), "multiple segments per y");
            assert!(packed.rows.windows(2).all(|pair| (pair[0].y, pair[0].x0) < (pair[1].y, pair[1].x0)));
            let mut next_start = 0;
            for row in packed.rows.iter().rev() {
                assert_eq!(row.start, next_start, "reverse walk must read contiguous bytes forwards");
                next_start += (row.x1 - row.x0) * 4;
            }
            assert!(packed.rows.iter().rev().zip(packed.rows.iter().rev().skip(1))
                .all(|(a, b)| a.start < b.start));
            assert_eq!(next_start, packed.bytes.len());
            display.set_shared_pixmap(&candidate);
            let mut expected = display.iface.frame.clone();
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let src = (y * w as usize + x) * 4;
                    let dst = ((page * h) as usize * w as usize
                        + (h as usize - 1 - y) * w as usize + w as usize - 1 - x) * 4;
                    let p = &candidate.data()[src..src + 4];
                    expected[dst..dst + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
            if repair {
                display.page_history = None;
                display.flush_packed_rects(&areas, &packed).unwrap();
                assert_eq!(display.present_damage, vec![display.bounding_box()], "full repair uses write_packed_rect");
            } else {
                assert!(!display.page_history.as_ref().unwrap().needs_repair(page as usize));
                let storage = (display.present_damage.as_ptr(), display.present_damage.capacity(), display.present_damage.len());
                display.flush_packed_rects(&areas, &packed).unwrap();
                assert_eq!((display.present_damage.as_ptr(), display.present_damage.capacity(), display.present_damage.len()),
                    storage, "direct path skips repair storage");
            }
            assert_eq!(display.iface.frame, expected, "page={page} repair={repair}");
        }
    }
}

#[test]
fn present_clean_packet_matches_legacy_repairs_and_reuses_damage_storage() {
    use super::*;
    let mut patterns = vec![
        ("row_band", vec![Rect::new(0, 5, 32, 4)]),
        ("disjoint", vec![Rect::new(1, 2, 7, 3), Rect::new(19, 13, 10, 5)]),
        ("full", vec![Rect::new(0, 0, 32, 24)]),
        ("overlap", vec![Rect::new(1, 2, 19, 10), Rect::new(9, 4, 18, 12)]),
        ("clipped", vec![Rect::new(-5, -3, 11, 9), Rect::new(29, 22, 9, 9)]),
        ("split_rows", vec![Rect::new(0, 0, 8, 20), Rect::new(24, 0, 8, 20)]),
        ("changing_runs", vec![Rect::new(1, 0, 7, 2), Rect::new(1, 2, 15, 2), Rect::new(1, 4, 7, 2)]),
        ("empty_with_band", vec![Rect::new(3, 3, 0, 5), Rect::new(13, 11, 7, 0), Rect::new(16, 18, 8, 2)]),
    ];
    let mut seed = 0x93ab12efu32;
    for _ in 0..32 {
        let mut areas = vec![Rect::new(0, 0, 1, 1)];
        for _ in 0..8 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            areas.push(Rect::new((seed % 48) as i32 - 8, (seed.rotate_left(7) % 40) as i32 - 8,
                seed.rotate_left(13) % 33, seed.rotate_left(19) % 25));
        }
        patterns.push(("random", areas));
    }
    for (name, areas) in patterns {
        for (mode, page) in [("clean", 0), ("clean_pan", 1), ("invalid", 1), ("stale", 0)] {
            let mut display = test_display(32, 24);
            let mut legacy = test_display(32, 24);
            for target in [&mut display, &mut legacy] {
                OBSERVED.with(|s| s.borrow_mut().0.yoffset = 0);
                target.clear(Color::new(17, 53, 91)).unwrap();
                target.flush().unwrap();
                if mode != "invalid" {
                    OBSERVED.with(|s| s.borrow_mut().0.yoffset = 24);
                    target.flush_rects(&[]).unwrap();
                }
                if mode == "stale" {
                    let old = Rect::new(8, 18, 9, 2);
                    crate::display::fill_rect(&mut target.pixmap_mut(), old, Color::new(31, 97, 241));
                    target.flush_rects(&[old]).unwrap();
                }
                OBSERVED.with(|s| s.borrow_mut().0.yoffset = page * 24);
                target.refresh_present_layout().unwrap();
            }
            let mut candidate = display.pixmap.as_ref().to_owned();
            for (i, area) in areas.iter().enumerate() {
                crate::display::fill_rect(&mut candidate.as_mut(), *area, Color::rgba(130 + i as u8, 37, 113, 211));
            }
            let candidate = Arc::new(candidate);
            let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
            display.set_shared_pixmap(&candidate);
            legacy.set_shared_pixmap(&candidate);
            let history = legacy.page_history.as_mut().unwrap();
            history.add(&areas);
            let damage = history.damage(page as usize);
            if mode.starts_with("clean") {
                assert_eq!(packed.coverage_rects, damage.len(), "{name}");
                assert_eq!(packed.bytes.len(), damage.iter().map(|r| r.w as usize * r.h as usize * 4).sum::<usize>());
            }
            for area in damage { legacy.write_packed_rect(area, &packed).unwrap(); }
            legacy.page_history.as_mut().unwrap().presented(page as usize);
            let storage_before = (display.present_damage.as_ptr(), display.present_damage.capacity());
            display.flush_packed_rects(&areas, &packed).unwrap();
            assert_eq!(display.iface.frame, legacy.iface.frame, "{name} {mode}");
            assert_visible(&display, page as usize, name);
            if mode.starts_with("clean") {
                assert_eq!((display.present_damage.as_ptr(), display.present_damage.capacity()), storage_before,
                    "direct hit changed repair output storage");
            } else {
                display.flush_rects(&areas).unwrap();
                let storage = (display.present_damage.as_ptr(), display.present_damage.capacity());
                for _ in 0..4 {
                    display.flush_rects(&areas).unwrap();
                    assert_eq!((display.present_damage.as_ptr(), display.present_damage.capacity()), storage,
                        "repair output storage changed: {name} {mode}");
                    assert_eq!(display.iface.frame, legacy.iface.frame);
                }
            }
        }
    }
    let mut display = test_display(33, 24);
    let candidate = Arc::new(Pixmap::new(33, 24).unwrap());
    for areas in [vec![], vec![Rect::new(3, 3, 0, 5)], vec![Rect::new(33, 3, 4, 5)],
        vec![Rect::new(-3, 3, 2, 5)], vec![Rect::new(3, 3, 5, 0)]] {
        assert!(display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().is_none(),
            "empty clipped damage must not expand into packed pixels: {areas:?}");
    }
}

#[test]
fn present_fragmented_packet_preserves_full_repair_fallback() {
    use super::*;
    let mut display = test_display(640, 128);
    display.flush().unwrap();
    let mut candidate = Pixmap::new(640, 128).unwrap();
    candidate.fill(tiny_skia::Color::WHITE);
    let candidate = Arc::new(candidate);
    let areas: Vec<_> = (0..128).flat_map(|y| (0..40)
        .map(move |x| Rect::new(x * 16 + y % 2 * 8, y, 8, 1))).collect();
    let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
    assert!(packed.coverage_rects > 4096);
    display.set_shared_pixmap(&candidate);
    display.flush_packed_rects(&areas, &packed).unwrap();
    assert_eq!(display.present_damage, vec![Rect::new(0, 0, 640, 128)]);
    assert_visible(&display, 0, "fragmentation full repair");
    assert!(display.iface.frame[640 * 128 * 4..].iter().all(|b| *b == 0xa5));
}

#[test]
fn present_compact_plus_neighbor_geometry_and_direct_copy_match_full_frame() {
    use super::*;
    use crate::theme::{render::{PreparedFrame, neighbor_test_arts as arts,
        neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    let theme = theme("compact-plus", "square");
    let arts = arts();
    for (from, to) in [(0, 1), (1, 0)] {
        let base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, from, &arts)).unwrap();
        let next = model(&theme, ScreenId::Recents, to, &arts);
        let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
        let mut display = test_display(640, 480);
        base.paint(&mut display);
        display.flush().unwrap();
        let packed = candidate.pack_neighbor(&mut display, 4 * 1024 * 1024, &mut Default::default()).unwrap().unwrap();
        candidate.recycle_neighbor_base(base);
        let damage = candidate.adopt_neighbor_damage(&mut display).unwrap();
        let trace = std::env::temp_dir().join(format!("present-geometry-{}-{from}-{to}.jsonl", std::process::id()));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        display.flush_packed_rects(&damage, &packed).unwrap();
        let counts = crate::perf_trace::probe_counts();
        crate::perf_trace::stop_probe();
        std::fs::remove_file(trace).unwrap();
        assert_visible(&display, 0, "compact-plus direct");
        assert_eq!(display.present_damage.capacity(), 0);
        assert_eq!(counts["packed_span_hit"], 1);
        assert_eq!(counts["converted_pixels"], 0);
        assert_eq!(counts["flushed_pixels"], packed.bytes.len() / 4);
        assert_eq!(counts["damage_pixels"], packed.bytes.len() / 4);
        assert_eq!(counts["damage_rects"], packed.coverage_rects);
        println!("PRESENT_GEOMETRY from={from} to={to} logical={damage:?} rows={} bytes={} rectangles={} full_width_rows={}",
            packed.rows.len(), packed.bytes.len(), packed.coverage_rects,
            packed.rows.iter().filter(|r| r.x0 == 0 && r.x1 == 640).count());
    }
}

#[cfg(feature = "perf-bench")]
#[test]
#[ignore = "cached-memory microbenchmark, not device framebuffer timing"]
fn benchmark_compose2_neighbor_adoption() {
    use crate::theme::{render::{PreparedFrame, neighbor_test_arts as arts,
        neighbor_test_model as model, neighbor_test_theme as theme}, pack::ScreenId};
    use std::{hint::black_box, time::Instant};
    let theme = theme("compact-plus", "square");
    let arts = arts();
    let mut base = PreparedFrame::prepare(theme.clone(), &model(&theme, ScreenId::Recents, 0, &arts)).unwrap();
    let mut display = test_display(640, 480);
    base.paint(&mut display);
    for keep_outgoing in [false, true, false, true] {
        let mut samples = Vec::new();
        for selected in (0..100).map(|n| (n + 1) % 2) {
            base.release_retired_pixels();
            base.restore_pixel_storage();
            let next = model(&theme, ScreenId::Recents, selected, &arts);
            let (mut candidate, _) = base.prepare_neighbor(&next, 4 * 1024 * 1024).unwrap().unwrap();
            let retired = candidate.recycle_neighbor_base(base);
            let outgoing = keep_outgoing.then(|| display.pixmap.0.clone());
            let start = Instant::now();
            black_box(candidate.adopt_neighbor_damage(black_box(&mut display)).unwrap());
            samples.push(start.elapsed().as_nanos());
            black_box(&outgoing);
            drop(outgoing);
            drop(retired);
            base = candidate;
        }
        samples.sort_unstable();
        println!("COMPOSE2_BENCH keep_outgoing={keep_outgoing} iterations={} median_ns={} p95_ns={}",
            samples.len(), samples[50], samples[95]);
    }
}

#[cfg(feature = "perf-bench")]
#[test]
#[ignore = "cached-memory microbenchmark, not device framebuffer timing"]
fn benchmark_present_packet_walk() {
    use super::*;
    use std::{hint::black_box, time::Instant};
    let areas = [Rect::new(152, 72, 256, 52), Rect::new(152, 128, 256, 52), Rect::new(425, 16, 206, 408)];
    let mut display = test_display(640, 480);
    display.flush().unwrap();
    let candidate = Arc::new(Pixmap::new(640, 480).unwrap());
    let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
    display.set_shared_pixmap(&candidate);
    assert_eq!(packed.bytes.len(), 445_952);
    for old in [true, false, true, false] {
        let iterations = 4000;
        let start = Instant::now();
        for _ in 0..iterations {
            if old {
                let h = display.page_history.as_mut().unwrap();
                h.add(black_box(&areas));
                let damage = h.damage(0);
                for area in damage { display.write_packed_rect(area, black_box(&packed)).unwrap(); }
                display.page_history.as_mut().unwrap().presented(0);
            } else {
                display.flush_packed_rects(black_box(&areas), black_box(&packed)).unwrap();
            }
            black_box(&display.iface.frame);
        }
        println!("PRESENT_BENCH path={} bytes={} rows={} iterations={iterations} ns_per_call={:.1}",
            if old { "legacy_scan_search" } else { "direct_packet" }, packed.bytes.len(), packed.rows.len(),
            start.elapsed().as_nanos() as f64 / iterations as f64);
    }
}

#[test]
fn prepack_page_history_and_fallbacks_match_normal_present_bytes() {
    use super::*;
    for case in ["both_valid", "one_valid", "external_pan", "stale_repair", "remap", "format", "identity", "damage", "read"] {
        let mut display = test_display(32, 24);
        let mut normal = test_display(32, 24);
        for target in [&mut display, &mut normal] {
            OBSERVED.with(|state| state.borrow_mut().0.yoffset = 0);
            target.clear(Color::new(17, 53, 91)).unwrap();
            target.flush().unwrap();
            if case != "one_valid" {
                OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
                target.flush_rects(&[]).unwrap();
                OBSERVED.with(|state| state.borrow_mut().0.yoffset = 0);
                target.refresh_present_layout().unwrap();
            }
            if case == "one_valid" {
                OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
                target.refresh_present_layout().unwrap();
            }
            if case == "stale_repair" {
                OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24);
                crate::display::fill_rect(&mut target.pixmap_mut(), Rect::new(23, 17, 2, 3), Color::new(99, 17, 28));
                target.flush_rects(&[Rect::new(23, 17, 2, 3)]).unwrap();
                OBSERVED.with(|state| state.borrow_mut().0.yoffset = 0);
                target.refresh_present_layout().unwrap();
            }
        }
        let mut candidate = display.pixmap.as_ref().to_owned();
        let areas = [Rect::new(-2, 2, 7, 3), Rect::new(4, 3, 7, 2), Rect::new(29, 22, 9, 9)];
        for (i, area) in areas.iter().enumerate() {
            crate::display::fill_rect(&mut candidate.as_mut(), *area, Color::new(130 + i as u8, 37, 213));
        }
        let candidate = Arc::new(candidate);
        let packed = display.pack_spans(&candidate, &areas, 1024 * 1024, &mut Default::default()).unwrap().unwrap();
        assert!(display.packed_base_matches(&packed));
        assert!(packed.rows.iter().any(|row| row.x0 == 0 && row.x1 == 16), "aligned union");
        assert!(packed.rows.iter().any(|row| row.x1 == 32), "right-edge expansion");
        display.set_shared_pixmap(&candidate);
        normal.set_shared_pixmap(&candidate);
        drop(candidate);
        if case == "external_pan" { OBSERVED.with(|state| state.borrow_mut().0.yoffset = 24); }
        if case == "format" { OBSERVED.with(|state| state.borrow_mut().0.red.offset = 7); }
        if case == "identity" {
            display.pixmap = display.pixmap.as_ref().to_owned().into();
        }
        if case == "read" { display.read_rect(Rect::new(0, 0, 1, 1)).unwrap(); normal.read_rect(Rect::new(0, 0, 1, 1)).unwrap(); }
        let damage = if case == "damage" { vec![Rect::new(0, 0, 32, 24)] } else { areas.to_vec() };
        let trace = std::env::temp_dir().join(format!("prepack-{}-{case}.jsonl", std::process::id()));
        crate::perf_trace::start_probe(trace.to_str().unwrap());
        if case == "remap" {
            OBSERVED.with(|state| state.borrow_mut().1.smem_start += 4096);
            NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; display.iface.frame.len()]));
        }
        display.flush_packed_rects(&damage, &packed).unwrap();
        let counts = crate::perf_trace::probe_counts();
        crate::perf_trace::stop_probe();
        std::fs::remove_file(trace).unwrap();
        if case == "remap" { NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; normal.iface.frame.len()])); }
        normal.flush_rects(&damage).unwrap();
        assert_eq!(display.iface.frame, normal.iface.frame, "{case}");
        let hit = matches!(case, "both_valid" | "one_valid" | "stale_repair");
        assert_eq!(counts["packed_span_hit"], u64::from(hit), "{case}");
        if case == "both_valid" { assert_eq!(counts["converted_pixels"], 0); }
        if case == "one_valid" { assert!(counts["converted_pixels"].as_u64().unwrap() > 0); }
        if case == "stale_repair" { assert!(counts["converted_pixels"].as_u64().unwrap() > 0); }
        if !matches!(case, "damage" | "read") { assert_visible(&display, if matches!(case, "external_pan" | "one_valid") { 1 } else { 0 }, case); }
        println!("PREPACK parity={case} hit={} converted={}", counts["packed_span_hit"], counts["converted_pixels"]);
    }
}

#[test]
fn prepack_capacity_is_charged_and_unsupported_format_is_not_packed() {
    use super::*;
    let mut display = test_display(640, 480);
    let candidate = Arc::new(Pixmap::new(640, 480).unwrap());
    let damage = [Rect::new(0, 0, 640, 174), Rect::new(0, 174, 128, 1)];
    let packed = display.pack_spans(&candidate, &damage, usize::MAX, &mut Default::default()).unwrap().unwrap();
    assert_eq!(packed.bytes.len(), 445_952);
    let charge = packed.charge();
    println!("PREPACK bytes={} rows={} row_capacity={} row_size={} damage_capacity={} payload_size={} charge={charge}",
        packed.bytes.capacity(), packed.rows.len(), packed.rows.capacity(), std::mem::size_of::<PackedRow>(),
        packed.damage.capacity(), std::mem::size_of::<PackedSpans>());
    let mut diagnostic = PackDiagnostic::default();
    assert!(display.pack_spans(&candidate, &damage, charge - 1, &mut diagnostic).unwrap().is_none());
    assert_eq!(diagnostic.reason, "pack_budget");
    assert_eq!((diagnostic.needed_bytes, diagnostic.available_bytes), (charge, charge - 1));
    assert_eq!(display.pack_spans(&candidate, &damage, charge, &mut Default::default()).unwrap().unwrap().charge(), charge);
    OBSERVED.with(|state| state.borrow_mut().0.red.offset = 0);
    assert!(display.pack_spans(&candidate, &damage, usize::MAX, &mut Default::default()).unwrap().is_none());
    OBSERVED.with(|state| state.borrow_mut().0.red.offset = 16);
    OBSERVED.with(|state| state.borrow_mut().0.bits_per_pixel = 64);
    // Keep the legacy wider-pixel layout valid, but outside packed admission.
    OBSERVED.with(|state| { let mut state = state.borrow_mut(); state.1.line_length *= 2; });
    NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; 640 * 480 * 8 * 2 + 64]));
    assert!(display.pack_spans(&candidate, &damage, usize::MAX, &mut Default::default()).unwrap().is_none());
}

#[test]
fn prepack_declines_name_each_refreshed_format_field_size_and_empty_damage() {
    use super::*;
    let mutations: &[(&str, fn(&mut VarScreeninfo))] = &[
        ("bits_per_pixel", |v| v.bits_per_pixel = 64),
        ("grayscale", |v| v.grayscale = 1),
        ("nonstd", |v| v.nonstd = 1),
        ("red", |v| v.red.offset = 0),
        ("red", |v| v.red.length = 7),
        ("red", |v| v.red.msb_right = 1),
        ("green", |v| v.green.offset = 0),
        ("green", |v| v.green.length = 7),
        ("green", |v| v.green.msb_right = 1),
        ("blue", |v| v.blue.offset = 16),
        ("blue", |v| v.blue.length = 7),
        ("blue", |v| v.blue.msb_right = 1),
        ("transp", |v| v.transp.offset = 0),
        ("transp", |v| v.transp.length = 7),
        ("transp", |v| v.transp.msb_right = 1),
    ];
    let candidate = Arc::new(Pixmap::new(32, 24).unwrap());
    let damage = [Rect::new(0, 0, 16, 8)];
    let trace = std::env::temp_dir().join(format!("prepack-declines-{}.jsonl", std::process::id()));
    crate::perf_trace::start_probe(trace.to_str().unwrap());
    for (field, mutate) in mutations {
        let mut display = test_display(32, 24);
        OBSERVED.with(|state| mutate(&mut state.borrow_mut().0));
        if *field == "bits_per_pixel" {
            OBSERVED.with(|state| state.borrow_mut().1.line_length *= 2);
            NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; 32 * 24 * 8 * 2 + 64]));
        }
        let mut diagnostic = PackDiagnostic::default();
        assert!(display.pack_spans(&candidate, &damage, usize::MAX, &mut diagnostic).unwrap().is_none());
        assert_eq!(diagnostic.reason, format!("pack_format:{field}"));
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_reason"], diagnostic.reason);
        println!("PREPACK3 decline={}", diagnostic.reason);
    }
    let mut display = test_display(32, 24);
    let mut diagnostic = PackDiagnostic::default();
    let wrong_size = Arc::new(Pixmap::new(31, 24).unwrap());
    assert!(display.pack_spans(&wrong_size, &damage, usize::MAX, &mut diagnostic).unwrap().is_none());
    assert_eq!(diagnostic.reason, "pack_size");
    for damage in [&[][..], &[Rect::new(32, 24, 8, 8)][..]] {
        assert!(display.pack_spans(&candidate, damage, 0, &mut diagnostic).unwrap().is_none());
        assert_eq!(diagnostic.reason, "pack_empty");
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_reason"], "pack_empty");
    }
    crate::perf_trace::stop_probe();
    std::fs::remove_file(trace).unwrap();
}

#[test]
fn prepack_triple_buffer_pan_and_unused_alpha_pack_with_two_resident_frames() {
    use super::*;
    let mut display = test_display(640, 480);
    OBSERVED.with(|state| state.borrow_mut().0.yres_virtual = 1440);
    NEW_BACKING.with(|backing| *backing.borrow_mut() = Some(vec![0xa5; 640 * 1440 * 4]));
    let candidate = Arc::new(Pixmap::new(640, 480).unwrap());
    let damage = [Rect::new(0, 0, 640, 174), Rect::new(0, 174, 128, 1)];
    let available = 4 * 1024 * 1024 - 2 * 640 * 480 * 4;
    let mut diagnostic = PackDiagnostic::default();
    let first = display.pack_spans(&candidate, &damage, available, &mut diagnostic).unwrap().unwrap();
    let revision = display.composition_revision;
    for page in [0, 1, 2] {
        OBSERVED.with(|state| { let mut state = state.borrow_mut();
            state.0.yoffset = page * 480;
            state.0.transp.length = 0;
            state.0.transp.offset = 0;
        });
        let packed = display.pack_spans(&candidate, &damage, available, &mut diagnostic).unwrap().unwrap();
        assert_eq!(packed.charge(), first.charge());
        assert_eq!(diagnostic.reason, "hit");
        assert_eq!(display.composition_revision, revision, "pan must not invalidate the base");
    }
    println!("PREPACK3 triple_buffer available={available} needed={} headroom={}", first.charge(), available - first.charge());
}


#[cfg(test)]
fn legacy_present_rects(display: &mut super::FramebufferDisplay, areas: &[super::Rect],
    packed: Option<&super::PackedSpans>) -> Result<()> {
    use super::*;
    display.refresh_present_layout()?;
    let packed = packed.filter(|p| {
        let mut layout = display.packed_layout();
        // The models have separate mappings, so their allocation addresses differ.
        layout[30] = p.layout[30];
        p.layout == layout
        && p.revision == display.composition_revision
        && p.candidate == Arc::as_ptr(&display.pixmap.0) as usize && p.damage == areas
    });
    if display.page_history.as_ref().is_none_or(|h| !h.matches(&display.iface)) {
        display.page_history = PageHistory::new(&display.iface);
    }
    let Some(history) = &mut display.page_history else { return display.flush(); };
    let Some(page) = history.page(display.iface.var_screen_info.yoffset) else { return display.flush(); };
    let direct = packed.is_some_and(|p| p.coverage_rects <= 4096)
        && !history.legacy_needs_repair(page);
    history.legacy_add(areas);
    if direct {
        let packed = packed.unwrap();
        let width = display.pixmap.width() as usize;
        let height = display.pixmap.height() as usize;
        let location = display.iface.var_screen_info.yoffset as usize * width * 4;
        for row in packed.rows.iter().rev() {
            let at = location + ((height - 1 - row.y) * width + width - row.x1) * 4;
            let len = (row.x1 - row.x0) * 4;
            copy_packed_row(&mut display.iface.frame[at..at + len], &packed.bytes[row.start..row.start + len]);
        }
    } else {
        let damage = history.legacy_damage(page);
        for area in damage {
            if let Some(packed) = packed { display.write_packed_rect(area, packed)?; }
            else { display.write_rect(area)?; }
        }
    }
    display.page_history.as_mut().unwrap().presented(page);
    Ok(())
}

#[test]
fn randomized_pending_history_matches_old_present_and_all_framebuffer_bytes() {
    use super::*;
    fn next(seed: &mut u32) -> u32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        *seed
    }
    for initial_pages in 1..=3 {
        let (w, h) = (73, 29);
        let mut display = test_display(w, h);
        let mut old = test_display(w, h);
        let mut seed = 0x79ab12efu32 ^ initial_pages;
        let mut pages = initial_pages;
        for step in 0..480 {
            if step % 61 == 0 { pages = 1 + next(&mut seed) % 3; }
            let page = next(&mut seed) % pages;
            OBSERVED.with(|state| {
                let mut state = state.borrow_mut();
                state.0.yres_virtual = pages * h;
                state.0.yoffset = if step % 47 == 0 && pages > 1 { 1 } else { page * h };
                if step % 37 == 0 { state.1.smem_start += 4096; }
            });
            for target in [&mut display, &mut old] {
                let remap = OBSERVED.with(|s| s.borrow().1.smem_start != target.iface.fix_screen_info.smem_start
                    || s.borrow().0.yres_virtual != target.iface.var_screen_info.yres_virtual);
                if remap {
                    NEW_BACKING.with(|b| *b.borrow_mut() = Some(vec![0xa5; (w * h * pages * 4) as usize + 64]));
                }
                target.refresh_present_layout().unwrap();
                if step % 53 == 0 { target.read_rect(Rect::new(0, 0, 1, 1)).unwrap(); }
                if step % 71 == 0 { target.flush_rect(Rect::new(0, 0, 1, 1)).unwrap(); }
            }
            let areas = match step % 8 {
                0 => vec![],
                1 => vec![Rect::new(-20, -20, 1, 1), Rect::new(3, 3, 0, 9)],
                2 => vec![Rect::new(-3, -4, 12, 13), Rect::new(70, 27, 20, 20)],
                3 => vec![Rect::new(0, 0, w, h)],
                _ => (0..1 + next(&mut seed) % 7).map(|_| Rect::new(
                    (next(&mut seed) % (w + 24)) as i32 - 12,
                    (next(&mut seed) % (h + 24)) as i32 - 12,
                    next(&mut seed) % (w + 1), next(&mut seed) % (h + 1))).collect(),
            };
            let mut candidate = display.pixmap.as_ref().to_owned();
            for area in &areas {
                crate::display::fill_rect(&mut candidate.as_mut(), *area,
                    Color::rgba(next(&mut seed) as u8, next(&mut seed) as u8, next(&mut seed) as u8, 255));
            }
            let candidate = Arc::new(candidate);
            let mut packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap();
            if let Some(packed) = &mut packed {
                if step % 9 == 0 { packed.coverage_rects = 4097; }
                if step % 11 == 0 { packed.revision = packed.revision.wrapping_add(1); }
            }
            display.set_shared_pixmap(&candidate);
            old.set_shared_pixmap(&candidate);
            let packed = if step % 4 == 0 { None } else { packed.as_ref() };
            let actual = display.present_rects(&areas, packed);
            let expected = legacy_present_rects(&mut old, &areas, packed);
            assert_eq!(actual.is_ok(), expected.is_ok(), "pages={initial_pages} step={step}");
            actual.unwrap(); expected.unwrap();
            assert_eq!(display.iface.frame, old.iface.frame, "pages={initial_pages} step={step}");
            match (&display.page_history, &old.page_history) {
                (Some(new), Some(old)) => new.assert_equivalent(old),
                (None, None) => {},
                _ => panic!("history presence differs at step {step}"),
            }
        }
    }
}

#[cfg(feature = "perf-bench")]
#[test]
fn direct_present_marks_only_hidden_pages_and_repairs_them_after_pan() {
    use super::*;
    for pages in 1..=3 {
        let mut display = test_display(32, 24);
        OBSERVED.with(|s| {
            let mut s = s.borrow_mut();
            s.0.yres_virtual = pages * 24;
            s.1.smem_start += 4096;
        });
        NEW_BACKING.with(|b| *b.borrow_mut() = Some(vec![0xa5; (32 * 24 * pages * 4) as usize + 64]));
        display.flush().unwrap();
        for page in 0..pages {
            OBSERVED.with(|s| s.borrow_mut().0.yoffset = page * 24);
            display.flush_rects(&[]).unwrap();
        }
        let areas = [Rect::new(3, 4, 7, 3)];
        let mut candidate = display.pixmap.as_ref().to_owned();
        crate::display::fill_rect(&mut candidate.as_mut(), areas[0], Color::new(130, 37, 213));
        let candidate = Arc::new(candidate);
        let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
        display.set_shared_pixmap(&candidate);
        crate::perf::take();
        display.flush_packed_rects(&areas, &packed).unwrap();
        let counts = crate::perf::take();
        assert_eq!(counts.page_history_scans, 0);
        assert_eq!(counts.page_history_marks, u64::from(pages - 1));
        for page in 0..pages - 1 {
            assert!(display.page_history.as_ref().unwrap().needs_repair(page as usize));
            OBSERVED.with(|s| s.borrow_mut().0.yoffset = page * 24);
            display.flush_rects(&[]).unwrap();
            assert_visible(&display, page as usize, "hidden damage repaired after pan");
        }
    }
}

#[cfg(feature = "perf-bench")]
#[test]
fn interrupted_direct_and_repair_copies_keep_old_destination_damage() {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    for repair in [false, true] {
        for interrupted in [false, true] {
            let mut display = test_display(32, 24);
            let mut old = test_display(32, 24);
            for target in [&mut display, &mut old] { target.flush().unwrap(); }
            let areas = [Rect::new(0, 2, 8, 4)];
            let mut candidate = display.pixmap.as_ref().to_owned();
            crate::display::fill_rect(&mut candidate.as_mut(), areas[0], Color::new(130, 37, 213));
            let candidate = Arc::new(candidate);
            let mut packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
            if repair { packed.coverage_rects = 4097; }
            if interrupted { packed.rows[1].start = packed.bytes.len() + 1; }
            display.set_shared_pixmap(&candidate);
            old.set_shared_pixmap(&candidate);
            let before = display.iface.frame.clone();
            crate::perf::take();
            let actual = catch_unwind(AssertUnwindSafe(|| display.flush_packed_rects(&areas, &packed)));
            let counts = crate::perf::take();
            let expected = catch_unwind(AssertUnwindSafe(|| legacy_present_rects(&mut old, &areas, Some(&packed))));
            assert_eq!(actual.is_err(), interrupted);
            assert_eq!(expected.is_err(), interrupted);
            assert_eq!(counts.page_history_marks, if repair || interrupted { 2 } else { 1 });
            if !interrupted { actual.unwrap().unwrap(); expected.unwrap().unwrap(); }
            else { assert_ne!(display.iface.frame, before, "copy stopped after writing some rows"); }
            assert_eq!(display.iface.frame, old.iface.frame, "repair={repair} interrupted={interrupted}");
            display.page_history.as_ref().unwrap().assert_equivalent(old.page_history.as_ref().unwrap());
            if interrupted {
                assert!(display.page_history.as_ref().unwrap().needs_repair(0));
                display.flush_rects(&[]).unwrap();
                assert_visible(&display, 0, "repair after interrupted copy");
            }
        }
    }
}


#[test]
fn fragmented_repair_fallback_matches_old_history_and_all_framebuffer_bytes() {
    use super::*;
    let mut display = test_display(640, 104);
    let mut old = test_display(640, 104);
    for target in [&mut display, &mut old] {
        for page in 0..2 {
            OBSERVED.with(|s| s.borrow_mut().0.yoffset = page * 104);
            target.flush_rects(&[]).unwrap();
        }
    }
    let areas: Vec<_> = (0..104).flat_map(|y| (0..80).filter_map(move |x| {
        (x % 2 == y % 2).then_some(Rect::new(x * 8, y, 1, 1))
    })).collect();
    let mut candidate = display.pixmap.as_ref().to_owned();
    for area in &areas {
        crate::display::fill_rect(&mut candidate.as_mut(), *area, Color::new(130, 37, 213));
    }
    let candidate = Arc::new(candidate);
    let packed = display.pack_spans(&candidate, &areas, usize::MAX, &mut Default::default()).unwrap().unwrap();
    assert!(packed.coverage_rects > 4096);
    display.set_shared_pixmap(&candidate);
    old.set_shared_pixmap(&candidate);
    display.flush_packed_rects(&areas, &packed).unwrap();
    legacy_present_rects(&mut old, &areas, Some(&packed)).unwrap();
    assert_eq!(display.present_damage, vec![display.bounding_box()]);
    assert_eq!(display.iface.frame, old.iface.frame);
    display.page_history.as_ref().unwrap().assert_equivalent(old.page_history.as_ref().unwrap());
    OBSERVED.with(|s| s.borrow_mut().0.yoffset = 0);
    display.flush_rects(&[]).unwrap();
    legacy_present_rects(&mut old, &[], None).unwrap();
    assert_eq!(display.iface.frame, old.iface.frame);
    assert_visible(&display, 0, "fragmented hidden page repaired after pan");
}
