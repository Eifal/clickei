//! recorder.rs — Collects `CapturedEvent` into `Vec<MacroEvent>` with relative delays.
//!
//! 100% safe Rust — no `unsafe` here. The only `unsafe` lives in `hooks.rs` which
//! feeds this module via an `mpsc::Receiver`.
//!
//! Parity with Core/MacroRecorder.cs:
//! - Drops redundant `MouseMove` at same coords
//! - Drops auto-repeat `KeyDown` (held-key re-press without `KeyUp`)
//! - Folds elapsed time of dropped samples into the *next kept event's* `delay_ms`
//!   so the tape stays compact but timing stays correct.
//! - Suppresses the app's own hotkey combos: never records them and rolls back
//!   any modifier presses recorded just before the combo started (so no stuck
//!   modifiers in the tape).
//! - Timestamps via `Instant::now()` (monotonic) — deltas in ms, clamped.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::config::SettingsBindings;
use crate::hooks::CapturedEvent;
use crate::hooks::CapturedEvent as Capt;
use crate::model::{MacroData, MacroEvent, MacroEventType, MouseButton};

pub struct Recorder {
    events: Vec<MacroEvent>,
    held_keys: HashSet<i32>,
    last_pos: (i32, i32),
    has_last_pos: bool,
    last_tick: Instant,
    pending_delay: Duration, // accumulated time from dropped samples

