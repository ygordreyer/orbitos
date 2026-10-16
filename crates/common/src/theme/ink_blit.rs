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
    let x0 = x.max(clip.0 as i32).min(dest.width() as i32);
    let y0 = y.max(clip.1 as i32).min(dest.height() as i32);
    let x1 = (x + source.width() as i32)
        .min((clip.0 + clip.2) as i32)
        .max(0)
        .min(dest.width() as i32);
    let y1 = (y + source.height() as i32)
        .min((clip.1 + clip.3) as i32)
        .max(0)
        .min(dest.height() as i32);
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
        let sa = ((yy - y) as usize * sw + (x0 - x) as usize) * 4;
        let da = (yy as usize * dw + x0 as usize) * 4;
        let len = (x1 - x0) as usize * 4;
        for (s, d) in source.data()[sa..sa + len]
            .chunks_exact(4)
            .zip(dest.data_mut()[da..da + len].chunks_exact_mut(4))
        {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{Pixmap, PixmapPaint, Transform};
    #[test]
    fn all_alpha_and_destination_values_match_pattern_pipeline() {
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
                d.copy_from_slice(&[dst, dst, dst, 255]);
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
            assert!(actual.data() == expected.data(), "alpha {a}");
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
