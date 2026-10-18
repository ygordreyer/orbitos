use super::*;
use common::display::{Display, RectHold, color::Color, overlay::{
    CapturedOverlay, OverlayContent, OverlayFormat, OverlayLayout, OverlayLayoutSpec, PreparedOverlay,
}};
use common::geom::Rect;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use tiny_skia::{PixmapMut, PixmapRef};

type MockDisplay = <DefaultPlatform as Platform>::Display;

struct FramebufferPlatform {
    frame: Arc<Mutex<Vec<u8>>>,
    writer_active: Arc<AtomicBool>,
}

#[async_trait::async_trait(?Send)]
impl Platform for FramebufferPlatform {
    type Display = FramebufferDisplay;
    type Battery = <DefaultPlatform as Platform>::Battery;
    type SuspendContext = ();

    fn new() -> Result<Self> {
        Ok(Self {
            frame: Arc::new(Mutex::new(vec![33; 640 * 480 * 4 * 3])),
            writer_active: Arc::new(AtomicBool::new(false)),
        })
    }
    fn display(&mut self) -> Result<Self::Display> {
        Ok(FramebufferDisplay {
            logical: MockDisplay::new(), frame: self.frame.clone(),
            writer_active: self.writer_active.clone(),
        })
    }
    fn battery(&self) -> Result<Self::Battery> { DefaultPlatform::new()?.battery() }
    async fn poll(&mut self) -> KeyEvent { std::future::pending().await }
    fn shutdown(&self) -> Result<()> { unreachable!() }
    fn suspend(&self) -> Result<()> { unreachable!() }
    fn unsuspend(&self, _: ()) -> Result<()> { unreachable!() }
    fn set_volume(&mut self, _: i32) -> Result<()> { unreachable!() }
    fn get_brightness(&self) -> Result<u8> { unreachable!() }
    fn set_brightness(&mut self, _: u8) -> Result<()> { unreachable!() }
    fn set_display_settings(&mut self, _: &mut DisplaySettings) -> Result<()> { unreachable!() }
    fn device_model() -> String { "OSD test framebuffer".into() }
    fn firmware() -> String { DefaultPlatform::firmware() }
    fn has_wifi() -> bool { false }
    fn has_lid() -> bool { false }
}

struct FramebufferDisplay {
    logical: MockDisplay,
    frame: Arc<Mutex<Vec<u8>>>,
    writer_active: Arc<AtomicBool>,
}

fn layout() -> OverlayLayout {
    let bytes = 640 * 480 * 4 * 3;
    OverlayLayout::checked(OverlayLayoutSpec {
        instance: 1, device: (0, 0, 0), backing_start: 0,
        organization: [0, 0, 0, 2], channel_msb_right: [0; 4], orientation: [0; 2],
        width: 640, height: 480, virtual_width: 640, virtual_height: 480 * 3,
        xoffset: 0, yoffset: 0, row_stride: 640 * 4,
        mapped_bytes: bytes, physical_bytes: bytes,
        format: OverlayFormat {
            bits_per_pixel: 32, red: (16, 8), green: (8, 8), blue: (0, 8), alpha: (24, 8),
        },
    }).unwrap()
}

