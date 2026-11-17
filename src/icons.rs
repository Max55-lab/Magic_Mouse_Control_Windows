//! Programmatically rendered icons and the Magic Mouse illustration.
//!
//! Shipping PNG assets is unnecessary for a small utility. Both the window
//! icon and the dynamic tray icon are rasterized from simple shapes at build
//! time (well, at runtime, but only once) and scaled with supersampling.

use eframe::egui::{Color32, Painter, Pos2, Rect, Rounding, Shape, Stroke, Vec2};

const ACCENT: [f32; 4] = [0.31, 0.84, 0.72, 1.0];
const BODY_TOP: [f32; 4] = [0.97, 0.98, 0.99, 1.0];
const BODY_BOTTOM: [f32; 4] = [0.82, 0.86, 0.90, 1.0];

/// One pixel in premultiplied linear-ish RGBA. Everything is drawn at 3-4x
/// resolution and box-filtered down, which is more than enough anti-aliasing
/// for an icon that is at most 256 px wide.
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![[0.0; 4]; width * height],
        }
    }

    fn blend(&mut self, x: usize, y: usize, color: [f32; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let a = color[3].clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let pixel = &mut self.pixels[y * self.width + x];
        let inv = 1.0 - a;
        pixel[0] = color[0] * a + pixel[0] * inv;
        pixel[1] = color[1] * a + pixel[1] * inv;
        pixel[2] = color[2] * a + pixel[2] * inv;
        pixel[3] = a + pixel[3] * inv;
    }

    fn fill_clipped(
        &mut self,
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
        clip: impl Fn(f32, f32) -> bool,
        color: impl Fn(f32, f32) -> [f32; 4],
    ) {
        let x0 = min_x.floor().max(0.0) as usize;
        let y0 = min_y.floor().max(0.0) as usize;
        let x1 = max_x.ceil().min(self.width as f32) as usize;
        let y1 = max_y.ceil().min(self.height as f32) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                if clip(px, py) {
                    self.blend(x, y, color(px, py));
                }
            }
        }
    }

    fn fill_round_rect(&mut self, rect: [f32; 4], radius: f32, color: [f32; 4]) {
        self.fill_round_rect_with(rect, radius, |_, _| color);
    }

    fn fill_round_rect_with(
        &mut self,
        rect: [f32; 4],
        radius: f32,
        color: impl Fn(f32, f32) -> [f32; 4],
    ) {
        let clip = move |x: f32, y: f32| round_rect_contains(rect, radius, x, y);
        self.fill_clipped(rect[0], rect[1], rect[2], rect[3], clip, color);
    }

    fn fill_circle(&mut self, center: (f32, f32), radius: f32, color: [f32; 4]) {
        let clip = move |x: f32, y: f32| {
            let dx = x - center.0;
            let dy = y - center.1;
            dx * dx + dy * dy <= radius * radius
        };
        self.fill_clipped(
            center.0 - radius,
            center.1 - radius,
            center.0 + radius,
            center.1 + radius,
            clip,
            |_, _| color,
        );
    }

    fn downsample(&self, size: u32) -> Vec<u8> {
        let factor_x = self.width / size as usize;
        let factor_y = self.height / size as usize;
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size as usize {
            for x in 0..size as usize {
                let mut sum = [0.0f32; 4];
                let mut count = 0.0f32;
                for sy in 0..factor_y {
                    for sx in 0..factor_x {
                        let px = x * factor_x + sx;
                        let py = y * factor_y + sy;
                        let p = self.pixels[py * self.width + px];
                        sum[0] += p[0];
                        sum[1] += p[1];
                        sum[2] += p[2];
                        sum[3] += p[3];
                        count += 1.0;
                    }
                }
                let a = sum[3] / count;
                let (r, g, b) = if a > 0.0001 {
                    (sum[0] / count / a, sum[1] / count / a, sum[2] / count / a)
                } else {
                    (0.0, 0.0, 0.0)
                };
                rgba.push((r.clamp(0.0, 1.0) * 255.0).round() as u8);
                rgba.push((g.clamp(0.0, 1.0) * 255.0).round() as u8);
                rgba.push((b.clamp(0.0, 1.0) * 255.0).round() as u8);
                rgba.push((a.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
        }
        rgba
    }
}

fn round_rect_contains(rect: [f32; 4], radius: f32, x: f32, y: f32) -> bool {
    let [x0, y0, x1, y1] = rect;
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0.0 || h <= 0.0 {
        return false;
    }
    let r = radius.min(w * 0.5).min(h * 0.5);
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let qx = (x - cx).abs() - (w * 0.5 - r);
    let qy = (y - cy).abs() - (h * 0.5 - r);
    let outside = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0);
    outside <= r
}

fn ellipse_contains(center: (f32, f32), radii: (f32, f32), x: f32, y: f32) -> bool {
    let dx = (x - center.0) / radii.0.max(0.001);
    let dy = (y - center.1) / radii.1.max(0.001);
    dx * dx + dy * dy <= 1.0
}

fn lerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Render the 256 px (or any square size) application icon.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    render_icon_with(size, false, ACCENT)
}

