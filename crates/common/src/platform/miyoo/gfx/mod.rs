//! Thread-confined MY283 GFX experiment. No presentation path uses this backend.
//!
//! Prepare performs cache maintenance; submit and wait are separate for measurement.
//! Pending operations borrow their buffers until completion. Failed waits prohibit CPU
//! access, and dropping a buffer with unconfirmed DMA deliberately leaks it for safety.
#![cfg_attr(
    not(all(feature = "miyoo", target_os = "linux", target_arch = "arm")),
    allow(dead_code)
)]
pub mod abi;
mod buffer;
mod runtime;
pub use buffer::DmaSurface;
use std::{fmt, rc::Rc};

#[derive(Clone, Debug)]
pub enum Error {
    Unavailable(String),
    Invalid(&'static str),
    Vendor { operation: &'static str, code: i32 },
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "MI_GFX unavailable: {message}"),
            Self::Invalid(message) => write!(f, "MI_GFX: {message}"),
            Self::Vendor { operation, code } => write!(f, "{operation}: {code:#010x}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

/// Shares one balanced SYS/GFX lifetime across all threads and surfaces.
/// Handles remain thread-confined; all vendor calls use one process-wide lock.
/// Other in-process MI_SYS clients must coordinate their calls with this backend.
#[derive(Clone)]
pub struct Gfx {
    runtime: Rc<runtime::Runtime>,
}
impl Gfx {
    pub fn open() -> Result<Self> {
        Ok(Self {
            runtime: runtime::load()?,
        })
    }
    pub fn allocate(&self, width: u32, height: u32, format: libc::c_int) -> Result<DmaSurface> {
        DmaSurface::allocate(self, width, height, format)
    }
    /// The vendored MI_SYS API has no free-MMA query. Allocation errors remain observable.
    pub fn mma_availability(&self) -> &'static str {
        "not offered by vendored MI_SYS API"
    }

    /// # Safety
    /// Caller must own exclusive access to the entire page-rounded physical range until
    /// the returned surface is dropped, including against external processes and scanout.
    pub unsafe fn map_external(&self, descriptor: abi::Surface) -> Result<DmaSurface> {
        unsafe { DmaSurface::external(self, descriptor) }
    }

    pub fn prepare_blit<'a>(
        &'a self,
        source: &'a DmaSurface,
        source_rect: abi::Rect,
        destination: &'a mut DmaSurface,
        destination_rect: abi::Rect,
        options: abi::Opt,
    ) -> Result<Prepared<'a>> {
        self.validate(source, source_rect)?;
        self.validate(destination, destination_rect)?;
        validate_rect(options.clip, &destination.descriptor)?;
        source.flush()?;
        destination.flush()?;
        Ok(Prepared {
            gfx: self,
            source: Some(source),
            destination,
            source_rect,
            destination_rect,
            options,
            color: 0,
        })
    }
    pub fn prepare_fill<'a>(
        &'a self,
        destination: &'a mut DmaSurface,
        rect: abi::Rect,
        color: u32,
    ) -> Result<Prepared<'a>> {
        self.validate(destination, rect)?;
        destination.flush()?;
        Ok(Prepared {
            gfx: self,
            source: None,
            destination,
            source_rect: abi::Rect::default(),
            destination_rect: rect,
            options: abi::Opt::default(),
            color,
        })
    }
    fn validate(&self, surface: &DmaSurface, rect: abi::Rect) -> Result<()> {
        if !self.runtime.same_driver(&surface.runtime) {
            return Err(Error::Invalid("surface belongs to another runtime"));
        }
        validate_rect(rect, &surface.descriptor)
    }
}
fn validate_rect(rect: abi::Rect, surface: &abi::Surface) -> Result<()> {
    if rect.x < 0
        || rect.y < 0
        || rect.width == 0
        || rect.height == 0
        || rect.x as u64 + rect.width as u64 > surface.width as u64
        || rect.y as u64 + rect.height as u64 > surface.height as u64
    {
        Err(Error::Invalid("rectangle outside surface"))
    } else {
        Ok(())
    }
}

