//! Shared visual language for the settings window.
//!
//! The window deliberately has no image assets: cards, switches, pills and
//! navigation are painted with egui primitives so the app scales cleanly on
//! high-DPI displays and remains a single small executable.

use eframe::egui::{
    self, Align, Button, Color32, FontFamily, FontId, Frame, Layout, Margin, Response, RichText,
    Rounding, Stroke, TextStyle, Ui, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(10, 14, 19);
pub const SIDEBAR: Color32 = Color32::from_rgb(13, 18, 24);
pub const CARD: Color32 = Color32::from_rgb(19, 25, 33);
pub const CARD_ALT: Color32 = Color32::from_rgb(24, 31, 40);
pub const CARD_HOVER: Color32 = Color32::from_rgb(29, 37, 47);
pub const BORDER: Color32 = Color32::from_rgb(36, 45, 56);
pub const BORDER_SOFT: Color32 = Color32::from_rgb(27, 35, 44);
pub const TEXT: Color32 = Color32::from_rgb(236, 241, 246);
pub const TEXT_SOFT: Color32 = Color32::from_rgb(197, 207, 218);
pub const MUTED: Color32 = Color32::from_rgb(133, 146, 161);
pub const FAINT: Color32 = Color32::from_rgb(92, 104, 118);
pub const ACCENT: Color32 = Color32::from_rgb(77, 212, 185);
pub const ACCENT_DARK: Color32 = Color32::from_rgb(14, 47, 40);
pub const AMBER: Color32 = Color32::from_rgb(242, 197, 108);
pub const RED: Color32 = Color32::from_rgb(255, 119, 119);
pub const BLUE: Color32 = Color32::from_rgb(126, 166, 255);

pub fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(30.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::new(13.5, FontFamily::Proportional));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::new(11.5, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(12.0, FontFamily::Monospace),
    );

    style.spacing.item_spacing = Vec2::new(10.0, 9.0);
    style.spacing.button_padding = Vec2::new(13.0, 7.0);
    style.spacing.slider_width = 180.0;
    style.spacing.interact_size.y = 24.0;
    style.animation_time = 0.15;

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_rounding = Rounding::same(16.0);
    visuals.menu_rounding = Rounding::same(10.0);
    visuals.extreme_bg_color = Color32::from_rgb(8, 11, 15);
    visuals.faint_bg_color = CARD_ALT;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = Color32::from_rgba_premultiplied(55, 155, 139, 80);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = AMBER;
    visuals.error_fg_color = RED;

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = CARD;
    widgets.noninteractive.weak_bg_fill = CARD;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_SOFT);
    widgets.noninteractive.rounding = Rounding::same(10.0);

    widgets.inactive.bg_fill = CARD_ALT;
    widgets.inactive.weak_bg_fill = CARD_ALT;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    widgets.inactive.rounding = Rounding::same(10.0);

    widgets.hovered.bg_fill = CARD_HOVER;
    widgets.hovered.weak_bg_fill = CARD_HOVER;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(58, 72, 87));
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    widgets.hovered.rounding = Rounding::same(10.0);

    widgets.active.bg_fill = Color32::from_rgb(34, 46, 55);
    widgets.active.weak_bg_fill = Color32::from_rgb(34, 46, 55);
    widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    widgets.active.rounding = Rounding::same(10.0);

    visuals.widgets.open = widgets.active.clone();
    style.visuals = visuals;
    ctx.set_style(style);
}

pub fn card() -> Frame {
    Frame::none()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(16.0))
        .inner_margin(Margin::same(20.0))
}

pub fn inset_card() -> Frame {
    Frame::none()
        .fill(Color32::from_rgb(16, 21, 28))
        .stroke(Stroke::new(1.0_f32, BORDER_SOFT))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::same(14.0))
}

pub fn eyebrow(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(10.5)
            .strong()
            .color(ACCENT),
    );
}

pub fn page_heading(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(29.0).strong().color(TEXT));
    ui.add_space(2.0);
    ui.label(RichText::new(subtitle).size(13.5).color(MUTED));
}

pub fn section_title(ui: &mut Ui, label: &str, description: &str) {
    ui.label(RichText::new(label).size(16.0).strong().color(TEXT));
    if !description.is_empty() {
        ui.add_space(1.0);
        ui.label(RichText::new(description).size(12.0).color(MUTED));
    }
    ui.add_space(8.0);
}

pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    Frame::none()
        .fill(color.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.34)))
        .rounding(Rounding::same(99.0))
        .inner_margin(Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).strong().color(color));
        })
        .response
}

pub fn status_pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    Frame::none()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.28)))
        .rounding(Rounding::same(99.0))
        .inner_margin(Margin::symmetric(10.0, 5.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, color);
                ui.add_space(3.0);
                ui.label(RichText::new(text).size(11.5).strong().color(color));
            });
        })
        .response
}

