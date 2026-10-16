use super::{Pixmap, resize_art};

pub(super) fn compose(source: Pixmap) -> Option<Pixmap> {
    let width = source.width().min(176);
    let height = width * 3 / 2;
    let mut background = resize_art(source.clone(), width, height, true)?;
    let (w, h) = (width as usize, height as usize);
    let mut horizontal = vec![0.0_f32; background.data().len()];
    for y in 0..h {
        for x in 0..w {
            for channel in 0..4 {
                let lo = x.saturating_sub(10);
                let hi = (x + 10).min(w - 1);
                horizontal[(y * w + x) * 4 + channel] = (lo..=hi)
                    .map(|xx| background.data()[(y * w + xx) * 4 + channel] as f32)
                    .sum::<f32>()
                    / (hi - lo + 1) as f32;
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let lo = y.saturating_sub(10);
            let hi = (y + 10).min(h - 1);
            let at = (y * w + x) * 4;
            let mut rgba = [0.0; 4];
            for channel in 0..4 {
                rgba[channel] = (lo..=hi)
                    .map(|yy| horizontal[(yy * w + x) * 4 + channel])
                    .sum::<f32>()
                    / (hi - lo + 1) as f32;
            }
            // The web fills a black 45 percent scrim over the blurred cover.
            for channel in 0..3 {
                background.data_mut()[at + channel] = (rgba[channel] * 0.55).round() as u8;
            }
            background.data_mut()[at + 3] = (rgba[3] * 0.55 + 255.0 * 0.45).round() as u8;
        }
    }
    let square = resize_art(source, width, width, true)?;
    background.draw_pixmap(
        0,
        ((height - width) / 3) as i32,
        square.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    Some(background)
}
