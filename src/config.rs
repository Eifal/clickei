//! config.rs — AppConfig load/save to JSON at %APPDATA%\Clickei\config.json
//!
//! 100% safe Rust. Mirrors Core/ConfigService.cs + Core/HotkeyCombo.cs + HotkeyManager SettingsBindings.
//! Corrupt/missing files fall back to defaults so the app never fails to start.
//! Migration: if %APPDATA%\TinyTaskEnhanced\config.json exists and %APPDATA%\Clickei\config.json does not, copy once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// HotkeyCombo
// ---------------------------------------------------------------------------

/// Modifier bitflags — kept identical to Win32 MOD_* so `ToModifierFlags` is trivial.
/// Also mirrors C# `HotkeyCombo.Mod*` values.
pub mod mod_flag {
    pub const ALT: i32 = 0x0001;
    pub const CONTROL: i32 = 0x0002;
    pub const SHIFT: i32 = 0x0004;
    pub const WIN: i32 = 0x0008;
}

/// A global-hotkey binding: virtual key + modifier set.
/// Serialized as `{ "Modifiers": <int>, "Key": <int> }` for C# compat.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HotkeyCombo {
    #[serde(rename = "Modifiers", alias = "modifiers")]
    pub modifiers: i32,
    #[serde(rename = "Key", alias = "key")]
    pub key: i32,
}

impl HotkeyCombo {
    pub fn new(modifiers: i32, key: i32) -> Self {
        Self { modifiers, key }
    }

    pub fn none() -> Self {
        Self { modifiers: 0, key: 0 }
    }

    pub fn has_modifiers(&self) -> bool {
        self.modifiers != 0
    }

    /// Convert to the `fsModifiers` value for `RegisterHotKey` (adds `extra` bits like MOD_NOREPEAT).
    pub fn to_modifier_flags(&self, extra: u32) -> u32 {
        let mut flags = extra;
        if self.modifiers & mod_flag::CONTROL != 0 {
            flags |= 0x0002; // MOD_CONTROL
        }
        if self.modifiers & mod_flag::SHIFT != 0 {
            flags |= 0x0004;
        }
        if self.modifiers & mod_flag::ALT != 0 {
            flags |= 0x0001;
        }
        if self.modifiers & mod_flag::WIN != 0 {
            flags |= 0x0008;
        }
        flags
    }

    pub fn display(&self) -> String {
        self.display_owned()
    }

    /// Human key name — mirrors C# `HotkeyCombo.KeyName`.
    pub fn key_name(&self) -> String {
        let k = self.key;
        if k == 0 {
            return String::new();
        }
        if (b'A' as i32..=b'Z' as i32).contains(&k) {
            return (k as u8 as char).to_string();
        }
        if (b'0' as i32..=b'9' as i32).contains(&k) {
            return (k as u8 as char).to_string();
        }
        match k {
            0x1B => return "Esc".to_string(),
            0x20 => return "Space".to_string(),
            0x0D => return "Enter".to_string(),
            0x09 => return "Tab".to_string(),
            0x08 => return "Backspace".to_string(),
            0x2E => return "Del".to_string(),
            0x2D => return "Ins".to_string(),
            0x24 => return "Home".to_string(),
            0x23 => return "End".to_string(),
            0x21 => return "PgUp".to_string(),
            0x22 => return "PgDn".to_string(),
            0x25 => return "Left".to_string(),
            0x27 => return "Right".to_string(),
            0x26 => return "Up".to_string(),
            0x28 => return "Down".to_string(),
            _ => {}
        }
        if (0x70..=0x87).contains(&k) {
            return format!("F{}", k - 0x70 + 1);
        }
        if (0x60..=0x69).contains(&k) {
            return format!("Num{}", k - 0x60);
        }
        format!("0x{:02X}", k)
    }

    pub fn display_owned(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.modifiers & mod_flag::CONTROL != 0 {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers & mod_flag::ALT != 0 {
            parts.push("Alt".to_string());
        }
        if self.modifiers & mod_flag::SHIFT != 0 {
            parts.push("Shift".to_string());
        }
        if self.modifiers & mod_flag::WIN != 0 {
            parts.push("Win".to_string());
        }
        if self.key != 0 {
            parts.push(self.key_name());
        }
        if parts.is_empty() {
            "(none)".to_string()
        } else {
            parts.join("+")
        }
    }
}

// ---------------------------------------------------------------------------
// SettingsBindings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SettingsBindings {
    pub record: HotkeyCombo,
    pub play: HotkeyCombo,
    pub stop: HotkeyCombo,
    pub static_clicker: HotkeyCombo,
}

