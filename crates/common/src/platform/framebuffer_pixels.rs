pub(super) fn validate_packed_layout(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    row_stride: usize,
    xoffset: usize,
    yoffset: usize,
    mapped_bytes: usize,
) -> anyhow::Result<()> {
    let row_bytes = width.checked_mul(bytes_per_pixel);
    let end = yoffset
        .checked_add(height)
        .and_then(|rows| rows.checked_mul(row_stride));
    anyhow::ensure!(
        width > 0
            && height > 0
            && bytes_per_pixel >= 4
            && row_bytes == Some(row_stride)
            && xoffset == 0
            && end.is_some_and(|end| end <= mapped_bytes),
        "Unsupported framebuffer layout: requires packed rows, zero xoffset and a mapped visible page"
    );
    Ok(())
}

use tiny_skia::PixmapRef;

pub(super) fn read_rotated_bgra(
    frame: &[u8],
    pixels: &mut tiny_skia::Pixmap,
    bytes_per_pixel: usize,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
) {
    let width = pixels.width() as usize;
    let height = pixels.height() as usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let at = ((height - 1 - y) * width + width - 1 - x) * bytes_per_pixel;
            pixels.pixels_mut()[y * width + x] = crate::display::color::Color::rgba(
                frame[at + 2],
                frame[at + 1],
                frame[at],
                frame[at + 3],
            )
            .into();
        }
    }
}

pub(super) fn capture_rotated_bgra(
    scratch: &mut Vec<u8>,
    pixels: &mut tiny_skia::Pixmap,
    bytes_per_pixel: usize,
    bounds @ (x0, y0, x1, y1): (usize, usize, usize, usize),
    mut read_span: impl FnMut(&mut [u8], usize),
) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width = pixels.width() as usize;
    let height = pixels.height() as usize;
    if bounds == (0, 0, width, height) {
        scratch.resize(width * height * bytes_per_pixel, 0);
        {
            let _phase = crate::perf_trace::open_phase("capture_bulk_read");
            read_span(scratch, 0);
        }
        let _phase = crate::perf_trace::open_phase("capture_convert");
        read_rotated_bgra(scratch, pixels, bytes_per_pixel, bounds);
        return;
    }
    let row_bytes = (x1 - x0) * bytes_per_pixel;
    if scratch.len() < row_bytes {
        scratch.resize(row_bytes, 0);
    }
    let row = &mut scratch[..row_bytes];
    let _phase = crate::perf_trace::open_phase("capture_rect");
    for fb_y in height - y1..height - y0 {
        read_span(row, (fb_y * width + width - x1) * bytes_per_pixel);
        let y = height - 1 - fb_y;
        for (x, pixel) in (x0..x1).rev().zip(row.chunks_exact(bytes_per_pixel)) {
            pixels.pixels_mut()[y * width + x] =
                crate::display::color::Color::rgba(pixel[2], pixel[1], pixel[0], pixel[3]).into();
        }
    }
}

pub(super) fn normalize_page(frame: &mut [u8], cached: &[u8]) {
    frame[..cached.len()].copy_from_slice(cached);
}

pub(super) fn write_rotated_bgra(
    frame: &mut [u8],
    scratch: &mut [u8],
    pixels: PixmapRef<'_>,
    location: usize,
    bytes_per_pixel: usize,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width = pixels.width() as usize;
    let height = pixels.height() as usize;
    if bytes_per_pixel == 4 {
        let _phase = crate::perf_trace::open_phase("present_convert_write");
        crate::perf_trace::count(
            crate::perf_trace::Count::ConvertedPixels,
            ((x1 - x0) * (y1 - y0)) as u64,
        );
        for y in (y0..y1).rev() {
            let start = location + ((height - 1 - y) * width + width - x1) * 4;
            let target = &mut frame[start..start + (x1 - x0) * 4];
            let source = &pixels.data()[(y * width + x0) * 4..(y * width + x1) * 4];
            write_reversed_bgra(target, source);
        }
        return;
    }
    {
        let _phase = crate::perf_trace::open_phase("present_convert");
        for y in (y0..y1).rev() {
            let start = ((height - 1 - y) * width + width - x1) * bytes_per_pixel;
            let row = &mut scratch[start..start + (x1 - x0) * bytes_per_pixel];
            let source = &pixels.pixels()[y * width + x0..y * width + x1];
            for (dest, pixel) in row
                .chunks_exact_mut(bytes_per_pixel)
                .zip(source.iter().rev())
            {
                dest[..4].copy_from_slice(&[
                    pixel.blue(),
                    pixel.green(),
                    pixel.red(),
                    pixel.alpha(),
                ]);
            }
        }
    }
    crate::perf_trace::count(
        crate::perf_trace::Count::ConvertedPixels,
        ((x1 - x0) * (y1 - y0)) as u64,
    );
    let _phase = crate::perf_trace::open_phase("present_write");
    for y in (y0..y1).rev() {
        let start = ((height - 1 - y) * width + width - x1) * bytes_per_pixel;
        let len = (x1 - x0) * bytes_per_pixel;
        let row = &mut frame[location + start..location + start + len];
        let source = &scratch[start..start + len];
        if bytes_per_pixel == 4 {
            row.copy_from_slice(source);
        } else {
            // Preserve padding bytes for the legacy wider-pixel layout.
            for (dest, pixel) in row
                .chunks_exact_mut(bytes_per_pixel)
                .zip(source.chunks_exact(bytes_per_pixel))
            {
                dest[..4].copy_from_slice(&pixel[..4]);
            }
        }
    }
}

