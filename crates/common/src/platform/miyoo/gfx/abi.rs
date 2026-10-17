//! ABI for the vendored MY283 SDK, not newer device-ID variants.
use libc::{c_int, c_void};

pub const RGB565: c_int = 6;
pub const ARGB8888: c_int = 11;
pub const ABGR8888: c_int = 14;
pub const RGBA8888: c_int = 20;
pub const BGRA8888: c_int = 21;
pub const ZERO: c_int = 0;
pub const ONE: c_int = 1;
pub const SRCALPHA: c_int = 4;
pub const INVSRCALPHA: c_int = 5;
pub const ROTATE_0: c_int = 0;
pub const ROTATE_90: c_int = 1;
pub const ROTATE_180: c_int = 2;
pub const ROTATE_270: c_int = 3;
pub const MIRROR_NONE: c_int = 0;
pub const MIRROR_HORIZONTAL: c_int = 1;
pub const MIRROR_VERTICAL: c_int = 2;
pub const MIRROR_BOTH: c_int = 3;
pub const NOFX: c_int = 0;
pub const SRC_PREMULTIPLY: c_int = 0x08;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    pub const fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ColorKeyValue {
    pub start: u32,
    pub end: u32,
}
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ColorKeyInfo {
    pub enabled: u8,
    pub operation: c_int,
    pub format: c_int,
    pub values: ColorKeyValue,
}
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Surface {
    pub physical: u64,
    pub format: c_int,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Opt {
    pub clip: Rect,
    pub source_key: ColorKeyInfo,
    pub destination_key: ColorKeyInfo,
    pub source_factor: c_int,
    pub destination_factor: c_int,
    pub mirror: c_int,
    pub rotate: c_int,
    pub flags: c_int,
    pub source_constant: u32,
    pub destination_constant: u32,
}
impl Opt {
    pub fn copy(width: u32, height: u32) -> Self {
        Self {
            clip: Rect::full(width, height),
            source_factor: ONE,
            ..Self::default()
        }
    }
}

pub type Init = unsafe extern "C" fn() -> i32;
pub type Alloc = unsafe extern "C" fn(*mut u8, u32, *mut u64) -> i32;
pub type Free = unsafe extern "C" fn(u64) -> i32;
pub type Mmap = unsafe extern "C" fn(u64, u32, *mut *mut c_void, u8) -> i32;
pub type Munmap = unsafe extern "C" fn(*mut c_void, u32) -> i32;
pub type Flush = unsafe extern "C" fn(*mut c_void, u32) -> i32;
pub type Wait = unsafe extern "C" fn(u8, u16) -> i32;
pub type Fill = unsafe extern "C" fn(*mut Surface, *mut Rect, u32, *mut u16) -> i32;
pub type Blit = unsafe extern "C" fn(
    *mut Surface,
    *mut Rect,
    *mut Surface,
    *mut Rect,
    *mut Opt,
    *mut u16,
) -> i32;

// These assertions compile on both the host and ARM, including every SDK field.
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<c_int>() == 4);
    assert!(size_of::<Rect>() == 16 && align_of::<Rect>() == 4);
    assert!(offset_of!(Rect, x) == 0 && offset_of!(Rect, y) == 4);
    assert!(offset_of!(Rect, width) == 8 && offset_of!(Rect, height) == 12);
    assert!(size_of::<ColorKeyValue>() == 8 && align_of::<ColorKeyValue>() == 4);
    assert!(offset_of!(ColorKeyValue, start) == 0 && offset_of!(ColorKeyValue, end) == 4);
    assert!(size_of::<ColorKeyInfo>() == 20 && align_of::<ColorKeyInfo>() == 4);
    assert!(offset_of!(ColorKeyInfo, enabled) == 0 && offset_of!(ColorKeyInfo, operation) == 4);
    assert!(offset_of!(ColorKeyInfo, format) == 8 && offset_of!(ColorKeyInfo, values) == 12);
    assert!(size_of::<Surface>() == 24 && align_of::<Surface>() == 8);
    assert!(offset_of!(Surface, physical) == 0 && offset_of!(Surface, format) == 8);
    assert!(
        offset_of!(Surface, width) == 12
            && offset_of!(Surface, height) == 16
            && offset_of!(Surface, stride) == 20
    );
    assert!(size_of::<Opt>() == 84 && align_of::<Opt>() == 4);
    assert!(
        offset_of!(Opt, clip) == 0
            && offset_of!(Opt, source_key) == 16
            && offset_of!(Opt, destination_key) == 36
    );
    assert!(offset_of!(Opt, source_factor) == 56 && offset_of!(Opt, destination_factor) == 60);
    assert!(
        offset_of!(Opt, mirror) == 64
            && offset_of!(Opt, rotate) == 68
            && offset_of!(Opt, flags) == 72
    );
    assert!(offset_of!(Opt, source_constant) == 76 && offset_of!(Opt, destination_constant) == 80);
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gfx_vendored_layout_and_copy_defaults() {
        assert_eq!(std::mem::size_of::<Opt>(), 84);
        assert_eq!(std::mem::align_of::<Surface>(), 8);
        let opt = Opt::copy(640, 480);
        assert_eq!(
            (opt.source_factor, opt.destination_factor, opt.flags),
            (ONE, ZERO, NOFX)
        );
        assert_eq!(opt.clip, Rect::full(640, 480));
    }
}
