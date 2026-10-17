use super::{
    Error, Gfx, Result, abi,
    runtime::{Runtime, check},
};
use std::{cell::Cell, ptr::NonNull, rc::Rc};

/// Persistent cached MMA allocation. CPU access always first confirms DMA completion.
pub struct DmaSurface {
    pub(crate) descriptor: abi::Surface,
    pub(crate) runtime: Rc<Runtime>,
    mapping: NonNull<u8>,
    allocation_len: u32,
    owned: bool,
    pub(crate) gpu_dirty: Cell<bool>,
}

pub(crate) fn page_size() -> Result<u32> {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size <= 0 || size as u64 > u32::MAX as u64 || !(size as u32).is_power_of_two() {
        return Err(Error::Invalid("invalid system page size"));
    }
    Ok(size as u32)
}

pub(crate) fn layout(width: u32, height: u32, page: u32) -> Result<(u32, u32)> {
    if width == 0 || height == 0 || !page.is_power_of_two() {
        return Err(Error::Invalid("empty surface or invalid page size"));
    }
    let stride = width
        .checked_mul(4)
        .and_then(|n| n.checked_add(15))
        .map(|n| n & !15)
        .ok_or(Error::Invalid("stride overflow"))?;
    let len = allocation_len(stride, height, page)?;
    Ok((stride, len))
}

fn allocation_len(stride: u32, height: u32, page: u32) -> Result<u32> {
    stride
        .checked_mul(height)
        .and_then(|n| n.checked_add(page - 1))
        .map(|n| n & !(page - 1))
        .filter(|&n| n > 0 && n as u64 <= isize::MAX as u64)
        .ok_or(Error::Invalid("allocation overflow"))
}

impl DmaSurface {
    pub(crate) fn allocate(
        gfx: &Gfx,
        width: u32,
        height: u32,
        format: libc::c_int,
    ) -> Result<Self> {
        validate_format(format)?;
        let page = page_size()?;
        let (stride, len) = layout(width, height, page)?;
        let mut physical = 0;
        {
            let calls = gfx.runtime.lock();
            check("MI_SYS_MMA_Alloc", unsafe {
                (calls.api.alloc)(std::ptr::null_mut(), len, &mut physical)
            })?;
        }
        let descriptor = abi::Surface {
            physical,
            format,
            width,
            height,
            stride,
        };
        // SAFETY: this is the newly allocated physical range, without other aliases.
        let surface = unsafe { Self::map(gfx, descriptor, len, true) };
        match surface {
            Ok(surface) => {
                // MMA contents are unspecified. Initialize before exposing safe byte slices.
                unsafe {
                    std::ptr::write_bytes(surface.mapping.as_ptr(), 0, len as usize);
                }
                Ok(surface)
            }
            Err(e) => {
                let calls = gfx.runtime.lock();
                unsafe {
                    (calls.api.free)(physical);
                }
                Err(e)
            }
        }
    }

    /// # Safety
    /// The physical range must remain valid and exclusively available until this surface is
    /// dropped. No other CPU or DMA user may access it. The range must cover the rounded size.
    pub(crate) unsafe fn external(gfx: &Gfx, descriptor: abi::Surface) -> Result<Self> {
        validate_format(descriptor.format)?;
        let page = page_size()?;
        let min_stride = descriptor
            .width
            .checked_mul(4)
            .ok_or(Error::Invalid("stride overflow"))?;
        if descriptor.width == 0
            || descriptor.height == 0
            || descriptor.stride < min_stride
            || descriptor.stride % 16 != 0
        {
            return Err(Error::Invalid("invalid external surface geometry"));
        }
        let len = allocation_len(descriptor.stride, descriptor.height, page)?;
        unsafe { Self::map(gfx, descriptor, len, false) }
    }

