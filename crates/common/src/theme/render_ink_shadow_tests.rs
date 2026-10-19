use super::*;

// Frozen pre-optimization convolution. Do not share arithmetic with the optimized path.
fn gaussian_reference(alpha: &mut [f32], w: u32, h: u32, blur: u32) {
    if blur == 0 {
        return;
    }
    let _gaussian = crate::perf_trace::recording()
        .then(|| crate::perf_trace::stage(crate::perf_trace::Stage::ShadowGaussian));
    let sigma = blur as f32 / 2.0;
    let reach = (sigma * 3.0).ceil() as i32;
    let mut kernel: Vec<f32> = (-reach..=reach)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = kernel.iter().sum();
    for k in &mut kernel {
        *k /= sum;
    }
    let mut scratch = vec![0.0; alpha.len()];
    for yy in 0..h as i32 {
        for xx in 0..w as i32 {
            scratch[(yy * w as i32 + xx) as usize] = (-reach..=reach)
                .filter(|d| (0..w as i32).contains(&(xx + d)))
                .map(|d| alpha[(yy * w as i32 + xx + d) as usize] * kernel[(d + reach) as usize])
                .sum();
        }
    }
    for yy in 0..h as i32 {
        for xx in 0..w as i32 {
            alpha[(yy * w as i32 + xx) as usize] = (-reach..=reach)
                .filter(|d| (0..h as i32).contains(&(yy + d)))
                .map(|d| {
                    scratch[((yy + d) * w as i32 + xx) as usize] * kernel[(d + reach) as usize]
                })
                .sum();
        }
    }
}

// Frozen tile rasterization, tinting and cutout from the original ink_shadow miss.
fn reference_tile(rect: Rect, radius: u32, shadow: &Shadow, left: i32, top: i32,
    w: u32, h: u32, color: Color) -> Pixmap {
    let mut pix = ink_pixmap(w, h).unwrap();
    let path = crate::display::build_rounded_rect_path(
        draw_rect(Rect(0, 0, rect.2, rect.3)),
        radius.min(rect.2 / 2).min(rect.3 / 2),
    )
    .unwrap();
    pix.fill_path(
        &path,
        &tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE),
            anti_alias: true,
            ..Default::default()
        },
        tiny_skia::FillRule::Winding,
        Transform::from_translate(
            rect.0 as f32 + shadow.x as f32 - left as f32,
            rect.1 as f32 + shadow.y as f32 - top as f32,
        ),
        None,
    );
    let mut alpha: Vec<f32> = pix
        .pixels()
        .iter()
        .map(|p| p.alpha() as f32 / 255.0)
        .collect();
    gaussian_reference(&mut alpha, w, h, shadow.blur);
    for (pixel, a) in pix.data_mut().chunks_exact_mut(4).zip(alpha) {
        let a = (a * shadow.alpha * color.a() as f32).round() as u32;
        pixel.copy_from_slice(&[
            ((color.r() as u32 * a + 127) / 255) as u8,
            ((color.g() as u32 * a + 127) / 255) as u8,
            ((color.b() as u32 * a + 127) / 255) as u8,
            a as u8,
        ]);
    }
    // An outer box shadow never paints inside its original border box.
    pix.fill_path(
        &path,
        &tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE),
            blend_mode: tiny_skia::BlendMode::DestinationOut,
            anti_alias: true,
            ..Default::default()
        },
        tiny_skia::FillRule::Winding,
        Transform::from_translate(rect.0 as f32 - left as f32, rect.1 as f32 - top as f32),
        None,
    );
    pix
}

