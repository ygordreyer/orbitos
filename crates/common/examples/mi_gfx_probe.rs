//! Offscreen by default. Build with --features miyoo for the Linux ARM backend.
//! No pan or vsync ioctl is issued, even with --display.
#[cfg(all(feature = "miyoo", target_os = "linux", target_arch = "arm"))]
use common::platform::miyoo::gfx;
// The parent Miyoo platform is feature-gated. This also makes ABI and fake-driver
// tests available to `cargo test -p common gfx` without enabling device dependencies.
#[cfg(not(all(feature = "miyoo", target_os = "linux", target_arch = "arm")))]
#[path = "../src/platform/miyoo/gfx/mod.rs"]
mod gfx;

use gfx::{Gfx, abi};
use std::{error::Error, hint::black_box};
type ProbeResult<T> = Result<T, Box<dyn Error>>;
const ITERATIONS: usize = 50;
const ALPHAS: [u8; 6] = [0, 1, 127, 128, 254, 255];

fn now() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC exists on both the host and the target. A failed clock must
    // not silently produce plausible benchmark numbers.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
fn elapsed(start: u64) -> u64 {
    now() - start
}
fn distribution(values: &mut [u64]) -> (u64, u64, u64) {
    assert!(!values.is_empty());
    values.sort_unstable();
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 0 {
        values[middle - 1] + (values[middle] - values[middle - 1]) / 2
    } else {
        values[middle]
    };
    (
        median,
        values[(values.len() * 9).div_ceil(10) - 1],
        values[0],
    )
}
fn print_stats(label: &str, values: &mut [u64]) {
    let (median, p90, min) = distribution(values);
    println!(
        "  {label}: median_ns={median} p90_ns={p90} min_ns={min} n={}",
        values.len()
    );
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Rotate,
    Fill,
    Blend,
    Scale,
}
#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    operation: Operation,
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
    destination_alpha: u8,
}
impl Case {
    fn new(
        name: &'static str,
        operation: Operation,
        sw: usize,
        sh: usize,
        w: usize,
        h: usize,
    ) -> Self {
        Self {
            name,
            operation,
            source_width: sw,
            source_height: sh,
            width: w,
            height: h,
            destination_alpha: 255,
        }
    }
}

fn pattern(width: usize, height: usize, alpha_edges: bool) -> Vec<u8> {
    let mut bytes = vec![0; width * height * 4];
    for (index, pixel) in bytes.chunks_exact_mut(4).enumerate() {
        let x = index % width;
        let y = index / width;
        let a = if alpha_edges {
            ALPHAS[x % ALPHAS.len()]
        } else {
            255
        };
        // Asymmetry exposes rotation, swizzle, row and edge errors.
        pixel.copy_from_slice(&[
            ((x * 7 + y * 3) as u8 as u16 * a as u16 / 255) as u8,
            ((x * 11 + y * 17 + 49) as u8 as u16 * a as u16 / 255) as u8,
            ((x * 19 + y * 5 + 101) as u8 as u16 * a as u16 / 255) as u8,
            a,
        ]);
    }
    bytes
}
fn background(width: usize, height: usize, alpha: u8) -> Vec<u8> {
    let pixel = [alpha / 4, alpha / 3, alpha / 2, alpha]; // BGRA, already premultiplied.
    let mut bytes = vec![0; width * height * 4];
    for p in bytes.chunks_exact_mut(4) {
        p.copy_from_slice(&pixel);
    }
    bytes
}