/// Render the dynamic tray icon. Connected + known battery is green, a low
/// battery is red, and a missing battery reading is amber.
pub fn tray_icon_rgba(size: u32, connected: bool, percent: Option<u8>, low: bool) -> Vec<u8> {
    let status = if !connected {
        [0.55, 0.60, 0.66, 1.0]
    } else if low {
        [1.0, 0.39, 0.39, 1.0]
    } else if percent.is_some() {
        ACCENT
    } else {
        [0.96, 0.77, 0.40, 1.0]
    };
    render_icon_with(size, true, status)
}

fn render_icon_with(size: u32, tray: bool, status: [f32; 4]) -> Vec<u8> {
    let size = size.max(16);
    let ss = if size <= 64 { 4 } else { 2 };
    let hi = size * ss;
    let mut canvas = Canvas::new(hi as usize, hi as usize);

    let u = |value: f32| value * hi as f32;
    let tile = [u(0.035), u(0.035), u(0.965), u(0.965)];
    let radius = u(0.22);

    // Soft outer shadow, then the tile itself with a vertical gradient.
    canvas.fill_round_rect(
        [tile[0], tile[1] + u(0.015), tile[2], tile[3] + u(0.015)],
        radius,
        [0.0, 0.0, 0.0, 0.28],
    );
    canvas.fill_round_rect_with(tile, radius, |_, y| {
        lerp(
            [0.11, 0.14, 0.18, 1.0],
            [0.045, 0.06, 0.08, 1.0],
            (y - tile[1]) / (tile[3] - tile[1]),
        )
    });

    // Mint glow near the upper-right corner, clipped by the rounded tile.
    let glow_center = (u(0.88), u(0.10));
    canvas.fill_clipped(
        tile[0],
        tile[1],
        tile[2],
        tile[3],
        |x, y| {
            round_rect_contains(tile, radius, x, y)
                && ellipse_contains(glow_center, (u(0.55), u(0.55)), x, y)
        },
        |x, y| {
            let dx = (x - glow_center.0) / u(0.55);
            let dy = (y - glow_center.1) / u(0.55);
            let falloff = (1.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            [ACCENT[0], ACCENT[1], ACCENT[2], 0.24 * falloff * falloff]
        },
    );

    // Hairline border.
    let inner = [
        tile[0] + u(0.007),
        tile[1] + u(0.007),
        tile[2] - u(0.007),
        tile[3] - u(0.007),
    ];
    canvas.fill_clipped(
        tile[0],
        tile[1],
        tile[2],
        tile[3],
        |x, y| round_rect_contains(tile, radius, x, y) && !round_rect_contains(inner, radius, x, y),
        |_, _| [1.0, 1.0, 1.0, 0.12],
    );

    // Mouse body: a tall capsule, with a soft drop shadow and a light gradient.
    let body = [u(0.33), u(0.195), u(0.67), u(0.815)];
    let body_radius = (body[2] - body[0]) * 0.5;
    canvas.fill_round_rect(
        [body[0], body[1] + u(0.018), body[2], body[3] + u(0.020)],
        body_radius,
        [0.0, 0.0, 0.0, 0.34],
    );
    canvas.fill_round_rect_with(body, body_radius, |_, y| {
        lerp(BODY_TOP, BODY_BOTTOM, (y - body[1]) / (body[3] - body[1]))
    });

    // Thin dark rim around the body makes the silhouette crisp on the tray.
    let body_inner = [
        body[0] + u(0.005),
        body[1] + u(0.005),
        body[2] - u(0.005),
        body[3] - u(0.005),
    ];
    canvas.fill_clipped(
        body[0],
        body[1],
        body[2],
        body[3],
        |x, y| {
            round_rect_contains(body, body_radius, x, y)
                && !round_rect_contains(body_inner, body_radius, x, y)
        },
        |_, _| [0.18, 0.21, 0.25, 0.25],
    );

    // Soft top highlight.
    canvas.fill_clipped(
        body[0],
        body[1],
        body[2],
        body[1] + (body[3] - body[1]) * 0.48,
        |x, y| round_rect_contains(body, body_radius, x, y),
        |_, y| {
            let t = ((y - body[1]) / ((body[3] - body[1]) * 0.48)).clamp(0.0, 1.0);
            [1.0, 1.0, 1.0, 0.26 * (1.0 - t)]
        },
    );

    if tray {
        // At 16-32 px only the silhouette and the status dot survive, and that
        // is exactly what matters in the notification area.
        let dot_center = (u(0.735), u(0.725));
        let dot_radius = u(0.105);
        canvas.fill_circle(dot_center, dot_radius + u(0.027), [0.055, 0.07, 0.09, 0.92]);
        canvas.fill_circle(dot_center, dot_radius + u(0.010), [0.83, 0.86, 0.90, 0.38]);
        let glow = [status[0], status[1], status[2], 0.20];
        canvas.fill_circle(dot_center, dot_radius + u(0.045), glow);
        canvas.fill_circle(dot_center, dot_radius, status);
        // A tiny specular dot keeps the status light from looking flat.
        canvas.fill_circle(
            (
                dot_center.0 - dot_radius * 0.28,
                dot_center.1 - dot_radius * 0.28,
            ),
            dot_radius * 0.24,
            [1.0, 1.0, 1.0, 0.55],
        );
    } else {
        // Three touch points on the top surface give the application icon a
        // recognisable identity even at 32 px.
        for (x, y, r) in [
            (0.455, 0.365, 0.024),
            (0.545, 0.365, 0.024),
            (0.500, 0.470, 0.028),
        ] {
            canvas.fill_circle((u(x), u(y)), u(r), [ACCENT[0], ACCENT[1], ACCENT[2], 0.82]);
        }
        // A subtle powered indicator at the bottom of the mouse.
        let dot_center = (u(0.500), u(0.735));
        canvas.fill_circle(dot_center, u(0.020), [0.30, 0.34, 0.38, 0.38]);
    }

    canvas.downsample(size)
}

/// Draw a large Magic Mouse illustration for the Features page.
pub fn draw_mouse(painter: &Painter, area: Rect) {
    if area.width() < 24.0 || area.height() < 24.0 {
        return;
    }
    let painter = painter.with_clip_rect(area);
    let height = (area.height() * 0.94).min(area.width() * 1.82);
    let width = height * 0.505;
    let center = area.center() + Vec2::new(0.0, 2.0);
    let body = Rect::from_center_size(center, Vec2::new(width, height));
    let rounding = Rounding::same(width * 0.5);

    // Ambient mint halo behind the device.
    painter.add(Shape::ellipse_filled(
        body.center() + Vec2::new(0.0, height * 0.03),
        Vec2::new(width * 0.85, height * 0.44),
        Color32::from_rgba_unmultiplied(78, 215, 185, 13),
    ));

    // Soft layered shadow.
    for i in (0..7).rev() {
        let expand = i as f32 * 1.1;
        let alpha = 4 + (6 - i) as u8 * 2;
        painter.rect_filled(
            body.expand(expand)
                .translate(Vec2::new(0.0, 5.0 + i as f32 * 1.7)),
            Rounding::same(width * 0.5 + expand),
            Color32::from_black_alpha(alpha),
        );
    }

    // Body and a faint rim.
    painter.rect_filled(body, rounding, Color32::from_rgb(233, 238, 243));
    painter.rect_stroke(
        body,
        rounding,
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 74)),
    );

    // Glassy highlights. These are deliberately soft and stay inside the
    // capsule silhouette at normal sizes.
    painter.add(Shape::ellipse_filled(
        Pos2::new(body.center().x, body.top() + height * 0.18),
        Vec2::new(width * 0.37, height * 0.13),
        Color32::from_rgba_unmultiplied(255, 255, 255, 145),
    ));
    painter.add(Shape::ellipse_filled(
        Pos2::new(body.center().x, body.top() + height * 0.31),
        Vec2::new(width * 0.30, height * 0.055),
        Color32::from_rgba_unmultiplied(255, 255, 255, 86),
    ));
    painter.add(Shape::ellipse_filled(
        Pos2::new(body.center().x, body.bottom() - height * 0.075),
        Vec2::new(width * 0.35, height * 0.06),
        Color32::from_rgba_unmultiplied(77, 91, 106, 30),
    ));

    // A very subtle front seam, like the real one-piece top shell.
    let seam_y = body.top() + height * 0.205;
    painter.line_segment(
        [
            Pos2::new(body.left() + width * 0.24, seam_y),
            Pos2::new(body.right() - width * 0.24, seam_y),
        ],
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 84)),
    );

    // A tiny center marker helps users understand that the middle-click zone
    // is the middle of the top shell.
    let p = Pos2::new(body.center().x, body.top() + height * 0.40);
    painter.add(Shape::circle_filled(
        p,
        3.0,
        Color32::from_rgba_unmultiplied(66, 205, 176, 90),
    ));
    painter.add(Shape::circle_filled(
        p,
        7.0,
        Color32::from_rgba_unmultiplied(66, 205, 176, 18),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_has_the_expected_size() {
        let rgba = app_icon_rgba(64);
        assert_eq!(rgba.len(), 64 * 64 * 4);
        assert!(rgba.chunks_exact(4).any(|p| p[3] == 255));
        // The corners should be transparent, the center should not be.
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
        let center = (32 * 64 + 32) * 4;
        assert!(rgba[center + 3] > 0);
    }

    #[test]
    fn tray_icon_is_valid_at_small_sizes() {
        for size in [16, 24, 32, 48] {
            let rgba = tray_icon_rgba(size, true, Some(42), false);
            assert_eq!(rgba.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn rounded_rect_contains_inside_and_outside_points() {
        let r = [0.0, 0.0, 10.0, 10.0];
        assert!(round_rect_contains(r, 5.0, 5.0, 5.0));
        assert!(!round_rect_contains(r, 5.0, 0.0, 0.0));
    }
}
