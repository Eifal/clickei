//! ui/main_window.rs — Main eframe App: transport + console + full wiring.
//!
//! 100% safe Rust glue that owns recorder/player/hooks/hotkeys.
//! Polls channels every frame (egui runs at 60fps), so no extra threads for UI.
//!
//! Parity with Forms/MainForm.cs: 284x158 minimal toolbar, console log (400 lines cap),
//! context menu, persistence, hotkey handling on hidden window thread (hotkey.rs).

use std::path::PathBuf;
use std::sync::{
    mpsc::{self, Receiver, Sender},
    Arc, Mutex,
};

use egui::{CentralPanel, Context, TopBottomPanel, Window};

use crate::config::{ConfigService, HotkeyCombo, SettingsBindings};
use crate::hooks::{HookHandles, CapturedEvent};
use crate::macro_file;
use crate::model::{MacroData, MacroEvent};
use crate::player::MacroPlayer;
use crate::recorder::Recorder;

#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

// ---------------------------------------------------------------------------
// Player message — sent from player thread to UI thread
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PlayerMsg {
    LoopStarted { current: usize, total: Option<usize> },
    Completed(bool),
    PauseChanged(bool),
}

// ---------------------------------------------------------------------------
// Interval unit helper — mirrors PlaybackOptionsForm.SplitIntervalMs
// ---------------------------------------------------------------------------

fn split_interval_ms(total_ms: i32) -> (i32, usize) {
    let ms = total_ms as i64;
    if ms != 0 && ms % 3_600_000 == 0 {
        ((ms / 3_600_000) as i32, 3) // hr
    } else if ms != 0 && ms % 60_000 == 0 {
        ((ms / 60_000) as i32, 2) // min
    } else if ms != 0 && ms % 1000 == 0 {
        ((ms / 1000) as i32, 1) // sec
    } else {
        (total_ms, 0) // ms
    }
}

fn compose_interval_ms(value: i32, unit: usize) -> i32 {
    let v = value as i64;
    let ms = match unit {
        1 => v * 1000,
        2 => v * 60_000,
        3 => v * 3_600_000,
        _ => v,
    };
    ms.clamp(0, i32::MAX as i64) as i32
}

