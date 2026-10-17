#![cfg(all(
    any(target_arch = "arm", target_arch = "aarch64"),
    target_feature = "neon"
))]
// ARMv7 intrinsics require unsafe blocks even when crate-wide NEON is enabled.
#![cfg_attr(target_arch = "aarch64", allow(unused_unsafe))]
#![cfg_attr(
    not(any(test, feature = "miyoo", feature = "perf-bench")),
    allow(dead_code)
)]

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::*;
#[cfg(target_arch = "arm")]
use core::arch::arm::*;

/// Source and destination contain the same number of tightly packed four-byte pixels.
#[inline]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
pub(super) unsafe fn write_reversed_bgra(target: &mut [u8], source: &[u8]) {
    assert_eq!(target.len(), source.len());
    assert_eq!(source.len() % 4, 0);
    let mut remaining = source.len();
    let mut written = 0;
    // Each structured load/store touches exactly 32 bytes, without alignment hints.
    // Exclusive slices cannot overlap. Stores advance sequentially through the row.
    unsafe {
        while remaining >= 32 {
            let channels = vld4_u8(source.as_ptr().add(remaining - 32));
            let bgra = uint8x8x4_t(
                vrev64_u8(channels.2),
                vrev64_u8(channels.1),
                vrev64_u8(channels.0),
                vrev64_u8(channels.3),
            );
            vst4_u8(target.as_mut_ptr().add(written), bgra);
            remaining -= 32;
            written += 32;
        }
    }
    for (dest, src) in target[written..]
        .chunks_exact_mut(4)
        .zip(source[..remaining].chunks_exact(4).rev())
    {
        dest.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
    }
}

#[inline]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn premultiply(channel: uint8x8_t, alpha: uint8x8_t) -> uint8x8_t {
    // The caller and module cfg guarantee NEON.
    unsafe {
        let product = vmull_u8(channel, alpha);
        // Exact floor(product / 255) for all products in 0..=65025, matching Color.
        let biased = vaddq_u16(
            vaddq_u16(product, vdupq_n_u16(1)),
            vshrq_n_u16::<8>(product),
        );
        vshrn_n_u16::<8>(biased)
    }
}

#[inline]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
pub(super) unsafe fn read_reversed_bgra(target: &mut [u8], source: &[u8]) {
    assert_eq!(target.len(), source.len());
    assert_eq!(source.len() % 4, 0);
    let mut remaining = source.len();
    let mut written = 0;
    // vld4/vst4 touch exactly eight pixels. Both spans are disjoint and in bounds.
    unsafe {
        while remaining >= 32 {
            let channels = vld4_u8(source.as_ptr().add(remaining - 32));
            let alpha = vrev64_u8(channels.3);
            let rgba = uint8x8x4_t(
                premultiply(vrev64_u8(channels.2), alpha),
                premultiply(vrev64_u8(channels.1), alpha),
                premultiply(vrev64_u8(channels.0), alpha),
                alpha,
            );
            vst4_u8(target.as_mut_ptr().add(written), rgba);
            remaining -= 32;
            written += 32;
        }
    }
    for (dest, src) in target[written..]
        .chunks_exact_mut(4)
        .zip(source[..remaining].chunks_exact(4).rev())
    {
        let alpha = src[3] as u16;
        dest.copy_from_slice(&[
            (src[2] as u16 * alpha / 255) as u8,
            (src[1] as u16 * alpha / 255) as u8,
            (src[0] as u16 * alpha / 255) as u8,
            src[3],
        ]);
    }
}