    suppressed_combos: Vec<crate::config::HotkeyCombo>,
    suppressing: bool,
    suppressed_key: i32,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            held_keys: HashSet::new(),
            last_pos: (i32::MIN, i32::MIN),
            has_last_pos: false,
            last_tick: Instant::now(),
            pending_delay: Duration::ZERO,
            suppressed_combos: Vec::new(),
            suppressing: false,
            suppressed_key: 0,
        }
    }

    pub fn reset(&mut self, hotkeys: Option<&SettingsBindings>) {
        self.events.clear();
        self.held_keys.clear();
        self.has_last_pos = false;
        self.last_pos = (i32::MIN, i32::MIN);
        self.last_tick = Instant::now();
        self.pending_delay = Duration::ZERO;
        self.suppressing = false;
        self.suppressed_key = 0;
        self.suppressed_combos.clear();
        if let Some(hk) = hotkeys {
            for combo in [&hk.record, &hk.play, &hk.stop] {
                if combo.key != 0 {
                    self.suppressed_combos.push(combo.clone());
                }
            }
        }
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn events(&self) -> &[MacroEvent] {
        &self.events
    }

    pub fn into_macro_data(self) -> MacroData {
        MacroData {
            events: self.events,
            ..Default::default()
        }
    }

    pub fn take_events(&mut self) -> Vec<MacroEvent> {
        std::mem::take(&mut self.events)
    }

    /// Drain a `Receiver<CapturedEvent>` until `stop` returns true or channel is empty.
    /// Called periodically from the UI thread (egui) while recording.
    pub fn drain_receiver(
        &mut self,
        rx: &Receiver<CapturedEvent>,
        on_count: Option<&dyn Fn(usize)>,
    ) {
        while let Ok(ev) = rx.try_recv() {
            self.handle_captured(ev);
            if let Some(cb) = on_count {
                cb(self.events.len());
            }
        }
    }

    fn handle_captured(&mut self, ev: CapturedEvent) {
        match ev {
            Capt::MouseMove { x, y } => {
                if self.has_last_pos && self.last_pos == (x, y) {
                    // Dropped — fold time into next event.
                    self.accumulate_time();
                    return;
                }
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::MouseMove,
                    x,
                    y,
                    button: MouseButton::None,
                    key_code: 0,
                    scan_code: 0,
                    wheel_delta: 0,
                    delay_ms: delay,
                });
                self.last_pos = (x, y);
                self.has_last_pos = true;
            }
            Capt::MouseDown { button, x, y } => {
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::MouseDown,
                    x,
                    y,
                    button,
                    key_code: 0,
                    scan_code: 0,
                    wheel_delta: 0,
                    delay_ms: delay,
                });
                self.last_pos = (x, y);
                self.has_last_pos = true;
            }
            Capt::MouseUp { button, x, y } => {
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::MouseUp,
                    x,
                    y,
                    button,
                    key_code: 0,
                    scan_code: 0,
                    wheel_delta: 0,
                    delay_ms: delay,
                });
                self.last_pos = (x, y);
                self.has_last_pos = true;
            }
            Capt::MouseWheel { delta } => {
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::MouseWheel,
                    x: 0,
                    y: 0,
                    button: MouseButton::None,
                    key_code: 0,
                    scan_code: 0,
                    wheel_delta: delta,
                    delay_ms: delay,
                });
            }
            Capt::KeyDown { vk, scan_code } => {
                // Auto-repeat: only first press is kept.
                if !self.held_keys.insert(vk) {
                    self.accumulate_time();
                    return;
                }
                if self.handle_suppression(vk) {
                    self.held_keys.remove(&vk);
                    self.accumulate_time();
                    return;
                }
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::KeyDown,
                    x: 0,
                    y: 0,
                    button: MouseButton::None,
                    key_code: vk,
                    scan_code: scan_code as i32,
                    wheel_delta: 0,
                    delay_ms: delay,
                });
            }
            Capt::KeyUp { vk, scan_code } => {
                let was_held = self.held_keys.remove(&vk);
                if self.suppressing && vk == self.suppressed_key {
                    self.suppressing = false;
                    self.suppressed_key = 0;
                    self.accumulate_time();
                    return;
                }
                if self.suppressing {
                    self.accumulate_time();
                    return;
                }
                if !was_held {
                    self.accumulate_time();
                    return;
                }
                self.record(|delay| MacroEvent {
                    event_type: MacroEventType::KeyUp,
                    x: 0,
                    y: 0,
                    button: MouseButton::None,
                    key_code: vk,
                    scan_code: scan_code as i32,
                    wheel_delta: 0,
                    delay_ms: delay,
                });
            }
        }
    }

    fn accumulate_time(&mut self) {
        let now = Instant::now();
        let delta = now.duration_since(self.last_tick);
        self.pending_delay += delta;
        self.last_tick = now;
    }

    fn record<F>(&mut self, fill: F)
    where
        F: FnOnce(i32) -> MacroEvent,
    {
        if self.suppressing {
            self.accumulate_time();
            return;
        }
        let now = Instant::now();
        let delta = now.duration_since(self.last_tick) + self.pending_delay;
        self.pending_delay = Duration::ZERO;
        self.last_tick = now;

        // Guard against huge deltas (system sleep) — clamp to i32::MAX ms.
        let delay_ms = (delta.as_millis() as i64).clamp(0, i32::MAX as i64) as i32;
        let evt = fill(delay_ms);
        // First event should have ~0 delay (time since Start)
        if self.events.is_empty() {
            // Keep measured delay but it's typically small; C# did same (first delta ~0)
        }
        self.events.push(evt);
    }

    // ---- Hotkey suppression (mirrors C# MacroRecorder.HandleSuppression) ----

    fn handle_suppression(&mut self, vk: i32) -> bool {
        if self.suppressed_combos.is_empty() {
            return false;
        }
        for combo in self.suppressed_combos.clone() {
            if combo.key != vk {
                continue;
            }
            if !self.modifiers_held_match(&combo) {
                continue;
            }
            self.rollback_modifier_start(&combo);
            self.suppressing = true;
            self.suppressed_key = vk;
            log::debug!("suppressing hotkey combo {}", combo.display_owned());
            return true;
        }
        false
    }

    fn modifiers_held_match(&self, combo: &crate::config::HotkeyCombo) -> bool {
        use crate::config::mod_flag;
        let need = |mask: i32, keys: &[i32]| -> bool {
            if combo.modifiers & mask != 0 {
                keys.iter().any(|k| self.held_keys.contains(k))
            } else {
                keys.iter().all(|k| !self.held_keys.contains(k))
            }
        };
        need(mod_flag::CONTROL, &[0x11, 0xA2, 0xA3])
            && need(mod_flag::SHIFT, &[0x10, 0xA0, 0xA1])
            && need(mod_flag::ALT, &[0x12, 0xA4, 0xA5])
            && need(mod_flag::WIN, &[0x5B, 0x5C])
    }

    fn rollback_modifier_start(&mut self, combo: &crate::config::HotkeyCombo) {
        use crate::config::mod_flag;
        // Modifier VKs we track
        let mods = [0x11, 0xA2, 0xA3, 0x10, 0xA0, 0xA1, 0x12, 0xA4, 0xA5, 0x5B, 0x5C];
        let combo_has = |vk: i32| -> bool {
            match vk {
                0x11 | 0xA2 | 0xA3 => combo.modifiers & mod_flag::CONTROL != 0,
                0x10 | 0xA0 | 0xA1 => combo.modifiers & mod_flag::SHIFT != 0,
                0x12 | 0xA4 | 0xA5 => combo.modifiers & mod_flag::ALT != 0,
                _ => combo.modifiers & mod_flag::WIN != 0,
            }
        };

        // Find *last* contiguous modifier run that belongs to this combo.
        // Scanning backwards avoids truncating an earlier, unrelated Ctrl press.
        let mut cut: Option<usize> = None;
        for (i, evt) in self.events.iter().enumerate().rev() {
            if evt.event_type == MacroEventType::KeyDown
                && mods.contains(&evt.key_code)
                && combo_has(evt.key_code)
            {
                cut = Some(i);
            } else if cut.is_some() {
                // Stop once we leave the trailing modifier run.
                // Only contiguous trailing modifiers belonging to combo are considered;
                // an earlier non-modifier (e.g. mouse) breaks the run.
                break;
            }
        }
        // If the modifier run is not at the tail (i.e. there is a non-modifier after it),
        // don't truncate — it was not the hotkey's prefix.
        if let Some(cut_idx) = cut {
            let tail_is_all_combo_mods = self.events[cut_idx..]
                .iter()
                .all(|e| e.event_type == MacroEventType::KeyDown && mods.contains(&e.key_code) && combo_has(e.key_code));
            if tail_is_all_combo_mods {
                for i in cut_idx..self.events.len() {
                    let evt = &self.events[i];
                    if evt.event_type == MacroEventType::KeyDown {
                        self.held_keys.remove(&evt.key_code);
                    }
                }
                self.events.truncate(cut_idx);
            }
        }
    }
}