impl Default for SettingsBindings {
    fn default() -> Self {
        Self {
            record: HotkeyCombo::new(
                mod_flag::CONTROL | mod_flag::SHIFT | mod_flag::ALT,
                b'R' as i32,
            ),
            play: HotkeyCombo::new(
                mod_flag::CONTROL | mod_flag::SHIFT | mod_flag::ALT,
                b'P' as i32,
            ),
            stop: HotkeyCombo::new(
                mod_flag::CONTROL | mod_flag::SHIFT | mod_flag::ALT,
                b'S' as i32,
            ),
            static_clicker: HotkeyCombo::new(0, 0x75), // F6
        }
    }
}

impl SettingsBindings {
    pub const KEY_RECORD: &'static str = "Record";
    pub const KEY_PLAY: &'static str = "Play";
    pub const KEY_STOP: &'static str = "Stop";
    pub const KEY_STATIC: &'static str = "StaticClicker";

    pub fn from_map(map: Option<&HashMap<String, HotkeyCombo>>) -> Self {
        let mut b = Self::default();
        if let Some(m) = map {
            if let Some(v) = m.get(Self::KEY_RECORD) {
                b.record = v.clone();
            }
            if let Some(v) = m.get(Self::KEY_PLAY) {
                b.play = v.clone();
            }
            if let Some(v) = m.get(Self::KEY_STOP) {
                b.stop = v.clone();
            }
            if let Some(v) = m.get(Self::KEY_STATIC) {
                b.static_clicker = v.clone();
            }
        }
        b
    }

    pub fn to_map(&self) -> HashMap<String, HotkeyCombo> {
        let mut m = HashMap::new();
        m.insert(Self::KEY_RECORD.to_string(), self.record.clone());
        m.insert(Self::KEY_PLAY.to_string(), self.play.clone());
        m.insert(Self::KEY_STOP.to_string(), self.stop.clone());
        m.insert(Self::KEY_STATIC.to_string(), self.static_clicker.clone());
        m
    }
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub hotkeys: HashMap<String, HotkeyCombo>,

    #[serde(rename = "DefaultSpeed", alias = "defaultSpeed", default = "default_speed")]
    pub default_speed: f64,

    #[serde(rename = "LoopCount", alias = "loopCount", default = "default_loop_count")]
    pub loop_count: i32,

    #[serde(rename = "InfiniteLoop", alias = "infiniteLoop", default)]
    pub infinite_loop: bool,

    #[serde(rename = "IntervalMs", alias = "intervalMs", default)]
    pub interval_ms: i32,

    #[serde(rename = "AlwaysOnTop", alias = "alwaysOnTop", default)]
    pub always_on_top: bool,

    #[serde(rename = "LastFilePath", alias = "lastFilePath", default)]
    pub last_file_path: Option<String>,

    #[serde(default)]
    pub sequence_panel_collapsed: bool,

    #[serde(default = "default_panel_width", alias = "sequencePanelWidth")]
    pub sequence_panel_width: f32,

    #[serde(default)]
    pub sequence_panel_popped_out: bool,

    // --- Static Clicker functional settings (persist across restarts) ---
    #[serde(default = "default_static_interval")]
    pub static_clicker_interval_ms: u64,

    #[serde(default = "default_static_button")]
    pub static_clicker_button: crate::model::MouseButton,

    #[serde(default)]
    pub static_clicker_click_type: crate::model::ClickType,

    #[serde(default = "default_true")]
    pub static_clicker_repeat_until_stopped: bool,

    #[serde(default = "default_repeat_count")]
    pub static_clicker_repeat_count: i32,

    #[serde(default)]
    pub static_clicker_cursor_mode: crate::model::CursorMode,

    #[serde(default = "default_true")]
    pub static_clicker_foreground: bool,

    #[serde(default)]
    pub static_clicker_bg_title: String,

    #[serde(default)]
    pub static_clicker_sequence_targets: Vec<crate::model::SequenceTarget>,

    #[serde(default = "default_true")]
    pub static_clicker_sequence_enabled: bool,

    #[serde(default)]
    pub static_clicker_interval_jitter_ms: u32,

    #[serde(default)]
    pub static_clicker_position_jitter_px: u32,
}

