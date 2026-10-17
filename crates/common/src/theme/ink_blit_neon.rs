//! Exact premultiplied RGBA rows, shared by ink bitmaps and cached text/art.

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::*;
#[cfg(all(target_arch = "arm", target_feature = "neon"))]
use core::arch::arm::*;

#[inline]
pub(super) fn source_over(dest: &mut [u8], source: &[u8]) {
    assert_eq!(dest.len(), source.len());
    assert_eq!(source.len() % 4, 0);
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ))]
    // SAFETY: aarch64 has NEON; ARM dispatch requires +neon. Slices contain equal complete rows. [code]
    unsafe {
        return source_over_neon(dest, source);
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    )))]
    source_over_scalar(dest, source);
}

pub(super) fn source_over_scalar(dest: &mut [u8], source: &[u8]) {
    for (s, d) in source.chunks_exact(4).zip(dest.chunks_exact_mut(4)) {
        match s[3] {
            0 => {}
            255 => d.copy_from_slice(s),
            a => {
                let inv = 255 - a as u32;
                for ch in 0..4 {
                    d[ch] = (s[ch] as u32 + (d[ch] as u32 * inv + 127) / 255) as u8;
                }
            }
        }
    }
}

#[inline]
pub(super) fn opaque_copy(dest: &mut [u8], source: &[u8]) {
    assert_eq!(dest.len(), source.len());
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ))]
    // SAFETY: slices have equal length and dispatch guarantees NEON. [code]
    unsafe {
        return opaque_copy_neon(dest, source);
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    )))]
    dest.copy_from_slice(source);
}

/// A premultiplied solid scrim with constant alpha, including nonopaque destinations.
/// Component/group opacity must still be applied after composing its internal layers.
pub(super) fn constant_source_over(dest: &mut [u8], source: [u8; 4]) {
    assert_eq!(dest.len() % 4, 0);
    if source[3] == 0 {
        return;
    }
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ))]
    // SAFETY: dispatch guarantees NEON, and dest contains complete RGBA pixels. [code]
    unsafe {
        return constant_source_over_neon(dest, source);
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    )))]
    constant_source_over_scalar(dest, source);
}

