//! model.rs — Serde-serializable macro data structures.
//!
//! 100% safe Rust — no `unsafe` here. This is the shared vocabulary between
//! recorder, player, file persistence and UI. The JSON shape is intentionally
//! kept compatible with the C# `.ttx` files so existing macros keep working.
//!
//! Design notes (parity with Core/MacroEvent.cs):
//! - `DelayMs` is the interval *before* this event (time since previous kept event).
//!   The player does `sleep(delay / speed)` then replays the event.
//! - `X`/`Y` are absolute screen coordinates (virtual desktop space). Multi-monitor
//!   aware — InputSender normalises them to 0..65535 for SendInput.
//! - `WheelDelta` is typically ±120 per notch.
//! - `ScanCode` is kept for completeness (not used by SendInput VK path).

use serde::{Deserialize, Serialize};

/// Kind of input captured during recording / replayed during playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MacroEventType {
    MouseMove,
    MouseDown,
    MouseUp,
    MouseWheel,
    KeyDown,
    KeyUp,
}

/// Mouse button identifiers — mirrors C# `MouseButton` enum values (bitflags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum MouseButton {
    #[default]
    None = 0,
    Left = 1,
    Right = 2,
    Middle = 4,
    X1 = 8,
    X2 = 16,
}

/// Target for Sequence Clicking (Multi Target mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceTarget {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "default_sequence_clicks")]
    pub clicks: u32,
    #[serde(default = "default_sequence_interval")]
    pub interval_ms: u32,
}

fn default_sequence_clicks() -> u32 {
    1
}
fn default_sequence_interval() -> u32 {
    500
}

impl Default for SequenceTarget {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            clicks: 1,
            interval_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClickType {
    Single,
    Double,
}

impl Default for ClickType {
    fn default() -> Self {
        ClickType::Single
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorMode {
    Current,
    Fixed { x: i32, y: i32 },
    MultiTarget,
}

impl Default for CursorMode {
    fn default() -> Self {
        CursorMode::Current
    }
}

/// Snapshot lengkap untuk preset Static Clicker (semua field static_clicker_* di AppConfig).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaticClickerPreset {
    pub name: String,
    pub interval_ms: u32,
    pub interval_jitter_ms: u32,
    pub position_jitter_px: u32,
    pub button: MouseButton,
    pub click_type: ClickType,
    pub repeat_until_stopped: bool,
    pub repeat_count: u32,
    pub cursor_mode: CursorMode,
    pub foreground: bool,
    pub bg_title: String,
    pub sequence_targets: Vec<SequenceTarget>,
    pub sequence_enabled: bool,
}

/// One recorded input sample.
///
/// `DelayMs` is serialized as `delayMs` to match the C# property name? We keep
/// Rust snake_case but accept both via `alias` for forward/back compat. The C#
/// files use PascalCase (`DelayMs`, `Type`, etc.) because System.Text.Json
/// defaults to property names. To stay compatible we use `rename_all = "PascalCase"`
/// is NOT used — instead we match the JSON that C# actually wrote with
/// `JsonStringEnumConverter` (string enum) and PascalCase keys. Simplest:
/// accept both casings and write camel/Pascal? We choose to write exactly like
/// C# for interop: PascalCase keys. So we use `rename_all` aliases via custom?
///
/// Actually System.Text.Json with default options writes property names as declared
/// (PascalCase). Serde `rename_all = "PascalCase"` would replicate that. We add
/// aliases for lowercases so hand-edited files still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroEvent {
    #[serde(rename = "Type", alias = "type", alias = "Type")]
    pub event_type: MacroEventType,

    #[serde(rename = "X", alias = "x")]
    pub x: i32,
    #[serde(rename = "Y", alias = "y")]
    pub y: i32,

    #[serde(rename = "Button", alias = "button", default)]
    pub button: MouseButton,

    /// Virtual key code (only for KeyDown/KeyUp).
    #[serde(rename = "KeyCode", alias = "keyCode", alias = "key_code", default)]
    pub key_code: i32,

    /// Raw scan code captured by the low-level hook.
    #[serde(rename = "ScanCode", alias = "scanCode", alias = "scan_code", default)]
    pub scan_code: i32,

    /// Wheel delta (±120).
    #[serde(rename = "WheelDelta", alias = "wheelDelta", alias = "wheel_delta", default)]
    pub wheel_delta: i32,

