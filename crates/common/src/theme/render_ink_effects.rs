use super::*;
use crate::theme::ink::Shadow;

pub(super) fn effect_padding(c: &Component) -> u32 {
    let p = c.paint.as_ref().unwrap();
    let shadow = [
        &p.shadow,
        &p.action_style.as_ref().and_then(|s| s.shadow.clone()),
        &p.primary_style.as_ref().and_then(|s| s.shadow.clone()),
        &p.action_focus_style.as_ref().and_then(|s| s.shadow.clone()),
    ]
    .into_iter()
    .flatten()
    .map(|s| s.blur * 3 / 2 + s.x.unsigned_abs().max(s.y.unsigned_abs()) + 1)
    .max()
    .unwrap_or(0);
    shadow.max(if c.focus_style == Some(FocusStyle::Glow) {
        c.glow_radius.unwrap_or(0)
    } else {
        0
    })
}

pub(super) fn paint_bounds(c: &Component) -> Rect {
    let mut pad = effect_padding(c);
    if c.kind == Kind::Text && c.binding == Some(Binding::KeyboardValue) {
        pad = pad.max(3);
    }
    if c.layout == Some(Arrangement::Shelf) {
        let scale = c.paint.as_ref().unwrap().focus_scale.unwrap_or(1.0);
        let extent = c.item_extent.unwrap_or(c.rect.2).max(c.rect.3) + 2 * pad;
        pad += ((extent as f32 * (scale - 1.0).max(0.0)) / 2.0).ceil() as u32 + 2;
    }
    let x = c.rect.0.saturating_sub(pad);
    let y = c.rect.1.saturating_sub(pad);
    Rect(
        x,
        y,
        (c.rect.0 + c.rect.2 + pad).min(640) - x,
        (c.rect.1 + c.rect.3 + pad).min(480) - y,
    )
}

thread_local! { static SHADOWS: RefCell<VecDeque<CachedText>> = const { RefCell::new(VecDeque::new()) }; }

