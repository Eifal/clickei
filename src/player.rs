//! player.rs — Playback via SendInput, with speed/loop/interval/pause/cancel.
//!
//! 100% safe Rust except for the narrow `unsafe` SendInput + timeBeginPeriod wrappers
//! (kept in `input` sub-module). The `unsafe` blocks are commented with invariants.
//!
//! Parity with Core/MacroPlayer.cs:
//! - Speed scaling: `delay / speed`
//! - Fixed loops or infinite
//! - Interval between loops (cancellable + pausable)
//! - Pause/Resume at any point (freezes timer while paused)
//! - Instant cancel that releases stuck keys/buttons
//! - Raises timer resolution to 1 ms via `timeBeginPeriod(1)` for accurate sleeps.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::model::{MacroData, MacroEventType};

// ---------------------------------------------------------------------------
// Playback options — mirrors C# PlaybackOptions + MacroData baked-in fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PlaybackOptions {
    pub speed: f64,       // 0.25..10.0
    pub loop_count: i32,  // 1..9999
    pub infinite: bool,
    pub interval_ms: i32,
}

impl From<&MacroData> for PlaybackOptions {
    fn from(d: &MacroData) -> Self {
        Self {
            speed: d.speed,
            loop_count: d.loop_count,
            infinite: d.infinite_loop,
            interval_ms: d.interval_ms,
        }
    }
}

impl PlaybackOptions {
    pub fn effective_iterations(&self) -> usize {
        if self.infinite {
            usize::MAX
        } else {
            (self.loop_count.max(1)) as usize
        }
    }
}

// ---------------------------------------------------------------------------
// Player — runs on a background thread, cancel via AtomicBool
// ---------------------------------------------------------------------------

