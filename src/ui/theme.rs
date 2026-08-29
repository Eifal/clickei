//! ui/theme.rs — Dark glass palette (egui Visuals + helpers).

use egui::{Color32, Rounding, Stroke, Visuals};

pub const BG_PRIMARY: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);
pub const BG_CARD: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x2E);
pub const BG_CARD_HOVER: Color32 = Color32::from_rgb(0x38, 0x38, 0x3B);
pub const BG_CARD_PRESSED: Color32 = Color32::from_rgb(0x42, 0x42, 0x46);
pub const BORDER: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3C);
pub const BORDER_HOVER: Color32 = Color32::from_rgb(0x54, 0x54, 0x58);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF7);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0x98, 0x98, 0x9D);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x6E, 0x6E, 0x73);
pub const ACCENT: Color32 = Color32::from_rgb(0x7C, 0x98, 0x85);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x8E, 0xAB, 0x97);
pub const RECORDING_RED: Color32 = Color32::from_rgb(0xD9, 0x77, 0x57);
pub const WARNING: Color32 = Color32::from_rgb(0xCC, 0x88, 0x33);

pub const CORNER_RADIUS: f32 = 8.0;
pub const PADDING_SMALL: f32 = 6.0;
pub const PADDING_MEDIUM: f32 = 12.0;

pub fn apply_dark_theme(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG_PRIMARY;
    visuals.window_fill = BG_PRIMARY;
    visuals.extreme_bg_color = BG_CARD;
    visuals.faint_bg_color = BG_CARD;
    visuals.widgets.noninteractive.bg_fill = BG_CARD;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    visuals.widgets.inactive.bg_fill = BG_CARD;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    visuals.widgets.hovered.bg_fill = BG_CARD_HOVER;
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.selection.bg_fill = ACCENT;
    visuals.window_rounding = Rounding::same(CORNER_RADIUS);
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    ctx.set_visuals(visuals);
}

pub fn card_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(BG_CARD)
        .rounding(Rounding::same(CORNER_RADIUS))
        .stroke(Stroke::new(1.0_f32, BORDER))
        .inner_margin(egui::Margin::same(PADDING_SMALL))
}