impl Display for FramebufferDisplay {
    fn width(&self) -> u32 { 640 }
    fn height(&self) -> u32 { 480 }
    fn pixmap(&self) -> PixmapRef<'_> { self.logical.pixmap() }
    fn pixmap_mut(&mut self) -> PixmapMut<'_> { self.logical.pixmap_mut() }
    fn map_pixels<F: FnMut(Color) -> Color>(&mut self, f: F) -> Result<()> { self.logical.map_pixels(f) }
    fn save(&mut self) -> Result<()> { self.logical.save() }
    fn load(&mut self, rect: Rect) -> Result<()> { self.logical.load(rect) }
    fn pop(&mut self) -> bool { self.logical.pop() }
    fn check_overlay(&mut self, _: &PreparedOverlay) -> Result<()> { Ok(()) }
    fn capture_overlay(&mut self, overlay: &PreparedOverlay) -> Result<CapturedOverlay> {
        CapturedOverlay::capture(overlay, layout(), &self.frame.lock().unwrap())
    }
    fn present_overlay(&mut self, overlay: &CapturedOverlay, content: OverlayContent) -> Result<()> {
        overlay.present(&layout(), &mut self.frame.lock().unwrap(), content)
    }
    fn hold_overlay(&mut self, overlay: Arc<CapturedOverlay>) -> Result<Option<RectHold>> {
        let frame = self.frame.clone();
        let active = self.writer_active.clone();
        let (started, ready) = mpsc::channel();
        let hold = RectHold::try_spawn(move |stop| {
            active.store(true, Ordering::Release);
            started.send(()).unwrap();
            while !stop.load(Ordering::Acquire) {
                overlay.poll(&layout(), &mut frame.lock().unwrap())?;
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
            active.store(false, Ordering::Release);
            Ok(())
        })?;
        ready.recv().unwrap();
        Ok(Some(hold))
    }
}

#[tokio::test]
async fn static_foreground_restores_saved_pixels_at_osd_expiry() {
    // Static apps can exec over the launcher without creating game info.
    for game_info in [None, Some(GameInfo::default())] {
        let target = OsdTarget::for_foreground(false, game_info.as_ref(), false);
        assert_eq!(target, OsdTarget::Framebuffer);
        let mut platform = FramebufferPlatform::new().unwrap();
        let original = platform.frame.lock().unwrap().clone();
        let mut osd = Osd::new(Stylesheet::default());
        osd.show(&mut platform, OsdKind::Volume, "Volume +", 0.4, target.repaints(), None).unwrap();
        assert!(*platform.frame.lock().unwrap() != original);
        assert!(platform.writer_active.load(Ordering::Acquire));
        tokio::time::sleep_until(osd.next_wake().unwrap()).await;
        osd.tick().unwrap();
        assert!(!osd.is_visible());
        assert!(!platform.writer_active.load(Ordering::Acquire));
        assert!(*platform.frame.lock().unwrap() == original,
            "a static foreground must recover all saved pixels at expiry");
    }
    let retroarch = GameInfo { has_menu: true, ..GameInfo::default() };
    assert!(OsdTarget::for_foreground(false, Some(&retroarch), false).repaints());
    assert!(!OsdTarget::for_foreground(true, Some(&retroarch), false).repaints());
}

#[tokio::test]
async fn foreground_renderers_retire_fallback_writer_before_sending() {
    use std::cell::RefCell;
    for target in [OsdTarget::Launcher, OsdTarget::RetroArch, OsdTarget::Framebuffer] {
        let mut platform = FramebufferPlatform::new().unwrap();
        let original = platform.frame.lock().unwrap().clone();
        let mut osd = Osd::new(Stylesheet::default());
        osd.show(&mut platform, OsdKind::Volume, "Volume +", 0.3, false, None).unwrap();
        let osd = RefCell::new(osd);
        let actions = RefCell::new(Vec::new());
        let assert_retired = || {
            assert!(!platform.writer_active.load(Ordering::Acquire), "send raced the framebuffer writer");
            assert!(!osd.borrow().is_visible());
            assert!(*platform.frame.lock().unwrap() == original);
        };
        present_foreground_osd(
            target, OsdEvent::new(OsdKind::Volume, 0.4, 1, 7), 0.4,
            |_| { assert_retired(); actions.borrow_mut().push("launcher"); Ok(()) },
            |_| {
                assert_retired();
                actions.borrow_mut().push("retroarch");
                std::future::ready(Ok(()))
            },
            |action| match action {
                FramebufferOsdAction::Retire => {
                    osd.borrow_mut().hide().unwrap();
                    actions.borrow_mut().push("retire");
                }
                FramebufferOsdAction::Show => {
                    assert!(platform.writer_active.load(Ordering::Acquire));
                    actions.borrow_mut().push("framebuffer");
                }
            },
        ).await;
        let expected: &[&str] = match target {
            OsdTarget::Launcher => &["retire", "launcher"],
            OsdTarget::RetroArch => &["retire", "retroarch"],
            OsdTarget::Framebuffer => &["framebuffer"],
        };
        assert_eq!(actions.borrow().as_slice(), expected);
        osd.borrow_mut().hide().unwrap();
    }
}