    unsafe fn map(gfx: &Gfx, descriptor: abi::Surface, len: u32, owned: bool) -> Result<Self> {
        if descriptor.physical == 0 || descriptor.physical % page_size()? as u64 != 0 {
            return Err(Error::Invalid("physical address is not page aligned"));
        }
        let mut address = std::ptr::null_mut();
        let calls = gfx.runtime.lock();
        check("MI_SYS_Mmap", unsafe {
            (calls.api.mmap)(descriptor.physical, len, &mut address, 1)
        })?;
        let mapping =
            NonNull::new(address.cast::<u8>()).filter(|p| p.as_ptr() != libc::MAP_FAILED.cast());
        let Some(mapping) = mapping else {
            return Err(Error::Invalid("MI_SYS_Mmap returned invalid address"));
        };
        if mapping.as_ptr() as usize % page_size()? as usize != 0 {
            unsafe {
                (calls.api.munmap)(address, len);
            }
            return Err(Error::Invalid("mapping is not page aligned"));
        }
        Ok(Self {
            descriptor,
            runtime: gfx.runtime.clone(),
            mapping,
            allocation_len: len,
            owned,
            gpu_dirty: Cell::new(false),
        })
    }

    pub fn width(&self) -> u32 {
        self.descriptor.width
    }
    pub fn height(&self) -> u32 {
        self.descriptor.height
    }
    pub fn stride(&self) -> usize {
        self.descriptor.stride as usize
    }
    pub fn allocation_len(&self) -> usize {
        self.allocation_len as usize
    }
    pub fn rect(&self) -> abi::Rect {
        abi::Rect::full(self.width(), self.height())
    }

    /// Clean and invalidate the full page-rounded cached mapping, after any pending DMA.
    pub fn flush(&self) -> Result<()> {
        let mut calls = self.runtime.lock();
        calls.wait()?;
        check("MI_SYS_FlushInvCache", unsafe {
            (calls.api.flush)(self.mapping.as_ptr().cast(), self.allocation_len)
        })?;
        self.gpu_dirty.set(false);
        Ok(())
    }

    fn acquire_cpu(&self) -> Result<()> {
        self.runtime.wait()?;
        if self.gpu_dirty.get() {
            self.flush()?;
        }
        Ok(())
    }
    pub fn bytes(&self) -> Result<&[u8]> {
        self.acquire_cpu()?;
        // SAFETY: the mapped range is initialized and no DMA is pending. Borrow excludes writes.
        Ok(unsafe {
            std::slice::from_raw_parts(
                self.mapping.as_ptr(),
                self.stride() * self.height() as usize,
            )
        })
    }
    pub fn bytes_mut(&mut self) -> Result<&mut [u8]> {
        self.acquire_cpu()?;
        // SAFETY: exclusive borrow excludes CPU aliases and submissions involving this surface.
        Ok(unsafe {
            std::slice::from_raw_parts_mut(
                self.mapping.as_ptr(),
                self.stride() * self.height() as usize,
            )
        })
    }
}

fn validate_format(format: libc::c_int) -> Result<()> {
    if matches!(
        format,
        abi::ARGB8888 | abi::ABGR8888 | abi::RGBA8888 | abi::BGRA8888
    ) {
        Ok(())
    } else {
        Err(Error::Invalid(
            "only 32-bit formats are supported by this buffer API",
        ))
    }
}

impl Drop for DmaSurface {
    fn drop(&mut self) {
        let mut calls = self.runtime.lock();
        if calls.wait().is_err() {
            // Keep the allocation, mapping, and initialized driver alive if DMA might still run.
            std::mem::forget(self.runtime.clone());
            return;
        }
        unsafe {
            let unmapped = (calls.api.munmap)(self.mapping.as_ptr().cast(), self.allocation_len);
            if unmapped == 0 && self.owned {
                (calls.api.free)(self.descriptor.physical);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gfx_layout_rounding_and_overflow() {
        assert_eq!(layout(176, 176, 4096).unwrap(), (704, 126976));
        assert_eq!(layout(1, 1, 4096).unwrap(), (16, 4096));
        assert_eq!(layout(640, 480, 4096).unwrap(), (2560, 1228800));
        for (w, h) in [(0, 1), (1, 0), (u32::MAX, 1), (640, u32::MAX)] {
            assert!(layout(w, h, 4096).is_err());
        }
    }
}