// Scalar equivalent of framebuffer_pixels.rs: reverse both axes, RGBA to BGRA,
// preserving alpha. Kept independent of the presentation lane.
fn rotate_swizzle(source: &[u8], destination: &mut [u8], width: usize, height: usize) {
    let count = width * height;
    for (index, pixel) in destination[..count * 4].chunks_exact_mut(4).enumerate() {
        let offset = (count - 1 - index) * 4;
        pixel.copy_from_slice(&[
            source[offset + 2],
            source[offset + 1],
            source[offset],
            source[offset + 3],
        ]);
    }
}
fn cpu_reference(case: Case, source: &[u8], original: &[u8], destination: &mut [u8]) {
    match case.operation {
        Operation::Rotate => rotate_swizzle(source, destination, case.width, case.height),
        Operation::Fill => {
            for pixel in destination.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[0x56, 0x34, 0x12, 0xff]);
            }
        }
        Operation::Blend => {
            for ((src, bg), dst) in source
                .chunks_exact(4)
                .zip(original.chunks_exact(4))
                .zip(destination.chunks_exact_mut(4))
            {
                let inv = 255 - src[3] as u32;
                for (out, (s, d)) in dst
                    .iter_mut()
                    .zip([src[2], src[1], src[0], src[3]].into_iter().zip(bg))
                {
                    *out = (s as u32 + (*d as u32 * inv + 127) / 255).min(255) as u8;
                }
            }
        }
        Operation::Scale => {
            // This SDK exposes no filter selector. Report deviation from nearest-neighbor
            // rather than presuming the firmware uses the same sampling rule.
            for y in 0..case.height {
                let sy = y * case.source_height / case.height;
                for x in 0..case.width {
                    let sx = x * case.source_width / case.width;
                    let s = (sy * case.source_width + sx) * 4;
                    let d = (y * case.width + x) * 4;
                    destination[d..d + 4].copy_from_slice(&[
                        source[s + 2],
                        source[s + 1],
                        source[s],
                        source[s + 3],
                    ]);
                }
            }
        }
    }
}
fn max_diff(actual: &[u8], reference: &[u8]) -> [u8; 4] {
    assert_eq!(actual.len(), reference.len());
    let mut max = [0; 4];
    for (a, b) in actual.chunks_exact(4).zip(reference.chunks_exact(4)) {
        for c in 0..4 {
            max[c] = max[c].max(a[c].abs_diff(b[c]));
        }
    }
    max
}
fn compare(case: Case, actual: &[u8], reference: &[u8]) -> u8 {
    let max = max_diff(actual, reference);
    println!(
        "  correctness max_channel_diff={} BGRA={max:?}",
        max.into_iter().max().unwrap()
    );
    if matches!(case.operation, Operation::Scale) {
        println!("  scale_reference=nearest_top_left; firmware_filter_unspecified");
    }
    if matches!(case.operation, Operation::Blend) {
        for (slot, alpha) in ALPHAS.into_iter().enumerate() {
            let mut max = [0u8; 4];
            for y in 0..case.height {
                for x in (slot..case.width).step_by(ALPHAS.len()) {
                    let i = (y * case.width + x) * 4;
                    for c in 0..4 {
                        max[c] = max[c].max(actual[i + c].abs_diff(reference[i + c]));
                    }
                }
            }
            println!(
                "  src_alpha={alpha} dst_alpha={} max_channel_diff={} BGRA={max:?}",
                case.destination_alpha,
                max.into_iter().max().unwrap()
            );
        }
    }
    max.into_iter().max().unwrap()
}

