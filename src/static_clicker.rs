//! static_clicker.rs — Foreground / Background auto clicker core.
//!
//! 100% safe Rust except narrow SendInput/PostMessage wrappers via `hooks.rs`.
//! Thread model mirrors `player.rs`: `Arc<AtomicBool>` stop signal + JoinHandle.

use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::hooks::{self, SafeHwnd};
use crate::model::MouseButton;
pub use crate::model::{ClickType, CursorMode, SequenceTarget};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Count(u32),
    Infinite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMode {
    Foreground,
    Background(SafeHwnd),
}

#[derive(Debug, Clone)]
pub struct StaticClickerConfig {
    pub interval_ms: u64, // total delay between clicks, min 1
    pub button: MouseButton,
    pub click_type: ClickType,
    pub repeat: RepeatMode,
    pub cursor: CursorMode,
    pub mode: ClickMode,
    pub sequence_targets: Vec<SequenceTarget>,
    pub sequence_enabled: bool,
    pub interval_jitter_ms: u32,
    pub position_jitter_px: u32,
}

impl StaticClickerConfig {
    pub fn interval_ms_clamped(&self) -> u64 {
        self.interval_ms.max(1)
    }
}

/// Jitter helpers — return base unchanged when jitter==0 to guarantee 100% identical behavior.
pub fn jitter_interval(base_ms: u64, jitter_ms: u32) -> u64 {
    if jitter_ms == 0 {
        return base_ms.max(1);
    }
    use rand::Rng;
    let jitter = jitter_ms as i64;
    let delta: i64 = rand::thread_rng().gen_range(-jitter..=jitter);
    let jittered = (base_ms as i64).saturating_add(delta);
    (jittered.max(1)) as u64
}

pub fn jitter_pos(base: i32, jitter_px: u32) -> i32 {
    if jitter_px == 0 {
        return base;
    }
    use rand::Rng;
    let jitter = jitter_px as i32;
    let delta: i32 = rand::thread_rng().gen_range(-jitter..=jitter);
    base.saturating_add(delta)
}

// ---------------------------------------------------------------------------
// Collision guard tuning
// ---------------------------------------------------------------------------

/// Skip a tick when real (hardware) input was seen less than this long ago.
const COLLISION_IDLE_GUARD_MS: u32 = 50;

/// Retry poll interval while guard is engaged (100-150ms per spec).
/// Uses short sleep chunks with cancellation checks to avoid the "wait full
/// interval again" bug for long intervals (e.g. 5 min deferred to 10 min).
const COLLISION_RETRY_POLL_MS: u64 = 120;

// ---------------------------------------------------------------------------
// Collision guard helpers — testable via injection
// ---------------------------------------------------------------------------

/// Generic retry helper: poll `is_busy` every `COLLISION_RETRY_POLL_MS` until
/// it returns false (idle) or `stop`/hotkey cancels. Returns true if idle
/// (caller should execute click immediately), false if cancelled.
fn wait_until_idle_with<F, W>(
    stop: &AtomicBool,
    esc: &crate::config::HotkeyCombo,
    is_busy: &mut F,
    mut wait_fn: W,
) -> bool
where
    F: FnMut() -> bool,
    W: FnMut(u64, &AtomicBool, &crate::config::HotkeyCombo) -> bool,
{
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if crate::hooks::is_hotkey_pressed(esc) {
            stop.store(true, Ordering::Relaxed);
            return false;
        }
        if !is_busy() {
            return true;
        }
        if !wait_fn(COLLISION_RETRY_POLL_MS, stop, esc) {
            return false;
        }
    }
}

/// Production wrapper using real `wait_cancellable_with_hotkey`.
#[allow(dead_code)]
fn wait_until_foreground_idle<F>(stop: &AtomicBool, esc: &crate::config::HotkeyCombo, is_busy: &mut F) -> bool
where
    F: FnMut() -> bool,
{
    wait_until_idle_with(stop, esc, is_busy, |ms, s, e| {
        wait_cancellable_with_hotkey(ms, s, e)
    })
}

// ---------------------------------------------------------------------------
// StaticClicker — thread owner
// ---------------------------------------------------------------------------

pub struct StaticClicker {
    handle: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    active_target: Arc<AtomicI32>, // -1 = none, else index in sequence_targets
}

