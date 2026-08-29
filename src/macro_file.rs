//! macro_file.rs — Save/load `.ttx` JSON macros.
//!
//! 100% safe Rust. Mirrors Core/MacroFileService.cs.

use std::path::Path;

use crate::model::MacroData;

pub const CURRENT_FORMAT_VERSION: i32 = 1;
pub const EXTENSION: &str = ".ttx";

#[derive(Debug)]
pub struct MalformedMacroFile(pub String);

impl std::fmt::Display for MalformedMacroFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for MalformedMacroFile {}

pub fn save(path: &Path, data: &MacroData) -> Result<(), MalformedMacroFile> {
    let json = serde_json::to_string_pretty(data)
        .map_err(|e| MalformedMacroFile(format!("serialize failed: {}", e)))?;
    std::fs::write(path, json).map_err(|e| MalformedMacroFile(format!("write {}: {}", path.display(), e)))
}

pub fn load(path: &Path) -> Result<MacroData, MalformedMacroFile> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| MalformedMacroFile(format!("read {}: {}", path.display(), e)))?;
    deserialize(&json)
}

pub fn deserialize(json: &str) -> Result<MacroData, MalformedMacroFile> {
    let mut data: MacroData = serde_json::from_str(json)
        .map_err(|e| MalformedMacroFile(format!("corrupt .ttx: {}", e)))?;

    // Allow version 0 (legacy) — sanitize will promote to 1. Reject only future versions.
    if data.format_version != CURRENT_FORMAT_VERSION && data.format_version != 0 {
        return Err(MalformedMacroFile(format!(
            "format v{} not supported (expected v{})",
            data.format_version, CURRENT_FORMAT_VERSION
        )));
    }

    data.sanitize();
    // Ensure version is current after sanitize
    data.format_version = CURRENT_FORMAT_VERSION;
    Ok(data)
}