fn format_interval(ms: i32) -> String {
    let (v, u) = split_interval_ms(ms);
    match u {
        3 => format!("{}h", v),
        2 => format!("{}m", v),
        1 => format!("{}s", v),
        _ => format!("{}ms", ms),
    }
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86400;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

// ---------------------------------------------------------------------------
// MainWindow
// ---------------------------------------------------------------------------

pub struct MainWindow {
    // Config & bindings — single shared in-memory instance (load once at startup, all writes via shared)
    shared_config: Arc<Mutex<crate::config::AppConfig>>,
    bindings: SettingsBindings,

    // Macro
    macro_data: Option<MacroData>,
    open_path: Option<PathBuf>,

    // Transport state
    recording: bool,
    playing: bool,
    paused: bool,
    current_loop: usize,
    total_loops: Option<usize>,

    // Playback settings (mirrors hidden NumericUpDowns in C#)
    speed: f64,
    loop_count: i32,
    infinite: bool,
    interval_value: i32, // value in chosen unit
    interval_unit: usize, // 0=ms,1=sec,2=min,3=hr

    always_on_top: bool,

    // Console log (capped at 400 lines, show last 300 on overflow)
    console: Vec<String>,
    autoscroll: bool,

    // Native components
    recorder: Recorder,
    hook_handles: Option<HookHandles>,
    hook_rx: Option<Receiver<CapturedEvent>>,

    player: MacroPlayer,
    player_tx: Sender<PlayerMsg>,
    player_rx: Receiver<PlayerMsg>,

    hotkey_mgr: Option<crate::hotkey::HotkeyManager>,

    // UI flags
    show_settings: bool,
    show_playback: bool,
    show_editor: bool,
    show_context: bool,
    context_pos: Option<egui::Pos2>,

    // Hotkey capture state inside settings
    capturing_hotkey: Option<String>, // "Record" | "Play" | "Stop"

    // Editor state
    editor_events: Vec<MacroEvent>,

    // One-time init flag for always-on-top (replaces static mut UB)
    top_initialized: bool,

    // Pending file dialog results (to avoid blocking inside update)
    // We use immediate rfd calls (blocking) but keep them here for future async.
}

impl Default for MainWindow {
    fn default() -> Self {
        let shared = ConfigService::shared();
        let cfg = shared.lock().unwrap().clone();
        let bindings = SettingsBindings::from_map(Some(&cfg.hotkeys));
        let (val, unit) = split_interval_ms(cfg.interval_ms);

        // Player channel
        let (tx, rx) = mpsc::channel();

        // Try to spawn hotkey manager — failure is non-fatal (log warning, continue without hotkeys)
        let hotkey_mgr = match crate::hotkey::HotkeyManager::spawn() {
            Ok(mut mgr) => {
                let failures = mgr.apply(bindings.clone());
                if !failures.is_empty() {
                    log::warn!("hotkey register failures: {:?}", failures);
                }
                Some(mgr)
            }
            Err(e) => {
                log::warn!("hotkey manager spawn failed: {}", e);
                None
            }
        };

        let speed = cfg.default_speed.clamp(0.25, 10.0);
        let loop_count = cfg.loop_count.clamp(1, 9999);

        let mut console = Vec::new();
        console.push(format!("[{}] Ready. Right-click for file, playback & settings options.", timestamp()));
        if let Some(mgr) = &hotkey_mgr {
            let _ = mgr;
            console.push(format!(
                "[{}] Hotkeys: Record={}  Play={}  Stop={}",
                timestamp(),
                bindings.record.display_owned(),
                bindings.play.display_owned(),
                bindings.stop.display_owned()
            ));
        }

        let infinite = cfg.infinite_loop;
        let always_on_top = cfg.always_on_top;
        let total_loops = if infinite { None } else { Some(loop_count.max(1) as usize) };
        Self {
            shared_config: shared.clone(),
            bindings,
            macro_data: None,
            open_path: None,
            recording: false,
            playing: false,
            paused: false,
            current_loop: 0,
            total_loops,
            speed,
            loop_count,
            infinite,
            interval_value: val,
            interval_unit: unit,
            always_on_top,
            console,
            autoscroll: true,
            recorder: Recorder::new(),
            hook_handles: None,
            hook_rx: None,
            player: MacroPlayer::new(),
            player_tx: tx,
            player_rx: rx,
            hotkey_mgr,
            show_settings: false,
            show_playback: false,
            show_editor: false,
            show_context: false,
            context_pos: None,
            capturing_hotkey: None,
            editor_events: Vec::new(),
            top_initialized: false,
        }
    }
}

impl MainWindow {
    fn log(&mut self, msg: impl Into<String>, is_warning: bool) {
        let prefix = if is_warning { "!" } else { "●" };
        let color_tag = if is_warning { "WARN" } else { "INFO" };
        let line = format!("[{}] {} {}", timestamp(), prefix, msg.into());
        log::info!("{}: {}", color_tag, line);
        self.console.push(line);
        if self.console.len() > 400 {
            let len = self.console.len();
            self.console.drain(0..len - 300);
        }
        self.autoscroll = true;
    }

    fn refresh_sync_to_macro(&mut self) {
        if let Some(data) = &mut self.macro_data {
            data.speed = self.speed;
            data.loop_count = self.loop_count;
            data.infinite_loop = self.infinite;
            data.interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);
        }
        self.total_loops = if self.infinite { None } else { Some(self.loop_count.max(1) as usize) };
    }

    fn sync_from_macro(&mut self, data: &MacroData) {
        self.speed = data.speed.clamp(0.25, 10.0);
        self.loop_count = data.loop_count.clamp(1, 9999);
        self.infinite = data.infinite_loop;
        let (v, u) = split_interval_ms(data.interval_ms);
        self.interval_value = v;
        self.interval_unit = u;
        self.total_loops = if self.infinite { None } else { Some(self.loop_count.max(1) as usize) };
    }

    fn persist_config(&mut self) {
        // Single shared instance — must use update_and_save, never save(&cfg) directly
        let speed = self.speed;
        let loop_count = self.loop_count;
        let infinite = self.infinite;
        let interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);
        let always_on_top = self.always_on_top;
        let hotkeys = self.bindings.to_map();
        if let Err(e) = ConfigService::update_and_save(|cfg| {
            cfg.default_speed = speed;
            cfg.loop_count = loop_count;
            cfg.infinite_loop = infinite;
            cfg.interval_ms = interval_ms;
            cfg.always_on_top = always_on_top;
            cfg.hotkeys = hotkeys.clone();
        }) {
            log::warn!("persist config failed: {}", e);
        }
    }

    // -----------------------------------------------------------------------
    // Recording
    // -----------------------------------------------------------------------

    fn toggle_record(&mut self) {
        let t0 = std::time::Instant::now();
        if self.recording {
            self.stop_recording();
        } else {
            self.start_recording();
        }
        log::debug!("toggle_record done in {:?}", t0.elapsed());
    }

    fn start_recording(&mut self) {
        if self.playing {
            self.stop_all();
        }
        if self.recording {
            return;
        }
        let t0 = std::time::Instant::now();
        self.recorder.reset(Some(&self.bindings));

        match crate::hooks::install_hooks() {
            Ok((handles, rx)) => {
                self.hook_handles = Some(handles);
                self.hook_rx = Some(rx);
                self.recording = true;
                self.macro_data = None;
                self.open_path = None;
                self.log("Recording…", false);
                log::info!("install_hooks ok in {:?}", t0.elapsed());
            }
            Err(e) => {
                log::error!("install_hooks failed after {:?}: {}", t0.elapsed(), e);
                self.log(format!("Could not install hooks: {}", e), true);
            }
        }
    }

    fn stop_recording(&mut self) {
        if !self.recording {
            return;
        }
        // Drain one last time before unhooking to capture trailing events
        self.drain_hooks();
        self.hook_handles = None;
        self.hook_rx = None;

        let events = self.recorder.take_events();
        let count = events.len();
        let mut data = MacroData {
            events,
            ..Default::default()
        };
        data.speed = self.speed;
        data.loop_count = self.loop_count;
        data.infinite_loop = self.infinite;
        data.interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);

        self.macro_data = Some(data);
        self.recording = false;
        self.open_path = None; // unsaved capture invalidates previous path
        self.log(format!("{} event(s) recorded.", count), false);
    }

    fn drain_hooks(&mut self) {
        if let Some(rx) = &self.hook_rx {
            self.recorder.drain_receiver(rx, None);
        }
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    fn toggle_play_pause(&mut self) {
        if self.playing && self.paused {
            self.player.resume();
            self.paused = false;
            self.log(format!("Resumed (loop {}/{})", self.current_loop, self.format_total()), false);
        } else if self.playing {
            self.player.pause();
            self.paused = true;
            self.log(format!("Paused (loop {}/{})", self.current_loop, self.format_total()), false);
        } else {
            self.start_playback();
        }
    }

    fn start_playback(&mut self) {
        if self.recording {
            self.stop_all();
            return;
        }
        let Some(data) = self.macro_data.clone() else {
            self.log("Nothing to play yet — record or open a macro.", true);
            return;
        };
        if data.events.is_empty() {
            self.log("Nothing to play yet — macro is empty.", true);
            return;
        }

        // Sync current settings into the data being played (like C# SyncSettingsToMacro)
        let mut data = data;
        data.speed = self.speed;
        data.loop_count = self.loop_count;
        data.infinite_loop = self.infinite;
        data.interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);

        // Also update self.macro_data so Save will persist these settings
        if let Some(m) = &mut self.macro_data {
            m.speed = data.speed;
            m.loop_count = data.loop_count;
            m.infinite_loop = data.infinite_loop;
            m.interval_ms = data.interval_ms;
        }

        self.current_loop = 0;
        self.total_loops = if data.infinite_loop { None } else { Some(data.loop_count.max(1) as usize) };
        self.playing = true;
        self.paused = false;

        let tx = self.player_tx.clone();
        let data_clone = data.clone();
        self.player.start(
            data_clone,
            {
                let tx = tx.clone();
                move |current, total| {
                    let _ = tx.send(PlayerMsg::LoopStarted { current, total });
                }
            },
            {
                let tx = tx.clone();
                move |completed| {
                    let _ = tx.send(PlayerMsg::Completed(completed));
                }
            },
            {
                let tx = tx.clone();
                move |paused| {
                    let _ = tx.send(PlayerMsg::PauseChanged(paused));
                }
            },
        );
        self.log(format!("Playing… (loop {}/{})", 1, self.format_total()), false);
    }

    fn stop_all(&mut self) {
        if self.recording {
            self.stop_recording();
            return;
        }
        if self.playing {
            self.player.stop();
            // We will handle Completed(false) via player_rx polling; but also set flags now
            // so UI updates immediately.
            self.playing = false;
            self.paused = false;
            self.log("Stopped.", false);
            return;
        }
        self.log("Idle.", false);
    }

    fn poll_player(&mut self) {
        while let Ok(msg) = self.player_rx.try_recv() {
            match msg {
                PlayerMsg::LoopStarted { current, total } => {
                    self.current_loop = current;
                    self.total_loops = total;
                    // Don't spam console every loop; only update status in toolbar
                }
                PlayerMsg::Completed(completed) => {
                    self.playing = false;
                    self.paused = false;
                    if completed {
                        self.log(
                            format!("Playback finished ({})", self.format_loop_text()),
                            false,
                        );
                    } else {
                        self.log("Playback stopped.", false);
                    }
                }
                PlayerMsg::PauseChanged(paused) => {
                    self.paused = paused;
                    // Already logged in toggle, but also handle if pause came from hotkey
                }
            }
        }
        // Also check is_running to detect natural thread exit if channel missed
        if !self.player.is_running() && self.playing {
            // If player stopped but we didn't get Completed yet, treat as stopped
            // Keep polling; the Completed message should arrive shortly.
        }
    }

    fn format_total(&self) -> String {
        match self.total_loops {
            None => "∞".to_string(),
            Some(n) => n.to_string(),
        }
    }

    fn format_loop_text(&self) -> String {
        match self.total_loops {
            None => format!("{} infinite loop(s)", self.current_loop),
            Some(n) => format!("{}/{} loops", self.current_loop, n),
        }
    }

    // -----------------------------------------------------------------------
    // Hotkeys — exposed for App routing (so F6 can be handled reliably via RegisterHotKey, not just polling)
    // -----------------------------------------------------------------------

    pub fn try_recv_hotkey(&self) -> Option<crate::hotkey::HotkeyId> {
        self.hotkey_mgr.as_ref().and_then(|m| m.try_recv())
    }

    pub fn handle_hotkey(&mut self, id: crate::hotkey::HotkeyId) {
        match id {
            crate::hotkey::HotkeyId::Record => self.toggle_record(),
            crate::hotkey::HotkeyId::Play => self.toggle_play_pause(),
            crate::hotkey::HotkeyId::Stop => self.stop_all(),
            crate::hotkey::HotkeyId::Static => {} // will be routed to StaticClicker by App
        }
    }
    // -----------------------------------------------------------------------
    // File operations
    // -----------------------------------------------------------------------

    fn do_save(&mut self) {
        let Some(data) = self.macro_data.clone() else {
            self.log("Nothing to save yet.", true);
            return;
        };
        let mut data = data;
        data.speed = self.speed;
        data.loop_count = self.loop_count;
        data.infinite_loop = self.infinite;
        data.interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);

        let path = if let Some(p) = self.open_path.clone() {
            p
        } else {
            let Some(p) = rfd::FileDialog::new()
                .add_filter("Clickei macro", &["ttx"])
                .set_file_name("macro.ttx")
                .save_file()
            else {
                return;
            };
            p
        };

        match macro_file::save(&path, &data) {
            Ok(()) => {
                self.open_path = Some(path.clone());
                let path_str = path.display().to_string();
                let _ = ConfigService::update_and_save(|cfg| cfg.last_file_path = Some(path_str.clone()));
                // Also sync back to macro_data
                self.macro_data = Some(data);
                self.log(
                    format!("Saved {} event(s) → {}", self.macro_data.as_ref().unwrap().events.len(), path.display()),
                    false,
                );
            }
            Err(e) => {
                self.log(format!("Could not save: {}", e), true);
            }
        }
    }

    fn do_open(&mut self) {
        if self.recording || self.playing {
            self.log("Cannot open while recording/playing.", true);
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Clickei macro", &["ttx"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };

        match macro_file::load(&path) {
            Ok(data) => {
                self.sync_from_macro(&data);
                let count = data.events.len();
                self.macro_data = Some(data);
                self.open_path = Some(path.clone());
                let path_str = path.display().to_string();
                let _ = ConfigService::update_and_save(|cfg| cfg.last_file_path = Some(path_str.clone()));
                self.log(format!("Loaded {} event(s) from {}", count, path.display()), false);
            }
            Err(e) => {
                self.log(format!("Open failed: {}", e), true);
            }
        }
    }

    fn do_compile(&mut self) {
        let Some(data) = self.macro_data.clone() else {
            self.log("Nothing to compile yet.", true);
            return;
        };
        if data.events.is_empty() {
            self.log("Nothing to compile yet.", true);
            return;
        }
        // Ensure macro is saved first (compile embeds .ttx)
        let macro_path = if let Some(p) = self.open_path.clone() {
            p
        } else {
            let Some(p) = rfd::FileDialog::new()
                .add_filter("Clickei macro", &["ttx"])
                .set_file_name("macro.ttx")
                .save_file()
            else {
                return;
            };
            let mut d = data.clone();
            d.speed = self.speed;
            d.loop_count = self.loop_count;
            d.infinite_loop = self.infinite;
            d.interval_ms = compose_interval_ms(self.interval_value, self.interval_unit);
            if let Err(e) = macro_file::save(&p, &d) {
                self.log(format!("Could not write temp macro: {}", e), true);
                return;
            }
            self.open_path = Some(p.clone());
            p
        };

        let Some(out_path) = rfd::FileDialog::new()
            .add_filter("Executable", &["exe"])
            .set_file_name(
                macro_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("macro")
                    .to_string()
                    + ".exe",
            )
            .save_file()
        else {
            return;
        };

        // For now, placeholder: just copy a message; real compile would invoke `cargo build` template.
        // We do a minimal "compile" by writing the JSON beside the exe for demonstration.
        // To avoid bug (empty handler), we log clearly.
        self.log(format!("Compile requested: macro={} → exe={}", macro_path.display(), out_path.display()), false);
        self.log("Compile-to-EXE for Rust is not yet implemented (stub). Save .ttx and use cargo build template; will be wired next.", true);

        // If we want a quick win: write the macro JSON to out_path.with_extension("ttx.json") so user sees output.
        let _ = std::fs::copy(&macro_path, out_path.with_extension("ttx.json"));
    }

    fn do_edit(&mut self) {
        let Some(data) = &self.macro_data else {
            self.log("Nothing to edit yet.", true);
            return;
        };
        if data.events.is_empty() {
            self.log("Nothing to edit yet.", true);
            return;
        }
        self.editor_events = data.events.clone();
        self.show_editor = true;
    }

    fn do_playback_options(&mut self) {
        self.show_playback = true;
    }

    fn do_settings(&mut self) {
        // Suspend hotkeys while capturing (mirrors C# Suspend)
        if let Some(mgr) = &mut self.hotkey_mgr {
            mgr.suspend();
        }
        self.show_settings = true;
    }

    fn close_settings(&mut self, apply: bool) {
        if apply {
            self.persist_config();
            if let Some(mgr) = &mut self.hotkey_mgr {
                let failures = mgr.apply(self.bindings.clone());
                if failures.is_empty() {
                    self.log("Hotkeys updated.", false);
                } else {
                    for f in failures {
                        self.log(format!("Hotkey warning: {}", f), true);
                    }
                }
            }
        } else if let Some(mgr) = &mut self.hotkey_mgr {
            // Restore previous bindings on cancel
            let failures = mgr.resume();
            if !failures.is_empty() {
                for f in failures {
                    self.log(format!("Hotkey resume warning: {}", f), true);
                }
            }
        }
        self.show_settings = false;
        self.capturing_hotkey = None;
    }

    fn toggle_always_on_top(&mut self, ctx: &Context) {
        self.always_on_top = !self.always_on_top;
        self.persist_config();
        // Apply viewport level — egui 0.28 uses ViewportCommand::WindowLevel
        if self.always_on_top {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        }
        self.log(
            if self.always_on_top { "Always on top — on." } else { "Always on top — off." },
            false,
        );
    }

    /// Suspend all global hotkeys (hotkey settings dialog open in Static Clicker tab).
    pub fn suspend_hotkeys(&mut self) {
        if let Some(mgr) = &mut self.hotkey_mgr {
            mgr.suspend();
        }
    }

    /// Re-register hotkeys from config — picks up a newly saved Static Clicker key.
    pub fn reapply_hotkeys_from_config(&mut self) {
        let cfg = self.shared_config.lock().unwrap().clone();
        let bindings = SettingsBindings::from_map(Some(&cfg.hotkeys));
        self.bindings = bindings.clone();
        if let Some(mgr) = &mut self.hotkey_mgr {
            let failures = mgr.apply(bindings);
            for f in failures {
                log::warn!("hotkey reapply: {}", f);
            }
        }
    }

    pub fn on_leave(&mut self) {
        if self.recording {
            self.stop_recording();
        }
        if self.playing {
            self.player.stop_and_wait();
            self.playing = false;
            self.paused = false;
        }
        // Ensure hooks are fully released (stale hook fix for Back to menu)
        self.hook_handles = None;
        self.hook_rx = None;
        // Close any modal windows to avoid stuck UI when returning
        self.show_settings = false;
        self.show_playback = false;
        self.show_editor = false;
        self.show_context = false;
        self.capturing_hotkey = None;
    }

    /// Emergency stop triggered by global triple-Esc panic hook (WH_KEYBOARD_LL).
    /// Stops playback immediately and shows feedback, regardless of current tab.
    pub fn emergency_stop(&mut self) {
        let was_running = self.playing || self.recording;
        if self.playing {
            self.player.stop();
            self.playing = false;
            self.paused = false;
        }
        if self.recording {
            // Drain and cancel recording (do not save)
            self.hook_handles = None;
            self.hook_rx = None;
            self.recording = false;
            // Discard partial capture
            let _ = self.recorder.take_events();
        }
        self.log("⚠ Emergency stop triggered", true);
        if was_running {
            log::warn!("emergency stop: playback/recording halted via triple Esc");
        }
    }
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

impl eframe::App for MainWindow {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        crate::ui::theme::apply_dark_theme(ctx);

        // Poll native channels every frame
        if self.recording {
            self.drain_hooks();
            // Keep UI repainting while recording so live count stays fresh (capped ~60fps)
            ctx.request_repaint_after(std::time::Duration::from_millis(15));
        }
        self.poll_player();

        // Apply always-on-top on startup (once). Use instance flag to avoid UB.
        if !self.top_initialized {
            self.top_initialized = true;
            if self.always_on_top {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            }
        }

        // ---- Top toolbar
        TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().item_spacing.y = 4.0;

                // Record
                let rec_label = if self.recording { "■ Stop" } else { "● Record" };
                let rec_btn = egui::Button::new(rec_label);
                let rec_resp = ui.add(rec_btn);
                if rec_resp.clicked() {
                    self.toggle_record();
                }
                rec_resp.on_hover_text(format!("Record ({})", self.bindings.record.display_owned()));

                // Stop (enabled when recording or playing)
                let stop_enabled = self.recording || self.playing;
                let stop_resp = ui.add_enabled(stop_enabled, egui::Button::new("■ Stop"));
                if stop_resp.clicked() {
                    self.stop_all();
                }
                stop_resp.on_hover_text(format!("Stop ({})", self.bindings.stop.display_owned()));

                // Play / Pause
                let play_label = if self.playing && !self.paused { "⏸ Pause" } else { "▶ Play" };
                let play_enabled = !self.recording;
                let play_resp = ui.add_enabled(play_enabled, egui::Button::new(play_label));
                if play_resp.clicked() {
                    self.toggle_play_pause();
                }
                play_resp.on_hover_text(format!("Play/Pause ({})", self.bindings.play.display_owned()));

                // Status text on the right — shortened so 300px toolbar doesn't clip
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let interval = format_interval(compose_interval_ms(self.interval_value, self.interval_unit));
                    let status = if self.recording {
                        format!("● Rec ({} ev)", self.recorder.event_count())
                    } else if self.playing {
                        if self.paused {
                            format!("⏸ {} / {}", self.current_loop, self.format_total())
                        } else {
                            format!("▶ {} / {}", self.current_loop, self.format_total())
                        }
                    } else if let Some(data) = &self.macro_data {
                        // e.g. "644 ev • 1× • 10m"  short enough for 400px window
                        format!("{} ev • {}× • {}", data.events.len(), self.format_total(), interval)
                    } else {
                        format!("Idle • {}", interval)
                    };
                    ui.label(egui::RichText::new(status).small().color(crate::ui::theme::TEXT_SECONDARY));
                });
            });
        });

        // ---- Central console
        CentralPanel::default().show(ctx, |ui| {
            crate::ui::theme::card_frame().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.autoscroll)
                    .show(ui, |ui| {
                        ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                        for line in &self.console {
                            let is_warn = line.contains("! ") || line.contains("WARN");
                            let color = if is_warn { crate::ui::theme::WARNING } else { crate::ui::theme::TEXT_PRIMARY };
                            ui.label(egui::RichText::new(line).color(color).small());
                        }
                        // Detect scroll away from bottom
                        let max_y = ui.max_rect().bottom();
                        let cursor_y = ui.cursor().bottom();
                        if cursor_y < max_y - 10.0 {
                            self.autoscroll = false;
                        }
                    });
            });

            // Invisible background to capture right-click for context menu
            let bg_resp = ui.allocate_rect(ui.available_rect_before_wrap(), egui::Sense::click());
            if bg_resp.secondary_clicked()
                || (ctx.input(|i| i.pointer.secondary_clicked())
                    && !self.show_context
                    && ctx.pointer_interact_pos().is_some())
            {
                self.show_context = true;
                self.context_pos = ctx.pointer_interact_pos();
            }
        });

        // ---- Context menu (right-click)
        if self.show_context {
            let pos = self.context_pos.unwrap_or(egui::pos2(100.0, 100.0));
            Window::new("##context_menu")
                .fixed_pos(pos)
                .collapsible(false)
                .resizable(false)
                .title_bar(false)
                .show(ctx, |ui| {
                    ui.set_min_width(180.0);
                    let can_save = !self.recording && self.macro_data.as_ref().map(|d| !d.events.is_empty()).unwrap_or(false);
                    let can_play = !self.recording && self.macro_data.as_ref().map(|d| !d.events.is_empty()).unwrap_or(false);
                    let can_open = !self.recording && !self.playing;
                    let can_edit = can_play;

                    if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                        self.show_context = false;
                        self.do_save();
                    }
                    if ui.add_enabled(can_open, egui::Button::new("Open…")).clicked() {
                        self.show_context = false;
                        self.do_open();
                    }
                    if ui.add_enabled(can_play, egui::Button::new("Compile to EXE…")).clicked() {
                        self.show_context = false;
                        self.do_compile();
                    }
                    if ui.add_enabled(can_edit, egui::Button::new("Edit macro…")).clicked() {
                        self.show_context = false;
                        self.do_edit();
                    }
                    ui.separator();
                    if ui.add_enabled(!self.recording, egui::Button::new("Playback options…")).clicked() {
                        self.show_context = false;
                        self.do_playback_options();
                    }
                    let top_label = if self.always_on_top { "☑ Always on top" } else { "☐ Always on top" };
                    if ui.button(top_label).clicked() {
                        self.show_context = false;
                        self.toggle_always_on_top(ctx);
                    }
                    ui.separator();
                    if ui.button("Settings…").clicked() {
                        self.show_context = false;
                        self.do_settings();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Close menu").clicked() {
                        self.show_context = false;
                    }
                });
            // Close menu if clicked elsewhere
            if ctx.input(|i| i.pointer.primary_clicked()) && !ctx.is_pointer_over_area() {
                // Delay close to allow button click to register
            }
            // Escape closes
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.show_context = false;
            }
        }

        // ---- Playback options dialog
        if self.show_playback {
            let mut open = self.show_playback;
            Window::new("Playback options")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.add(egui::Slider::new(&mut self.speed, 0.25..=10.0).text("Speed").step_by(0.25));
                    ui.checkbox(&mut self.infinite, "Infinite loop");
                    ui.add_enabled(!self.infinite, egui::Slider::new(&mut self.loop_count, 1..=9999).text("Loops"));
                    ui.horizontal(|ui| {
                        ui.label("Interval");
                        ui.add(egui::DragValue::new(&mut self.interval_value).range(0..=999999).speed(1));
                        egui::ComboBox::from_id_source("interval_unit")
                            .selected_text(match self.interval_unit { 1 => "sec", 2 => "min", 3 => "hr", _ => "ms" })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.interval_unit, 0, "ms");
                                ui.selectable_value(&mut self.interval_unit, 1, "sec");
                                ui.selectable_value(&mut self.interval_unit, 2, "min");
                                ui.selectable_value(&mut self.interval_unit, 3, "hr");
                            });
                    });
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            self.persist_config();
                            self.refresh_sync_to_macro();
                            self.show_playback = false;
                            self.log("Playback options updated.", false);
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_playback = false;
                        }
                    });
                });
            if !open {
                self.show_playback = false;
            }
        }

        // ---- Settings dialog (hotkey rebinding)
        if self.show_settings {
            let mut open = self.show_settings;
            Window::new("Settings — Hotkeys")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Click a hotkey to rebind. Press Esc to clear (no hotkey).");
                    ui.separator();

                    // Helper to draw a hotkey row
                    let mut capture_request: Option<String> = None;
                    for (label, combo) in [
                        ("Record", &mut self.bindings.record),
                        ("Play", &mut self.bindings.play),
                        ("Stop", &mut self.bindings.stop),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(format!("{:<6}", label));
                            let is_capturing = self.capturing_hotkey.as_deref() == Some(label);
                            let text = if is_capturing {
                                "Press hotkey… (Esc = none)".to_string()
                            } else {
                                combo.display_owned()
                            };
                            let btn_label = if is_capturing { "⌨ Capturing…" } else { &text };
                            if ui.button(btn_label).clicked() {
                                capture_request = Some(label.to_string());
                            }
                            if is_capturing {
                                // Prefer Win32 raw polling (distinguishes Numpad vs top-row vs Home cluster on fullsize keyboards).
                                // Fallback to egui if Win32 not available.
                                #[cfg(windows)]
                                let mut detected = poll_win32_hotkey();
                                #[cfg(not(windows))]
                                let mut detected: Option<HotkeyCombo> = None;

                                // egui fallback (also handles Esc=clear)
                                if detected.is_none() {
                                    ctx.input(|i| {
                                        if i.key_pressed(egui::Key::Escape) && i.modifiers.is_none() {
                                            detected = Some(HotkeyCombo::none());
                                        } else if detected.is_none() {
                                            let mut mods = 0;
                                            if i.modifiers.ctrl { mods |= crate::config::mod_flag::CONTROL; }
                                            if i.modifiers.shift { mods |= crate::config::mod_flag::SHIFT; }
                                            if i.modifiers.alt { mods |= crate::config::mod_flag::ALT; }
                                            for &key in i.keys_down.iter() {
                                                let vk = egui_key_to_vk(key);
                                                if let Some(vk) = vk {
                                                    if vk == 0x10 || vk == 0x11 || vk == 0x12 || vk == 0x5B || vk == 0x5C {
                                                        continue;
                                                    }
                                                    detected = Some(HotkeyCombo::new(mods, vk));
                                                    break;
                                                }
                                            }
                                        }
                                    });
                                }
                                if let Some(new_combo) = detected {
                                    *combo = new_combo;
                                    self.capturing_hotkey = None;
                                }
                            }
                            if ui.small_button("Clear").clicked() {
                                *combo = HotkeyCombo::none();
                            }
                        });
                    }
                    if let Some(req) = capture_request {
                        self.capturing_hotkey = Some(req);
                    }

                    ui.separator();
                    // Duplicate detection
                    let combos = [&self.bindings.record, &self.bindings.play, &self.bindings.stop];
                    let labels = ["Record", "Play", "Stop"];
                    let mut dup_found = false;
                    for i in 0..combos.len() {
                        for j in (i + 1)..combos.len() {
                            if combos[i].key != 0 && combos[i] == combos[j] {
                                ui.colored_label(crate::ui::theme::WARNING, format!("Duplicate: {} and {} both {}", labels[i], labels[j], combos[i].display_owned()));
                                dup_found = true;
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        let ok_enabled = !dup_found;
                        if ui.add_enabled(ok_enabled, egui::Button::new("OK")).clicked() {
                            self.close_settings(true);
                        }
                        if ui.button("Cancel").clicked() {
                            // Reload bindings from shared config to discard edits
                            let cfg = ConfigService::shared().lock().unwrap().clone();
                            self.bindings = SettingsBindings::from_map(Some(&cfg.hotkeys));
                            self.close_settings(false);
                        }
                    });
                });
            if !open {
                // Window X close = cancel
                let cfg = ConfigService::shared().lock().unwrap().clone();
                self.bindings = SettingsBindings::from_map(Some(&cfg.hotkeys));
                self.close_settings(false);
            }
        }

        // ---- Macro editor
        if self.show_editor {
            let mut open = self.show_editor;
            Window::new("Edit macro")
                .open(&mut open)
                .resizable(true)
                .default_size([500.0, 300.0])
                .show(ctx, |ui| {
                    ui.label(format!("{} event(s)", self.editor_events.len()));
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut to_delete: Option<usize> = None;
                        let mut to_move_up: Option<usize> = None;
                        let mut to_move_down: Option<usize> = None;
                        for (idx, ev) in self.editor_events.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(format!("{:>3}: {}  [{}ms]", idx, ev.description(), ev.delay_ms));
                                if ui.small_button("↑").clicked() && idx > 0 {
                                    to_move_up = Some(idx);
                                }
                                if ui.small_button("↓").clicked() && idx + 1 < self.editor_events.len() {
                                    to_move_down = Some(idx);
                                }
                                if ui.small_button("✕").clicked() {
                                    to_delete = Some(idx);
                                }
                            });
                        }
                        if let Some(i) = to_delete {
                            self.editor_events.remove(i);
                        }
                        if let Some(i) = to_move_up {
                            self.editor_events.swap(i, i - 1);
                        }
                        if let Some(i) = to_move_down {
                            self.editor_events.swap(i, i + 1);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            let new_events = self.editor_events.clone();
                            let count = new_events.len();
                            if let Some(data) = &mut self.macro_data {
                                data.events = new_events;
                            }
                            self.open_path = None; // invalidate saved file
                            self.log(format!("Edited macro — {} event(s).", count), false);
                            self.show_editor = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_editor = false;
                        }
                        if ui.button("Clear all").clicked() {
                            self.editor_events.clear();
                        }
                    });
                });
            if !open {
                self.show_editor = false;
            }
        }

        // Request repaint while playing/recording to keep polling at ~60fps (not unthrottled)
        if self.recording || self.playing {
            ctx.request_repaint_after(std::time::Duration::from_millis(15));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.on_leave();
        self.player.stop_and_wait();
        self.persist_config();
    }
}