impl PreparedFrame {
    pub(super) fn ink_shadow(&mut self, rect: Rect, radius: u32, shadow: &Shadow) {
        if shadow.alpha == 0.0 {
            return;
        }
        let margin = shadow.blur * 3 / 2 + 1;
        let x = rect.0 as i32 + shadow.x - margin as i32;
        let y = rect.1 as i32 + shadow.y - margin as i32;
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + (rect.2 + margin * 2) as i32).min(self.pixels.width() as i32);
        let bottom = (y + (rect.3 + margin * 2) as i32).min(self.pixels.height() as i32);
        if right <= left || bottom <= top {
            return;
        }
        let w = (right - left) as u32;
        let h = (bottom - top) as u32;
        let key = format!("{rect:?}:{radius}:{shadow:?}:{left}:{top}:{w}:{h}");
        let weak = self.theme.weak();
        let found = SHADOWS.with(|cache| {
            let mut cache = cache.borrow_mut();
            let index = cache.iter().position(|e| {
                e.theme.ptr_eq(&weak) && e.theme.strong_count() > 0 && e.key == key
            })?;
            let entry = cache.remove(index).unwrap();
            let pixels = entry.pixels.clone();
            cache.push_front(entry);
            Some(pixels)
        });
        let pixels = found.unwrap_or_else(|| {
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
            if shadow.blur > 0 {
                // CSS blur radius is twice the Gaussian standard deviation.
                let sigma = shadow.blur as f32 / 2.0;
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
                            .map(|d| {
                                alpha[(yy * w as i32 + xx + d) as usize]
                                    * kernel[(d + reach) as usize]
                            })
                            .sum();
                    }
                }
                for yy in 0..h as i32 {
                    for xx in 0..w as i32 {
                        alpha[(yy * w as i32 + xx) as usize] = (-reach..=reach)
                            .filter(|d| (0..h as i32).contains(&(yy + d)))
                            .map(|d| {
                                scratch[((yy + d) * w as i32 + xx) as usize]
                                    * kernel[(d + reach) as usize]
                            })
                            .sum();
                    }
                }
            }
            let color = self.color(shadow.color);
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
            let pixels = Arc::new(pix);
            SHADOWS.with(|cache| {
                let mut cache = cache.borrow_mut();
                while !cache.is_empty()
                    && (cache.len() >= 64
                        || cache.iter().map(|e| e.pixels.data().len()).sum::<usize>()
                            + pixels.data().len()
                            > 2 * 1024 * 1024)
                {
                    cache.pop_back();
                }
                cache.push_front(CachedText {
                    theme: weak,
                    key,
                    pixels: pixels.clone(),
                });
            });
            pixels
        });
        bitmap(&mut self.pixels, left, top, pixels.as_ref().as_ref(), true);
    }

    pub(super) fn ink_glow(&mut self, r: Rect, c: &Component) {
        let color = self.color(c.focus_color.unwrap_or(ColorRole::Focus));
        for glow in (1..=c.glow_radius.unwrap_or(0)).rev() {
            let path = crate::display::build_rounded_rect_path(
                draw_rect(Rect(r.0 - glow, r.1 - glow, r.2 + 2 * glow, r.3 + 2 * glow)),
                c.radius.unwrap_or(0) + glow,
            )
            .unwrap();
            self.pixels.fill_path(
                &path,
                &tiny_skia::Paint {
                    shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                        color.r(),
                        color.g(),
                        color.b(),
                        (110 / (glow + 1)) as u8,
                    )),
                    anti_alias: true,
                    ..Default::default()
                },
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    pub(super) fn ink_wifi(&mut self, x: f32, y: f32, scale: f32, role: ColorRole, online: bool) {
        let mut b = tiny_skia::PathBuilder::new();
        b.move_to(1.0, 7.0);
        b.quad_to(12.0, -5.0, 23.0, 7.0);
        b.move_to(6.0, 10.0);
        b.quad_to(12.0, 4.0, 18.0, 10.0);
        let color = self.color(role);
        let paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(color.into()),
            anti_alias: true,
            ..Default::default()
        };
        let transform =
            Transform::from_scale((24.0 * scale).round() / 24.0, (16.0 * scale).round() / 16.0)
                .post_translate(x, y);
        self.pixels.stroke_path(
            &b.finish().unwrap(),
            &paint,
            &tiny_skia::Stroke {
                width: 2.0,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            },
            transform,
            None,
        );
        let mut dot = tiny_skia::PathBuilder::new();
        dot.push_circle(12.0, 14.0, 2.0);
        self.pixels.fill_path(
            &dot.finish().unwrap(),
            &paint,
            tiny_skia::FillRule::Winding,
            transform,
            None,
        );
        if !online {
            let mut slash = tiny_skia::PathBuilder::new();
            slash.move_to(2.0, 2.0);
            slash.line_to(22.0, 14.0);
            self.pixels.stroke_path(
                &slash.finish().unwrap(),
                &paint,
                &tiny_skia::Stroke {
                    width: 2.0,
                    ..Default::default()
                },
                transform,
                None,
            );
        }
    }

    pub(super) fn ink_battery_scaled(
        &mut self,
        x: f32,
        cy: f32,
        scale: f32,
        percent: u8,
        color: ColorRole,
    ) {
        let left = x.round() as u32;
        let top = (cy - 6.0 * scale).round().max(0.0) as u32;
        let right = (x + 24.0 * scale).round() as u32;
        let bottom = (cy + 6.0 * scale).round() as u32;
        let r = Rect(left, top, right - left, bottom - top);
        let sx = r.2 as f32 / 24.0;
        let sy = r.3 as f32 / 12.0;
        self.float_panel(left as f32, top as f32, r.2 as f32, sy, color);
        self.float_panel(left as f32, bottom as f32 - sy, r.2 as f32, sy, color);
        self.float_panel(left as f32, top as f32, sx, r.3 as f32, color);
        self.float_panel(right as f32 - sx, top as f32, sx, r.3 as f32, color);
        let fill = (20 * u32::from(percent.min(100)) / 100) as f32;
        if fill > 0.0 {
            self.float_panel(
                x + 2.0 * sx,
                top as f32 + 2.0 * sy,
                fill * sx,
                8.0 * sy,
                color,
            );
        }
        let cap_top = (cy - 3.0 * scale).round();
        let cap_bottom = (cy + 3.0 * scale).round();
        self.float_panel(
            right as f32,
            cap_top,
            (x + 26.0 * scale).round() - right as f32,
            cap_bottom - cap_top,
            color,
        );
    }
    fn float_panel(&mut self, x: f32, y: f32, w: f32, h: f32, role: ColorRole) {
        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            self.pixels.fill_rect(
                rect,
                &tiny_skia::Paint {
                    shader: tiny_skia::Shader::SolidColor(self.color(role).into()),
                    anti_alias: true,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
}