fn default_speed() -> f64 {
    1.0
}
fn default_loop_count() -> i32 {
    1
}
fn default_panel_width() -> f32 {
    380.0
}
fn default_static_interval() -> u64 {
    100
}
fn default_static_button() -> crate::model::MouseButton {
    crate::model::MouseButton::Left
}
fn default_true() -> bool {
    true
}
fn default_repeat_count() -> i32 {
    1
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkeys: HashMap::new(),
            default_speed: 1.0,
            loop_count: 1,
            infinite_loop: false,
            interval_ms: 0,
            always_on_top: false,
            last_file_path: None,
            sequence_panel_collapsed: false,
            sequence_panel_width: default_panel_width(),
            sequence_panel_popped_out: false,
            static_clicker_interval_ms: default_static_interval(),
            static_clicker_button: default_static_button(),
            static_clicker_click_type: crate::model::ClickType::default(),
            static_clicker_repeat_until_stopped: default_true(),
            static_clicker_repeat_count: default_repeat_count(),
            static_clicker_cursor_mode: crate::model::CursorMode::default(),
            static_clicker_foreground: default_true(),
            static_clicker_bg_title: String::new(),
            static_clicker_sequence_targets: Vec::new(),
            static_clicker_sequence_enabled: default_true(),
            static_clicker_interval_jitter_ms: 0,
            static_clicker_position_jitter_px: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigService — load/save helpers
// ---------------------------------------------------------------------------

pub struct ConfigService;

// Shared in-memory instance — load sekali di awal, semua modifikasi lewat sini
static GLOBAL_CONFIG: OnceLock<Arc<Mutex<AppConfig>>> = OnceLock::new();

impl ConfigService {
    /// Single shared instance, load sekali di awal. Semua komponen harus
    /// modifikasi lewat `update_and_save` atau lock shared ini, bukan
    /// `load()`/`save()` independen, untuk menghindari lost update.
    pub fn shared() -> Arc<Mutex<AppConfig>> {
        GLOBAL_CONFIG
            .get_or_init(|| Arc::new(Mutex::new(Self::load())))
            .clone()
    }

    /// Atomically modify shared in-memory config and persist to file.
    /// Selalu tulis versi terbaru, tidak ada stale copy.
    pub fn update_and_save<F>(f: F) -> Result<(), String>
    where
        F: FnOnce(&mut AppConfig),
    {
        let shared = Self::shared();
        let to_save = {
            let mut cfg = shared.lock().unwrap();
            f(&mut *cfg);
            cfg.clone()
        };
        Self::save(&to_save)
    }

    fn new_config_path_raw() -> PathBuf {
        if let Some(mut dir) = dirs::config_dir() {
            dir.push("Clickei");
            dir.push("config.json");
            return dir;
        }
        let mut p = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        p.push("config.json");
        p
    }

    fn old_config_path_raw() -> PathBuf {
        if let Some(mut dir) = dirs::config_dir() {
            dir.push("TinyTaskEnhanced");
            dir.push("config.json");
            return dir;
        }
        let mut p = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        p.push("config.json");
        // For old fallback we used exe dir as well, but keep same as new fallback to avoid extra.
        // To distinguish, check also subfolder old name if config_dir is None.
        p
    }

    fn migrate_old_config_if_needed() {
        let new_path = Self::new_config_path_raw();
        let old_path = Self::old_config_path_raw();
        if new_path == old_path {
            return;
        }
        if new_path.exists() {
            return;
        }
        if !old_path.exists() {
            return;
        }
        // Old exists, new doesn't — copy once
        if let Some(parent) = new_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("migrate mkdir {}: {}", parent.display(), e);
                return;
            }
        }
        match std::fs::copy(&old_path, &new_path) {
            Ok(_) => log::info!("migrated config {} -> {}", old_path.display(), new_path.display()),
            Err(e) => log::warn!("migrate copy {} -> {} failed: {}", old_path.display(), new_path.display(), e),
        }
    }

    pub fn config_path() -> PathBuf {
        Self::migrate_old_config_if_needed();
        Self::new_config_path_raw()
    }

    pub fn app_data_dir() -> PathBuf {
        Self::config_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn load() -> AppConfig {
        Self::load_from(&Self::config_path())
    }

    pub fn load_from(path: &Path) -> AppConfig {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(json) => match serde_json::from_str::<AppConfig>(&json) {
                    Ok(cfg) => return cfg,
                    Err(e) => {
                        log::warn!("config deserialize failed ({}): {}", path.display(), e);
                    }
                },
                Err(e) => {
                    log::warn!("config read failed ({}): {}", path.display(), e);
                }
            }
        }
        AppConfig::default()
    }

    fn save(config: &AppConfig) -> Result<(), String> {
        let res = Self::save_to(config, &Self::config_path());
        if res.is_ok() {
            if let Some(shared) = GLOBAL_CONFIG.get() {
                if let Ok(mut g) = shared.try_lock() {
                    *g = config.clone();
                }
            }
        }
        res
    }

    fn save_to(config: &AppConfig, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
        }
        let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("write {}: {}", path.display(), e))?;
        Ok(())
    }
}
