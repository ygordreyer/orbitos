use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() { LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, layout); }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(p, layout, size) };
        if !next.is_null() { LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed); }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
#[ignore = "offline allocation and OSD latency measurement; run alone with --nocapture"]
fn daemon_residency_profile() {
    use common::theme::{FontBank, ThemeHandle, runtime::ThemeSnapshot};
    use common::platform::DefaultPlatform;
    let start = LIVE.load(Ordering::Relaxed);
    let locale = common::locale::Locale::new("en-US");
    eprintln!("locale live delta={}", LIVE.load(Ordering::Relaxed) - start);
    let styles = common::stylesheet::Stylesheet::load().unwrap();
    eprintln!("locale+styles live delta={}", LIVE.load(Ordering::Relaxed) - start);
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let bank = FontBank::bind(&repo.join("static/.allium/fonts")).unwrap();
    for id in ["compact-plus", "orbit-hero", "orbit-row"] {
        let before = LIVE.load(Ordering::Relaxed);
        let theme = ThemeHandle::prepare(ThemeSnapshot::load(&repo.join("themes").join(id)).unwrap(), &bank, 1).unwrap();
        eprintln!("{id} load delta={} font_mapped_bytes={} font_owned_bytes={}",
            LIVE.load(Ordering::Relaxed) - before, bank.accounting().1, bank.heap_bytes());
        crate::osd::Osd::<DefaultPlatform>::warm_theme(theme.clone()).unwrap();
        eprintln!("{id} warm cumulative delta={}", LIVE.load(Ordering::Relaxed) - before);
        for repainting in [false, true] {
            let now = std::time::Instant::now();
            let overlay = crate::osd::prepare_overlay(theme.clone(), crate::osd::OsdKind::Volume, "Volume +", 0.6, repainting).unwrap();
            eprintln!("{id} first prepared repainting={repainting} us={} payload={}", now.elapsed().as_micros(), overlay.accounted_bytes());
        }
        drop(theme);
        eprintln!("{id} after drop delta={}", LIVE.load(Ordering::Relaxed) - before);
    }
    use common::platform::Platform;
    let before = LIVE.load(Ordering::Relaxed);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let menu = rt.block_on(allium_menu::AlliumMenu::new(DefaultPlatform::new().unwrap(), styles.clone())).unwrap();
    eprintln!("idle menu+runtime Rust heap delta={}", LIVE.load(Ordering::Relaxed) - before);
    drop((menu, rt, locale, styles, bank));
}
