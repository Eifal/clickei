//! ui/static_clicker_window.rs — Static Clicker UI (OP Auto Clicker style + Click mode + Multi Target).
//!
//! Adds "Multi Target" as a third CursorPositionMode alongside Current/Fixed.
//! Sequence window is a separate native viewport positioned to the right of the main window.

use egui::{CentralPanel, ComboBox, RichText, SidePanel};

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use crate::hooks::{self, SafeHwnd};
use crate::model::MouseButton;
use crate::static_clicker::{
    ClickMode, ClickType, CursorMode, RepeatMode, SequenceTarget, StaticClicker, StaticClickerConfig,
};
use crate::ui::theme;

pub struct StaticClickerWindow {
    // Interval
    hours: i32,
    mins: i32,
    secs: i32,
    millis: i32,
    interval_jitter_ms: i32,
    position_jitter_px: i32,

    // Click options
    button: MouseButton,
    click_type: ClickType,

    // Repeat
    repeat_until_stopped: bool,
    repeat_count: i32,

    // Cursor
    cursor_mode: CursorMode,
    fixed_x: i32,
    fixed_y: i32,
    picking_location: bool,
    picking_location_armed: bool,

    // Sequence (Multi Target) — stored separately so list persists when switching modes
    sequence_targets: Vec<SequenceTarget>,
    sequence_enabled: bool,
    sequence_picking: bool,
    sequence_picking_armed: bool,
    sequence_panel_collapsed: bool,
    sequence_panel_width: f32,
    sequence_panel_popped_out: bool,
    sequence_expanded_row: Option<usize>,

    // Click mode
    foreground: bool,
    picking_window: bool,
    picking_window_armed: bool,
    bg_hwnd: Option<SafeHwnd>,
    bg_title: String,

    // Runtime
    clicker: StaticClicker,
    status: String,
    is_warning: bool,
    clicks_done: u32,
    shared_count: Arc<AtomicU32>,
    shared_error: Arc<std::sync::Mutex<Option<String>>>,
    hotkey: crate::config::HotkeyCombo,
    always_on_top: bool,
    top_initialized: bool,

    // Hotkey settings dialog
    show_hk_settings: bool,
    capturing_hk: bool,
    hk_others: (crate::config::HotkeyCombo, crate::config::HotkeyCombo, crate::config::HotkeyCombo),

    // Latch: ignore stale/duplicate WM_HOTKEY while the key is still held down
    hk_was_down: bool,

    // Presets
    preset_selected: String,
    preset_input: String,
    show_overwrite_confirm: bool,
    pending_overwrite_name: String,
    show_delete_confirm: bool,
    pending_delete_name: String,
}

impl Default for StaticClickerWindow {
    fn default() -> Self {
        let cfg = crate::config::ConfigService::shared().lock().unwrap().clone();
        let b = crate::config::SettingsBindings::from_map(Some(&cfg.hotkeys));
        let (hotkey, hk_rec, hk_play, hk_stop) = (b.static_clicker.clone(), b.record.clone(), b.play.clone(), b.stop.clone());
        let status = format!("Ready. Set interval and press Start ({})", hotkey.display_owned());
        // Split total interval ms into h/m/s/ms for UI
        let total_ms = cfg.static_clicker_interval_ms.max(1);
        let hours = (total_ms / 3_600_000) as i32;
        let rem = total_ms % 3_600_000;
        let mins = (rem / 60_000) as i32;
        let rem2 = rem % 60_000;
        let secs = (rem2 / 1000) as i32;
        let millis = (rem2 % 1000) as i32;
        let (fixed_x, fixed_y) = match cfg.static_clicker_cursor_mode {
            CursorMode::Fixed { x, y } => (x, y),
            _ => (0, 0),
        };
        // Try to restore Background HWND from saved title (best effort, may be invalid after restart)
        let bg_title = cfg.static_clicker_bg_title.clone();
        let bg_hwnd = if !cfg.static_clicker_foreground && !bg_title.is_empty() {
            // Attempt to find window by title; if not found, leave None and UI will show title with warning
            crate::hooks::find_window_by_title(&bg_title)
        } else {
            None
        };
        Self {
            hours,
            mins,
            secs,
            millis,
            interval_jitter_ms: (cfg.static_clicker_interval_jitter_ms as i32).clamp(0, 9999),
            position_jitter_px: (cfg.static_clicker_position_jitter_px as i32).clamp(0, 20),
            button: cfg.static_clicker_button,
            click_type: cfg.static_clicker_click_type,
            repeat_until_stopped: cfg.static_clicker_repeat_until_stopped,
            repeat_count: cfg.static_clicker_repeat_count,
            cursor_mode: cfg.static_clicker_cursor_mode,
            fixed_x,
            fixed_y,
            picking_location: false,
            picking_location_armed: false,
            sequence_targets: cfg.static_clicker_sequence_targets.clone(),
            sequence_enabled: cfg.static_clicker_sequence_enabled,
            sequence_picking: false,
            sequence_picking_armed: false,
            sequence_panel_collapsed: cfg.sequence_panel_collapsed,
            sequence_panel_width: cfg.sequence_panel_width.max(280.0).min(500.0),
            sequence_panel_popped_out: cfg.sequence_panel_popped_out,
            sequence_expanded_row: None,
            foreground: cfg.static_clicker_foreground,
            picking_window: false,
            picking_window_armed: false,
            bg_hwnd,
            bg_title,
            clicker: StaticClicker::new(),
            status,
            is_warning: false,
            clicks_done: 0,
            shared_count: Arc::new(AtomicU32::new(0)),
            shared_error: Arc::new(std::sync::Mutex::new(None)),
            hotkey,
            always_on_top: cfg.always_on_top,
            top_initialized: false,

            show_hk_settings: false,
            capturing_hk: false,
            hk_others: (hk_rec, hk_play, hk_stop),
            hk_was_down: false,

            preset_selected: String::new(),
            preset_input: String::new(),
            show_overwrite_confirm: false,
            pending_overwrite_name: String::new(),
            show_delete_confirm: false,
            pending_delete_name: String::new(),
        }
    }
}

impl StaticClickerWindow {
    /// Whether the docked Sequence panel should reserve width this frame.
    /// Used as outer guard in App::update so Recording tab never reserves space.
    pub fn is_docked_panel_active(&self) -> bool {
        self.cursor_mode == CursorMode::MultiTarget && !self.sequence_panel_popped_out
    }

