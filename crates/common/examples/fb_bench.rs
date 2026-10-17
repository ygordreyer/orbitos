//! Cached-RAM CPU benchmark. Device mmap store performance must be measured separately.
//! cargo run --offline --release -p common --example fb_bench
pub use common::{display, geom, perf_trace};
#[path = "../src/platform/framebuffer_pixels.rs"]
mod framebuffer_pixels;
#[path = "../src/platform/framebuffer_pixels_neon.rs"]
mod framebuffer_pixels_neon;

use std::{hint::black_box, time::Instant};
use tiny_skia::{Pixmap, PixmapRef};
type Bounds = (usize, usize, usize, usize);

fn before(
    frame: &mut [u8],
    _: &mut [u8],
    pixels: PixmapRef<'_>,
    location: usize,
    _: usize,
    (x0, y0, x1, y1): Bounds,
) {
    let _phase = perf_trace::open_phase("present_convert_write");
    perf_trace::count(
        perf_trace::Count::ConvertedPixels,
        ((x1 - x0) * (y1 - y0)) as u64,
    );
    let (width, height) = (pixels.width() as usize, pixels.height() as usize);
    for y in (y0..y1).rev() {
        let start = location + ((height - 1 - y) * width + width - x1) * 4;
        let target = &mut frame[start..start + (x1 - x0) * 4];
        let source = &pixels.data()[(y * width + x0) * 4..(y * width + x1) * 4];
        if let (Ok(target), Ok(source)) = (
            bytemuck::try_cast_slice_mut::<u8, u32>(target),
            bytemuck::try_cast_slice::<u8, u32>(source),
        ) {
            for (dest, src) in target.iter_mut().zip(source.iter().rev()) {
                let word = u32::from_le(*src);
                *dest =
                    ((word & 0xff00ff00) | ((word & 0xff) << 16) | ((word >> 16) & 0xff)).to_le();
            }
        } else {
            for (dest, src) in target.chunks_exact_mut(4).zip(source.chunks_exact(4).rev()) {
                dest.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
            }
        }
    }
}

fn capture_before(frame: &[u8], pixels: &mut Pixmap, _: usize, (x0, y0, x1, y1): Bounds) {
    let (width, height) = (pixels.width() as usize, pixels.height() as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let at = ((height - 1 - y) * width + width - 1 - x) * 4;
            pixels.pixels_mut()[y * width + x] =
                display::color::Color::rgba(frame[at + 2], frame[at + 1], frame[at], frame[at + 3])
                    .into();
        }
    }
}

fn main() {
    let mut pixels = Pixmap::new(640, 480).unwrap();
    for (i, b) in pixels.data_mut().iter_mut().enumerate() {
        *b = (i.wrapping_mul(37) ^ (i >> 8)) as u8;
    }
    let mut frame = vec![0; 640 * 480 * 4];
    for (name, bounds) in [
        ("640x480", (0, 0, 640, 480)),
        ("176x176", (233, 151, 409, 327)),
    ] {
        let count = (bounds.2 - bounds.0) * (bounds.3 - bounds.1);
        let mut expected = frame.clone();
        before(&mut expected, &mut [], pixels.as_ref(), 0, 4, bounds);
        framebuffer_pixels::write_rotated_bgra(&mut frame, &mut [], pixels.as_ref(), 0, 4, bounds);
        assert_eq!(frame, expected);
        for (label, kernel) in [
            (
                "before",
                before as fn(&mut [u8], &mut [u8], PixmapRef<'_>, usize, usize, Bounds),
            ),
            ("after", framebuffer_pixels::write_rotated_bgra),
        ] {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                for _ in 0..2000 {
                    kernel(
                        black_box(&mut frame),
                        &mut [],
                        black_box(pixels.as_ref()),
                        0,
                        4,
                        black_box(bounds),
                    );
                }
                samples.push(count as f64 * 2000.0 / start.elapsed().as_secs_f64() / 1e6);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "present {name} {label}: {:.2} Mpixel/s (median of 5 x 2000 frames, stride 2560)",
                samples[2]
            );
        }
        let mut capture = pixels.clone();
        let mut expected = pixels.clone();
        capture_before(&frame, &mut expected, 4, bounds);
        framebuffer_pixels::read_rotated_bgra(&frame, &mut capture, 4, bounds);
        assert_eq!(capture.data(), expected.data());
        for (label, kernel) in [
            (
                "before",
                capture_before as fn(&[u8], &mut Pixmap, usize, Bounds),
            ),
            ("after", framebuffer_pixels::read_rotated_bgra),
        ] {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                for _ in 0..500 {
                    kernel(
                        black_box(&frame),
                        black_box(&mut capture),
                        4,
                        black_box(bounds),
                    );
                }
                samples.push(count as f64 * 500.0 / start.elapsed().as_secs_f64() / 1e6);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "capture {name} {label}: {:.2} Mpixel/s (median of 5 x 500 frames, stride 2560)",
                samples[2]
            );
        }
    }
}