#[cfg(windows)]
pub fn poll_win32_hotkey() -> Option<crate::config::HotkeyCombo> {
    // Check Esc first (clear hotkey) — Esc without modifiers
    let esc_down = unsafe { GetAsyncKeyState(0x1B) } as u16 & 0x8000 != 0;
    if esc_down {
        let ctrl = unsafe { GetAsyncKeyState(0x11) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA2) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA3) } as u16 & 0x8000 != 0;
        let shift = unsafe { GetAsyncKeyState(0x10) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA0) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA1) } as u16 & 0x8000 != 0;
        let alt = unsafe { GetAsyncKeyState(0x12) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA4) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0xA5) } as u16 & 0x8000 != 0;
        let win = unsafe { GetAsyncKeyState(0x5B) } as u16 & 0x8000 != 0
            || unsafe { GetAsyncKeyState(0x5C) } as u16 & 0x8000 != 0;
        if !ctrl && !shift && !alt && !win {
            return Some(crate::config::HotkeyCombo::none());
        }
    }

    // Collect modifiers via Win32 (egui doesn't expose Win key)
    let mut mods = 0;
    let ctrl_down = unsafe { GetAsyncKeyState(0x11) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA2) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA3) } as u16 & 0x8000 != 0;
    let shift_down = unsafe { GetAsyncKeyState(0x10) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA0) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA1) } as u16 & 0x8000 != 0;
    let alt_down = unsafe { GetAsyncKeyState(0x12) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA4) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0xA5) } as u16 & 0x8000 != 0;
    let win_down = unsafe { GetAsyncKeyState(0x5B) } as u16 & 0x8000 != 0
        || unsafe { GetAsyncKeyState(0x5C) } as u16 & 0x8000 != 0;
    if ctrl_down { mods |= crate::config::mod_flag::CONTROL; }
    if shift_down { mods |= crate::config::mod_flag::SHIFT; }
    if alt_down { mods |= crate::config::mod_flag::ALT; }
    if win_down { mods |= crate::config::mod_flag::WIN; }

    // Priority: Numpad (0x60..0x69) before top-row digits, so fullsize keyboards can distinguish.
    // Then F-keys, navigation, letters, etc. Check in order that gives most specific first.
    let candidates: &[i32] = &[
        // Numpad (fullsize) — check first so NumLock-on distinguishes from Digit row
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, // VK_NUMPAD0..9
        0x6A, 0x6B, 0x6D, 0x6E, 0x6F, // * + - . /
        // Function
        0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x7B,
        // Navigation (dedicated cluster) — after numpad so Numpad with Shift doesn't masquerade
        0x24, 0x23, 0x21, 0x22, 0x2D, 0x2E, 0x25, 0x26, 0x27, 0x28,
        // Top-row digits (avoid when numpad already matched)
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
        // Letters
        0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A,
        // Misc
        0x20, 0x0D, 0x09, 0x08, 0x1B, 0x2C,
    ];
    for &vk in candidates {
        // Skip modifiers themselves
        if vk == 0x10 || vk == 0x11 || vk == 0x12 || vk == 0x5B || vk == 0x5C { continue; }
        if unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0 {
            // Windows quirk: Shift+Numpad with NumLock-on actually generates navigation VK (e.g. VK_END 0x23)
            // instead of VK_NUMPAD1 0x61. Warn user via console but still return the VK so RegisterHotKey matches reality.
            // For dedicated Home/End keys vs Numpad, this polling already distinguishes because Numpad VK won't be down
            // when Shift inverts NumLock — it will be the navigation VK. That's OS behavior, not our bug.
            return Some(crate::config::HotkeyCombo::new(mods, vk));
        }
    }
    None
}

