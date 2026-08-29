//! ui/sequence_window.rs — Sequence Clicking content (docked SidePanel + popped viewport).
//!
//! Dark glass styling (theme.rs) — sage green accent.
//! Compact row default: number, X, Y, summary "3× / 500ms", expand ▼, delete.
//! Expanded row shows full clicks/interval DragValues.

use egui::{Align, Layout, RichText};

use crate::model::SequenceTarget;
use crate::ui::theme;

/// Draw Sequence Clicking content.
///
/// `targets` / `enabled` / `picking` owned by `StaticClickerWindow` (persist when hidden).
/// `active_idx` polled from clicker thread (no delay, set before SendInput).
/// `expanded_row` tracks which row is expanded (single expanded at a time, simpler).
/// `is_popped` true when rendering inside popped-out viewport (shows Dock back), false when docked (shows Pop out).
/// `popped_out` is mutable to toggle mode — caller persists to config via existing save path.
/// `needs_persist` is set to true when a DragValue edit finishes (drag_stopped or lost_focus) or a single-click action (delete/enable toggle) occurs — caller should persist via shared config only then, to avoid write-storm.
pub fn sequence_window_ui(
    ui: &mut egui::Ui,
    targets: &mut Vec<SequenceTarget>,
    enabled: &mut bool,
    picking: &mut bool,
    active_idx: Option<usize>,
    expanded_row: &mut Option<usize>,
    is_popped: bool,
    popped_out: &mut bool,
    needs_persist: &mut bool,
) {
    // --- Header: title + OFF/ON + Pop out / Dock back ---
    ui.horizontal(|ui| {
        // PNG icon (white source, tinted to theme) — same loader installed in main.rs
        let seq_tint = if *enabled { theme::ACCENT } else { theme::TEXT_PRIMARY };
        ui.add(
            egui::Image::new(egui::include_image!("../icons/ic_sequence.png"))
                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                .tint(seq_tint),
        );
        ui.label(
            RichText::new("Sequence Clicking")
                .strong()
                .color(theme::TEXT_PRIMARY)
                .size(14.0),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Pop out / Dock back button — PNG icon + text
            let pop_tooltip = if is_popped {
                "Kembali ke docked panel"
            } else {
                "Buka sebagai window terpisah"
            };
            let pop_text = if is_popped { "Dock back" } else { "Pop out" };
            let pop_icon = egui::Image::new(egui::include_image!("../icons/ic_popup.png"))
                .fit_to_exact_size(egui::vec2(14.0, 14.0))
                .tint(theme::TEXT_MUTED);
            let pop_btn = egui::Button::image_and_text(pop_icon, RichText::new(pop_text).small().color(theme::TEXT_MUTED))
                .fill(theme::BG_CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .rounding(egui::Rounding::same(4.0));
            if ui.add_sized([78.0, 20.0], pop_btn).on_hover_text(pop_tooltip).clicked() {
                *popped_out = !*popped_out;
                *needs_persist = true;
            }

            ui.add_space(6.0);
            // ON button
            let on_fill = if *enabled { theme::ACCENT } else { theme::BG_CARD };
            let on_text = if *enabled { theme::TEXT_PRIMARY } else { theme::TEXT_MUTED };
            let on_btn = egui::Button::new(RichText::new("ON").strong().color(on_text).size(11.0))
                .fill(on_fill)
                .rounding(egui::Rounding::same(4.0));
            if ui.add_sized([36.0, 22.0], on_btn).clicked() {
                *enabled = true;
                *needs_persist = true;
            }
            // OFF button
            let off_fill = if !*enabled { theme::BG_CARD_HOVER } else { theme::BG_CARD };
            let off_text = if !*enabled { theme::TEXT_PRIMARY } else { theme::TEXT_MUTED };
            let off_btn = egui::Button::new(RichText::new("OFF").strong().color(off_text).size(11.0))
                .fill(off_fill)
                .stroke(if !*enabled { egui::Stroke::new(1.0_f32, theme::BORDER_HOVER) } else { egui::Stroke::NONE })
                .rounding(egui::Rounding::same(4.0));
            if ui.add_sized([40.0, 22.0], off_btn).clicked() {
                *enabled = false;
                *needs_persist = true;
            }
        });
    });

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(6.0);

    // --- Start Picking full-width toggle button ---
    let picking_label = if *picking { "Stop Picking" } else { "Start Picking" };
    let picking_fill = if *picking { theme::ACCENT } else { theme::BG_CARD };
    let picking_stroke = if *picking { egui::Stroke::NONE } else { egui::Stroke::new(1.0_f32, theme::BORDER) };
    let btn = egui::Button::new(
        RichText::new(picking_label)
            .strong()
            .color(theme::TEXT_PRIMARY)
            .size(13.0),
    )
    .fill(picking_fill)
    .stroke(picking_stroke)
    .rounding(egui::Rounding::same(6.0));
    let resp = ui.add_sized([ui.available_width(), 30.0], btn);
    if resp.clicked() {
        *picking = !*picking;
        // picking is transient, not persisted
    }
    if *picking {
        ui.add_space(4.0);
        ui.label(
            RichText::new("Click anywhere on screen — each left-click adds a target")
                .small()
                .color(theme::WARNING),
        );
    }

    ui.add_space(8.0);

    // --- List (compact row) ---
    let mut to_delete: Option<usize> = None;
    let mut toggle_expand: Option<usize> = None;

    egui::ScrollArea::vertical()
        .max_height(320.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (idx, target) in targets.iter_mut().enumerate() {
                let is_active = active_idx == Some(idx);
                let is_expanded = *expanded_row == Some(idx);
                let frame = if is_active {
                    egui::Frame::none()
                        .fill(theme::ACCENT.gamma_multiply(0.18))
                        .stroke(egui::Stroke::new(1.5_f32, theme::ACCENT))
                        .rounding(egui::Rounding::same(theme::CORNER_RADIUS))
                        .inner_margin(egui::Margin::same(theme::PADDING_SMALL))
                } else {
                    theme::card_frame()
                };
                frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Row 1 compact
                    ui.horizontal(|ui| {
                        // Active indicator dot
                        let dot = if is_active { "●" } else { "○" };
                        let dot_color = if is_active { theme::ACCENT } else { theme::TEXT_MUTED };
                        ui.label(RichText::new(dot).color(dot_color).size(10.0));
                        ui.add_space(2.0);
                        let num_color = if is_active { theme::ACCENT } else { theme::TEXT_SECONDARY };
                        ui.label(
                            RichText::new(format!("{}", idx + 1))
                                .strong()
                                .color(num_color)
                                .size(12.0),
                        );
                        ui.add_space(4.0);

                        // X compact — value updates in memory every frame, persist throttled to drag end
                        ui.label(RichText::new("x").small().color(theme::TEXT_MUTED));
                        let resp_x = ui.add(
                            egui::DragValue::new(&mut target.x)
                                .speed(1)
                                .range(i32::MIN..=i32::MAX),
                        );
                        if resp_x.drag_stopped() || resp_x.lost_focus() {
                            *needs_persist = true;
                        }
                        ui.add_space(3.0);
                        // Y compact
                        ui.label(RichText::new("y").small().color(theme::TEXT_MUTED));
                        let resp_y = ui.add(
                            egui::DragValue::new(&mut target.y)
                                .speed(1)
                                .range(i32::MIN..=i32::MAX),
                        );
                        if resp_y.drag_stopped() || resp_y.lost_focus() {
                            *needs_persist = true;
                        }
                        ui.add_space(6.0);

                        // Summary clicks× / interval
                        let summary = format!("{}× / {}ms", target.clicks, target.interval_ms);
                        let summary_color = if is_active { theme::ACCENT } else { theme::TEXT_MUTED };
                        ui.label(RichText::new(summary).small().color(summary_color));

                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            // Delete
                            let del_btn = egui::Button::new(RichText::new("\u{1F5D1}").size(12.0).color(theme::TEXT_MUTED))
                                .fill(theme::BG_CARD)
                                .rounding(egui::Rounding::same(4.0));
                            if ui.add_sized([24.0, 20.0], del_btn).clicked() {
                                to_delete = Some(idx);
                            }
                            ui.add_space(2.0);
                            // Expand toggle — custom-painted triangle (font-independent, no Unicode glyph)
                            // U+25B2/U+25BC missing in egui default font atlas → draw solid triangle via Painter
                            let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 20.0), egui::Sense::click());
                            let bg = if response.hovered() {
                                theme::BG_CARD_HOVER
                            } else if is_expanded {
                                theme::BG_CARD_HOVER
                            } else {
                                theme::BG_CARD
                            };
                            ui.painter().rect_filled(rect, egui::Rounding::same(4.0), bg);
                            let center = rect.center();
                            let size = 4.0;
                            let tri_color = if response.hovered() {
                                theme::TEXT_SECONDARY
                            } else {
                                theme::TEXT_MUTED
                            };
                            let points = if is_expanded {
                                // ▲ pointer ke atas
                                vec![
                                    egui::pos2(center.x, center.y - size),
                                    egui::pos2(center.x - size, center.y + size * 0.6),
                                    egui::pos2(center.x + size, center.y + size * 0.6),
                                ]
                            } else {
                                // ▼ pointer ke bawah
                                vec![
                                    egui::pos2(center.x, center.y + size),
                                    egui::pos2(center.x - size, center.y - size * 0.6),
                                    egui::pos2(center.x + size, center.y - size * 0.6),
                                ]
                            };
                            ui.painter()
                                .add(egui::Shape::convex_polygon(points, tri_color, egui::Stroke::NONE));
                            let response = response.on_hover_text("Edit clicks & interval");
                            if response.clicked() {
                                toggle_expand = Some(idx);
                            }
                        });
                    });

                    // Row 2 detail (only when expanded) — DragValues throttled to drag end
                    if is_expanded {
                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("clicks").small().color(theme::TEXT_MUTED));
                            let mut clicks_i = target.clicks as i32;
                            let resp = ui.add(
                                egui::DragValue::new(&mut clicks_i)
                                    .speed(0.5)
                                    .range(1..=9999),
                            );
                            // Update in memory every frame (real-time), but persist only on drag end
                            if resp.changed() {
                                target.clicks = (clicks_i.max(1) as u32).max(1);
                            }
                            if resp.drag_stopped() || resp.lost_focus() {
                                // Ensure latest value is committed
                                target.clicks = (clicks_i.max(1) as u32).max(1);
                                *needs_persist = true;
                            }
                            ui.add_space(12.0);
                            ui.label(RichText::new("interval").small().color(theme::TEXT_MUTED));
                            let mut interval_i = target.interval_ms as i32;
                            let resp2 = ui.add(
                                egui::DragValue::new(&mut interval_i)
                                    .speed(5)
                                    .range(1..=999999)
                                    .suffix(" ms"),
                            );
                            if resp2.changed() {
                                target.interval_ms = (interval_i.max(1) as u32).max(1);
                            }
                            if resp2.drag_stopped() || resp2.lost_focus() {
                                target.interval_ms = (interval_i.max(1) as u32).max(1);
                                *needs_persist = true;
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }

            if targets.is_empty() {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("No targets yet — click \"Start Picking\" and left-click on screen")
                            .small()
                            .color(theme::TEXT_MUTED)
                            .italics(),
                    );
                });
            }
        });

    if let Some(idx) = toggle_expand {
        if *expanded_row == Some(idx) {
            *expanded_row = None;
        } else {
            *expanded_row = Some(idx);
        }
        // expand toggle is not persisted (transient UI state)
    }

    if let Some(idx) = to_delete {
        targets.remove(idx);
        *needs_persist = true;
        // Adjust expanded_row
        if let Some(exp) = *expanded_row {
            if exp == idx {
                *expanded_row = None;
            } else if exp > idx {
                *expanded_row = Some(exp - 1);
            }
        }
    }

    // Bottom hint when toggle OFF
    if !*enabled && !targets.is_empty() {
        ui.add_space(6.0);
        ui.label(
            RichText::new("Sequence is OFF — Start will fallback to Current location")
                .small()
                .color(theme::WARNING),
        );
    }
}
