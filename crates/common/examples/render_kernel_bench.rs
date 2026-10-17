//! Fixed-input CPU kernel measurements. Run the release binary on the device.
#![allow(dead_code)]
#![cfg(not(test))]
#[cfg(feature = "perf-bench")]
pub use common::perf;
pub use common::{display, geom, perf_trace, theme};
use std::{hint::black_box, time::Instant};
use tiny_skia::Pixmap;

use common::theme::{fonts::FontBank, render::PreparedFrame, runtime::ThemeSnapshot, ThemeHandle};

fn measure<T>(
    name: &str,
    pixels: u64,
    iterations: u32,
    context: &mut T,
    mut reset: impl FnMut(&mut T),
    mut run: impl FnMut(&mut T),
) {
    reset(context);
    run(context);
    let mut ns = 0u128;
    for _ in 0..iterations {
        reset(context);
        let start = Instant::now();
        run(context);
        ns += start.elapsed().as_nanos();
    }
    println!(
        "KERNEL name={name} iterations={iterations} pixels_per_iteration={pixels} total_ns={ns} ns_per_pixel={:.3}",
        ns as f64 / (pixels * iterations as u64) as f64
    );
}
fn main() {
    let iterations = std::env::args()
        .nth(1)
        .map(|s| s.parse::<u32>().expect("positive iteration count"))
        .unwrap_or(20);
    assert!(iterations > 0);
    println!(
        "KERNEL_SCOPE production sources; allocations inside kernels included; input reset and warmup excluded; shadow cache miss includes raster/mask/composition; framebuffer is RAM, not mapped device memory"
    );
    let mut source = Pixmap::new(480, 480).unwrap();
    for (i, p) in source.data_mut().chunks_exact_mut(4).enumerate() {
        let a = [0, 1, 127, 128, 254, 255][i % 6];
        p.copy_from_slice(&[(i % (a as usize + 1)) as u8, a / 2, a / 3, a]);
    }
    measure(
        "fit_resize_cover_480x480_to_320x200",
        320 * 200,
        iterations,
        &mut (),
        |_| {},
        |_| {
            black_box(common::theme::runtime::benchmark_fit(black_box(&source), 320, 200).unwrap());
        },
    );
    let mut blurred = source.clone();
    measure(
        "box_blur_radius4_480x480",
        480 * 480,
        iterations,
        &mut blurred,
        |p| p.data_mut().copy_from_slice(source.data()),
        |p| common::theme::runtime::benchmark_blur(p, 4).unwrap(),
    );
    let bank = FontBank::bind(&std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../static/.allium/fonts").canonicalize().unwrap()).unwrap();
    let theme = ThemeHandle::prepare(ThemeSnapshot::builtin("compact").unwrap(), &bank, 1).unwrap();
    let mut shadow = PreparedFrame::prepare(theme, &common::theme::frame::FrameInputs::new(
        common::theme::pack::ScreenId::Library)).unwrap();
    measure(
        "shadow_miss_blur8_240x160_padded",
        266 * 186,
        iterations,
        &mut shadow,
        PreparedFrame::benchmark_shadow_reset,
        PreparedFrame::benchmark_shadow,
    );
    let mut dest = source.clone();
    measure(
        "ink_src_over_mixed_alpha",
        480 * 480,
        iterations,
        &mut dest,
        |p| p.data_mut().copy_from_slice(source.data()),
        |p| {
            common::theme::render::benchmark_blit(&mut p.as_mut(), 0, 0, black_box(source.as_ref()));
            black_box(p.data());
        },
    );
    let mut framebuffer_source = Pixmap::new(640, 480).unwrap();
    for (i, p) in framebuffer_source
        .data_mut()
        .chunks_exact_mut(4)
        .enumerate()
    {
        let at = (i % (480 * 480)) * 4;
        p.copy_from_slice(&source.data()[at..at + 4]);
    }
    let mut frame = vec![0u8; framebuffer_source.data().len()];
    let mut scratch = vec![0u8; framebuffer_source.data().len()];
    measure(
        "framebuffer_rotate_swizzle_ram_640x480",
        640 * 480,
        iterations,
        &mut frame,
        |_| {},
        |p| {
            common::platform::benchmark_write_rotated_bgra(
                p,
                &mut scratch,
                black_box(framebuffer_source.as_ref()),
                0,
                4,
                (0, 0, 640, 480),
            );
            black_box(&p[..]);
        },
    );
}