pub fn accent_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        Button::new(
            RichText::new(text)
                .size(13.0)
                .strong()
                .color(Color32::from_rgb(7, 27, 22)),
        )
        .fill(ACCENT)
        .stroke(Stroke::new(1.0_f32, ACCENT))
        .rounding(Rounding::same(10.0))
        .min_size(Vec2::new(0.0, 34.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        Button::new(RichText::new(text).size(13.0).color(TEXT))
            .fill(CARD_ALT)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .rounding(Rounding::same(10.0))
            .min_size(Vec2::new(0.0, 34.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A compact animated switch. Returns the click response, so callers can
/// persist the new value and synchronise the daemon.
pub fn toggle(ui: &mut Ui, value: &mut bool) -> Response {
    let desired_size = Vec2::new(44.0, 24.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let animation = ui.ctx().animate_bool_with_time(
        response.id.with("switch"),
        *value,
        0.14,
    );
    let painter = ui.painter();
    let track = if *value {
        Color32::from_rgb(38, 121, 107)
    } else {
        Color32::from_rgb(44, 54, 65)
    };
    painter.rect_filled(rect, Rounding::same(rect.height() * 0.5), track);
    if response.hovered() {
        painter.rect_stroke(
            rect.expand(1.0),
            Rounding::same(rect.height() * 0.5 + 1.0),
            Stroke::new(1.0_f32, if *value { ACCENT } else { BORDER }),
        );
    }
    let knob_radius = rect.height() * 0.5 - 3.0;
    let knob_x = egui::lerp(
        rect.left() + knob_radius + 3.0..=rect.right() - knob_radius - 3.0,
        animation,
    );
    let knob_center = egui::pos2(knob_x, rect.center().y);
    painter.circle_filled(
        knob_center + Vec2::new(0.0, 1.0),
        knob_radius,
        Color32::from_black_alpha(45),
    );
    painter.circle_filled(
        knob_center,
        knob_radius,
        if *value { Color32::WHITE } else { Color32::from_rgb(177, 188, 200) },
    );
    response
}

pub fn setting_toggle(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    value: &mut bool,
) -> Response {
    let inner = ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new((ui.available_width() - 60.0).max(120.0), 0.0),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.label(RichText::new(title).size(14.0).color(TEXT));
                if !subtitle.is_empty() {
                    ui.add_space(1.0);
                    ui.label(RichText::new(subtitle).size(11.7).color(MUTED));
                }
            },
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| toggle(ui, value))
            .inner
    });
    inner.inner
}

pub fn setting_slider(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    decimals: usize,
    suffix: &str,
) -> Response {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new((ui.available_width() - 235.0).max(140.0), 0.0),
            Layout::top_down(Align::LEFT),
            |ui| {
                ui.label(RichText::new(title).size(14.0).color(TEXT));
                if !subtitle.is_empty() {
                    ui.add_space(1.0);
                    ui.label(RichText::new(subtitle).size(11.7).color(MUTED));
                }
            },
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Slider::new(value, range)
                    .show_value(true)
                    .fixed_decimals(decimals)
                    .suffix(suffix)
                    .trailing_fill(true),
            )
        })
        .inner
    })
    .inner
}

pub fn nav_item(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter();
    let fill = if selected {
        Color32::from_rgba_unmultiplied(77, 212, 185, 24)
    } else if response.hovered() {
        CARD_ALT
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, Rounding::same(10.0), fill);
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(
                rect.left_top() + Vec2::new(0.0, 10.0),
                Vec2::new(3.0, rect.height() - 20.0),
            ),
            Rounding::same(2.0),
            ACCENT,
        );
    }
    painter.text(
        rect.left_center() + Vec2::new(15.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::new(14.0, FontFamily::Proportional),
        if selected { ACCENT } else { TEXT_SOFT },
    );
    response
}

pub fn keycap(ui: &mut Ui, text: &str) {
    Frame::none()
        .fill(Color32::from_rgb(31, 39, 49))
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(5.0))
        .inner_margin(Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(FontId::new(11.0, FontFamily::Monospace))
                    .color(TEXT_SOFT),
            );
        });
}

pub fn hairline(ui: &mut Ui) {
    ui.add_space(8.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(rect.right(), rect.top()),
        ],
        Stroke::new(1.0_f32, BORDER_SOFT),
    );
    ui.add_space(8.0);
}

pub fn link_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        Button::new(RichText::new(text).size(12.5).color(ACCENT))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .rounding(Rounding::same(6.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn small_muted(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(MUTED));
}

pub fn bullet_line(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 18.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 2.5, ACCENT.gamma_multiply(0.8));
        ui.label(RichText::new(text).size(12.8).color(TEXT_SOFT));
    });
}

pub fn notice_banner(ui: &mut Ui, text: &str, error: bool) {
    let color = if error { RED } else { ACCENT };
    Frame::none()
        .fill(color.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.3)))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
                ui.label(RichText::new(text).size(12.2).color(if error { TEXT } else { color }));
            });
        });
}