#[test]
fn shadow_fast_matches_frozen_tiles_and_counts_products() {
    use crate::theme::render_navigation::tests::theme;
    let theme = theme("compact-plus", "square");
    let frame = PreparedFrame::prepare(theme, &FrameInputs::new(ScreenId::Recents)).unwrap();
    let exact = [
        (Rect(144, 96, 208, 328), 12, 0.12, (246, 366)),
        (Rect(160, 112, 176, 176), 8, 0.23, (214, 214)),
        (Rect(368, 96, 256, 328), 12, 0.12, (291, 366)),
        (Rect(432, 16, 192, 408), 12, 0.10, (227, 446)),
        (Rect(448, 84, 160, 160), 8, 0.23, (198, 198)),
    ];
    let mut before = 0;
    let mut after = 0;
    for (index, (rect, radius, opacity, dimensions)) in exact.into_iter().enumerate() {
        let shadow = Shadow { x: 0, y: 4, blur: 12, color: ColorRole::Text, alpha: opacity };
        let (key, left, top) = frame.shadow_key(rect, radius, &shadow).unwrap();
        assert_eq!(key.tile, dimensions);
        let mut job = ShadowJob::new(key.clone());
        let mut reference = job.gaussian.alpha.clone();
        gaussian_reference(&mut reference, key.tile.0, key.tile.1, shadow.blur);
        while !job.step() {}
        assert!(reference.iter().zip(&job.gaussian.alpha).all(|(a, b)| a.to_bits() == b.to_bits()));
        let original = reference_tile(rect, radius, &shadow, left, top, key.tile.0, key.tile.1,
            frame.color(shadow.color));
        assert_eq!(job.pixels.data(), original.data(), "{rect:?}");
        let (w, h) = (key.tile.0 as usize, key.tile.1 as usize);
        let products = h * (w * 37 - 18 * 19) + w * (h * 37 - 18 * 19);
        println!("SHADOW_PRODUCTS rect={rect:?} tile={w}x{h} before={products} after={}", job.gaussian.products);
        if index < 3 { before += products; after += job.gaussian.products; }
    }
    assert_eq!(before, 17_352_638);
    assert!(after < before / 3);
    println!("DETAILS_PRODUCTS before={before} after={after}");

    for (rect, radius) in [
        (Rect(0, 0, 31, 22), 5), (Rect(620, 0, 32, 24), 8),
        (Rect(0, 465, 32, 24), 8), (Rect(623, 467, 31, 22), 12),
        (Rect(17, 19, 31, 22), 7), (Rect(60, 70, 32, 24), 0),
        (Rect(1, 1, 1, 1), 9), (Rect(20, 20, 2, 3), 2),
    ] {
        for blur in [0, 1, 2, 3, 12] {
            for (rgba, opacity) in [([48, 36, 30, 255], 0.12), ([90, 180, 230, 128], 0.53),
                ([255, 0, 255, 0], 1.0), ([0, 0, 0, 255], 0.0)] {
                let shadow = Shadow { x: -3, y: 4, blur, color: ColorRole::Text, alpha: opacity };
                // Include zero-opacity generation even though lookup correctly skips it.
                let nonzero = Shadow { alpha: 1.0, ..shadow.clone() };
                let Some((mut key, left, top)) = frame.shadow_key(rect, radius, &nonzero) else { continue; };
                key.alpha = opacity.to_bits();
                key.color = rgba;
                let mut job = ShadowJob::new(key.clone());
                let mut alpha = job.gaussian.alpha.clone();
                gaussian_reference(&mut alpha, key.tile.0, key.tile.1, blur);
                while !job.step() {}
                assert!(alpha.iter().zip(&job.gaussian.alpha).all(|(a, b)| a.to_bits() == b.to_bits()),
                    "alpha {rect:?} blur={blur}");
                let original = reference_tile(rect, radius, &shadow, left, top, key.tile.0, key.tile.1,
                    Color::rgba(rgba[0], rgba[1], rgba[2], rgba[3]));
                assert_eq!(job.pixels.data(), original.data(), "tile {rect:?} blur={blur}");
            }
        }
    }
}

#[test]
fn shadow_fast_generic_masks_keep_float_bits_and_tap_order() {
    // Non-consecutive duplicate lines and signed zeros exercise bitwise reuse.
    for (w, h) in [(1, 1), (2, 3), (7, 8), (16, 15), (37, 38)] {
        let mut alpha: Vec<f32> = (0..w * h).map(|i| {
            if i % 7 == 0 { -0.0 } else { ((i * 79) % 256) as f32 / 255.0 }
        }).collect();
        if h > 3 { for x in 0..w { alpha[(h as usize - 1) * w as usize + x as usize] = alpha[x as usize]; } }
        for blur in [0, 1, 2, 5, 12] {
            let mut expected = alpha.clone();
            gaussian_reference(&mut expected, w, h, blur);
            let mut actual = alpha.clone();
            gaussian_alpha(&mut actual, w, h, blur);
            assert!(expected.iter().zip(&actual).all(|(a, b)| a.to_bits() == b.to_bits()),
                "{w}x{h} blur={blur}");
        }
    }
}