fn run_case(gfx: Option<&Gfx>, case: Case) -> ProbeResult<bool> {
    println!(
        "op={} {:?} {}x{} to {}x{}",
        case.name, case.operation, case.source_width, case.source_height, case.width, case.height
    );
    let source = pattern(
        case.source_width,
        case.source_height,
        matches!(case.operation, Operation::Blend),
    );
    let original = background(case.width, case.height, case.destination_alpha);
    let mut reference = vec![0; case.width * case.height * 4];
    let mut cpu = [0; ITERATIONS];
    for sample in &mut cpu {
        let start = now();
        cpu_reference(
            case,
            black_box(&source),
            black_box(&original),
            black_box(&mut reference),
        );
        *sample = elapsed(start);
        black_box(&reference);
    }
    print_stats("cpu_equivalent", &mut cpu);
    let Some(gfx) = gfx else {
        return Ok(true);
    };
    // Fill uses no source allocation. All allocations persist across the 50 samples.
    let mut src = if matches!(case.operation, Operation::Fill) {
        None
    } else {
        Some(gfx.allocate(
            case.source_width as u32,
            case.source_height as u32,
            abi::ABGR8888,
        )?)
    };
    let mut dst = gfx.allocate(case.width as u32, case.height as u32, abi::ARGB8888)?;
    assert_eq!(dst.stride(), case.width * 4);
    if let Some(src) = &src {
        assert_eq!(src.stride(), case.source_width * 4);
    }
    let bytes = dst.allocation_len() + src.as_ref().map_or(0, |s| s.allocation_len());
    println!(
        "  persistent_mma_bytes={bytes} upload_bytes_per_iteration={} preflush_bytes_per_iteration={bytes} readback_flush_bytes_per_iteration={}",
        src.as_ref().map_or(0, |_| source.len())
            + if matches!(case.operation, Operation::Blend) {
                original.len()
            } else {
                0
            },
        dst.allocation_len()
    );
    let mut upload = [0; ITERATIONS];
    let mut flush = [0; ITERATIONS];
    let mut submit = [0; ITERATIONS];
    let mut wait = [0; ITERATIONS];
    let mut readback_flush = [0; ITERATIONS];
    let mut total = [0; ITERATIONS];
    let mut worst = 0;
    for i in 0..ITERATIONS {
        let total_start = now();
        let start = now();
        if let Some(src) = &mut src {
            src.bytes_mut()?.copy_from_slice(&source);
        }
        if matches!(case.operation, Operation::Blend) {
            dst.bytes_mut()?.copy_from_slice(&original);
        }
        upload[i] = elapsed(start);
        let start = now();
        let rect = dst.rect();
        let prepared = if let Some(src) = &src {
            let mut options = abi::Opt::copy(case.width as u32, case.height as u32);
            if matches!(case.operation, Operation::Rotate) {
                options.rotate = abi::ROTATE_180;
            }
            if matches!(case.operation, Operation::Blend) {
                options.destination_factor = abi::INVSRCALPHA;
            }
            gfx.prepare_blit(src, src.rect(), &mut dst, rect, options)?
        } else {
            gfx.prepare_fill(&mut dst, rect, 0xff123456)?
        };
        flush[i] = elapsed(start);
        let start = now();
        let mut pending = prepared.submit()?;
        submit[i] = elapsed(start);
        let start = now();
        pending.wait()?;
        wait[i] = elapsed(start);
        drop(pending);
        let start = now();
        dst.flush()?;
        readback_flush[i] = elapsed(start);
        total[i] = elapsed(total_start);
        // Compare every iteration, outside the timed operation.
        let actual = dst.bytes()?;
        worst = worst.max(max_diff(actual, &reference).into_iter().max().unwrap());
    }
    for (label, samples) in [
        ("upload_memcpy", &mut upload),
        ("pre_dma_flush", &mut flush),
        ("submit", &mut submit),
        ("wait_all_done", &mut wait),
        ("readback_flush", &mut readback_flush),
        ("upload_through_readback_flush", &mut total),
    ] {
        print_stats(label, samples);
    }
    compare(case, dst.bytes()?, &reference);
    println!("  worst_max_channel_diff_over_50={worst}");
    // Filter and alpha rounding are measurements, not assumptions. Exact copy/fill
    // failures are unambiguous and make the process fail after all cases finish.
    Ok(!matches!(case.operation, Operation::Rotate | Operation::Fill) || worst == 0)
}

