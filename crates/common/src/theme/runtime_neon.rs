//! Integer row kernels shared by ARMv7 and AArch64.
#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::*;
#[cfg(target_arch = "arm")]
use core::arch::arm::*;

pub(super) const WEIGHT_BITS: u32 = 12;
pub(super) const WEIGHT_ONE: u32 = 1 << WEIGHT_BITS;
pub(super) const BLUR_RECIPROCAL: [u32; 10] = [
    0, 65536, 32768, 21846, 16384, 13108, 10923, 9363, 8192, 7282,
];

pub(super) fn bilinear_row(top: &[u32], bottom: &[u32], weight: u32, out: &mut [u32]) {
    assert_eq!(top.len(), out.len());
    assert_eq!(bottom.len(), out.len());
    assert!(weight <= WEIGHT_ONE);
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ))]
    // SAFETY: AArch64 guarantees NEON; the ARM build enables it at compile time.
    unsafe {
        bilinear_row_neon(top, bottom, weight, out)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    )))]
    bilinear_row_scalar(top, bottom, weight, out);
}

fn bilinear_row_scalar(top: &[u32], bottom: &[u32], weight: u32, out: &mut [u32]) {
    for ((out, top), bottom) in out.iter_mut().zip(top).zip(bottom) {
        *out = top * (WEIGHT_ONE - weight) + bottom * weight;
    }
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn bilinear_row_neon(top: &[u32], bottom: &[u32], weight: u32, out: &mut [u32]) {
    let end = out.len() / 4 * 4;
    for i in (0..end).step_by(4) {
        // SAFETY: validated equal slices contain all four lanes. Q12 samples are
        // at most 255 * 4096, so the Q24 weighted sum fits in u32.
        unsafe {
            let a = vld1q_u32(top.as_ptr().add(i));
            let b = vld1q_u32(bottom.as_ptr().add(i));
            vst1q_u32(
                out.as_mut_ptr().add(i),
                vmlaq_n_u32(vmulq_n_u32(a, WEIGHT_ONE - weight), b, weight),
            );
        }
    }
    bilinear_row_scalar(&top[end..], &bottom[end..], weight, &mut out[end..]);
}

pub(super) fn blur_row(sums: &mut [u16], add: &[u8], remove: &[u8], count: u16, out: &mut [u8]) {
    assert_eq!(sums.len(), out.len());
    assert!(add.is_empty() || add.len() == out.len());
    assert!(remove.is_empty() || remove.len() == out.len());
    assert!((1..=9).contains(&count));
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    ))]
    // SAFETY: NEON availability and matching slice lengths are checked above.
    unsafe {
        blur_row_neon(sums, add, remove, count, out)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "arm", target_feature = "neon")
    )))]
    blur_row_scalar(sums, add, remove, count, out);
}

fn blur_row_scalar(sums: &mut [u16], add: &[u8], remove: &[u8], count: u16, out: &mut [u8]) {
    for (i, (sum, out)) in sums.iter_mut().zip(out).enumerate() {
        if !remove.is_empty() {
            *sum -= u16::from(remove[i]);
        }
        if !add.is_empty() {
            *sum += u16::from(add[i]);
        }
        *out = ((*sum + count / 2) / count) as u8;
    }
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "arm", target_feature = "neon")
))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
unsafe fn blur_row_neon(sums: &mut [u16], add: &[u8], remove: &[u8], count: u16, out: &mut [u8]) {
    let end = out.len() / 8 * 8;
    let reciprocal = BLUR_RECIPROCAL[count as usize] as u16;
    for i in (0..end).step_by(8) {
        // SAFETY: each full block is within all nonempty slices. Sums contain
        // at most nine bytes. For divisors 2..=9, ceil(65536/d) gives exact
        // division throughout 0..=255*d+d/2 (exhaustively checked below).
        unsafe {
            let mut sum = vld1q_u16(sums.as_ptr().add(i));
            if !remove.is_empty() {
                sum = vsubq_u16(sum, vmovl_u8(vld1_u8(remove.as_ptr().add(i))));
            }
            if !add.is_empty() {
                sum = vaddq_u16(sum, vmovl_u8(vld1_u8(add.as_ptr().add(i))));
            }
            vst1q_u16(sums.as_mut_ptr().add(i), sum);
            let result = if count == 1 {
                sum
            } else {
                let rounded = vaddq_u16(sum, vdupq_n_u16(count / 2));
                vcombine_u16(
                    vshrn_n_u32::<16>(vmull_n_u16(vget_low_u16(rounded), reciprocal)),
                    vshrn_n_u32::<16>(vmull_n_u16(vget_high_u16(rounded), reciprocal)),
                )
            };
            vst1_u8(out.as_mut_ptr().add(i), vqmovn_u16(result));
        }
    }
    blur_row_scalar(
        &mut sums[end..],
        if add.is_empty() { add } else { &add[end..] },
        if remove.is_empty() {
            remove
        } else {
            &remove[end..]
        },
        count,
        &mut out[end..],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reciprocal_division_is_exact_for_all_blur_sums() {
        for count in 1u32..=9 {
            let reciprocal = BLUR_RECIPROCAL[count as usize];
            assert_eq!(reciprocal, 65536u32.div_ceil(count));
            for sum in 0..=255 * count {
                let rounded = sum + count / 2;
                assert_eq!((rounded * reciprocal) >> 16, rounded / count);
            }
        }
    }

    #[test]
    fn integer_rows_neon_match_scalar_with_tails_and_offset_slices() {
        let mut seed = 1u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for len in [0, 1, 7, 8, 15, 16, 17, 31, 65, 767] {
            for alpha in [0, 1, 127, 128, 254, 255] {
                let top: Vec<_> = (0..len)
                    .map(|_| next() % (alpha * WEIGHT_ONE + 1))
                    .collect();
                let bottom: Vec<_> = (0..len)
                    .map(|_| next() % (alpha * WEIGHT_ONE + 1))
                    .collect();
                for weight in [0, 1, 2047, 2048, 4095, 4096] {
                    let mut actual = vec![u32::MAX; len + 2];
                    let mut expected = actual.clone();
                    bilinear_row_scalar(&top, &bottom, weight, &mut expected[1..len + 1]);
                    bilinear_row(&top, &bottom, weight, &mut actual[1..len + 1]);
                    assert_eq!(actual, expected);
                }
                for count in 1..=9 {
                    let add: Vec<_> = (0..len).map(|_| (next() % (alpha + 1)) as u8).collect();
                    let remove: Vec<_> = (0..len).map(|_| (next() % (alpha + 1)) as u8).collect();
                    for use_add in [false, true] {
                        for use_remove in [false, true] {
                            let initial: Vec<_> = (0..len)
                                .map(|i| {
                                    (next() % (alpha * (count as u32 - u32::from(use_add)) + 1))
                                        as u16
                                        + if use_remove { u16::from(remove[i]) } else { 0 }
                                })
                                .collect();
                            let mut actual_sum = initial.clone();
                            let mut expected_sum = initial;
                            let mut actual = vec![197; len + 2];
                            let mut expected = actual.clone();
                            let a = if use_add { &add[..] } else { &[] };
                            let r = if use_remove { &remove[..] } else { &[] };
                            blur_row_scalar(
                                &mut expected_sum,
                                a,
                                r,
                                count,
                                &mut expected[1..len + 1],
                            );
                            blur_row(&mut actual_sum, a, r, count, &mut actual[1..len + 1]);
                            assert_eq!(actual_sum, expected_sum);
                            assert_eq!(actual, expected);
                        }
                    }
                }
            }
        }
    }
}