    /// Milliseconds since the previous *kept* event.
    #[serde(rename = "DelayMs", alias = "delayMs", alias = "delay_ms", default)]
    pub delay_ms: i32,
}

impl MacroEvent {
    pub fn description(&self) -> String {
        match self.event_type {
            MacroEventType::MouseMove => format!("Move  ({}, {})", self.x, self.y),
            MacroEventType::MouseDown => format!("Down  {:?} ({}, {})", self.button, self.x, self.y),
            MacroEventType::MouseUp => format!("Up    {:?} ({}, {})", self.button, self.x, self.y),
            MacroEventType::MouseWheel => {
                let sign = if self.wheel_delta >= 0 { "+" } else { "" };
                format!("Wheel {}{}", sign, self.wheel_delta)
            }
            MacroEventType::KeyDown => format!("KeyDown  VK {} (0x{:02X})", self.key_code, self.key_code),
            MacroEventType::KeyUp => format!("KeyUp    VK {} (0x{:02X})", self.key_code, self.key_code),
        }
    }
}

/// Complete macro: event list plus baked-in playback settings (saved in `.ttx`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroData {
    #[serde(rename = "AppName", alias = "appName", default = "default_app_name")]
    pub app_name: String,

    #[serde(rename = "FormatVersion", alias = "formatVersion", default = "default_format_version")]
    pub format_version: i32,

    /// ISO-8601 UTC timestamp — serde handles string <-> DateTime via chrono normally,
    /// but to avoid extra deps we store as String. C# writes `DateTime.UtcNow` as ISO.
    #[serde(rename = "SavedAtUtc", alias = "savedAtUtc", default)]
    pub saved_at_utc: Option<String>,

    #[serde(rename = "Speed", alias = "speed", default = "default_speed")]
    pub speed: f64,

    #[serde(rename = "LoopCount", alias = "loopCount", default = "default_loop_count")]
    pub loop_count: i32,

    #[serde(rename = "InfiniteLoop", alias = "infiniteLoop", default)]
    pub infinite_loop: bool,

    #[serde(rename = "IntervalMs", alias = "intervalMs", default)]
    pub interval_ms: i32,

    #[serde(rename = "Events", alias = "events", default)]
    pub events: Vec<MacroEvent>,
}

fn default_app_name() -> String {
    "Clickei".to_string()
}
fn default_format_version() -> i32 {
    1
}
fn default_speed() -> f64 {
    1.0
}
fn default_loop_count() -> i32 {
    1
}

impl Default for MacroData {
    fn default() -> Self {
        Self {
            app_name: default_app_name(),
            format_version: default_format_version(),
            saved_at_utc: None,
            speed: 1.0,
            loop_count: 1,
            infinite_loop: false,
            interval_ms: 0,
            events: Vec::new(),
        }
    }
}