#[must_use = "submit the prepared operation"]
pub struct Prepared<'a> {
    gfx: &'a Gfx,
    source: Option<&'a DmaSurface>,
    destination: &'a mut DmaSurface,
    source_rect: abi::Rect,
    destination_rect: abi::Rect,
    options: abi::Opt,
    color: u32,
}
impl<'a> Prepared<'a> {
    pub fn submit(mut self) -> Result<Pending<'a>> {
        // Other operations may have been submitted since preparation. Serialize this batch.
        let mut calls = self.gfx.runtime.lock();
        calls.wait()?;
        let mut dst = self.destination.descriptor;
        let mut fence = 0;
        calls.mark_pending();
        self.destination.gpu_dirty.set(true);
        let (name, code) = if let Some(source) = self.source {
            let mut src = source.descriptor;
            ("MI_GFX_BitBlit", unsafe {
                (calls.api.blit)(
                    &mut src,
                    &mut self.source_rect,
                    &mut dst,
                    &mut self.destination_rect,
                    &mut self.options,
                    &mut fence,
                )
            })
        } else {
            ("MI_GFX_QuickFill", unsafe {
                (calls.api.fill)(&mut dst, &mut self.destination_rect, self.color, &mut fence)
            })
        };
        if let Err(error) = runtime::check(name, code) {
            // Even a driver error does not prove no work was queued.
            calls.wait()?;
            return Err(error);
        }
        drop(calls);
        Ok(Pending {
            operation: self,
            fence,
            completed: false,
        })
    }
}

#[must_use = "wait before accessing participating buffers"]
pub struct Pending<'a> {
    operation: Prepared<'a>,
    fence: u16,
    completed: bool,
}
impl Pending<'_> {
    pub fn fence(&self) -> u16 {
        self.fence
    }
    pub fn wait(&mut self) -> Result<()> {
        self.operation.gfx.runtime.wait()?;
        self.completed = true;
        Ok(())
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::tests::{events, reset, runtime};
    fn gfx() -> Gfx {
        Gfx {
            runtime: runtime().unwrap(),
        }
    }
    #[test]
    fn gfx_allocation_failure_and_mapping_cleanup() {
        reset(Some("alloc"));
        {
            let gfx = gfx();
            assert!(gfx.allocate(17, 1, abi::ARGB8888).is_err());
        }
        assert_eq!(events(), ["init", "open", "alloc", "close", "exit"]);
        reset(Some("mmap"));
        {
            let gfx = gfx();
            assert!(gfx.allocate(17, 1, abi::ARGB8888).is_err());
        }
        assert_eq!(
            events(),
            ["init", "open", "alloc", "mmap", "free", "close", "exit"]
        );
    }
    #[test]
    fn gfx_borrowed_submission_cache_and_drop_order() {
        reset(None);
        {
            let gfx = gfx();
            let src = gfx.allocate(17, 1, abi::ABGR8888).unwrap();
            let mut dst = gfx.allocate(17, 1, abi::ARGB8888).unwrap();
            assert!(dst.bytes().unwrap().iter().all(|&v| v == 0));
            let rect = dst.rect();
            let prepared = gfx
                .prepare_blit(&src, src.rect(), &mut dst, rect, abi::Opt::copy(17, 1))
                .unwrap();
            let pending = prepared.submit().unwrap();
            assert_eq!(pending.fence(), 0);
            drop(pending);
            dst.bytes().unwrap();
        }
        assert_eq!(
            events(),
            [
                "init", "open", "alloc", "mmap", "alloc", "mmap", "flush", "flush", "blit", "wait",
                "flush", "munmap", "free", "munmap", "free", "close", "exit"
            ]
        );
    }
    #[test]
    fn gfx_failed_wait_blocks_cpu_access_until_confirmed() {
        reset(None);
        let gfx = gfx();
        let mut dst = gfx.allocate(8, 8, abi::ARGB8888).unwrap();
        let rect = dst.rect();
        let mut pending = gfx
            .prepare_fill(&mut dst, rect, 0)
            .unwrap()
            .submit()
            .unwrap();
        reset(Some("wait"));
        assert!(pending.wait().is_err());
        drop(pending);
        assert!(dst.bytes_mut().is_err());
        assert_eq!(events(), ["wait", "wait", "wait"]);
        reset(None);
        dst.bytes_mut().unwrap();
        assert_eq!(events(), ["wait", "flush"]);
    }
    #[test]
    fn gfx_rejects_out_of_bounds_before_cache_or_submission() {
        reset(None);
        let gfx = gfx();
        let mut dst = gfx.allocate(8, 8, abi::ARGB8888).unwrap();
        reset(None);
        assert!(
            gfx.prepare_fill(&mut dst, abi::Rect::full(9, 8), 0)
                .is_err()
        );
        assert!(events().is_empty());
    }
    #[cfg(not(all(feature = "miyoo", target_os = "linux", target_arch = "arm")))]
    #[test]
    fn gfx_host_backend_is_unavailable() {
        assert!(matches!(Gfx::open(), Err(Error::Unavailable(_))));
    }
}