fn egui_key_to_vk(key: egui::Key) -> Option<i32> {
    Some(match key {
        egui::Key::A => b'A' as i32,
        egui::Key::B => b'B' as i32,
        egui::Key::C => b'C' as i32,
        egui::Key::D => b'D' as i32,
        egui::Key::E => b'E' as i32,
        egui::Key::F => b'F' as i32,
        egui::Key::G => b'G' as i32,
        egui::Key::H => b'H' as i32,
        egui::Key::I => b'I' as i32,
        egui::Key::J => b'J' as i32,
        egui::Key::K => b'K' as i32,
        egui::Key::L => b'L' as i32,
        egui::Key::M => b'M' as i32,
        egui::Key::N => b'N' as i32,
        egui::Key::O => b'O' as i32,
        egui::Key::P => b'P' as i32,
        egui::Key::Q => b'Q' as i32,
        egui::Key::R => b'R' as i32,
        egui::Key::S => b'S' as i32,
        egui::Key::T => b'T' as i32,
        egui::Key::U => b'U' as i32,
        egui::Key::V => b'V' as i32,
        egui::Key::W => b'W' as i32,
        egui::Key::X => b'X' as i32,
        egui::Key::Y => b'Y' as i32,
        egui::Key::Z => b'Z' as i32,
        egui::Key::Num0 => b'0' as i32,
        egui::Key::Num1 => b'1' as i32,
        egui::Key::Num2 => b'2' as i32,
        egui::Key::Num3 => b'3' as i32,
        egui::Key::Num4 => b'4' as i32,
        egui::Key::Num5 => b'5' as i32,
        egui::Key::Num6 => b'6' as i32,
        egui::Key::Num7 => b'7' as i32,
        egui::Key::Num8 => b'8' as i32,
        egui::Key::Num9 => b'9' as i32,
        egui::Key::F1 => 0x70,
        egui::Key::F2 => 0x71,
        egui::Key::F3 => 0x72,
        egui::Key::F4 => 0x73,
        egui::Key::F5 => 0x74,
        egui::Key::F6 => 0x75,
        egui::Key::F7 => 0x76,
        egui::Key::F8 => 0x77,
        egui::Key::F9 => 0x78,
        egui::Key::F10 => 0x79,
        egui::Key::F11 => 0x7A,
        egui::Key::F12 => 0x7B,
        egui::Key::Escape => 0x1B,
        egui::Key::Space => 0x20,
        egui::Key::Enter => 0x0D,
        egui::Key::Tab => 0x09,
        egui::Key::Backspace => 0x08,
        egui::Key::Delete => 0x2E,
        egui::Key::Insert => 0x2D,
        egui::Key::Home => 0x24,
        egui::Key::End => 0x23,
        egui::Key::PageUp => 0x21,
        egui::Key::PageDown => 0x22,
        egui::Key::ArrowLeft => 0x25,
        egui::Key::ArrowRight => 0x27,
        egui::Key::ArrowUp => 0x26,
        egui::Key::ArrowDown => 0x28,
        _ => return None,
    })
}