fn write_reversed_bgra(target: &mut [u8], source: &[u8]) {
    if let (Ok(target), Ok(source)) = (
        bytemuck::try_cast_slice_mut::<u8, u32>(target),
        bytemuck::try_cast_slice::<u8, u32>(source),
    ) {
        for (dest, src) in target.iter_mut().zip(source.iter().rev()) {
            let word = u32::from_le(*src);
            *dest = ((word & 0xff00ff00) | ((word & 0xff) << 16) | ((word >> 16) & 0xff)).to_le();
        }
    } else {
        for (dest, src) in target.chunks_exact_mut(4).zip(source.chunks_exact(4).rev()) {
            dest.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use tiny_skia::Pixmap;

    #[test]
    fn dl15_randomized_partial_present_preserves_other_pages_and_sentinels() {
        let mut seed = 0x31373u32;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for case in 0..600 {
            let width = 1 + random() as usize % 97;
            let height = 1 + random() as usize % 63;
            let bpp = if case % 3 == 0 { 8 } else { 4 };
            let page = case % 3;
            let yoffset = page * height;
            let stride = width * bpp;
            let location = yoffset * stride;
            let mut frame = vec![0xa5; stride * height * 3 + 32];
            validate_packed_layout(width, height, bpp, stride, 0, yoffset, frame.len()).unwrap();
            let mut pixels = Pixmap::new(width as u32, height as u32).unwrap();
            for p in pixels.data_mut().chunks_exact_mut(4) {
                p.copy_from_slice(&[random() as u8, random() as u8, random() as u8, 255]);
            }
            let rect = Rect::new(
                (random() % (width as u32 * 3)) as i32 - width as i32,
                (random() % (height as u32 * 3)) as i32 - height as i32,
                random() % (width as u32 * 2 + 1),
                random() % (height as u32 * 2 + 1),
            );
            let bounds = (
                rect.x.max(0) as usize,
                rect.y.max(0) as usize,
                (rect.right().max(0) as usize).min(width),
                (rect.bottom().max(0) as usize).min(height),
            );
            let mut expected = frame.clone();
            for y in bounds.1..bounds.3 {
                for x in bounds.0..bounds.2 {
                    let p = pixels.pixel(x as u32, y as u32).unwrap();
                    let at = location + ((height - 1 - y) * width + width - 1 - x) * bpp;
                    expected[at..at + 4].copy_from_slice(&[
                        p.blue(),
                        p.green(),
                        p.red(),
                        p.alpha(),
                    ]);
                }
            }
            write_rotated_bgra(
                &mut frame,
                &mut vec![0x77; width * height * bpp],
                pixels.as_ref(),
                location,
                bpp,
                bounds,
            );
            assert_eq!(frame, expected, "case={case} page={page} rect={rect:?}");
            assert!(
                validate_packed_layout(width, height, bpp, stride + 4, 0, yoffset, frame.len())
                    .is_err()
            );
            assert!(
                validate_packed_layout(width, height, bpp, stride, 1, yoffset, frame.len())
                    .is_err()
            );
            assert!(
                validate_packed_layout(
                    width,
                    height,
                    bpp,
                    stride,
                    0,
                    yoffset,
                    location + stride * height - 1
                )
                .is_err()
            );
        }
    }
    #[test]
    fn perf9_padded_stride_page_is_rejected_before_packed_io() {
        let (width, height, stride, yoffset) = (640, 480, 2624, 480);
        let mut frame = vec![0x55; stride * height * 2];
        for y in 0..height {
            for x in 0..width {
                let at = (yoffset + y) * stride + x * 4;
                frame[at..at + 4].copy_from_slice(&[x as u8, y as u8, 37, 255]);
            }
        }
        let reference_at = (yoffset + height - 1) * stride + (width - 1) * 4;
        let packed_at = ((yoffset + height - 1) * width + width - 1) * 4;
        assert_eq!(&frame[reference_at..reference_at + 4], &[127, 223, 37, 255]);
        assert_ne!(
            &frame[packed_at..packed_at + 4],
            &frame[reference_at..reference_at + 4]
        );
        assert!(validate_packed_layout(width, height, 4, stride, 0, yoffset, frame.len()).is_err());
        assert!(
            validate_packed_layout(width, height, 4, width * 4, 0, yoffset, width * height * 8)
                .is_ok()
        );
        assert!(
            validate_packed_layout(width, height, 4, width * 4, 1, yoffset, frame.len()).is_err()
        );
        assert!(validate_packed_layout(width, height, 4, width * 4, 0, yoffset, 1).is_err());
        assert!(
            validate_packed_layout(usize::MAX, height, 4, stride, 0, yoffset, frame.len()).is_err()
        );
    }

    #[test]
    fn rotated_rect_matches_original_byte_writes() {
        let (width, height) = (73, 51);
        let mut pixels = Pixmap::new(width as u32, height as u32).unwrap();
        let mut seed = 0x913a58bdu32;
        for byte in pixels.data_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
        for bpp in [4, 8] {
            for location in [0, 12, width * bpp * 3 + 8] {
                for rect in [
                    Rect::new(0, 0, width as u32, height as u32),
                    Rect::new(1, 7, 19, 13),
                    Rect::new(-5, -9, 17, 23),
                    Rect::new(69, 49, 20, 20),
                    Rect::new(72, 50, 1, 1),
                    Rect::new(80, 60, 3, 7),
                    Rect::new(-30, -30, 2, 2),
                    Rect::new(11, 19, 0, 3),
                ] {
                    let bounds = (
                        rect.x.max(0) as usize,
                        rect.y.max(0) as usize,
                        (rect.right().max(0) as usize).min(width),
                        (rect.bottom().max(0) as usize).min(height),
                    );
                    let mut old = vec![0xa5; location + width * height * bpp + 32];
                    let mut new = old.clone();
                    for y in bounds.1..bounds.3 {
                        for x in bounds.0..bounds.2 {
                            let pixel = pixels.pixels()[y * width + x];
                            let at = location + ((height - y - 1) * width + width - x - 1) * bpp;
                            old[at] = pixel.blue();
                            old[at + 1] = pixel.green();
                            old[at + 2] = pixel.red();
                            old[at + 3] = pixel.alpha();
                        }
                    }
                    write_rotated_bgra(
                        &mut new,
                        &mut vec![0; width * height * bpp],
                        pixels.as_ref(),
                        location,
                        bpp,
                        bounds,
                    );
                    assert_eq!(old, new, "rect={rect:?}, bpp={bpp}, offset={location}");
                }
            }
        }
    }
    #[test]
    fn partial_and_empty_capture_keep_scratch_lazy() {
        let mut pixels = Pixmap::new(73, 51).unwrap();
        let mut scratch = Vec::new();
        for bounds in [(0, 0, 0, 51), (80, 60, 73, 51), (0, 30, 73, 20)] {
            capture_rotated_bgra(&mut scratch, &mut pixels, 4, bounds, |_, _| {
                panic!("empty read")
            });
            assert_eq!(scratch.capacity(), 0);
        }
        let mut spans = Vec::new();
        capture_rotated_bgra(
            &mut scratch,
            &mut pixels,
            4,
            (3, 5, 10, 9),
            |row, offset| {
                spans.push((offset, row.len()));
                row.fill(0);
            },
        );
        assert_eq!(scratch.len(), 7 * 4);
        assert_eq!(spans.len(), 4);
        assert!(spans.iter().all(|(_, len)| *len == 7 * 4));
        capture_rotated_bgra(
            &mut scratch,
            &mut pixels,
            4,
            (0, 0, 73, 51),
            |page, offset| {
                assert_eq!(offset, 0);
                page.fill(0);
            },
        );
        assert_eq!(scratch.len(), 73 * 51 * 4);
        let storage = scratch.as_ptr();
        capture_rotated_bgra(&mut scratch, &mut pixels, 4, (3, 5, 10, 9), |row, _| {
            row.fill(0)
        });
        assert_eq!(scratch.as_ptr(), storage);
    }

    #[test]
    fn randomized_capture_normalization_and_present_match_uncached_paths() {
        let mut seed = 0xc013ab29u32;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        for (width, height) in [(1usize, 1usize), (73, 51), (640, 480)] {
            for bpp in [4, 8] {
                let size = width * height * bpp;
                let mut scratch = Vec::new();
                let mut storage = None;
                for iteration in 0..4 {
                    let initial: Vec<u8> = (0..size * 4 + 32).map(|_| random()).collect();
                    for yoffset in [0, height, height * 2] {
                        for xoffset in [0, 3] {
                            let location = (yoffset * width + xoffset) * bpp;
                            let rects = [
                                (0, 0, width, height),
                                (0, 0, 1, 1),
                                (width - 1, height - 1, width, height),
                                (0, height / 2, width, height),
                                (width / 3, 0, width, height / 2),
                                (0, 0, 0, height),
                            ];
                            for bounds in rects {
                                let mut old_pixels =
                                    Pixmap::new(width as u32, height as u32).unwrap();
                                for byte in old_pixels.data_mut() {
                                    *byte = random();
                                }
                                let mut new_pixels = old_pixels.clone();
                                for y in bounds.1..bounds.3 {
                                    for x in bounds.0..bounds.2 {
                                        let at = location
                                            + ((height - 1 - y) * width + width - 1 - x) * bpp;
                                        old_pixels.pixels_mut()[y * width + x] =
                                            crate::display::color::Color::rgba(
                                                initial[at + 2],
                                                initial[at + 1],
                                                initial[at],
                                                initial[at + 3],
                                            )
                                            .into();
                                    }
                                }
                                let mut copied = 0;
                                let mut spans = Vec::new();
                                capture_rotated_bgra(
                                    &mut scratch,
                                    &mut new_pixels,
                                    bpp,
                                    bounds,
                                    |row, offset| {
                                        spans.push((offset, row.len()));
                                        copied += row.len();
                                        row.copy_from_slice(
                                            &initial
                                                [location + offset..location + offset + row.len()],
                                        );
                                    },
                                );
                                assert_eq!(
                                    copied,
                                    (bounds.2 - bounds.0) * (bounds.3 - bounds.1) * bpp
                                );
                                assert!(spans.windows(2).all(|pair| pair[0].0 < pair[1].0));
                                if bounds == (0, 0, width, height) {
                                    assert_eq!(spans, [(0, size)]);
                                    let previous = storage.get_or_insert(scratch.as_ptr());
                                    assert_eq!(scratch.as_ptr(), *previous);
                                }
                                assert_eq!(
                                    old_pixels.data(),
                                    new_pixels.data(),
                                    "capture {width}x{height} bpp={bpp} x={xoffset} y={yoffset} iteration={iteration} bounds={bounds:?}"
                                );
                                let mut old_frame = initial.clone();
                                let mut new_frame = initial.clone();
                                if yoffset != 0 {
                                    old_frame.copy_within(location..location + size, 0);
                                    capture_rotated_bgra(
                                        &mut scratch,
                                        &mut new_pixels,
                                        bpp,
                                        (0, 0, width, height),
                                        |cached, offset| {
                                            cached.copy_from_slice(
                                                &initial[location + offset
                                                    ..location + offset + cached.len()],
                                            );
                                        },
                                    );
                                    normalize_page(&mut new_frame, &scratch[..size]);
                                }
                                assert_eq!(old_frame, new_frame, "page normalization");
                                new_pixels = old_pixels.clone();
                                let present_location = xoffset * bpp;
                                for y in bounds.1..bounds.3 {
                                    for x in bounds.0..bounds.2 {
                                        let p = old_pixels.pixels()[y * width + x];
                                        let at = present_location
                                            + ((height - 1 - y) * width + width - 1 - x) * bpp;
                                        old_frame[at..at + 4].copy_from_slice(&[
                                            p.blue(),
                                            p.green(),
                                            p.red(),
                                            p.alpha(),
                                        ]);
                                    }
                                }
                                write_rotated_bgra(
                                    &mut new_frame,
                                    &mut scratch,
                                    new_pixels.as_ref(),
                                    present_location,
                                    bpp,
                                    bounds,
                                );
                                assert_eq!(
                                    old_frame, new_frame,
                                    "present bpp={bpp}, offsets={xoffset},{yoffset} bounds={bounds:?}"
                                );
                                assert_eq!(scratch.as_ptr(), storage.unwrap());
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod dl16_tests {
    #[test]
    fn reversed_bgra_words_match_unaligned_byte_reference() {
        for length in [0, 1, 3, 32, 390, 640] {
            for src_offset in 0..4 {
                for dst_offset in 0..4 {
                    let source: Vec<u8> =
                        (0..length * 4 + 4).map(|i| (i * 37 + 19) as u8).collect();
                    let source = &source[src_offset..src_offset + length * 4];
                    let mut dest = vec![0xa5; length * 4 + 8];
                    super::write_reversed_bgra(
                        &mut dest[dst_offset..dst_offset + length * 4],
                        source,
                    );
                    for (index, p) in source.chunks_exact(4).rev().enumerate() {
                        assert_eq!(
                            &dest[dst_offset + index * 4..dst_offset + (index + 1) * 4],
                            &[p[2], p[1], p[0], p[3]]
                        );
                    }
                    assert!(
                        dest[..dst_offset]
                            .iter()
                            .chain(&dest[dst_offset + length * 4..])
                            .all(|b| *b == 0xa5)
                    );
                }
            }
        }
    }
}