    /// Docked Sequence panel — MUST be called BEFORE TopBottomPanel/CentralPanel.
    /// Egui allocates panels in order added: SidePanel first claims its width,
    /// then TopBottomPanel/CentralPanel fill the remaining space (window_width - panel_width).
    /// If TopBottomPanel (tab header) is added first, it spans the full window width
    /// including the area later claimed by SidePanel, causing Recording/Static Clicker
    /// tabs to stretch when the Sequence panel is expanded.
    pub fn show_docked_side_panel(&mut self, ctx: &egui::Context) {
        // Apply Always on Top once on first show (also handled in show(), but do it early here)
        if !self.top_initialized {
            self.top_initialized = true;
            if self.always_on_top {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            }
        }

        // ---- Picking & F6 polling (before UI) ----
        self.poll_picking();
        self.poll_f6_hotkey();

        let is_multitarget = self.cursor_mode == CursorMode::MultiTarget;

        // ---- Window auto widen/narrow for docked panel (single monitor friendly) ----
        {
            let desired_width = if is_multitarget && !self.sequence_panel_popped_out {
                if self.sequence_panel_collapsed {
                    520.0 + 36.0
                } else {
                    520.0 + self.sequence_panel_width
                }
            } else {
                520.0
            };
            let current_width = ctx.input(|i| i.viewport().inner_rect.map(|r| r.width()).unwrap_or(520.0));
            if (desired_width - current_width).abs() > 8.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_width, 460.0)));
            }
        }

        // ---- Docked SidePanel (default) — direct borrow, no take/restore ----
        // State collapsed/width/popped persisted via config (existing save path, no race)
        if is_multitarget && !self.sequence_panel_popped_out {
            if self.sequence_panel_collapsed {
                SidePanel::right("sequence_panel_collapsed")
                    .resizable(false)
                    .default_width(36.0)
                    .width_range(36.0..=36.0)
                    .show(ctx, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(8.0);
                            let btn = egui::Button::new(RichText::new("◀").size(14.0).color(theme::TEXT_PRIMARY))
                                .fill(theme::ACCENT)
                                .rounding(egui::Rounding::same(4.0));
                            if ui.add_sized([28.0, 28.0], btn).on_hover_text("Expand Sequence panel").clicked() {
                                self.sequence_panel_collapsed = false;
                                self.persist_panel_state();
                            }
                            ui.add_space(12.0);
                            ui.separator();
                            ui.add_space(8.0);
                            let count = self.sequence_targets.len();
                            let badge_color = if count > 0 { theme::ACCENT } else { theme::TEXT_MUTED };
                            ui.label(RichText::new(format!("{}", count)).strong().size(16.0).color(badge_color));
                            ui.label(RichText::new("targets").small().color(theme::TEXT_MUTED));
                            if count == 0 && self.sequence_enabled {
                                ui.add_space(8.0);
                                ui.label(RichText::new("!").strong().color(theme::WARNING));
                            }
                            ui.add_space(8.0);
                        });
                    });
            } else {
                let panel_width = self.sequence_panel_width;
                let prev_popped = self.sequence_panel_popped_out;
                let mut needs_persist = false;
                let resp = SidePanel::right("sequence_panel")
                    .resizable(true)
                    .default_width(panel_width)
                    .width_range(280.0..=500.0)
                    .show(ctx, |ui| {
                        // Top bar: collapse (pop out handled inside sequence_window_ui header)
                        ui.horizontal(|ui| {
                            let collapse_btn = egui::Button::new(RichText::new("▶").small().color(theme::TEXT_MUTED))
                                .fill(theme::BG_CARD)
                                .rounding(egui::Rounding::same(4.0));
                            if ui.add_sized([22.0, 20.0], collapse_btn).on_hover_text("Collapse panel").clicked() {
                                self.sequence_panel_collapsed = true;
                                self.persist_panel_state();
                            }
                            ui.label(RichText::new(format!("{} targets", self.sequence_targets.len())).small().color(theme::TEXT_MUTED));
                        });
                        ui.separator();
                        ui.add_space(4.0);
                        // Direct borrow — no take/restore for docked (active_idx read directly each frame)
                        // Repaint throttling kept at 10ms while running for indicator responsiveness (see request_repaint_after below)
                        let active_idx = self.clicker.active_target_index();
                        crate::ui::sequence_window::sequence_window_ui(
                            ui,
                            &mut self.sequence_targets,
                            &mut self.sequence_enabled,
                            &mut self.sequence_picking,
                            active_idx,
                            &mut self.sequence_expanded_row,
                            false,
                            &mut self.sequence_panel_popped_out,
                            &mut needs_persist,
                        );
                    });
                let new_width = resp.response.rect.width();
                if (new_width - panel_width).abs() > 2.0 && new_width >= 280.0 && new_width <= 500.0 {
                    self.sequence_panel_width = new_width;
                    self.persist_panel_state();
                }
                if self.sequence_panel_popped_out != prev_popped {
                    self.persist_panel_state();
                }
                if needs_persist {
                    self.persist_static_state();
                }
            }
        }
    }

    /// Show UI inside the shared window (tab bar is drawn by App).
    /// NOTE: SidePanel must already have been shown via show_docked_side_panel()
    /// BEFORE the TopBottomPanel tab header (see App::update order). Do not
    /// re-create SidePanel here — CentralPanel will automatically fill the
    /// remaining width (window_width - panel_width).
    pub fn show(&mut self, ctx: &egui::Context) {
        // Ensure Always on Top / polling still run if show() is called without
        // prior show_docked_side_panel() (defensive; sequential calls are no-ops).
        if !self.top_initialized {
            self.top_initialized = true;
            if self.always_on_top {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            }
        }
        // Polling already done in show_docked_side_panel() when docked, but
        // re-poll here for popped-out mode and for robustness (poll is idempotent).
        self.poll_picking();
        self.poll_f6_hotkey();

        let is_multitarget = self.cursor_mode == CursorMode::MultiTarget;
        // When MultiTarget is selected and toggle ON, global interval is ignored (spec)
        let interval_global_disabled = is_multitarget && self.sequence_enabled;

        // Window widen already handled in show_docked_side_panel(); re-check here
        // for cases where show() is called standalone.
        {
            let desired_width = if is_multitarget && !self.sequence_panel_popped_out {
                if self.sequence_panel_collapsed {
                    520.0 + 36.0
                } else {
                    520.0 + self.sequence_panel_width
                }
            } else {
                520.0
            };
            let current_width = ctx.input(|i| i.viewport().inner_rect.map(|r| r.width()).unwrap_or(520.0));
            if (desired_width - current_width).abs() > 8.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(desired_width, 460.0)));
            }
        }

        CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                ui.add_space(6.0);

                // --- Presets (Save/Load) — snapshot lengkap static clicker settings ---
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Presets").strong().color(theme::TEXT_PRIMARY));
                    ui.add_space(4.0);
                    // Fetch preset names from config
                    let preset_names: Vec<String> = crate::config::ConfigService::shared()
                        .lock()
                        .unwrap()
                        .static_clicker_presets
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();
                    ui.horizontal(|ui| {
                        // ComboBox for existing presets + New
                        ComboBox::from_id_source("preset_combo")
                            .selected_text(if self.preset_selected.is_empty() {
                                "— New Preset —"
                            } else {
                                &self.preset_selected
                            })
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_value(&mut self.preset_selected, String::new(), "— New Preset —").clicked() {
                                    // keep input as is for new preset creation
                                }
                                for name in &preset_names {
                                    if ui.selectable_value(&mut self.preset_selected, name.clone(), name).clicked() {
                                        self.preset_input = name.clone();
                                    }
                                }
                            });
                        // TextEdit for new preset name
                        let name_edit = egui::TextEdit::singleline(&mut self.preset_input)
                            .hint_text("preset name")
                            .desired_width(120.0);
                        ui.add(name_edit);
                        if ui.button("💾 Save").clicked() {
                            self.handle_preset_save();
                        }
                        let load_enabled = !preset_names.is_empty();
                        if ui.add_enabled(load_enabled, egui::Button::new("📂 Load")).clicked() {
                            self.handle_preset_load();
                        }
                        let delete_enabled = !self.preset_selected.is_empty() || !self.preset_input.trim().is_empty();
                        if ui.add_enabled(delete_enabled && !preset_names.is_empty(), egui::Button::new("🗑 Delete")).clicked() {
                            self.handle_preset_delete();
                        }
                    });
                    if preset_names.is_empty() {
                        ui.label(RichText::new("No presets yet — enter a name then Save").small().color(theme::TEXT_MUTED));
                    }
                });

                ui.add_space(6.0);

                // --- Click interval ---
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Click interval").strong().color(theme::TEXT_PRIMARY));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("").size(10.0));
                        let enabled = !self.clicker.is_running() && !interval_global_disabled;
                        let mut interval_changed = false;
                        let resp = ui.add_enabled_ui(enabled, |ui| {
                            ui.horizontal(|ui| {
                                if Self::interval_field(ui, &mut self.hours, "hours") { interval_changed = true; }
                                if Self::interval_field(ui, &mut self.mins, "mins") { interval_changed = true; }
                                if Self::interval_field(ui, &mut self.secs, "secs") { interval_changed = true; }
                                if Self::interval_field(ui, &mut self.millis, "milliseconds") { interval_changed = true; }
                            });
                        });
                        if interval_changed && !interval_global_disabled {
                            self.persist_static_state();
                        }
                        if interval_global_disabled {
                            resp.response.on_hover_text("Interval diatur per-target lewat Sequence Clicking");
                        }
                    });
                    let total = self.total_ms();
                    let mut text = format!("= {} ms total{}", total, if total < 1 { " (clamped to 1ms)" } else { "" });
                    if interval_global_disabled {
                        text.push_str(" — overridden (Multi Target active)");
                    }
                    ui.label(
                        RichText::new(text)
                            .small()
                            .color(if interval_global_disabled { theme::WARNING } else { theme::TEXT_MUTED }),
                    );
                    if interval_global_disabled {
                        ui.label(
                            RichText::new("Timing is controlled per-target in Sequence Clicking")
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    }
                    ui.add_space(4.0);
                    // Interval jitter — small DragValue with debounce persist
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("±").small().color(theme::TEXT_MUTED));
                        let enabled = !self.clicker.is_running();
                        let mut jitter_changed = false;
                        ui.add_enabled_ui(enabled, |ui| {
                            let resp = ui.add(
                                egui::DragValue::new(&mut self.interval_jitter_ms)
                                    .range(0..=9999)
                                    .speed(1)
                                    .clamp_to_range(true),
                            );
                            if resp.drag_stopped() || resp.lost_focus() { jitter_changed = true; }
                            ui.label(RichText::new("ms jitter").small().color(theme::TEXT_MUTED));
                        });
                        if jitter_changed {
                            self.interval_jitter_ms = self.interval_jitter_ms.clamp(0, 9999);
                            self.persist_static_state();
                        }
                    });
                });

                ui.add_space(6.0);

                // --- Click options + Click repeat (two columns) ---
                ui.columns(2, |cols| {
                    // Left: Click options
                    let old_button = self.button;
                    let old_click_type = self.click_type;
                    theme::card_frame().show(&mut cols[0], |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Click options").strong().color(theme::TEXT_PRIMARY));
                        ui.add_space(4.0);
                        // Fixed-width label column so both combos align vertically
                        const OPT_LABEL_W: f32 = 90.0;
                        ui.horizontal(|ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(OPT_LABEL_W, 18.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new("Mouse button:").small().color(theme::TEXT_SECONDARY));
                                },
                            );
                            ComboBox::from_id_source("static_btn")
                                .selected_text(button_name(self.button))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.button, MouseButton::Left, "Left");
                                    ui.selectable_value(&mut self.button, MouseButton::Right, "Right");
                                    ui.selectable_value(&mut self.button, MouseButton::Middle, "Middle");
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(OPT_LABEL_W, 18.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new("Click type:").small().color(theme::TEXT_SECONDARY));
                                },
                            );
                            ComboBox::from_id_source("static_type")
                                .selected_text(match self.click_type {
                                    ClickType::Single => "Single",
                                    ClickType::Double => "Double",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.click_type, ClickType::Single, "Single");
                                    ui.selectable_value(&mut self.click_type, ClickType::Double, "Double");
                                });
                        });
                    });
                    if old_button != self.button || old_click_type != self.click_type {
                        self.persist_static_state();
                    }

                    // Right: Click repeat
                    let old_repeat_until = self.repeat_until_stopped;
                    let mut repeat_count_drag_finished = false;
                    theme::card_frame().show(&mut cols[1], |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Click repeat").strong().color(theme::TEXT_PRIMARY));
                        ui.add_space(4.0);
                        let enabled = !self.clicker.is_running();
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.repeat_until_stopped, false, "");
                                ui.label(RichText::new("Repeat").small());
                                let resp = ui.add_enabled(
                                    !self.repeat_until_stopped,
                                    egui::DragValue::new(&mut self.repeat_count).range(1..=999999).speed(1),
                                );
                                if resp.drag_stopped() || resp.lost_focus() { repeat_count_drag_finished = true; }
                                ui.label(RichText::new("times").small());
                            });
                            ui.radio_value(&mut self.repeat_until_stopped, true, "Repeat until stopped");
                            if is_multitarget && self.sequence_enabled {
                                ui.label(
                                    RichText::new("Repeats = full sequence cycles")
                                        .small()
                                        .color(theme::TEXT_MUTED),
                                );
                            }
                        });
                    });
                    if old_repeat_until != self.repeat_until_stopped || repeat_count_drag_finished {
                        self.persist_static_state();
                    }
                });

                ui.add_space(6.0);

                // --- Cursor position ---
                let old_cursor = self.cursor_mode;
                let mut fixed_drag_finished = false;
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Cursor position").strong().color(theme::TEXT_PRIMARY));
                    ui.add_space(4.0);
                    let enabled = !self.clicker.is_running();
                    ui.add_enabled_ui(enabled, |ui| {
                        // Row 1: Current + Pick button + X/Y
                        ui.horizontal(|ui| {
                            if ui.radio_value(&mut self.cursor_mode, CursorMode::Current, "Current location").clicked() {
                                // keep sequence list but hide viewport
                            }
                            ui.add_space(16.0);
                            let btn_label = if self.picking_location {
                                "Click anywhere… (Esc to cancel)"
                            } else {
                                "Pick location"
                            };
                            let pick_enabled = self.cursor_mode != CursorMode::MultiTarget;
                            if ui.add_enabled(pick_enabled, egui::Button::new(btn_label)).clicked() {
                                self.picking_location = !self.picking_location;
                                self.picking_location_armed = false;
                            }
                            ui.label(RichText::new("X").small());
                            // Enable X/Y only when Fixed — value updates in memory every frame, disk persist throttled to drag end
                            let is_fixed = matches!(self.cursor_mode, CursorMode::Fixed { .. });
                            let resp_x = ui.add_enabled(is_fixed, egui::DragValue::new(&mut self.fixed_x).speed(1));
                            if resp_x.drag_stopped() || resp_x.lost_focus() { fixed_drag_finished = true; }
                            ui.label(RichText::new("Y").small());
                            let resp_y = ui.add_enabled(is_fixed, egui::DragValue::new(&mut self.fixed_y).speed(1));
                            if resp_y.drag_stopped() || resp_y.lost_focus() { fixed_drag_finished = true; }
                        });
                        // Row 2: Fixed
                        ui.horizontal(|ui| {
                            let is_fixed = matches!(self.cursor_mode, CursorMode::Fixed { .. });
                            if ui.radio(is_fixed, "Fixed location").clicked() {
                                self.cursor_mode = CursorMode::Fixed { x: self.fixed_x, y: self.fixed_y };
                            }
                            // Keep fixed_x/y in sync when radio is Fixed
                            if let CursorMode::Fixed { x, y } = self.cursor_mode {
                                if x != self.fixed_x || y != self.fixed_y {
                                    self.cursor_mode = CursorMode::Fixed { x: self.fixed_x, y: self.fixed_y };
                                }
                            }
                        });
                        // Row 3: Multi Target
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.cursor_mode, CursorMode::MultiTarget, "Multi Target");
                            ui.label(RichText::new("(Sequence Clicking)").small().color(theme::TEXT_MUTED));
                            if is_multitarget {
                                ui.label(
                                    RichText::new(format!("{} target(s)", self.sequence_targets.len()))
                                        .small()
                                        .color(if self.sequence_targets.is_empty() { theme::WARNING } else { theme::ACCENT }),
                                );
                                if !self.sequence_enabled {
                                    ui.label(RichText::new("— OFF (fallback to Current)").small().color(theme::WARNING));
                                }
                            }
                        });
                        if self.picking_location {
                            ui.label(
                                RichText::new("Move cursor to target and left-click — coordinates will auto-fill")
                                    .small()
                                    .color(theme::WARNING),
                            );
                        }
                        if is_multitarget && self.sequence_targets.is_empty() && self.sequence_enabled {
                            ui.label(
                                RichText::new("Add at least 1 target first")
                                    .small()
                                    .color(theme::WARNING),
                            );
                        }
                        ui.add_space(4.0);
                        // Position jitter — clamp 0..=20 to avoid large misclicks
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("±").small().color(theme::TEXT_MUTED));
                            let enabled = !self.clicker.is_running();
                            let mut pos_jitter_changed = false;
                            ui.add_enabled_ui(enabled, |ui| {
                                let resp = ui.add(
                                    egui::DragValue::new(&mut self.position_jitter_px)
                                        .range(0..=20)
                                        .speed(1)
                                        .clamp_to_range(true),
                                );
                                if resp.drag_stopped() || resp.lost_focus() { pos_jitter_changed = true; }
                                ui.label(RichText::new("px jitter").small().color(theme::TEXT_MUTED));
                            });
                            if pos_jitter_changed {
                                self.position_jitter_px = self.position_jitter_px.clamp(0, 20);
                                self.persist_static_state();
                            }
                        });
                    });
                });
                if old_cursor != self.cursor_mode || fixed_drag_finished {
                    self.persist_static_state();
                }

                ui.add_space(6.0);

                // --- Click mode (new section, required) ---
                let old_foreground = self.foreground;
                let old_bg_title = self.bg_title.clone();
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Click mode").strong().color(theme::TEXT_PRIMARY));
                    ui.add_space(4.0);
                    let enabled = !self.clicker.is_running();
                    ui.add_enabled_ui(enabled, |ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.foreground, true, "Foreground");
                            ui.label(RichText::new("(moves real cursor)").small().color(theme::TEXT_MUTED));
                            ui.add_space(16.0);
                            ui.radio_value(&mut self.foreground, false, "Background");
                            ui.label(RichText::new("(sends message)").small().color(theme::TEXT_MUTED));
                        });

                        if !self.foreground {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                let pick_label = if self.picking_window {
                                    "Click target window… (Esc to cancel)"
                                } else {
                                    "Pick window"
                                };
                                if ui.button(pick_label).clicked() {
                                    self.picking_window = !self.picking_window;
                                    self.picking_window_armed = false;
                                }
                                let title = if self.bg_title.is_empty() {
                                    "(no window selected)".to_string()
                                } else {
                                    self.bg_title.clone()
                                };
                                ui.label(RichText::new(title).small().color(theme::ACCENT));
                            });
                            if self.picking_window {
                                ui.label(
                                    RichText::new("Click anywhere inside the target window")
                                        .small()
                                        .color(theme::WARNING),
                                );
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Background mode doesn't work in all apps/games — try Foreground mode if the target doesn't respond.",
                        )
                        .small()
                        .color(theme::TEXT_MUTED),
                    );
                });
                if old_foreground != self.foreground || old_bg_title != self.bg_title {
                    self.persist_static_state();
                }

                ui.add_space(10.0);

                // --- Start / Stop (big) ---
                let running = self.clicker.is_running();
                let hk = self.hotkey.display_owned();
                ui.columns(2, |cols| {
                    let start_btn = egui::Button::new(
                        RichText::new(format!("Start ({})", hk)).strong().color(theme::TEXT_PRIMARY),
                    )
                    .fill(if running { theme::BG_CARD } else { theme::ACCENT })
                    .rounding(egui::Rounding::same(6.0));
                    let stop_btn = egui::Button::new(
                        RichText::new(format!("Stop ({})", hk)).strong().color(theme::TEXT_PRIMARY),
                    )
                    .fill(if running { theme::RECORDING_RED } else { theme::BG_CARD })
                    .rounding(egui::Rounding::same(6.0));

                    if cols[0].add_enabled(!running, start_btn).clicked() {
                        self.start_clicker();
                    }
                    if cols[1].add_enabled(running, stop_btn).clicked() {
                        self.stop_clicker();
                    }
                });

                ui.add_space(6.0);

                // --- Status line (was in old top bar) ---
                {
                    let running = self.clicker.is_running();
                    let txt = if running {
                        format!("Running… {} clicks", self.clicks_done)
                    } else {
                        self.status.clone()
                    };
                    let col = if self.is_warning { theme::WARNING } else { theme::TEXT_SECONDARY };
                    ui.label(RichText::new(txt).small().color(col));
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button("Hotkey setting").clicked() {
                        // Snapshot other bindings for duplicate check — read from shared
                        let cfg = crate::config::ConfigService::shared().lock().unwrap().clone();
                        let b = crate::config::SettingsBindings::from_map(Some(&cfg.hotkeys));
                        self.hk_others = (b.record, b.play, b.stop);
                        self.capturing_hk = false;
                        self.show_hk_settings = true;
                    }
                    if ui.small_button("Help >>").clicked() {
                        self.status = "Foreground = moves cursor (works everywhere). Background = PostMessage (no cursor move, fails on some games)".to_string();
                        self.is_warning = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut top = self.always_on_top;
                        if ui.checkbox(&mut top, "Always on top").changed() {
                            self.always_on_top = top;
                            // Persist via shared (avoid lost update)
                            let _ = crate::config::ConfigService::update_and_save(|cfg| cfg.always_on_top = top);
                            if top {
                                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
                            } else {
                                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
                            }
                        }
                    });
                });
            });
        });

        // ---- Popped-out viewport (optional, for users who want separate OS window) ----
        // Only when MultiTarget && popped_out == true — keep take/restore for viewport borrow-checker
        // Docked mode uses direct borrow above, no take/restore needed
        if self.cursor_mode == CursorMode::MultiTarget && self.sequence_panel_popped_out {
            // Calculate position to the right of main window
            let outer_rect = ctx.input(|i| i.viewport().outer_rect);
            let pos = if let Some(rect) = outer_rect {
                egui::pos2(rect.max.x + 8.0, rect.min.y)
            } else {
                egui::pos2(560.0, 80.0)
            };
            let viewport_id = egui::ViewportId::from_hash_of("sequence_clicking_viewport");
            let builder = egui::ViewportBuilder::default()
                .with_title("Sequence Clicking")
                .with_inner_size([420.0, 460.0])
                .with_position(pos)
                .with_resizable(true)
                .with_min_inner_size([360.0, 300.0]);

            // Use take/restore pattern only for popped viewport (required for show_viewport_immediate)
            let mut targets = std::mem::take(&mut self.sequence_targets);
            let mut enabled = self.sequence_enabled;
            let mut picking = self.sequence_picking;
            let mut expanded = self.sequence_expanded_row;
            let mut popped = self.sequence_panel_popped_out;
            let active_idx = self.clicker.active_target_index();

            let (new_targets, new_enabled, new_picking, new_expanded, new_popped, new_needs_persist) = ctx.show_viewport_immediate(
                viewport_id,
                builder,
                move |ctx, class| {
                    let mut inner_needs = false;
                    // Handle OS close (X) as Dock back
                    if ctx.input(|i| i.viewport().close_requested()) {
                        popped = false;
                    }
                    if class == egui::ViewportClass::Embedded {
                        // Fallback for backends without viewport support: embed as window inside main viewport
                        egui::Window::new("Sequence Clicking")
                            .resizable(true)
                            .default_size([420.0, 460.0])
                            .show(ctx, |ui| {
                                crate::ui::sequence_window::sequence_window_ui(
                                    ui,
                                    &mut targets,
                                    &mut enabled,
                                    &mut picking,
                                    active_idx,
                                    &mut expanded,
                                    true,
                                    &mut popped,
                                    &mut inner_needs,
                                );
                            });
                    } else {
                        crate::ui::theme::apply_dark_theme(ctx);
                        egui::CentralPanel::default().show(ctx, |ui| {
                            crate::ui::sequence_window::sequence_window_ui(
                                ui,
                                &mut targets,
                                &mut enabled,
                                &mut picking,
                                active_idx,
                                &mut expanded,
                                true,
                                &mut popped,
                                &mut inner_needs,
                            );
                        });
                    }
                    // If Dock back was clicked inside header, popped is now false — will be handled next frame
                    (targets, enabled, picking, expanded, popped, inner_needs)
                },
            );

            let popped_changed = new_popped != self.sequence_panel_popped_out;
            self.sequence_targets = new_targets;
            self.sequence_enabled = new_enabled;
            self.sequence_picking = new_picking;
            self.sequence_expanded_row = new_expanded;
            self.sequence_panel_popped_out = new_popped;
            if popped_changed {
                self.persist_panel_state();
            }
            if new_needs_persist {
                self.persist_static_state();
            }
            if !self.sequence_picking {
                self.sequence_picking_armed = false;
            }
        }

        // ---- Hotkey settings dialog ----
        if self.show_hk_settings {
            let mut open = self.show_hk_settings;
            egui::Window::new("Static Clicker — Hotkey")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Global hotkey to Start/Stop the clicker.");
                    ui.separator();

                    let capturing = self.capturing_hk;
                    let label = if capturing {
                        "⌨ Press hotkey… (Esc = clear)".to_string()
                    } else {
                        self.hotkey.display_owned()
                    };
                    if ui.add_enabled(!capturing, egui::Button::new(label)).clicked() {
                        self.capturing_hk = true;
                    }
                    if capturing {
                        // Win32 raw polling — distinguishes Numpad vs top-row etc.
                        #[cfg(windows)]
                        let detected = crate::ui::main_window::poll_win32_hotkey();
                        #[cfg(not(windows))]
                        let detected: Option<crate::config::HotkeyCombo> = None;
                        if let Some(c) = detected {
                            self.hotkey = c;
                            self.capturing_hk = false;
                        }
                    }
                    if ui.small_button("Clear").clicked() {
                        self.hotkey = crate::config::HotkeyCombo::none();
                    }

                    // Duplicate check vs Recording hotkeys
                    let mut dup_names: Vec<&str> = Vec::new();
                    if self.hotkey.key != 0 {
                        if self.hotkey == self.hk_others.0 { dup_names.push("Record"); }
                        if self.hotkey == self.hk_others.1 { dup_names.push("Play"); }
                        if self.hotkey == self.hk_others.2 { dup_names.push("Stop"); }
                    }
                    for name in &dup_names {
                        ui.colored_label(theme::WARNING, format!("Conflicts with Recording hotkey: {}", name));
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.add_enabled(dup_names.is_empty(), egui::Button::new("OK")).clicked() {
                            let hk = self.hotkey.clone();
                            let _ = crate::config::ConfigService::update_and_save(|cfg| {
                                let mut b = crate::config::SettingsBindings::from_map(Some(&cfg.hotkeys));
                                b.static_clicker = hk.clone();
                                cfg.hotkeys = b.to_map();
                            });
                            self.show_hk_settings = false;
                            self.status = format!("Hotkey set: {} — press to Start/Stop", self.hotkey.display_owned());
                            self.is_warning = false;
                        }
                        if ui.button("Cancel").clicked() {
                            let cfg = crate::config::ConfigService::shared().lock().unwrap().clone();
                            let b = crate::config::SettingsBindings::from_map(Some(&cfg.hotkeys));
                            self.hotkey = b.static_clicker;
                            self.show_hk_settings = false;
                        }
                    });
                });
            if !open {
                self.show_hk_settings = false;
                self.capturing_hk = false;
            }
        }

        // ---- Preset overwrite confirm dialog ----
        if self.show_overwrite_confirm {
            let mut open = self.show_overwrite_confirm;
            egui::Window::new("Overwrite preset?")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(format!("Preset '{}' already exists. Overwrite?", self.pending_overwrite_name));
                    ui.label(RichText::new("The previous preset will be overwritten.").small().color(theme::WARNING));
                    ui.horizontal(|ui| {
                        if ui.button("Yes, Overwrite").clicked() {
                            let name = self.pending_overwrite_name.clone();
                            let preset = self.build_preset(name);
                            self.do_save_preset(preset);
                            self.show_overwrite_confirm = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_overwrite_confirm = false;
                        }
                    });
                });
            if !open {
                self.show_overwrite_confirm = false;
            }
        }

        // ---- Preset delete confirm dialog ----
        if self.show_delete_confirm {
            let mut open = self.show_delete_confirm;
            egui::Window::new("Delete preset?")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(format!("Delete preset '{}'?", self.pending_delete_name));
                    ui.label(RichText::new("This cannot be undone.").small().color(theme::WARNING));
                    ui.horizontal(|ui| {
                        if ui.button("Yes, Delete").clicked() {
                            let name = self.pending_delete_name.clone();
                            self.do_delete_preset(name);
                            self.show_delete_confirm = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_delete_confirm = false;
                        }
                    });
                });
            if !open {
                self.show_delete_confirm = false;
            }
        }

        // Keep the UI awake so WM_HOTKEY events are drained even when the user
        // is idle (egui sleeps without input — global hotkeys would feel dead)
        // Faster repaint while sequence is running so active indicator has no delay
        if self.clicker.is_running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(10));
        } else if self.sequence_picking {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
    }

    fn interval_field(ui: &mut egui::Ui, val: &mut i32, label: &str) -> bool {
        let resp = ui.add(egui::DragValue::new(val).range(0..=9999).speed(1));
        ui.label(RichText::new(label).small().color(theme::TEXT_MUTED));
        // Only persist on drag end or lost focus to avoid write-storm (value updates in memory every frame via &mut, but disk write throttled)
        resp.drag_stopped() || resp.lost_focus()
    }

    fn total_ms(&self) -> u64 {
        let h = (self.hours.max(0) as u64) * 3_600_000;
        let m = (self.mins.max(0) as u64) * 60_000;
        let s = (self.secs.max(0) as u64) * 1000;
        let ms = self.millis.max(0) as u64;
        (h + m + s + ms).max(1)
    }

    fn persist_panel_state(&self) {
        // Single shared in-memory instance — update via shared to avoid lost update
        let collapsed = self.sequence_panel_collapsed;
        let width = self.sequence_panel_width;
        let popped = self.sequence_panel_popped_out;
        let _ = crate::config::ConfigService::update_and_save(|cfg| {
            cfg.sequence_panel_collapsed = collapsed;
            cfg.sequence_panel_width = width;
            cfg.sequence_panel_popped_out = popped;
        });
    }

    fn persist_static_state(&self) {
        let interval_ms = self.total_ms();
        let button = self.button;
        let click_type = self.click_type;
        let repeat_until_stopped = self.repeat_until_stopped;
        let repeat_count = self.repeat_count;
        let cursor_mode = self.cursor_mode;
        let foreground = self.foreground;
        let bg_title = self.bg_title.clone();
        let sequence_targets = self.sequence_targets.clone();
        let sequence_enabled = self.sequence_enabled;
        let interval_jitter_ms = self.interval_jitter_ms.clamp(0, 9999) as u32;
        let position_jitter_px = self.position_jitter_px.clamp(0, 20) as u32;
        let _ = crate::config::ConfigService::update_and_save(|cfg| {
            cfg.static_clicker_interval_ms = interval_ms;
            cfg.static_clicker_button = button;
            cfg.static_clicker_click_type = click_type;
            cfg.static_clicker_repeat_until_stopped = repeat_until_stopped;
            cfg.static_clicker_repeat_count = repeat_count;
            cfg.static_clicker_cursor_mode = cursor_mode;
            cfg.static_clicker_foreground = foreground;
            cfg.static_clicker_bg_title = bg_title.clone();
            cfg.static_clicker_sequence_targets = sequence_targets.clone();
            cfg.static_clicker_sequence_enabled = sequence_enabled;
            cfg.static_clicker_interval_jitter_ms = interval_jitter_ms;
            cfg.static_clicker_position_jitter_px = position_jitter_px;
        });
    }

    // -----------------------------------------------------------------------
    // Presets — build / save / load / delete
    // -----------------------------------------------------------------------

    fn build_preset(&self, name: String) -> crate::model::StaticClickerPreset {
        crate::model::StaticClickerPreset {
            name,
            interval_ms: self.total_ms().min(u32::MAX as u64) as u32,
            interval_jitter_ms: self.interval_jitter_ms.clamp(0, 9999) as u32,
            position_jitter_px: self.position_jitter_px.clamp(0, 20) as u32,
            button: self.button,
            click_type: self.click_type,
            repeat_until_stopped: self.repeat_until_stopped,
            repeat_count: self.repeat_count.max(1) as u32,
            cursor_mode: self.cursor_mode,
            foreground: self.foreground,
            bg_title: self.bg_title.clone(),
            sequence_targets: self.sequence_targets.clone(),
            sequence_enabled: self.sequence_enabled,
        }
    }

    fn handle_preset_save(&mut self) {
        let name = self.preset_input.trim().to_string();
        if name.is_empty() {
            self.status = "Preset name cannot be empty — enter a name first".to_string();
            self.is_warning = true;
            return;
        }
        // Check duplicate
        let exists = crate::config::ConfigService::shared()
            .lock()
            .unwrap()
            .static_clicker_presets
            .iter()
            .any(|p| p.name == name);
        if exists {
            self.pending_overwrite_name = name;
            self.show_overwrite_confirm = true;
        } else {
            let preset = self.build_preset(name);
            self.do_save_preset(preset);
        }
    }

    fn do_save_preset(&mut self, preset: crate::model::StaticClickerPreset) {
        let name = preset.name.clone();
        let preset_clone = preset.clone();
        let _ = crate::config::ConfigService::update_and_save(|cfg| {
            if let Some(pos) = cfg.static_clicker_presets.iter().position(|p| p.name == preset_clone.name) {
                cfg.static_clicker_presets[pos] = preset_clone.clone();
            } else {
                cfg.static_clicker_presets.push(preset_clone.clone());
            }
        });
        self.preset_selected = name.clone();
        self.status = format!("Preset '{}' saved", name);
        self.is_warning = false;
    }

    fn handle_preset_load(&mut self) {
        if self.clicker.is_running() {
            self.status = "Stop the clicker before loading a preset".to_string();
            self.is_warning = true;
            return;
        }
        let name = if !self.preset_selected.is_empty() {
            self.preset_selected.clone()
        } else {
            self.preset_input.trim().to_string()
        };
        if name.is_empty() {
            self.status = "Select a preset first".to_string();
            self.is_warning = true;
            return;
        }
        let preset_opt = crate::config::ConfigService::shared()
            .lock()
            .unwrap()
            .static_clicker_presets
            .iter()
            .find(|p| p.name == name)
            .cloned();
        let Some(preset) = preset_opt else {
            self.status = format!("Preset '{}' not found", name);
            self.is_warning = true;
            return;
        };
        self.apply_preset(&preset);
        // Persist as active config as well
        self.persist_static_state();
        self.preset_selected = name.clone();
        self.preset_input = name.clone();
        self.status = format!("Preset '{}' loaded", name);
        self.is_warning = false;
    }

    fn apply_preset(&mut self, preset: &crate::model::StaticClickerPreset) {
        // interval split h/m/s/ms
        let total = preset.interval_ms as u64;
        self.hours = (total / 3_600_000) as i32;
        let rem = total % 3_600_000;
        self.mins = (rem / 60_000) as i32;
        let rem2 = rem % 60_000;
        self.secs = (rem2 / 1000) as i32;
        self.millis = (rem2 % 1000) as i32;
        self.interval_jitter_ms = (preset.interval_jitter_ms as i32).clamp(0, 9999);
        self.position_jitter_px = (preset.position_jitter_px as i32).clamp(0, 20);
        self.button = preset.button;
        self.click_type = preset.click_type;
        self.repeat_until_stopped = preset.repeat_until_stopped;
        self.repeat_count = (preset.repeat_count as i32).clamp(1, 999999);
        self.cursor_mode = preset.cursor_mode;
        if let CursorMode::Fixed { x, y } = preset.cursor_mode {
            self.fixed_x = x;
            self.fixed_y = y;
        }
        self.foreground = preset.foreground;
        self.bg_title = preset.bg_title.clone();
        self.bg_hwnd = if !preset.foreground && !preset.bg_title.is_empty() {
            crate::hooks::find_window_by_title(&preset.bg_title)
        } else {
            None
        };
        self.sequence_targets = preset.sequence_targets.clone();
        self.sequence_enabled = preset.sequence_enabled;
    }

    fn handle_preset_delete(&mut self) {
        let name = if !self.preset_selected.is_empty() {
            self.preset_selected.clone()
        } else {
            self.preset_input.trim().to_string()
        };
        if name.is_empty() {
            self.status = "Select a preset first".to_string();
            self.is_warning = true;
            return;
        }
        let exists = crate::config::ConfigService::shared()
            .lock()
            .unwrap()
            .static_clicker_presets
            .iter()
            .any(|p| p.name == name);
        if !exists {
            self.status = format!("Preset '{}' not found", name);
            self.is_warning = true;
            return;
        }
        self.pending_delete_name = name;
        self.show_delete_confirm = true;
    }

    fn do_delete_preset(&mut self, name: String) {
        let _ = crate::config::ConfigService::update_and_save(|cfg| {
            cfg.static_clicker_presets.retain(|p| p.name != name);
        });
        if self.preset_selected == name {
            self.preset_selected.clear();
        }
        if self.preset_input == name {
            self.preset_input.clear();
        }
        self.status = format!("Preset '{}' deleted", name);
        self.is_warning = false;
    }

    fn start_clicker(&mut self) {
        if self.clicker.is_running() {
            return;
        }
        let interval = self.total_ms();
        if interval < 1 {
            self.status = "Interval must be at least 1ms".to_string();
            self.is_warning = true;
            return;
        }
        // MultiTarget validation: empty list not allowed when enabled
        if self.cursor_mode == CursorMode::MultiTarget && self.sequence_enabled && self.sequence_targets.is_empty() {
            self.status = "Add at least 1 target first".to_string();
            self.is_warning = true;
            return;
        }
        if !self.foreground && self.bg_hwnd.is_none() {
            self.status = "Background mode: pick a window first".to_string();
            self.is_warning = true;
            return;
        }
        if !self.foreground {
            if let Some(hwnd) = self.bg_hwnd {
                if !hwnd.is_valid() {
                    self.status = "Selected window is no longer valid — pick again".to_string();
                    self.is_warning = true;
                    return;
                }
            }
        }

        let repeat = if self.repeat_until_stopped {
            RepeatMode::Infinite
        } else {
            RepeatMode::Count(self.repeat_count.max(1) as u32)
        };
        // Sync Fixed coords into enum if needed
        let cursor = match self.cursor_mode {
            CursorMode::Current => CursorMode::Current,
            CursorMode::Fixed { .. } => CursorMode::Fixed { x: self.fixed_x, y: self.fixed_y },
            CursorMode::MultiTarget => CursorMode::MultiTarget,
        };
        // Keep enum in sync for Fixed
        self.cursor_mode = cursor;

        let mode = if self.foreground {
            ClickMode::Foreground
        } else {
            ClickMode::Background(self.bg_hwnd.unwrap())
        };

        let cfg = StaticClickerConfig {
            interval_ms: interval,
            button: self.button,
            click_type: self.click_type,
            repeat,
            cursor,
            mode,
            sequence_targets: self.sequence_targets.clone(),
            sequence_enabled: self.sequence_enabled,
            interval_jitter_ms: self.interval_jitter_ms.clamp(0, 9999) as u32,
            position_jitter_px: self.position_jitter_px.clamp(0, 20) as u32,
        };

        self.clicks_done = 0;
        self.shared_count.store(0, Ordering::Relaxed);
        *self.shared_error.lock().unwrap() = None;
        self.is_warning = false;
        if cursor == CursorMode::MultiTarget && self.sequence_enabled {
            self.status = format!("Running sequence… {} target(s)", self.sequence_targets.len());
        } else {
            self.status = format!("Running… interval {} ms", interval);
        }

        let count_clone = self.shared_count.clone();
        let err_clone = self.shared_error.clone();
        self.clicker.start(
            cfg,
            move |count, _total| {
                count_clone.store(count, Ordering::Relaxed);
            },
            move |completed, err| {
                if let Some(e) = &err {
                    log::warn!("static clicker finished with error: {}", e);
                    *err_clone.lock().unwrap() = Some(e.clone());
                } else if completed {
                    log::info!("static clicker completed");
                    *err_clone.lock().unwrap() = Some("Completed".to_string());
                } else {
                    *err_clone.lock().unwrap() = Some("Stopped".to_string());
                }
            },
        );
    }

    fn stop_clicker(&mut self) {
        self.clicker.stop();
        self.status = "Stopping…".to_string();
        self.is_warning = false;
        // Actual stop will be detected next frame via is_running false; we don't block UI
    }

    fn poll_picking(&mut self) {
        #[cfg(windows)]
        {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

            // Cancel with Esc
            let esc_down = unsafe { GetAsyncKeyState(0x1B) } as u16 & 0x8000 != 0;
            if esc_down {
                if self.picking_location {
                    self.picking_location = false;
                    self.picking_location_armed = false;
                    self.status = "Pick location cancelled".to_string();
                }
                if self.picking_window {
                    self.picking_window = false;
                    self.picking_window_armed = false;
                    self.status = "Pick window cancelled".to_string();
                }
                if self.sequence_picking {
                    self.sequence_picking = false;
                    self.sequence_picking_armed = false;
                    self.status = "Sequence picking cancelled".to_string();
                }
            }

            if self.picking_location {
                let l_down = unsafe { GetAsyncKeyState(0x01) } as u16 & 0x8000 != 0;
                if !self.picking_location_armed {
                    if !l_down {
                        self.picking_location_armed = true;
                    }
                } else if l_down {
                    if let Some((x, y)) = hooks::get_cursor_pos() {
                        self.fixed_x = x;
                        self.fixed_y = y;
                        self.cursor_mode = CursorMode::Fixed { x, y };
                        self.status = format!("Location picked: {} , {}", x, y);
                        self.is_warning = false;
                        self.persist_static_state();
                    }
                    self.picking_location = false;
                    self.picking_location_armed = false;
                }
            }

            // If not in MultiTarget, ensure sequence picking is off (list persists, but picking stops)
            if self.cursor_mode != CursorMode::MultiTarget && self.sequence_picking {
                self.sequence_picking = false;
                self.sequence_picking_armed = false;
            }

            if self.sequence_picking {
                let l_down = unsafe { GetAsyncKeyState(0x01) } as u16 & 0x8000 != 0;
                if !self.sequence_picking_armed {
                    if !l_down {
                        self.sequence_picking_armed = true;
                    }
                } else if l_down {
                    if let Some((x, y)) = hooks::get_cursor_pos() {
                        // Bugfix: klik pada tombol Stop Picking (di dalam window Sequence) jangan dihitung sebagai target.
                        // Abaikan klik yang jatuh di dalam window aplikasi (main atau sequence).
                        if hooks::is_point_in_app_window(x, y) {
                            // Jangan tambah target, tapi tetap reset armed agar tidak spam
                            // Tetap butuh lepas tombol sebelum deteksi berikutnya
                            self.sequence_picking_armed = false;
                        } else {
                            self.sequence_targets.push(SequenceTarget {
                                x,
                                y,
                                clicks: 1,
                                interval_ms: 500,
                            });
                            self.status = format!("Target added: {} , {} ({} total)", x, y, self.sequence_targets.len());
                            self.is_warning = false;
                            self.persist_static_state();
                            // Keep picking active for next target; only stop when button toggled or Esc
                            // To avoid double-adding from holding mouse, wait until button up again
                            self.sequence_picking_armed = false;
                        }
                    } else {
                        self.sequence_picking_armed = false;
                    }
                }
            }

            if self.picking_window {
                let l_down = unsafe { GetAsyncKeyState(0x01) } as u16 & 0x8000 != 0;
                if !self.picking_window_armed {
                    if !l_down {
                        self.picking_window_armed = true;
                    }
                } else if l_down {
                    if let Some((x, y)) = hooks::get_cursor_pos() {
                        let hwnd = hooks::window_from_point(x, y);
                        if hwnd.0.is_null() {
                            self.status = "No window at cursor".to_string();
                            self.is_warning = true;
                        } else {
                            let root = hooks::ancestor_root(hwnd);
                            let title = hooks::get_window_title(root);
                            self.bg_hwnd = Some(SafeHwnd(root));
                            self.bg_title = title.clone();
                            self.status = format!("Window picked: {}", title);
                            self.is_warning = false;
                            self.persist_static_state();
                        }
                    }
                    self.picking_window = false;
                    self.picking_window_armed = false;
                }
            }
        }
    }

    pub fn on_leave(&mut self) {
        self.picking_location = false;
        self.picking_location_armed = false;
        self.picking_window = false;
        self.picking_window_armed = false;
        self.sequence_picking = false;
        self.sequence_picking_armed = false;
        // Close dialog so App resumes hotkeys when leaving this tab
        self.show_hk_settings = false;
        self.capturing_hk = false;
        if self.clicker.is_running() {
            self.clicker.stop_and_wait();
        }
    }

    /// Emergency stop for global triple-Esc panic (tab-independent).
    /// Stops clicker, cancels all picking modes, shows status feedback.
    pub fn emergency_stop(&mut self) {
        let was_picking = self.picking_location || self.picking_window || self.sequence_picking;
        self.picking_location = false;
        self.picking_location_armed = false;
        self.picking_window = false;
        self.picking_window_armed = false;
        self.sequence_picking = false;
        self.sequence_picking_armed = false;
        if self.clicker.is_running() {
            self.clicker.stop();
        }
        self.status = "⚠ Emergency stop triggered".to_string();
        self.is_warning = true;
        if was_picking {
            log::warn!("emergency stop: picking cancelled via triple Esc");
        } else {
            log::warn!("emergency stop: static clicker halted via triple Esc");
        }
    }

    fn is_hotkey_down(&self) -> bool {
        crate::hooks::is_hotkey_pressed(&self.hotkey)
    }

    pub fn handle_hotkey(&mut self) {
        if self.picking_location || self.picking_window || self.sequence_picking || self.show_hk_settings {
            return;
        }
        // Latch: while the hotkey is physically held, ignore further events
        // (kills stale queued WM_HOTKEY double-delivery → no stop→restart race)
        if self.hk_was_down {
            return;
        }
        self.hk_was_down = self.is_hotkey_down();

        if self.clicker.is_running() {
            // Atomic stop: join the thread NOW so is_running is accurate for the
            // next toggle (thread exits within ~1-2ms thanks to 1ms stop polling)
            self.clicker.stop_and_wait();
            self.clicks_done = self.shared_count.load(Ordering::Relaxed);
            self.status = format!("Stopped — {} clicks", self.clicks_done);
            self.is_warning = false;
        } else {
            self.start_clicker();
        }
    }

    fn poll_f6_hotkey(&mut self) {
        // Sync live count from clicker thread
        let live = self.shared_count.load(Ordering::Relaxed);
        if live != self.clicks_done {
            self.clicks_done = live;
        }

        // Release the hotkey latch once the key is physically up again
        if !self.is_hotkey_down() {
            self.hk_was_down = false;
        }
        // Poll clicker finished status to update status text
        if !self.clicker.is_running()
            && (self.status.starts_with("Running") || self.status.starts_with("Stopping"))
        {
            if let Some(err) = self.shared_error.lock().unwrap().take() {
                if err == "Completed" {
                    self.status = format!("Completed — {} clicks", self.clicks_done);
                    self.is_warning = false;
                } else if err == "Stopped" {
                    self.status = format!("Stopped — {} clicks", self.clicks_done);
                    self.is_warning = false;
                } else {
                    self.status = err;
                    self.is_warning = true;
                }
            } else {
                self.status = format!("Stopped — {} clicks", self.clicks_done);
                self.is_warning = false;
            }
        }
    }

    /// Whether the hotkey settings dialog is open (App suspends global hotkeys then).
    pub fn hotkey_dialog_open(&self) -> bool {
        self.show_hk_settings
    }
}

fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "Left",
        MouseButton::Right => "Right",
        MouseButton::Middle => "Middle",
        _ => "Left",
    }
}