#[cfg(all(feature = "miyoo", target_os = "linux", target_arch = "arm"))]
fn display_probe(gfx: &Gfx) -> ProbeResult<()> {
    use framebuffer::Framebuffer;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fb0")?;
    let fix = Framebuffer::get_fix_screeninfo(&file)?;
    let var = Framebuffer::get_var_screeninfo(&file)?;
    if var.bits_per_pixel != 32
        || var.xres != 640
        || var.yres != 480
        || var.xoffset != 0
        || var.xres_virtual < var.xres
        || var.yres_virtual < var.yres
        || var.yoffset as u64 + var.yres as u64 > var.yres_virtual as u64
        || fix.line_length < var.xres * 4
        || fix.line_length % 16 != 0
        || var.grayscale != 0
        || var.nonstd != 0
        || fix.fb_type != 0
        || fix.visual != 2
        || (
            var.red.offset,
            var.red.length,
            var.green.offset,
            var.green.length,
            var.blue.offset,
            var.blue.length,
            var.transp.offset,
            var.transp.length,
        ) != (16, 8, 8, 8, 0, 8, 24, 8)
    {
        return Err("--display requires a standard 640x480 ARGB8888 framebuffer".into());
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let visible_end = var.yoffset as u64 + var.yres as u64;
    let yoffset = (0..var.yres_virtual / var.yres)
        .map(|p| p * var.yres)
        .find(|&y| {
            let end = y as u64 + var.yres as u64;
            (end <= var.yoffset as u64 || y as u64 >= visible_end) && end <= var.yres_virtual as u64
        })
        .ok_or("no entirely hidden framebuffer page")?;
    let offset = yoffset as u64 * fix.line_length as u64;
    let extent = (var.yres as u64 * fix.line_length as u64).div_ceil(page_size) * page_size;
    if offset
        .checked_add(extent)
        .is_none_or(|end| end > fix.smem_len as u64)
        || var.yres_virtual as u64 * fix.line_length as u64 > fix.smem_len as u64
    {
        return Err("hidden page exceeds framebuffer memory/mapping extent".into());
    }
    let physical = (fix.smem_start as u64)
        .checked_add(offset)
        .ok_or("physical address overflow")?;
    let descriptor = abi::Surface {
        physical,
        format: abi::ARGB8888,
        width: var.xres,
        height: var.yres,
        stride: fix.line_length,
    };
    let mut src = gfx.allocate(var.xres, var.yres, abi::ABGR8888)?;
    src.bytes_mut()?.copy_from_slice(&pattern(640, 480, false));
    // --display is an explicit diagnostic mode. Run with other framebuffer writers
    // stopped, so this hidden physical range remains exclusively owned by the probe.
    let mut dst = unsafe { gfx.map_external(descriptor)? };
    let current = Framebuffer::get_var_screeninfo(&file)?;
    if current.yoffset != var.yoffset
        || current.xoffset != var.xoffset
        || current.xres != var.xres
        || current.yres != var.yres
        || current.yres_virtual != var.yres_virtual
    {
        return Err("framebuffer changed before hidden-page submission".into());
    }
    let mut options = abi::Opt::copy(var.xres, var.yres);
    options.rotate = abi::ROTATE_180;
    let rect = dst.rect();
    gfx.prepare_blit(&src, src.rect(), &mut dst, rect, options)?
        .submit()?
        .wait()?;
    println!(
        "display hidden_yoffset={yoffset} physical={physical:#x} bytes={extent}; no pan issued"
    );
    Ok(())
}
#[cfg(not(all(feature = "miyoo", target_os = "linux", target_arch = "arm")))]
fn display_probe(_: &Gfx) -> ProbeResult<()> {
    Err("--display requires Linux ARM with feature miyoo".into())
}

fn main() -> ProbeResult<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg != "--display" && arg != "--help") {
        return Err("usage: mi_gfx_probe [--display]".into());
    }
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "mi_gfx_probe [--display]\nDefault: offscreen MMA only, 50 samples per operation.\n--display: write a hidden fb page; stop other framebuffer writers first. Never pans."
        );
        return Ok(());
    }
    println!("MI_GFX probe iterations={ITERATIONS} clock=CLOCK_MONOTONIC units=ns");
    println!("formats: source=ABGR8888(RGBA bytes) destination=ARGB8888(BGRA bytes)");
    let gfx = match Gfx::open() {
        Ok(gfx) => {
            println!("MMA availability: {}", gfx.mma_availability());
            Some(gfx)
        }
        Err(gfx::Error::Unavailable(reason)) => {
            println!("Unavailable: {reason}; CPU references only");
            None
        }
        Err(error) => return Err(error.into()),
    };
    let mut cases = vec![
        Case::new(
            "rotate_swizzle_640x480",
            Operation::Rotate,
            640,
            480,
            640,
            480,
        ),
        Case::new(
            "rotate_swizzle_176x176",
            Operation::Rotate,
            176,
            176,
            176,
            176,
        ),
        Case::new("quick_fill_640x480", Operation::Fill, 0, 0, 640, 480),
        Case::new("quick_fill_64x64", Operation::Fill, 0, 0, 64, 64),
        Case::new(
            "premultiplied_src_over_opaque",
            Operation::Blend,
            176,
            176,
            176,
            176,
        ),
        Case::new(
            "premultiplied_src_over_translucent",
            Operation::Blend,
            176,
            176,
            176,
            176,
        ),
        Case::new(
            "scale_480x480_to_176x176",
            Operation::Scale,
            480,
            480,
            176,
            176,
        ),
    ];
    cases[5].destination_alpha = 128;
    let mut exact = true;
    for case in cases {
        exact &= run_case(gfx.as_ref(), case)?;
    }
    if args.iter().any(|arg| arg == "--display") {
        display_probe(
            gfx.as_ref()
                .ok_or("display requested but MI_GFX unavailable")?,
        )?;
    }
    if !exact {
        return Err("exact copy/fill correctness failed, see max channel differences".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gfx_probe_distribution() {
        let mut values: Vec<u64> = (1..=50).rev().collect();
        assert_eq!(distribution(&mut values), (25, 45, 1));
    }
    #[test]
    fn gfx_probe_rotate_asymmetric_and_alpha_edges() {
        let input = [
            1, 2, 3, 0, 4, 5, 6, 1, 7, 8, 9, 127, 10, 11, 12, 128, 13, 14, 15, 254, 16, 17, 18, 255,
        ];
        let mut output = [0; 24];
        rotate_swizzle(&input, &mut output, 3, 2);
        assert_eq!(
            output,
            [
                18, 17, 16, 255, 15, 14, 13, 254, 12, 11, 10, 128, 9, 8, 7, 127, 6, 5, 4, 1, 3, 2,
                1, 0
            ]
        );
    }
    #[test]
    fn gfx_probe_blend_alpha_extremes_and_translucent_destination() {
        let case = Case {
            destination_alpha: 128,
            ..Case::new("test", Operation::Blend, 6, 1, 6, 1)
        };
        let source: Vec<u8> = ALPHAS.into_iter().flat_map(|a| [a, 0, a / 2, a]).collect();
        let original = background(6, 1, 128);
        let mut output = [0; 24];
        cpu_reference(case, &source, &original, &mut output);
        assert_eq!(&output[..4], &original[..4]);
        assert_eq!(&output[20..], &[127, 0, 255, 255]);
        for (i, a) in ALPHAS.into_iter().enumerate() {
            let inv = 255 - a as u32;
            assert_eq!(
                output[i * 4 + 3],
                (a as u32 + (128 * inv + 127) / 255) as u8
            );
        }
        assert_eq!(max_diff(&output, &output), [0; 4]);
    }
    #[test]
    fn gfx_probe_fill_and_scale_reference() {
        let mut output = [0; 16];
        let fill = Case::new("test", Operation::Fill, 0, 0, 2, 2);
        cpu_reference(fill, &[], &[], &mut output);
        assert_eq!(output, [0x56, 0x34, 0x12, 255].repeat(4).as_slice());
        let scale = Case::new("test", Operation::Scale, 2, 2, 1, 1);
        cpu_reference(
            scale,
            &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255],
            &[],
            &mut output[..4],
        );
        assert_eq!(&output[..4], &[3, 2, 1, 255]);
    }
}
