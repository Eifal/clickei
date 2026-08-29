# Clickei

Rust rewrite of OP Auto Clicker — native macro recorder & auto clicker for Windows.

## Features

- **Recording** — global low-level hooks (WH_MOUSE_LL / WH_KEYBOARD_LL), SendInput playback with speed / loop / interval.
- **Static Clicker** — Foreground (SendInput) / Background (PostMessage) modes, Fixed / Current / Multi-Target sequence, interval & position jitter.
- **Hotkeys** — global RegisterHotKey (Record / Play / Stop / Static Clicker) + triple-Esc emergency panic (WH_KEYBOARD_LL).
- **UI** — single window, two tabs (Recording / Static Clicker) built with egui/eframe, dark theme, acrylic.

## Build & Run

```powershell
cargo build              # debug (console)
cargo build --release    # release (no console, ~3.9 MB)
cargo test
cargo check
```

Run: `target/debug/clickei.exe` or `target/release/clickei.exe`

Config: `%APPDATA%/Clickei/config.json` — hotkeys, last file path, static clicker settings.

## Requirements

- Windows 10/11
- Rust stable