pub struct MacroPlayer {
    handle: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl Default for MacroPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl MacroPlayer {
    pub fn new() -> Self {
        Self {
            handle: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            pause_flag: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.pause_flag.load(Ordering::Relaxed)
    }

    /// Start playback on a background thread. No-op if already running.
    pub fn start<F, G, H>(&mut self, data: MacroData, on_loop: F, on_finished: G, _on_pause: H)
    where
        F: Fn(usize, Option<usize>) + Send + 'static,
        G: Fn(bool) + Send + 'static,
        H: Fn(bool) + Send + 'static,
    {
        if self.is_running() {
            return;
        }
        // Join any previous finished thread to avoid leaking JoinHandle.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.stop_flag.store(false, Ordering::Relaxed);
        self.pause_flag.store(false, Ordering::Relaxed);
        self.running.store(true, Ordering::Relaxed);

        let stop = self.stop_flag.clone();
        let pause = self.pause_flag.clone();
        let running = self.running.clone();

        let handle = thread::Builder::new()
            .name("MacroPlayer".into())
            .spawn(move || {
                let completed = run_player(data, stop.clone(), pause.clone(), on_loop, _on_pause);
                running.store(false, Ordering::Relaxed);
                on_finished(completed);
            })
            .expect("spawn MacroPlayer");

        self.handle = Some(handle);
    }

    pub fn pause(&self) {
        if !self.is_running() || self.is_paused() {
            return;
        }
        self.pause_flag.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        if !self.is_running() {
            return;
        }
        self.pause_flag.store(false, Ordering::Relaxed);
    }

    pub fn toggle_pause(&self) {
        if self.is_paused() {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.pause_flag.store(false, Ordering::Relaxed);
    }

    pub fn stop_and_wait(&mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MacroPlayer {
    fn drop(&mut self) {
        self.stop_and_wait();
    }
}

// ---------------------------------------------------------------------------
// Inner run loop — mirrors C# MacroPlayer.Run / PlayOnce / Wait
// ---------------------------------------------------------------------------

fn run_player<F, H>(
    data: MacroData,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    on_loop: F,
    on_pause: H,
) -> bool
where
    F: Fn(usize, Option<usize>),
    H: Fn(bool),
{
    let _timer_guard = TimerResolutionGuard::begin(1);

    let opts = PlaybackOptions::from(&data);
    let events = data.events;
    let iterations = opts.effective_iterations();

    if events.is_empty() {
        return true;
    }

    // Track pressed state across the whole run so we can release on cancel.
    let mut down_keys: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut pressed_buttons: std::collections::HashSet<crate::model::MouseButton> =
        std::collections::HashSet::new();

    let mut completed = true;
    let mut was_paused = false;

    // Use explicit counter to avoid allocating a huge range for infinite case.
    let is_infinite = opts.infinite;
    let mut i: usize = 1;
    loop {
        if !is_infinite && i > iterations {
            break;
        }
        if stop.load(Ordering::Relaxed) {
            completed = false;
            break;
        }

        loop {
            let is_paused = pause.load(Ordering::Relaxed);
            if is_paused != was_paused {
                on_pause(is_paused);
                was_paused = is_paused;
            }
            if is_paused {
                if stop.load(Ordering::Relaxed) {
                    completed = false;
                    break;
                }
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            break;
        }
        if stop.load(Ordering::Relaxed) {
            completed = false;
            break;
        }

        on_loop(i, if opts.infinite { None } else { Some(iterations) });

        if !play_once(
            &events,
            &opts,
            &stop,
            &pause,
            &on_pause,
            &mut down_keys,
            &mut pressed_buttons,
        ) {
            completed = false;
            break;
        }

        // Interval between loops (not after final finite iteration)
        let is_last_finite = !is_infinite && i == iterations;
        if !is_last_finite {
            if opts.interval_ms > 0 {
                if !wait_cancellable(opts.interval_ms as u64, &stop, &pause, &on_pause) {
                    completed = false;
                    break;
                }
            } else if stop.load(Ordering::Relaxed) {
                completed = false;
                break;
            }
        } else {
            break;
        }

        if is_infinite {
            // Wrapping increment, but stop flag will break infinite playback.
            i = i.wrapping_add(1);
            if i == 0 {
                i = 1; // avoid 0 iteration number after wrap
            }
        } else {
            i += 1;
        }
    }

    // Release anything still held — mirrors C# ReleaseStuckInput.
    // This runs even if playback completed normally (sets should be empty then).
    release_stuck_input(&mut down_keys, &mut pressed_buttons);
    completed
}

fn play_once(
    events: &[crate::model::MacroEvent],
    opts: &PlaybackOptions,
    stop: &AtomicBool,
    pause: &AtomicBool,
    on_pause: &dyn Fn(bool),
    down_keys: &mut std::collections::HashSet<i32>,
    pressed_buttons: &mut std::collections::HashSet<crate::model::MouseButton>,
) -> bool {
    for e in events {
        if stop.load(Ordering::Relaxed) {
            return false;
        }

        let delay = (e.delay_ms as f64 / opts.speed).round() as i64;
        if delay > 0 && !wait_cancellable(delay as u64, stop, pause, on_pause) {
            return false;
        }
        if stop.load(Ordering::Relaxed) {
            return false;
        }

        match e.event_type {
            MacroEventType::MouseMove => input::move_to(e.x, e.y),
            MacroEventType::MouseDown => {
                if pressed_buttons.insert(e.button) {
                    input::button(e.button, true, e.x, e.y);
                }
            }
            MacroEventType::MouseUp => {
                pressed_buttons.remove(&e.button);
                input::button(e.button, false, e.x, e.y);
            }
            MacroEventType::MouseWheel => input::wheel(e.wheel_delta),
            MacroEventType::KeyDown => {
                if down_keys.insert(e.key_code) {
                    input::key(e.key_code as u32, true);
                }
            }
            MacroEventType::KeyUp => {
                down_keys.remove(&e.key_code);
                input::key(e.key_code as u32, false);
            }
        }
    }
    true
}

fn wait_cancellable(
    ms: u64,
    stop: &AtomicBool,
    pause: &AtomicBool,
    on_pause: &dyn Fn(bool),
) -> bool {
    if ms == 0 {
        return !stop.load(Ordering::Relaxed);
    }
    let start = Instant::now();
    let mut paused = pause.load(Ordering::Relaxed);
    let mut pause_start: Option<Instant> = None;
    let mut accumulated_pause = Duration::ZERO;

    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now_paused = pause.load(Ordering::Relaxed);
        if now_paused != paused {
            on_pause(now_paused);
            paused = now_paused;
        }
        if now_paused {
            if pause_start.is_none() {
                pause_start = Some(Instant::now());
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        } else if let Some(ps) = pause_start.take() {
            accumulated_pause += ps.elapsed();
        }

        let elapsed = start.elapsed() - accumulated_pause;
        if elapsed.as_millis() as u64 >= ms {
            return !stop.load(Ordering::Relaxed);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn release_stuck_input(
    down_keys: &mut std::collections::HashSet<i32>,
    pressed_buttons: &mut std::collections::HashSet<crate::model::MouseButton>,
) {
    for vk in down_keys.drain() {
        input::key(vk as u32, false);
    }
    for btn in pressed_buttons.drain() {
        input::release_button(btn);
    }
    log::debug!("player: stuck input released");
}

// ---------------------------------------------------------------------------
// Timer resolution guard — SAFETY: tiny unsafe wrapper
// ---------------------------------------------------------------------------

struct TimerResolutionGuard {
    period: u32,
}

impl TimerResolutionGuard {
    fn begin(period: u32) -> Self {
        unsafe {
            windows::Win32::Media::timeBeginPeriod(period);
        }
        Self { period }
    }
}

impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Media::timeEndPeriod(self.period);
        }
    }
}

// ---------------------------------------------------------------------------
// Input helpers — SendInput wrappers (isolated unsafe)
// ---------------------------------------------------------------------------

mod input {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEINPUT, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    fn absolute_coords(screen_x: i32, screen_y: i32) -> (i32, i32) {
        // SAFETY: GetSystemMetrics with known SM_* constants is always safe.
        let (v_left, v_top, v_width, v_height) = unsafe {
            let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            // Ensure at least 1 to avoid division by zero; use virtual size directly
            // and clamp later. Using w/h directly (not w-1) avoids off-by-one and zero-div.
            (l, t, w.max(1), h.max(1))
        };
        // Normalise to 0..65535. For single-pixel virtual screen, all coords map to 0.
        let fx = if v_width <= 1 {
            0
        } else {
            ((screen_x - v_left) as i64 * 65535) / (v_width as i64 - 1)
        };
        let fy = if v_height <= 1 {
            0
        } else {
            ((screen_y - v_top) as i64 * 65535) / (v_height as i64 - 1)
        };
        (fx.clamp(0, 65535) as i32, fy.clamp(0, 65535) as i32)
    }

    pub fn move_to(screen_x: i32, screen_y: i32) {
        let (dx, dy) = absolute_coords(screen_x, screen_y);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn button(button: crate::model::MouseButton, down: bool, x: i32, y: i32) {
        use crate::model::MouseButton as MB;
        let flag = match (button, down) {
            (MB::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MB::Left, false) => MOUSEEVENTF_LEFTUP,
            (MB::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MB::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MB::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MB::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            (MB::X1, true) | (MB::X2, true) => MOUSEEVENTF_XDOWN,
            (MB::X1, false) | (MB::X2, false) => MOUSEEVENTF_XUP,
            _ => MOUSEEVENTF_MOVE,
        };
        let data = match button {
            MB::X1 => 1,
            MB::X2 => 2,
            _ => 0,
        };
        let (dx, dy) = absolute_coords(x, y);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn wheel(delta: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    /// Release a mouse button without moving the cursor (for stuck-input cleanup).
    pub fn release_button(button: crate::model::MouseButton) {
        use crate::model::MouseButton as MB;
        let flag = match button {
            MB::Left => MOUSEEVENTF_LEFTUP,
            MB::Right => MOUSEEVENTF_RIGHTUP,
            MB::Middle => MOUSEEVENTF_MIDDLEUP,
            MB::X1 | MB::X2 => MOUSEEVENTF_XUP,
            MB::None => return,
        };
        let data = match button {
            MB::X1 => 1,
            MB::X2 => 2,
            _ => 0,
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn key(vk: u32, down: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk as u16),
                    wScan: 0,
                    dwFlags: if down { Default::default() } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    fn send(input: INPUT) {
        // SAFETY: SendInput takes a raw pointer + size, copies synchronously.
        unsafe {
            let sent = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            if sent == 0 {
                let err = std::io::Error::last_os_error();
                log::warn!("SendInput failed: {}", err);
            }
        }
    }
}