fn constant_source_over_scalar(dest: &mut [u8], source: [u8; 4]) {
    if source[3] == 0 {
        return;
    }
    let inv = 255 - source[3] as u32;
    for d in dest.chunks_exact_mut(4) {
        for ch in 0..4 {
            d[ch] = (source[ch] as u32 + (d[ch] as u32 * inv + 127) / 255) as u8;
        }
    }
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[inline]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn over_channel(source: uint8x8_t, dest: uint8x8_t, inv: uint8x8_t) -> uint8x8_t {
    // SAFETY: ARM code is compiled only with crate-wide +neon; aarch64 uses the feature attribute. [code]
    #[allow(unused_unsafe)]
    unsafe {
        let t = vaddq_u16(vmull_u8(dest, inv), vdupq_n_u16(128));
        let rounded = vshrn_n_u16::<8>(vaddq_u16(t, vshrq_n_u16::<8>(t)));
        vadd_u8(source, rounded)
    }
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn source_over_neon(dest: &mut [u8], source: &[u8]) {
    let end = source.len() / 32 * 32;
    // SAFETY: each block covers 8 complete pixels inside both nonoverlapping slices;
    // unaligned NEON loads/stores are supported. No load/store reaches the tail. [code]
    unsafe {
        for at in (0..end).step_by(32) {
            let s = vld4_u8(source.as_ptr().add(at));
            let alpha = vget_lane_u64::<0>(vreinterpret_u64_u8(s.3));
            if alpha == 0 {
                continue;
            }
            let out = if alpha == u64::MAX {
                s
            } else {
                let d = vld4_u8(dest.as_ptr().add(at));
                let inv = vsub_u8(vdup_n_u8(255), s.3);
                uint8x8x4_t(
                    over_channel(s.0, d.0, inv),
                    over_channel(s.1, d.1, inv),
                    over_channel(s.2, d.2, inv),
                    over_channel(s.3, d.3, inv),
                )
            };
            vst4_u8(dest.as_mut_ptr().add(at), out);
        }
    }
    source_over_scalar(&mut dest[end..], &source[end..]);
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn opaque_copy_neon(dest: &mut [u8], source: &[u8]) {
    let end = source.len() / 32 * 32;
    // SAFETY: each pair of unaligned 16-byte accesses is within equal, disjoint slices. [code]
    unsafe {
        for at in (0..end).step_by(32) {
            vst1q_u8(dest.as_mut_ptr().add(at), vld1q_u8(source.as_ptr().add(at)));
            vst1q_u8(
                dest.as_mut_ptr().add(at + 16),
                vld1q_u8(source.as_ptr().add(at + 16)),
            );
        }
    }
    dest[end..].copy_from_slice(&source[end..]);
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn constant_source_over_neon(dest: &mut [u8], source: [u8; 4]) {
    let end = dest.len() / 32 * 32;
    // SAFETY: blocks of 8 complete RGBA pixels stay within dest, including unaligned rows.
    // ARM intrinsics require the caller's crate-wide +neon; cfg excludes other ARM builds. [code]
    unsafe {
        let s = uint8x8x4_t(
            vdup_n_u8(source[0]),
            vdup_n_u8(source[1]),
            vdup_n_u8(source[2]),
            vdup_n_u8(source[3]),
        );
        let inv = vdup_n_u8(255 - source[3]);
        for at in (0..end).step_by(32) {
            let out = if source[3] == 255 {
                s
            } else {
                let d = vld4_u8(dest.as_ptr().add(at));
                uint8x8x4_t(
                    over_channel(s.0, d.0, inv),
                    over_channel(s.1, d.1, inv),
                    over_channel(s.2, d.2, inv),
                    over_channel(s.3, d.3, inv),
                )
            };
            vst4_u8(dest.as_mut_ptr().add(at), out);
        }
    }
    constant_source_over_scalar(&mut dest[end..], source);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random(seed: &mut u64) -> u8 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (*seed >> 32) as u8
    }

    fn pixel(seed: &mut u64, alpha: u8) -> [u8; 4] {
        let channel = |seed: &mut u64| (random(seed) as u16 % (alpha as u16 + 1)) as u8;
        [channel(seed), channel(seed), channel(seed), alpha]
    }

    #[test]
    fn blit_div255_exhaustive_alpha_and_channel() {
        for alpha in 0..=255u32 {
            for channel in 0..=255u32 {
                let product = channel * (255 - alpha);
                let t = product + 128;
                assert_eq!((t + (t >> 8)) >> 8, (product + 127) / 255);
            }
        }
    }

    #[test]
    fn blit_all_alpha_and_channels_match_scalar() {
        for alpha in 0..=255u8 {
            let source: Vec<_> = (0..=255u32)
                .flat_map(|c| [c.min(alpha as u32) as u8, alpha / 2, 0, alpha])
                .collect();
            let mut expected: Vec<_> = (0..=255u8).flat_map(|c| [c, c, c, 255]).collect();
            let mut actual = expected.clone();
            source_over_scalar(&mut expected, &source);
            source_over(&mut actual, &source);
            assert_eq!(actual, expected, "alpha={alpha}");
            for source in [[0, 0, 0, alpha], [alpha, alpha / 2, alpha / 3, alpha]] {
                let mut expected: Vec<_> = (0..=255u8).flat_map(|c| [c, c, c, 255]).collect();
                let mut actual = expected.clone();
                constant_source_over_scalar(&mut expected, source);
                constant_source_over(&mut actual, source);
                assert_eq!(actual, expected, "constant alpha={alpha}");
            }
        }
    }

    #[test]
    fn blit_randomized_rows_tails_offsets_and_odd_strides() {
        let mut seed = 782341u64;
        for width in [0, 1, 7, 8, 15, 16, 17, 23, 31, 32, 33, 176, 639, 640] {
            for offset in [0, 1, 3, 7, 15] {
                for mode in 0..12 {
                    let stride = width * 4 + 13;
                    let mut source = vec![91; offset + stride * 3 + 17];
                    let mut expected = vec![73; source.len()];
                    for row in 0..3 {
                        let start = offset + row * stride;
                        for x in 0..width {
                            let a = match mode {
                                0..=5 => [0, 1, 127, 128, 254, 255][mode],
                                6 => {
                                    if x / 8 % 2 == 0 {
                                        0
                                    } else {
                                        255
                                    }
                                }
                                7 => {
                                    if x % 2 == 0 {
                                        0
                                    } else {
                                        255
                                    }
                                }
                                8 => x as u8,
                                _ => random(&mut seed),
                            };
                            let d_alpha = random(&mut seed);
                            let at = start + x * 4;
                            source[at..at + 4].copy_from_slice(&pixel(&mut seed, a));
                            expected[at..at + 4].copy_from_slice(&pixel(&mut seed, d_alpha));
                        }
                    }
                    let mut actual = expected.clone();
                    for row in 0..3 {
                        let start = offset + row * stride;
                        let range = start..start + width * 4;
                        source_over_scalar(&mut expected[range.clone()], &source[range.clone()]);
                        source_over(&mut actual[range.clone()], &source[range]);
                    }
                    assert_eq!(
                        actual, expected,
                        "width={width} offset={offset} mode={mode}"
                    );
                    for alpha in [0, 1, 127, 128, 254, 255] {
                        let color = pixel(&mut seed, alpha);
                        let mut reference = actual.clone();
                        let mut constant = actual.clone();
                        for row in 0..3 {
                            let start = offset + row * stride;
                            constant_source_over_scalar(
                                &mut reference[start..start + width * 4],
                                color,
                            );
                            constant_source_over(&mut constant[start..start + width * 4], color);
                        }
                        assert_eq!(
                            constant, reference,
                            "constant width={width} offset={offset} alpha={alpha}"
                        );
                    }
                    let mut copy = expected.clone();
                    for row in 0..3 {
                        let start = offset + row * stride;
                        let range = start..start + width * 4;
                        expected[range.clone()].copy_from_slice(&source[range.clone()]);
                        opaque_copy(&mut copy[range.clone()], &source[range]);
                    }
                    assert_eq!(copy, expected, "copy width={width} offset={offset}");
                }
            }
        }
    }

    #[test]
    #[ignore]
    fn blit_host_benchmark() {
        use std::{hint::black_box, time::Instant};
        for (width, height) in [(640, 480), (176, 176)] {
            for mode in [
                "opaque",
                "transparent",
                "50/50 blocks",
                "50/50 pixels",
                "gradient",
                "constant alpha",
            ] {
                let mut seed = 418951u64;
                let mut source = vec![0; width * height * 4];
                let mut background = source.clone();
                for (i, (s, d)) in source
                    .chunks_exact_mut(4)
                    .zip(background.chunks_exact_mut(4))
                    .enumerate()
                {
                    let a = match mode {
                        "opaque" => 255,
                        "transparent" => 0,
                        "50/50 blocks" => {
                            if i / 8 % 2 == 0 {
                                0
                            } else {
                                255
                            }
                        }
                        "50/50 pixels" => {
                            if i % 2 == 0 {
                                0
                            } else {
                                255
                            }
                        }
                        "constant alpha" => 153,
                        _ => i as u8,
                    };
                    s.copy_from_slice(&pixel(&mut seed, a));
                    if mode == "constant alpha" {
                        s.copy_from_slice(&[14, 21, 25, 153]);
                    }
                    d.copy_from_slice(&pixel(&mut seed, 255));
                }
                for variant in ["scalar", "neon", "copy", "constant"] {
                    if variant == "copy" && mode != "opaque" {
                        continue;
                    }
                    if variant == "constant" && mode != "constant alpha" {
                        continue;
                    }
                    let mut dest = background.clone();
                    let iterations = if width == 640 { 8 } else { 64 };
                    let mut samples = Vec::new();
                    for _ in 0..11 {
                        dest.copy_from_slice(&background);
                        let start = Instant::now();
                        for _ in 0..iterations {
                            for (d, s) in black_box(&mut dest)
                                .chunks_exact_mut(width * 4)
                                .zip(black_box(&source).chunks_exact(width * 4))
                            {
                                match variant {
                                    "scalar" => source_over_scalar(d, s),
                                    "copy" => opaque_copy(d, s),
                                    "constant" => constant_source_over(d, [14, 21, 25, 153]),
                                    _ => source_over(d, s),
                                }
                            }
                            black_box(&dest);
                        }
                        samples.push(start.elapsed().as_secs_f64());
                    }
                    samples.sort_by(f64::total_cmp);
                    let mpixels = (width * height * iterations) as f64 / 1e6 / samples[5];
                    println!(
                        "BLIT_BENCH {width}x{height} {mode} {variant}: {mpixels:.2} Mpixel/s release={}",
                        !cfg!(debug_assertions)
                    );
                }
            }
        }
    }
}
