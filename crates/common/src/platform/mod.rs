#[cfg(any(test, feature = "miyoo"))]
#[path = "miyoo/cpu_tuning.rs"]
pub mod cpu_tuning;

#[cfg(any(test, feature = "miyoo", feature = "perf-bench"))]
mod framebuffer_pixels;
mod framebuffer_pixels_neon;

#[cfg(all(any(test, feature = "perf-bench"), not(feature = "miyoo")))]
#[path = "miyoo/framebuffer.rs"]
mod framebuffer_host_tests;

#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
mod mock;

#[cfg(feature = "miyoo")]
pub mod miyoo;
#[cfg(feature = "simulator")]
pub mod simulator;

use anyhow::Result;
use async_trait::async_trait;
use enum_map::Enum;
use serde::{Deserialize, Serialize};

use crate::{
    battery::Battery,
    display::{Display, settings::DisplaySettings},
};

#[cfg(feature = "miyoo")]
pub type DefaultPlatform = miyoo::MiyooPlatform;

#[cfg(feature = "simulator")]
pub type DefaultPlatform = simulator::SimulatorPlatform;

#[cfg(not(any(feature = "miyoo", feature = "simulator")))]
pub type DefaultPlatform = mock::MockPlatform;

// Platform is not threadsafe because it is ?Send
#[async_trait(?Send)]
pub trait Platform {
    type Display: Display;
    type Battery: Battery + 'static;
    type SuspendContext;

    fn new() -> Result<Self>
    where
        Self: Sized;

    fn display(&mut self) -> Result<Self::Display>;

    /// A display whose pixmap may start empty, for callers that fill only the part of the
    /// frame they need with `Display::read_rect`
    fn display_partial(&mut self) -> Result<Self::Display> {
        self.display()
    }

    fn battery(&self) -> Result<Self::Battery>;

    /// Discard events queued before a new input session, when supported.
    fn reset_input(&mut self) -> Result<()> {
        Ok(())
    }

    async fn poll(&mut self) -> KeyEvent;

    fn shutdown(&self) -> Result<()>;

    fn suspend(&self) -> Result<Self::SuspendContext>;

    fn unsuspend(&self, ctx: Self::SuspendContext) -> Result<()>;

    fn set_volume(&mut self, volume: i32) -> Result<()>;

    fn get_brightness(&self) -> Result<u8>;

    fn set_brightness(&mut self, brightness: u8) -> Result<()>;

    fn set_display_settings(&mut self, settings: &mut DisplaySettings) -> Result<()>;

    fn device_model() -> String;

    fn firmware() -> String;

    fn has_wifi() -> bool;

    fn has_lid() -> bool;

    fn daemon(&self) {}
}

#[cfg(any(test, feature = "miyoo"))]
fn reset_input_reader<T>(reader: &mut T, open: impl FnOnce() -> Result<T>) -> Result<()> {
    // Open first so a failed reset preserves the current reader and its queue.
    *reader = open()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Pressed(Key),
    Released(Key),
    Autorepeat(Key),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Enum)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    X,
    Y,
    Start,
    Select,
    L,
    R,
    Menu,
    L2,
    R2,
    Power,
    VolDown,
    VolUp,
    LidClose,
    Unknown,
}

#[cfg(test)]
mod input_session_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::fd::AsRawFd;

    struct Reader {
        queued: VecDeque<KeyEvent>,
        _fd: std::io::PipeWriter,
    }

    fn reader() -> (Reader, std::io::PipeReader) {
        let (read, write) = std::io::pipe().unwrap();
        (
            Reader {
                queued: VecDeque::new(),
                _fd: write,
            },
            read,
        )
    }

    fn writer_closed(read: &std::io::PipeReader) -> bool {
        let mut fd = libc::pollfd {
            fd: read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut fd, 1, 0) } >= 0);
        fd.revents & libc::POLLHUP != 0
    }

    #[test]
    fn reset_discards_old_menu_press_keeps_new_edges_and_closes_old_fd() {
        let (mut input, old_fd) = reader();
        input.queued.push_back(KeyEvent::Pressed(Key::Menu));
        let (next, new_fd) = reader();
        reset_input_reader(&mut input, || Ok(next)).unwrap();
        assert!(writer_closed(&old_fd), "old reader leaked its file descriptor");
        assert!(!writer_closed(&new_fd));
        input.queued.push_back(KeyEvent::Pressed(Key::A));
        assert_eq!(input.queued.pop_front(), Some(KeyEvent::Pressed(Key::A)));
        assert_eq!(input.queued.pop_front(), None);
    }

    #[test]
    fn failed_reset_preserves_reader_and_queued_events() {
        let (mut input, old_fd) = reader();
        input.queued.push_back(KeyEvent::Pressed(Key::Menu));
        assert!(reset_input_reader(&mut input, || anyhow::bail!("open failed")).is_err());
        assert!(!writer_closed(&old_fd));
        assert_eq!(input.queued.pop_front(), Some(KeyEvent::Pressed(Key::Menu)));
    }
}

#[cfg(all(feature = "perf-bench", not(feature = "miyoo")))]
pub use framebuffer_host_tests::PresenterProbe;

#[cfg(feature = "perf-bench")]
#[doc(hidden)]
pub fn benchmark_write_rotated_bgra(
    frame: &mut [u8], scratch: &mut [u8], pixels: tiny_skia::PixmapRef<'_>,
    yoffset: usize, bytes_per_pixel: usize, area: (usize, usize, usize, usize),
) {
    framebuffer_pixels::write_rotated_bgra(frame, scratch, pixels, yoffset, bytes_per_pixel, area);
}
