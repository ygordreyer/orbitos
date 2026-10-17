//! Identity source-over for ink's already rasterized, premultiplied bitmaps.
use tiny_skia::{PixmapMut, PixmapRef};

pub(super) fn draw(dest: &mut PixmapMut<'_>, x: i32, y: i32, source: PixmapRef<'_>) {
    draw_impl(
        dest,
        x,
        y,
        source,
        (0, 0, dest.width(), dest.height()),
        false,
    );
}
pub(super) fn draw_clipped(
    dest: &mut PixmapMut<'_>,
    x: i32,
    y: i32,
    source: PixmapRef<'_>,
    clip: (u32, u32, u32, u32),
) {
    draw_impl(dest, x, y, source, clip, true);
}
fn draw_impl(
    dest: &mut PixmapMut<'_>,
    x: i32,
    y: i32,
    source: PixmapRef<'_>,
    clip: (u32, u32, u32, u32),
    auxiliary: bool,
) {
    let dw = dest.width() as i64;
    let dh = dest.height() as i64;
    let x0 = (x as i64).max(clip.0 as i64).min(dw);
    let y0 = (y as i64).max(clip.1 as i64).min(dh);
    let x1 = (x as i64 + source.width() as i64)
        .min(clip.0 as i64 + clip.2 as i64)
        .clamp(0, dw);
    let y1 = (y as i64 + source.height() as i64)
        .min(clip.1 as i64 + clip.3 as i64)
        .clamp(0, dh);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let dw = dest.width() as usize;
    let sw = source.width() as usize;
    crate::perf_trace::count(
        crate::perf_trace::Count::ComposedPixels,
        ((x1 - x0) * (y1 - y0)) as u64,
    );
    if auxiliary {
        crate::perf_trace::count(
            crate::perf_trace::Count::InkBitmapPixels,
            ((x1 - x0) * (y1 - y0)) as u64,
        );
    }
    for yy in y0..y1 {
        let sa = ((yy - y as i64) as usize * sw + (x0 - x as i64) as usize) * 4;
        let da = (yy as usize * dw + x0 as usize) * 4;
        let len = (x1 - x0) as usize * 4;
        crate::theme::ink_blit_neon::source_over(
            &mut dest.data_mut()[da..da + len],
            &source.data()[sa..sa + len],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{Pixmap, PixmapPaint, Transform};
    #[test]
    fn blit_randomized_clips_nonopaque_destinations_match_scalar_and_pattern() {
        let mut seed = 591723u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u32
        };
        for case in 0..240 {
            let sw = [1, 7, 8, 15, 16, 17, 31, 65][case % 8];
            let sh = 1 + random() % 13;
            let mut source = Pixmap::new(sw, sh).unwrap();
            let mut background = Pixmap::new(71, 19).unwrap();
            for image in [&mut source, &mut background] {
                for p in image.data_mut().chunks_exact_mut(4) {
                    let a = [0, 1, 127, 128, 254, 255, random() as u8][random() as usize % 7];
                    for ch in &mut p[..3] {
                        *ch = (random() % (a as u32 + 1)) as u8;
                    }
                    p[3] = a;
                }
            }
            let x = (random() % 91) as i32 - 30;
            let y = (random() % 31) as i32 - 15;
            let clip = (random() % 81, random() % 24, random() % 89, random() % 25);
            let mut expected = background.clone();
            let mut actual = background.clone();
            for dy in 0..19i64 {
                for dx in 0..71i64 {
                    if dx < clip.0 as i64
                        || dx >= clip.0 as i64 + clip.2 as i64
                        || dy < clip.1 as i64
                        || dy >= clip.1 as i64 + clip.3 as i64
                    {
                        continue;
                    }
                    let sx = dx - x as i64;
                    let sy = dy - y as i64;
                    if sx >= 0 && sx < sw as i64 && sy >= 0 && sy < sh as i64 {
                        let sa = (sy as usize * sw as usize + sx as usize) * 4;
                        let da = (dy as usize * 71 + dx as usize) * 4;
                        crate::theme::ink_blit_neon::source_over_scalar(
                            &mut expected.data_mut()[da..da + 4],
                            &source.data()[sa..sa + 4],
                        );
                    }
                }
            }
            draw_clipped(&mut actual.as_mut(), x, y, source.as_ref(), clip);
            assert_eq!(actual.data(), expected.data(), "case={case} clip={clip:?}");
            let mut pattern = background.clone();
            // tiny-skia paints a degenerate row when a 1px source ends at y=0.
            // Fully offscreen draws use the existing scalar compositor as the oracle. [code]
            if x < 71 && y < 19 && x as i64 + sw as i64 > 0 && y as i64 + sh as i64 > 0 {
                pattern.draw_pixmap(
                    x,
                    y,
                    source.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
            draw(&mut background.as_mut(), x, y, source.as_ref());
            assert_eq!(background.data(), pattern.data(), "pattern case={case}");
            for (x, y, clip) in [
                (i32::MAX, i32::MAX, (0, 0, u32::MAX, u32::MAX)),
                (i32::MIN, i32::MIN, (0, 0, 71, 19)),
                (0, 0, (u32::MAX, u32::MAX, u32::MAX, u32::MAX)),
            ] {
                let mut offscreen = actual.clone();
                draw_clipped(&mut offscreen.as_mut(), x, y, source.as_ref(), clip);
                assert_eq!(offscreen.data(), actual.data());
            }
        }
    }

    #[test]
    fn all_alpha_and_destination_values_match_pattern_pipeline() {
        for opaque in [false, true] {
            for a in 0..=255u32 {
                let mut source = Pixmap::new(256, 256).unwrap();
                let mut expected = Pixmap::new(256, 256).unwrap();
                for (i, (s, d)) in source
                    .data_mut()
                    .chunks_exact_mut(4)
                    .zip(expected.data_mut().chunks_exact_mut(4))
                    .enumerate()
                {
                    let src = (i / 256) as u32 % (a + 1);
                    s.copy_from_slice(&[src as u8, (src / 2) as u8, (a - src) as u8, a as u8]);
                    let dst = (i % 256) as u8;
                    d.copy_from_slice(&if opaque {
                        [dst, dst, dst, 255]
                    } else {
                        [dst, dst / 2, 0, dst]
                    });
                }
                let mut actual = expected.clone();
                expected.draw_pixmap(
                    0,
                    0,
                    source.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                draw(&mut actual.as_mut(), 0, 0, source.as_ref());
                assert!(
                    actual.data() == expected.data(),
                    "alpha {a}, opaque destination {opaque}"
                );
            }
        }
    }
}

pub(super) fn reference() -> bool {
    #[cfg(test)]
    {
        std::env::var_os("ALLIUM_INK_REFERENCE").is_some()
    }
    #[cfg(not(test))]
    {
        false
    }
}

pub(super) fn bitmap(dest: &mut tiny_skia::Pixmap, x: i32, y: i32, source: PixmapRef<'_>) {
    if reference() {
        dest.draw_pixmap(
            x,
            y,
            source,
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    } else {
        let clip = (0, 0, dest.width(), dest.height());
        draw_clipped(&mut dest.as_mut(), x, y, source, clip);
    }
}