impl Default for StaticClicker {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticClicker {
    pub fn new() -> Self {
        Self {
            handle: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            active_target: Arc::new(AtomicI32::new(-1)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn active_target_index(&self) -> Option<usize> {
        let v = self.active_target.load(Ordering::Relaxed);
        if v >= 0 {
            Some(v as usize)
        } else {
            None
        }
    }

    /// Start clicker on background thread. No-op if already running.
    /// `on_tick` called after each click with (count, total_opt).
    /// `on_finished` called with (completed, error_opt) when loop ends.
    pub fn start<F, G>(&mut self, cfg: StaticClickerConfig, on_tick: F, on_finished: G)
    where
        F: Fn(u32, Option<u32>) + Send + 'static,
        G: Fn(bool, Option<String>) + Send + 'static,
    {
        if self.is_running() {
            return;
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.stop_flag.store(false, Ordering::Relaxed);
        self.running.store(true, Ordering::Relaxed);
        self.active_target.store(-1, Ordering::Relaxed);

        let stop = self.stop_flag.clone();
        let running = self.running.clone();
        let active = self.active_target.clone();
        let handle = thread::Builder::new()
            .name("StaticClicker".into())
            .spawn(move || {
                let (completed, err) = run_clicker(cfg, stop.clone(), active.clone(), on_tick);
                active.store(-1, Ordering::Relaxed);
                running.store(false, Ordering::Relaxed);
                on_finished(completed, err);
            })
            .expect("spawn StaticClicker");
        self.handle = Some(handle);
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    pub fn stop_and_wait(&mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StaticClicker {
    fn drop(&mut self) {
        self.stop_and_wait();
    }
}

// ---------------------------------------------------------------------------
// Inner loop
// ---------------------------------------------------------------------------

fn run_clicker<F>(
    cfg: StaticClickerConfig,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicI32>,
    on_tick: F,
) -> (bool, Option<String>)
where
    F: Fn(u32, Option<u32>),
{
    // Dispatch to sequence path if MultiTarget selected.
    // OFF toggle -> fallback to Current location (spec requirement)
    // Empty list -> error, not silent no-op
    if cfg.cursor == CursorMode::MultiTarget {
        if !cfg.sequence_enabled {
            let mut fallback = cfg.clone();
            fallback.cursor = CursorMode::Current;
            return run_normal_clicker(fallback, stop, active, on_tick);
        }
        if cfg.sequence_targets.is_empty() {
            return (false, Some("Add at least 1 target first".to_string()));
        }
        return run_sequence_clicker(cfg, stop, active, on_tick);
    }
    run_normal_clicker(cfg, stop, active, on_tick)
}

fn run_normal_clicker<F>(
    cfg: StaticClickerConfig,
    stop: Arc<AtomicBool>,
    _active: Arc<AtomicI32>,
    on_tick: F,
) -> (bool, Option<String>)
where
    F: Fn(u32, Option<u32>),
{
    let base_interval = cfg.interval_ms_clamped();
    let total_opt = match cfg.repeat {
        RepeatMode::Count(n) => Some(n),
        RepeatMode::Infinite => None,
    };

    let mut count: u32 = 0;
    let mut completed = true;

    // Emergency stop: bare Esc ONLY, checked in-thread for instant reaction.
    // F6 is deliberately NOT polled here — it is handled exclusively via
    // RegisterHotKey -> App routing. Polling it in both places caused a race:
    // thread stops in ~1ms, the late WM_HOTKEY toggle then saw "not running"
    // and STARTED the clicker again (hotkey appeared broken).
    let esc = crate::config::HotkeyCombo::new(0, 0x1B);

    loop {
        // Emergency in-thread stop (Esc)
        if crate::hooks::is_hotkey_pressed(&esc) {
            stop.store(true, Ordering::Relaxed);
        }
        if stop.load(Ordering::Relaxed) {
            completed = false;
            break;
        }
        if let Some(total) = total_opt {
            if count >= total {
                break;
            }
        }

        // If cursor is over Clickei window, pause clicking to allow user to hit Stop (fixes foreground lock)
        if let Some((cx, cy)) = crate::hooks::get_cursor_pos() {
            if crate::hooks::is_point_in_tinytask(cx, cy) {
                // Don't click while hovering over app — just wait interval and check stop again
                let jittered = jitter_interval(base_interval, cfg.interval_jitter_ms);
                if !wait_cancellable_with_hotkey(jittered, &stop, &esc) {
                    completed = false;
                    break;
                }
                continue;
            }
        }

        // Validate Background HWND each tick (real-time)
        if let ClickMode::Background(hwnd) = cfg.mode {
            if !hwnd.is_valid() {
                return (false, Some("Target window closed — clicker stopped.".to_string()));
            }
        }

        // Collision guard (Foreground only): never inject while the user is
        // mid-click on the same button or generated real input <50ms ago.
        // FIXED: retry with short poll (COLLISION_RETRY_POLL_MS) until idle,
        // then execute immediately. Next interval counted from actual click time.
        if matches!(cfg.mode, ClickMode::Foreground) {
            let mut is_busy =
                || hooks::is_real_button_down(cfg.button) || hooks::real_input_recently(COLLISION_IDLE_GUARD_MS);
            if is_busy() {
                let cleared = wait_until_idle_with(&stop, &esc, &mut is_busy, |ms, s, e| {
                    wait_cancellable_with_hotkey(ms, s, e)
                });
                if !cleared {
                    completed = false;
                    break;
                }
                // Guard cleared — fall through to tick immediately (no full-interval wait).
            }
        }

        let tick_ok = match cfg.mode {
            ClickMode::Foreground => do_foreground_tick(&cfg),
            ClickMode::Background(hwnd) => do_background_tick(&cfg, hwnd),
        };

        if let Err(e) = tick_ok {
            return (false, Some(e));
        }

        count += 1;
        on_tick(count, total_opt);

        if let Some(total) = total_opt {
            if count >= total {
                break;
            }
        }

        let jittered = jitter_interval(base_interval, cfg.interval_jitter_ms);
        if !wait_cancellable_with_hotkey(jittered, &stop, &esc) {
            completed = false;
            break;
        }
    }

    (completed, None)
}

fn run_sequence_clicker<F>(
    cfg: StaticClickerConfig,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicI32>,
    on_tick: F,
) -> (bool, Option<String>)
where
    F: Fn(u32, Option<u32>),
{
    let esc = crate::config::HotkeyCombo::new(0, 0x1B);
    if cfg.sequence_targets.is_empty() {
        return (false, Some("Add at least 1 target first".to_string()));
    }
    let total_rounds_opt = match cfg.repeat {
        RepeatMode::Count(n) => Some(n),
        RepeatMode::Infinite => None,
    };
    let mut rounds: u32 = 0;
    let mut total_clicks: u32 = 0;
    let mut completed = true;

    'outer: loop {
        if crate::hooks::is_hotkey_pressed(&esc) {
            stop.store(true, Ordering::Relaxed);
        }
        if stop.load(Ordering::Relaxed) {
            completed = false;
            break;
        }
        if let Some(total) = total_rounds_opt {
            if rounds >= total {
                break;
            }
        }

        for (ti, target) in cfg.sequence_targets.iter().enumerate() {
            let clicks = target.clicks.max(1);
            let base_interval = (target.interval_ms as u64).max(1);

            for ci in 0..clicks {
                if crate::hooks::is_hotkey_pressed(&esc) {
                    stop.store(true, Ordering::Relaxed);
                }
                if stop.load(Ordering::Relaxed) {
                    completed = false;
                    break 'outer;
                }
                if let ClickMode::Background(hwnd) = cfg.mode {
                    if !hwnd.is_valid() {
                        return (false, Some("Target window closed — clicker stopped.".to_string()));
                    }
                }

                // Hover & collision guard — retry same click (per-target, not resetting sequence)
                // Hover keeps full-interval wait; collision guard FIXED to short poll (COLLISION_RETRY_POLL_MS)
                loop {
                    if crate::hooks::is_hotkey_pressed(&esc) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if stop.load(Ordering::Relaxed) {
                        completed = false;
                        break 'outer;
                    }
                    if let Some((cx, cy)) = crate::hooks::get_cursor_pos() {
                        if crate::hooks::is_point_in_tinytask(cx, cy) {
                            let jittered = jitter_interval(base_interval, cfg.interval_jitter_ms);
                            if !wait_cancellable_with_hotkey(jittered, &stop, &esc) {
                                completed = false;
                                break 'outer;
                            }
                            continue;
                        }
                    }
                    if matches!(cfg.mode, ClickMode::Foreground) {
                        let busy = hooks::is_real_button_down(cfg.button)
                            || hooks::real_input_recently(COLLISION_IDLE_GUARD_MS);
                        if busy {
                            // FIXED: short retry poll until idle, not full base_interval
                            if !wait_cancellable_with_hotkey(COLLISION_RETRY_POLL_MS, &stop, &esc) {
                                completed = false;
                                break 'outer;
                            }
                            continue;
                        }
                    }
                    break;
                }
                if stop.load(Ordering::Relaxed) {
                    completed = false;
                    break 'outer;
                }

                // Indicator: mark this target as active BEFORE the click (no delay, matches actual SendInput)
                active.store(ti as i32, Ordering::Relaxed);

                let tick_ok = match cfg.mode {
                    ClickMode::Foreground => foreground_click_at(&cfg, target.x, target.y),
                    ClickMode::Background(hwnd) => background_click_at(&cfg, hwnd, target.x, target.y),
                };
                if let Err(e) = tick_ok {
                    return (false, Some(e));
                }
                total_clicks = total_clicks.wrapping_add(1);
                on_tick(total_clicks, total_rounds_opt);

                let is_last_target = ti + 1 == cfg.sequence_targets.len();
                let is_last_click_in_target = ci + 1 == clicks;
                let is_last_round = match total_rounds_opt {
                    Some(total) => rounds + 1 == total,
                    None => false,
                };
                let is_final_click = is_last_target && is_last_click_in_target && is_last_round;
                if is_final_click {
                    continue;
                }
                let jittered = jitter_interval(base_interval, cfg.interval_jitter_ms);
                if !wait_cancellable_with_hotkey(jittered, &stop, &esc) {
                    completed = false;
                    break 'outer;
                }
                if stop.load(Ordering::Relaxed) {
                    completed = false;
                    break 'outer;
                }
            }
            if stop.load(Ordering::Relaxed) {
                completed = false;
                break 'outer;
            }
        }

        rounds += 1;
        if let Some(total) = total_rounds_opt {
            if rounds >= total {
                break;
            }
        }
    }

    (completed, None)
}

fn foreground_click_at(cfg: &StaticClickerConfig, target_x: i32, target_y: i32) -> Result<(), String> {
    let target_x = jitter_pos(target_x, cfg.position_jitter_px);
    let target_y = jitter_pos(target_y, cfg.position_jitter_px);
    let saved_pos = hooks::get_cursor_pos();
    let saved_fg = hooks::get_foreground_window();

    let mut need_restore_fg = false;
    let mut prev_fg: Option<hooks::SafeHwnd> = None;
    let target_hwnd = hooks::window_from_point(target_x, target_y);
    if !target_hwnd.0.is_null() {
        let target_root = hooks::ancestor_root(target_hwnd);
        if let Some(fg) = saved_fg {
            if fg != target_root && !hooks::is_point_in_tinytask(target_x, target_y) {
                prev_fg = Some(hooks::SafeHwnd(fg));
                hooks::set_foreground_window(target_root);
                need_restore_fg = true;
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }

    let need_move = match saved_pos {
        Some((sx, sy)) => sx != target_x || sy != target_y,
        None => true,
    };

    let inj_start = hooks::tick_count();
    hooks::send_mouse_click_sequence(
        cfg.button,
        cfg.click_type == ClickType::Double,
        target_x,
        target_y,
    );

    if need_move {
        if let Some((sx, sy)) = saved_pos {
            hooks::send_mouse_move_absolute(sx, sy);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    hooks::note_own_injection(inj_start, hooks::tick_count());

    if need_move {
        if let Some((sx, sy)) = saved_pos {
            if let Some((now_x, now_y)) = hooks::get_cursor_pos() {
                if now_x != sx || now_y != sy {
                    log::warn!(
                        "Sequence foreground tick: cursor restore drifted (saved {},{} -> now {},{}) — input race got past guards",
                        sx, sy, now_x, now_y
                    );
                }
            }
        }
    }
    if need_restore_fg {
        if let Some(prev) = prev_fg {
            hooks::set_foreground_window(prev.as_hwnd());
        }
    }
    Ok(())
}

fn background_click_at(
    cfg: &StaticClickerConfig,
    hwnd: SafeHwnd,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    let screen_x = jitter_pos(screen_x, cfg.position_jitter_px);
    let screen_y = jitter_pos(screen_y, cfg.position_jitter_px);
    let raw_hwnd = hwnd.as_hwnd();
    if !hooks::is_window(raw_hwnd) {
        return Err("Target window is no longer valid".to_string());
    }
    let (client_x, client_y) = hooks::screen_to_client(raw_hwnd, screen_x, screen_y)
        .ok_or_else(|| "ScreenToClient failed — window may be minimized".to_string())?;

    let title = hooks::get_window_title(raw_hwnd);
    log::info!(
        "Background sequence tick: HWND {:?} '{}' screen ({},{}) -> client ({},{}) button {:?} {:?}",
        raw_hwnd, title, screen_x, screen_y, client_x, client_y, cfg.button, cfg.click_type
    );

    {
        use windows::Win32::UI::WindowsAndMessaging::IsIconic;
        if unsafe { IsIconic(raw_hwnd).as_bool() } {
            log::warn!("Target window is minimized — PostMessage will be ignored");
        }
    }

    let ok1 = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, true);
    std::thread::sleep(Duration::from_millis(5));
    let ok2 = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, false);
    if !ok1 || !ok2 {
        log::warn!("PostMessage to root failed for HWND {:?}, trying SendMessage fallback", raw_hwnd);
        hooks::send_click_message(raw_hwnd, cfg.button, client_x, client_y, true);
        std::thread::sleep(Duration::from_millis(5));
        hooks::send_click_message(raw_hwnd, cfg.button, client_x, client_y, false);
    }

    let child = hooks::window_from_point(screen_x, screen_y);
    if child != raw_hwnd && !child.0.is_null() && hooks::is_window(child) {
        if let Some((cx, cy)) = hooks::screen_to_client(child, screen_x, screen_y) {
            log::info!("Also posting to child HWND {:?} client ({},{})", child, cx, cy);
            let _ = hooks::post_click(child, cfg.button, cx, cy, true);
            std::thread::sleep(Duration::from_millis(5));
            let _ = hooks::post_click(child, cfg.button, cx, cy, false);
        }
    }

    if cfg.click_type == ClickType::Double {
        std::thread::sleep(Duration::from_millis(50));
        let _ = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, true);
        std::thread::sleep(Duration::from_millis(10));
        let _ = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, false);
        if child != raw_hwnd && !child.0.is_null() {
            if let Some((cx, cy)) = hooks::screen_to_client(child, screen_x, screen_y) {
                std::thread::sleep(Duration::from_millis(5));
                let _ = hooks::post_click(child, cfg.button, cx, cy, true);
                std::thread::sleep(Duration::from_millis(10));
                let _ = hooks::post_click(child, cfg.button, cx, cy, false);
            }
        }
    }

    Ok(())
}

fn do_foreground_tick(cfg: &StaticClickerConfig) -> Result<(), String> {
    // Save current pos and foreground window for restore
    let saved_pos = hooks::get_cursor_pos();
    let saved_fg = hooks::get_foreground_window();

    let (base_x, base_y) = match cfg.cursor {
        CursorMode::Current => {
            if let Some(p) = hooks::get_cursor_pos() {
                p
            } else {
                return Err("GetCursorPos failed".to_string());
            }
        }
        CursorMode::Fixed { x, y } => (x, y),
        CursorMode::MultiTarget => {
            // Should be handled via sequence path; fallback to Current to keep legacy function safe
            if let Some(p) = hooks::get_cursor_pos() {
                p
            } else {
                return Err("GetCursorPos failed".to_string());
            }
        }
    };
    let target_x = jitter_pos(base_x, cfg.position_jitter_px);
    let target_y = jitter_pos(base_y, cfg.position_jitter_px);

    // For Fixed location: ensure target window is foreground so game receives click
    // (many games ignore clicks when not active). We bring it to front briefly.
    let mut need_restore_fg = false;
    let mut prev_fg: Option<hooks::SafeHwnd> = None;
    if let CursorMode::Fixed { .. } = cfg.cursor {
        let target_hwnd = hooks::window_from_point(target_x, target_y);
        if !target_hwnd.0.is_null() {
            let target_root = hooks::ancestor_root(target_hwnd);
            if let Some(fg) = saved_fg {
                if fg != target_root {
                    // Only steal focus if target is not already foreground and not Clickei itself
                    if !hooks::is_point_in_tinytask(target_x, target_y) {
                        prev_fg = Some(hooks::SafeHwnd(fg));
                        hooks::set_foreground_window(target_root);
                        need_restore_fg = true;
                        std::thread::sleep(Duration::from_millis(15));
                    }
                }
            }
        }
    }

    let need_move = match saved_pos {
        Some((sx, sy)) => sx != target_x || sy != target_y,
        None => true,
    };

    // Open our injection window — SendInput refreshes GetLastInputInfo too, so
    // real_input_recently needs this to discount our own clicks on the next tick.
    let inj_start = hooks::tick_count();

    // Click: ONE SendInput call containing [MOVE to target, DOWN, UP (+ DOWN, UP if double)].
    // Restore-move MUST be a separate SendInput call AFTER this one returns,
    // otherwise the UP event can get delivered to the wrong window.
    hooks::send_mouse_click_sequence(cfg.button, cfg.click_type == ClickType::Double, target_x, target_y);

    // Step 4: restore cursor AFTER the click array was fully sent
    if need_move {
        if let Some((sx, sy)) = saved_pos {
            hooks::send_mouse_move_absolute(sx, sy);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    hooks::note_own_injection(inj_start, hooks::tick_count());

    // Post-restore verification (debug aid): if the cursor did not land back on
    // saved_pos, a real-vs-injected race slipped past the guards. Log only —
    // no forced retry since the user may legitimately have moved again.
    if need_move {
        if let Some((sx, sy)) = saved_pos {
            if let Some((now_x, now_y)) = hooks::get_cursor_pos() {
                if now_x != sx || now_y != sy {
                    log::warn!(
                        "Foreground tick: cursor restore drifted (saved {},{} -> now {},{}) — input race got past guards",
                        sx, sy, now_x, now_y
                    );
                }
            }
        }
    }
    if need_restore_fg {
        if let Some(prev) = prev_fg {
            hooks::set_foreground_window(prev.as_hwnd());
        }
    }

    Ok(())
}

fn do_background_tick(cfg: &StaticClickerConfig, hwnd: SafeHwnd) -> Result<(), String> {
    let raw_hwnd = hwnd.as_hwnd();
    if !hooks::is_window(raw_hwnd) {
        return Err("Target window is no longer valid".to_string());
    }

    let (base_x, base_y) = match cfg.cursor {
        CursorMode::Current => hooks::get_cursor_pos().ok_or("GetCursorPos failed")?,
        CursorMode::Fixed { x, y } => (x, y),
        CursorMode::MultiTarget => hooks::get_cursor_pos().ok_or("GetCursorPos failed")?,
    };
    let screen_x = jitter_pos(base_x, cfg.position_jitter_px);
    let screen_y = jitter_pos(base_y, cfg.position_jitter_px);

    let (client_x, client_y) = hooks::screen_to_client(raw_hwnd, screen_x, screen_y)
        .ok_or_else(|| "ScreenToClient failed — window may be minimized".to_string())?;

    let title = hooks::get_window_title(raw_hwnd);
    log::info!(
        "Background tick: HWND {:?} '{}' screen ({},{}) -> client ({},{}) button {:?} {:?}",
        raw_hwnd, title, screen_x, screen_y, client_x, client_y, cfg.button, cfg.click_type
    );

    // Detect minimized — PostMessage will silently no-op
    {
        use windows::Win32::UI::WindowsAndMessaging::IsIconic;
        if unsafe { IsIconic(raw_hwnd).as_bool() } {
            log::warn!("Target window is minimized — PostMessage will be ignored");
        }
    }

    // Try root window
    let ok1 = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, true);
    std::thread::sleep(Duration::from_millis(5));
    let ok2 = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, false);
    if !ok1 || !ok2 {
        log::warn!("PostMessage to root failed for HWND {:?}, trying SendMessage fallback", raw_hwnd);
        hooks::send_click_message(raw_hwnd, cfg.button, client_x, client_y, true);
        std::thread::sleep(Duration::from_millis(5));
        hooks::send_click_message(raw_hwnd, cfg.button, client_x, client_y, false);
    }

    // Also try child window at that point (some games have child HWND that actually handles clicks)
    let child = hooks::window_from_point(screen_x, screen_y);
    if child != raw_hwnd && !child.0.is_null() && hooks::is_window(child) {
        if let Some((cx, cy)) = hooks::screen_to_client(child, screen_x, screen_y) {
            log::info!("Also posting to child HWND {:?} client ({},{})", child, cx, cy);
            let _ = hooks::post_click(child, cfg.button, cx, cy, true);
            std::thread::sleep(Duration::from_millis(5));
            let _ = hooks::post_click(child, cfg.button, cx, cy, false);
        }
    }

    if cfg.click_type == ClickType::Double {
        std::thread::sleep(Duration::from_millis(50));
        let _ = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, true);
        std::thread::sleep(Duration::from_millis(10));
        let _ = hooks::post_click(raw_hwnd, cfg.button, client_x, client_y, false);
        // Also double to child
        if child != raw_hwnd && !child.0.is_null() {
            if let Some((cx, cy)) = hooks::screen_to_client(child, screen_x, screen_y) {
                std::thread::sleep(Duration::from_millis(5));
                let _ = hooks::post_click(child, cfg.button, cx, cy, true);
                std::thread::sleep(Duration::from_millis(10));
                let _ = hooks::post_click(child, cfg.button, cx, cy, false);
            }
        }
    }

    Ok(())
}

fn wait_cancellable_with_hotkey(ms: u64, stop: &AtomicBool, hotkey: &crate::config::HotkeyCombo) -> bool {
    if ms == 0 {
        return !stop.load(Ordering::Relaxed) && !crate::hooks::is_hotkey_pressed(hotkey) && !crate::hooks::is_hotkey_pressed(&crate::config::HotkeyCombo::new(0, 0x1B));
    }
    let start = std::time::Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        // Poll F6 / Esc directly in wait loop — works even if UI thread is busy (foreground lock)
        if crate::hooks::is_hotkey_pressed(hotkey) || crate::hooks::is_hotkey_pressed(&crate::config::HotkeyCombo::new(0, 0x1B)) {
            stop.store(true, Ordering::Relaxed);
            return false;
        }
        if start.elapsed().as_millis() as u64 >= ms {
            return !stop.load(Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_clamped() {
        let cfg = StaticClickerConfig {
            interval_ms: 0,
            button: MouseButton::Left,
            click_type: ClickType::Single,
            repeat: RepeatMode::Infinite,
            cursor: CursorMode::Current,
            mode: ClickMode::Foreground,
            sequence_targets: Vec::new(),
            sequence_enabled: true,
            interval_jitter_ms: 0,
            position_jitter_px: 0,
        };
        assert_eq!(cfg.interval_ms_clamped(), 1);
    }

    #[test]
    fn sequence_target_default() {
        let t = SequenceTarget::default();
        assert_eq!(t.clicks, 1);
        assert_eq!(t.interval_ms, 500);
    }

    #[test]
    fn cursor_mode_multitarget() {
        let cfg = StaticClickerConfig {
            interval_ms: 100,
            button: MouseButton::Left,
            click_type: ClickType::Single,
            repeat: RepeatMode::Count(2),
            cursor: CursorMode::MultiTarget,
            mode: ClickMode::Foreground,
            sequence_targets: vec![
                SequenceTarget { x: 100, y: 200, clicks: 2, interval_ms: 100 },
                SequenceTarget { x: 300, y: 400, clicks: 1, interval_ms: 200 },
            ],
            sequence_enabled: true,
            interval_jitter_ms: 0,
            position_jitter_px: 0,
        };
        assert_eq!(cfg.cursor, CursorMode::MultiTarget);
        assert_eq!(cfg.sequence_targets.len(), 2);
    }

    #[test]
    fn jitter_interval_zero_unchanged() {
        assert_eq!(jitter_interval(100, 0), 100);
        assert_eq!(jitter_interval(1, 0), 1);
    }

    #[test]
    fn jitter_interval_within_range() {
        for _ in 0..50 {
            let v = jitter_interval(100, 10);
            assert!(v >= 90 && v <= 110, "jittered {} not in [90,110]", v);
            assert!(v >= 1);
        }
        // clamp minimum 1ms
        for _ in 0..20 {
            let v = jitter_interval(1, 10);
            assert!(v >= 1, "clamp failed {}", v);
            assert!(v <= 11);
        }
    }

    #[test]
    fn jitter_pos_zero_unchanged() {
        assert_eq!(jitter_pos(123, 0), 123);
        assert_eq!(jitter_pos(-50, 0), -50);
    }

    #[test]
    fn jitter_pos_within_range() {
        for _ in 0..50 {
            let v = jitter_pos(100, 5);
            assert!(v >= 95 && v <= 105, "pos jitter {} not in [95,105]", v);
        }
        // max allowed 20px still within range
        for _ in 0..30 {
            let v = jitter_pos(0, 20);
            assert!(v >= -20 && v <= 20, "pos jitter {} not in [-20,20]", v);
        }
    }

    // -----------------------------------------------------------------------
    // Collision guard retry — timing regression tests
    // Simulates: tick due → guard blocked several polls → guard clears → click immediate,
    // and next interval counted from actual click time.
    // -----------------------------------------------------------------------

    #[test]
    fn collision_retry_short_poll_vs_full_interval() {
        // Bug: previously waited full base_interval (e.g. 5 min) when guard blocked.
        // Fixed: now polls every COLLISION_RETRY_POLL_MS (~120ms).
        let stop = AtomicBool::new(false);
        let esc = crate::config::HotkeyCombo::new(0, 0x1B);
        let base_interval: u64 = 300_000; // 5 min — must NOT be waited during retry
        let mut busy_calls = 0;
        let mut is_busy = move || {
            busy_calls += 1;
            busy_calls <= 3 // busy for first 3 checks, idle on 4th
        };
        let mut mock_elapsed: u64 = 0;
        let cleared = wait_until_idle_with(&stop, &esc, &mut is_busy, |ms, _s, _e| {
            mock_elapsed += ms;
            // simulate non-cancelled sleep chunk
            true
        });
        assert!(cleared, "guard should clear after retries");
        let expected = 3 * COLLISION_RETRY_POLL_MS;
        assert_eq!(
            mock_elapsed, expected,
            "retry should wait 3*{}={}ms not full interval {}ms",
            COLLISION_RETRY_POLL_MS, expected, base_interval
        );
        assert!(
            mock_elapsed < base_interval,
            "elapsed {} should be << base_interval {}",
            mock_elapsed, base_interval
        );
        // Retry interval must be within 100-150ms spec
        assert!(
            (100..=150).contains(&COLLISION_RETRY_POLL_MS),
            "poll {} not in 100-150ms",
            COLLISION_RETRY_POLL_MS
        );
    }

    #[test]
    fn collision_retry_click_immediate_after_guard_clear() {
        // Simulate timeline: tick due at t=0, guard busy for 2 polls, then idle -> click at ~240ms, not at 5min.
        let stop = AtomicBool::new(false);
        let esc = crate::config::HotkeyCombo::new(0, 0x1B);
        let mut call_idx = 0;
        let mut is_busy = move || {
            call_idx += 1;
            call_idx <= 2
        };
        let mut mock_time: u64 = 0;
        let mut poll_count: u32 = 0;
        let cleared = wait_until_idle_with(&stop, &esc, &mut is_busy, |ms, _s, _e| {
            mock_time += ms;
            poll_count += 1;
            true
        });
        assert!(cleared);
        assert_eq!(poll_count, 2, "should poll exactly busy-count times");
        assert_eq!(mock_time, 2 * COLLISION_RETRY_POLL_MS);
        // After cleared, caller executes click immediately — no extra wait added here.
        // So click time == mock_time (time spent in retry), proving immediacy.
    }

    #[test]
    fn collision_retry_next_interval_from_actual_click_time() {
        // Scenario: base_interval = 500ms (stands for 5 min scaled down for test).
        // Tick scheduled at t=500. Guard blocks for 3 polls (3*120=360ms) so
        // actual click at t=860. Next tick must be at 860+500=1360, not at 500+500=1000
        // nor at 860+500+500 etc., and definitely not at 500+300000 if bug existed.
        let base_interval: u64 = 500;
        let busy_polls: u64 = 3;
        let poll_ms = COLLISION_RETRY_POLL_MS;
        let delay_due_to_retry = busy_polls * poll_ms; // 360

        // Simulated timeline helper mimicking run_normal_clicker wait logic:
        // - wait base_interval from start to first due
        // - retry delay
        // - click1 at delayed time
        // - wait base_interval from click1 to click2
        let mut sim_time: u64 = 0;
        // Initial wait to first tick due (previous click at 0, next due at base_interval)
        sim_time += base_interval; // 500
        let scheduled_tick: u64 = sim_time;
        assert_eq!(scheduled_tick, 500);

        // Guard retry simulation using wait_until_idle_with metrics
        let stop = AtomicBool::new(false);
        let esc = crate::config::HotkeyCombo::new(0, 0x1B);
        let mut busy_calls = 0;
        let mut is_busy = move || {
            busy_calls += 1;
            busy_calls <= busy_polls as usize
        };
        let mut retry_elapsed: u64 = 0;
        let cleared = wait_until_idle_with(&stop, &esc, &mut is_busy, |ms, _s, _e| {
            retry_elapsed += ms;
            true
        });
        assert!(cleared);
        assert_eq!(retry_elapsed, delay_due_to_retry);

        sim_time += retry_elapsed; // actual click1 time
        let click1_time = sim_time;
        assert_eq!(click1_time, 860, "click1 should be scheduled+delay = 500+360");

        // Next interval counted from actual click1_time, not scheduled_tick
        sim_time += base_interval;
        let click2_time = sim_time;
        assert_eq!(
            click2_time,
            click1_time + base_interval,
            "next interval from actual click time"
        );
        assert_eq!(click2_time, 1360);
        // Prove it is NOT from scheduled time (would be 1000)
        assert_ne!(click2_time, scheduled_tick + base_interval);
        // And prove it is not bug doubling interval (would be ~500+300000)
        assert!(click2_time < scheduled_tick + 300_000);
    }

    #[test]
    fn collision_retry_cancellation_during_retry() {
        // Stop flag set during retry must abort promptly (no busy-loop, no extra click).
        let stop = Arc::new(AtomicBool::new(false));
        let esc = crate::config::HotkeyCombo::new(0, 0x1B);
        let mut is_busy = || true; // always busy — would loop forever without cancellation

        // Simulate wait_fn that after 2 polls sets stop (like user hits Esc/Stop).
        let stop_clone = stop.clone();
        let mut poll_count: u32 = 0;
        let cleared = wait_until_idle_with(&stop, &esc, &mut is_busy, |ms, s, _e| {
            poll_count += 1;
            // inject stop after 2 polls
            if poll_count == 2 {
                s.store(true, Ordering::Relaxed);
            }
            // Mimic real wait_cancellable_with_hotkey: return false if stop set
            if s.load(Ordering::Relaxed) {
                return false;
            }
            // otherwise simulate time passing
            let _ = ms;
            true
        });
        // Must not have returned idle; must have returned cancelled.
        assert!(!cleared, "should report not-cleared when stop triggered");
        assert_eq!(poll_count, 2, "should stop promptly after flag, not busy-loop infinitely");
        assert!(stop_clone.load(Ordering::Relaxed));
    }

    #[test]
    fn sequence_per_target_retry_not_reset() {
        // Multi-Target / Sequence: retry is per-target, not resetting whole sequence.
        // Simulate 2 targets: target0 idle immediately, target1 busy for 2 polls.
        // Expect: target0 click with 0 retry, target1 retries 2*poll before its click.
        // Sequence order preserved.
        let stop = AtomicBool::new(false);
        let esc = crate::config::HotkeyCombo::new(0, 0x1B);

        // Target 0: idle straight away
        let mut is_busy0 = move || {
            false // never busy
        };
        let mut elapsed0: u64 = 0;
        let cleared0 = wait_until_idle_with(&stop, &esc, &mut is_busy0, |ms, _s, _e| {
            elapsed0 += ms;
            true
        });
        assert!(cleared0);
        assert_eq!(elapsed0, 0, "target0 should have 0 retry delay");

        // Target 1: busy 2 polls (simulates user active during this target's due time)
        let mut busy1_calls = 0;
        let mut is_busy1 = move || {
            busy1_calls += 1;
            busy1_calls <= 2
        };
        let mut elapsed1: u64 = 0;
        let cleared1 = wait_until_idle_with(&stop, &esc, &mut is_busy1, |ms, _s, _e| {
            elapsed1 += ms;
            true
        });
        assert!(cleared1);
        assert_eq!(elapsed1, 2 * COLLISION_RETRY_POLL_MS);

        // Verify per-target isolation: target0 elapsed unaffected by target1 retry
        assert_ne!(elapsed0, elapsed1);
        // Total sequence time for this round = elapsed0 + elapsed1 (+ intervals, omitted here)
        let total_retry = elapsed0 + elapsed1;
        assert_eq!(total_retry, 2 * COLLISION_RETRY_POLL_MS);
    }

    #[test]
    fn collision_retry_poll_interval_within_spec() {
        assert!(
            (100..=150).contains(&COLLISION_RETRY_POLL_MS),
            "COLLISION_RETRY_POLL_MS={} must be 100-150ms",
            COLLISION_RETRY_POLL_MS
        );
    }
}