impl MacroData {
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Clamp playback settings to sane bounds (mirrors MacroFileService.Deserialize).
    pub fn sanitize(&mut self) {
        if !self.speed.is_finite() || self.speed == 0.0 {
            self.speed = 1.0;
        } else if self.speed < 0.25 || self.speed > 10.0 {
            self.speed = self.speed.clamp(0.25, 10.0);
        }
        self.loop_count = self.loop_count.clamp(1, 9999);
        if self.interval_ms < 0 {
            self.interval_ms = 0;
        }
        if self.format_version == 0 {
            self.format_version = 1;
        }
        // Clamp per-event delays to non-negative to avoid negative sleep
        for e in &mut self.events {
            if e.delay_ms < 0 {
                e.delay_ms = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_pascal_case() {
        let data = MacroData {
            events: vec![MacroEvent {
                event_type: MacroEventType::KeyDown,
                x: 0,
                y: 0,
                button: MouseButton::None,
                key_code: 0x41,
                scan_code: 0,
                wheel_delta: 0,
                delay_ms: 10,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&data).unwrap();
        // C# writes PascalCase keys
        assert!(json.contains("\"Type\""));
        assert!(json.contains("\"KeyCode\""));
        let back: MacroData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.events[0].key_code, 0x41);
    }

    #[test]
    fn sanitize_clamps() {
        let mut d = MacroData {
            speed: 99.0,
            loop_count: 99999,
            interval_ms: -5,
            ..Default::default()
        };
        d.sanitize();
        assert_eq!(d.speed, 10.0);
        assert_eq!(d.loop_count, 9999);
        assert_eq!(d.interval_ms, 0);
    }

    #[test]
    fn cursor_mode_serde_roundtrip() {
        // Externally tagged: unit variants -> string, Fixed -> {"Fixed":{"x":..,"y":..}}
        let cases = [
            CursorMode::Current,
            CursorMode::Fixed { x: 100, y: 200 },
            CursorMode::MultiTarget,
        ];
        for orig in cases {
            let json = serde_json::to_string(&orig).unwrap();
            match orig {
                CursorMode::Current => assert_eq!(json, "\"Current\""),
                CursorMode::MultiTarget => assert_eq!(json, "\"MultiTarget\""),
                CursorMode::Fixed { x, y } => {
                    assert!(json.contains("\"Fixed\""), "Fixed json: {}", json);
                    assert!(json.contains(&format!("\"x\":{x}")));
                    assert!(json.contains(&format!("\"y\":{y}")));
                }
            }
            let back: CursorMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, orig, "roundtrip failed for {:?} -> {}", orig, json);
        }
    }

    #[test]
    fn click_type_serde_roundtrip() {
        for orig in [ClickType::Single, ClickType::Double] {
            let json = serde_json::to_string(&orig).unwrap();
            let back: ClickType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, orig);
        }
    }

    #[test]
    fn appconfig_static_clicker_persist_roundtrip() {
        // Verify AppConfig with new static clicker fields round-trips and old json still loads via #[serde(default)]
        let mut cfg = crate::config::AppConfig::default();
        cfg.static_clicker_cursor_mode = CursorMode::Fixed { x: 123, y: 456 };
        cfg.static_clicker_interval_ms = 2500;
        cfg.static_clicker_button = MouseButton::Right;
        cfg.static_clicker_click_type = ClickType::Double;
        cfg.static_clicker_sequence_targets = vec![
            SequenceTarget { x: 10, y: 20, clicks: 2, interval_ms: 300 },
            SequenceTarget { x: 30, y: 40, clicks: 1, interval_ms: 500 },
        ];
        let json = serde_json::to_string(&cfg).unwrap();
        // Ensure new fields appear
        assert!(json.contains("static_clicker_cursor_mode"));
        assert!(json.contains("static_clicker_sequence_targets"));
        let back: crate::config::AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.static_clicker_cursor_mode, CursorMode::Fixed { x: 123, y: 456 });
        assert_eq!(back.static_clicker_interval_ms, 2500);
        assert_eq!(back.static_clicker_button, MouseButton::Right);
        assert_eq!(back.static_clicker_click_type, ClickType::Double);
        assert_eq!(back.static_clicker_sequence_targets.len(), 2);
        assert_eq!(back.static_clicker_sequence_targets[0].x, 10);

        // Old config without new fields must still parse (migration)
        let old_json = r#"{"DefaultSpeed":1.0,"LoopCount":1}"#;
        let old_cfg: crate::config::AppConfig = serde_json::from_str(old_json).unwrap();
        assert_eq!(old_cfg.static_clicker_cursor_mode, CursorMode::Current);
        assert_eq!(old_cfg.static_clicker_interval_ms, 100);
        assert_eq!(old_cfg.static_clicker_sequence_targets.len(), 0);
        assert!(old_cfg.static_clicker_foreground);
    }

    #[test]
    fn static_clicker_preset_serde_roundtrip() {
        let preset = StaticClickerPreset {
            name: "BossFarm".to_string(),
            interval_ms: 1500,
            interval_jitter_ms: 200,
            position_jitter_px: 5,
            button: MouseButton::Left,
            click_type: ClickType::Double,
            repeat_until_stopped: false,
            repeat_count: 10,
            cursor_mode: CursorMode::Fixed { x: 123, y: 456 },
            foreground: false,
            bg_title: "MyGame".to_string(),
            sequence_targets: vec![
                SequenceTarget { x: 10, y: 20, clicks: 2, interval_ms: 300 },
                SequenceTarget { x: 30, y: 40, clicks: 1, interval_ms: 500 },
            ],
            sequence_enabled: true,
        };
        let json = serde_json::to_string(&preset).unwrap();
        let back: StaticClickerPreset = serde_json::from_str(&json).unwrap();
        assert_eq!(back, preset);
        // Also verify cloning via AppConfig presets field roundtrip
        let mut cfg = crate::config::AppConfig::default();
        cfg.static_clicker_presets = vec![preset.clone()];
        let json2 = serde_json::to_string(&cfg).unwrap();
        assert!(json2.contains("static_clicker_presets"));
        let back2: crate::config::AppConfig = serde_json::from_str(&json2).unwrap();
        assert_eq!(back2.static_clicker_presets.len(), 1);
        assert_eq!(back2.static_clicker_presets[0], preset);
    }
}