#[test]
fn recents_and_details_shadow_tiles_share_the_existing_cache_budget() {
    use crate::theme::render_navigation::tests::{arts, model, theme};
    let handle = theme("compact-plus", "square");
    let art = arts();
    let input = model(&handle, ScreenId::Recents, 0, &art);
    SHADOWS.with(|cache| cache.borrow_mut().clear());
    let frame = PreparedFrame::prepare(handle, &input).unwrap();
    assert_eq!(cached_shadow_count(), 2);
    let mut work = frame.detail_shadow_warm();
    let mut batches = 0;
    while !frame.prewarm_detail_shadow_step(&mut work) {
        batches += 1;
        assert!(batches < 1000);
    }
    SHADOWS.with(|cache| {
        let cache = cache.borrow();
        assert_eq!(cache.len(), 5);
        assert!(cache.iter().map(|entry| entry.pixels.data().len()).sum::<usize>() <= 1_531_136,
            "fitted cover shadows must stay within the original square-shadow budget");
    });
}

#[test]
fn speculative_shadow_admission_never_evicts_existing_entries() {
    use crate::theme::render_navigation::tests::{arts, model, theme};
    let handle = theme("big-picture", "square");
    let input = model(&handle, ScreenId::Recents, 0, &arts());
    SHADOWS.with(|cache| cache.borrow_mut().clear());
    let frame = PreparedFrame::prepare(handle, &input).unwrap();
    let existing = SHADOWS.with(|cache| cache.borrow().iter()
        .map(|entry| (entry.key.clone(), entry.pixels.clone())).collect::<Vec<_>>());
    assert!(!existing.is_empty());
    let mut work = frame.detail_shadow_warm();
    let requested = work.keys.iter().filter(|key| shadow_lookup(&frame.theme.weak(), key).is_none())
        .map(|key| key.tile.0 as usize * key.tile.1 as usize * 4).sum::<usize>();
    assert!(existing.iter().map(|(_, pixels)| pixels.data().len()).sum::<usize>() + requested > 2 * 1024 * 1024);
    let mut steps = 0;
    while !frame.prewarm_detail_shadow_step(&mut work) {
        steps += 1;
        assert!(steps < 1000);
    }
    assert!(work.job.is_none() && work.keys.is_empty());
    SHADOWS.with(|cache| {
        let cache = cache.borrow();
        assert!(cache.iter().map(|entry| entry.pixels.data().len()).sum::<usize>() <= 2 * 1024 * 1024);
        for (key, pixels) in &existing {
            assert!(cache.iter().any(|entry| entry.key == *key && Arc::ptr_eq(&entry.pixels, pixels)));
        }
    });
    for _ in 0..3 {
        assert!(frame.prewarm_detail_shadow_step(&mut work));
        assert!(work.job.is_none() && work.keys.is_empty());
    }
    // Recheck admission at completion if render-time entries filled the budget mid-job.
    let key = existing[0].0.clone();
    let pixels = Arc::new(Pixmap::new(640, 480).unwrap());
    assert!(!shadow_prewarm_admit(frame.theme.weak(), key, pixels));
    for (key, pixels) in existing {
        assert!(Arc::ptr_eq(&shadow_lookup(&frame.theme.weak(), &key).unwrap(), &pixels));
    }
}

#[test]
fn details_shadow_job_survives_selection_and_navigation_stamp_changes() {
    use crate::theme::render_navigation::tests::{arts, model, theme};
    let handle = theme("compact-plus", "square");
    let arts = arts();
    SHADOWS.with(|cache| cache.borrow_mut().clear());
    let frame = PreparedFrame::prepare(handle.clone(), &model(&handle, ScreenId::Recents, 0, &arts)).unwrap();
    let mut work = frame.detail_shadow_warm();
    assert!(!frame.prewarm_detail_shadow_step(&mut work));
    assert!(!frame.prewarm_detail_shadow_step(&mut work));
    let pixels = work.job.as_ref().unwrap().pixels.data().as_ptr();
    let rows = work.job.as_ref().unwrap().gaussian.row;
    let selected = PreparedFrame::prepare(handle.clone(), &model(&handle, ScreenId::Recents, 1, &arts)).unwrap();
    assert!(!selected.navigation_is_current(&frame.navigation_stamp()));
    assert!(work.theme_matches(&handle));
    assert!(!selected.prewarm_detail_shadow_step(&mut work));
    assert_eq!(work.job.as_ref().unwrap().pixels.data().as_ptr(), pixels);
    assert!(work.job.as_ref().unwrap().gaussian.row > rows);
    let mut steps = 0;
    while !selected.prewarm_detail_shadow_step(&mut work) {
        steps += 1;
        assert!(steps < 1000);
    }
    assert!(work.job.is_none() && work.keys.is_empty());
}
