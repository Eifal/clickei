//! hooks.rs — Low-level mouse & keyboard hooks (WH_MOUSE_LL / WH_KEYBOARD_LL).
//!
//! All `unsafe` Win32 interaction is isolated here. The rest of the app (recorder,
//! player, UI) only sees a safe channel of `CapturedEvent`s.
//!
//! # Architecture vs C# parity
//! C# `MacroRecorder` installed hooks from the WinForms UI thread and handled
//! callbacks directly on that thread. In Rust we cannot use a closure as the
//! `extern "system"` hook proc — it must be a plain function pointer with no
//! captures. So we bridge via a global `OnceLock<Mutex<Option<Sender>>>`.
//!
//! The recorder owns the `HookHandles` (RAII) and drains the `mpsc::Receiver`.
//! Filtering (dedup mouse moves, suppress auto-repeat, suppress hotkey combos) is
//! done in the safe `recorder.rs` layer on the receiver side, not here. This
//! keeps the hook callbacks *tiny and infallible*: they only try to `send` and
//! immediately call `CallNextHookEx`. If the channel is full/disconnected we
//! silently drop the event — never block inside the hook (would freeze input!).
//!
//! # Safety invariants
//! - Hook procs are `extern "system" fn` with no captures, matching `HOOKPROC` ABI.
//! - `lParam` points to `KBDLLHOOKSTRUCT` / `MSLLHOOKSTRUCT` owned by the OS; we
//!   only read via `ptr::read_unaligned` inside the unsafe block, never write.
//! - `SetWindowsHookExW` / `UnhookWindowsHookEx` / `CallNextHookEx` are `unsafe`
//!   because they dereference function pointers and touch global hook state. We
//!   guard them with narrow unsafe blocks and store the returned `HHOOK`.
//! - The global `HOOK_SENDER` is `Mutex<Option<Sender>>` — lock ordering is trivial
//!   (only one lock). We never hold the lock across Win32 calls.
//! - Hooks are installed with `hMod = null, threadId = 0` (global low-level hooks)
//!   — documented to work without a DLL.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Mutex, OnceLock,
};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, HC_ACTION, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

// ---------------------------------------------------------------------------
// Public captured event — what the recorder receives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CapturedEvent {
    MouseMove { x: i32, y: i32 },
    MouseDown { button: crate::model::MouseButton, x: i32, y: i32 },
    MouseUp { button: crate::model::MouseButton, x: i32, y: i32 },
    MouseWheel { delta: i32 },
    KeyDown { vk: i32, scan_code: u32 },
    KeyUp { vk: i32, scan_code: u32 },
}

// ---------------------------------------------------------------------------
// Low-level hook structs — raw Win32 layout, #[repr(C)]
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct MsLlHookStruct {
    pt: Point,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

// ---------------------------------------------------------------------------
// Global channel bridge — the only shared mutable state touched by hook procs
// ---------------------------------------------------------------------------

static HOOK_SENDER: OnceLock<Mutex<Option<Sender<CapturedEvent>>>> = OnceLock::new();

fn hook_sender() -> &'static Mutex<Option<Sender<CapturedEvent>>> {
    HOOK_SENDER.get_or_init(|| Mutex::new(None))
}

// ---------------------------------------------------------------------------
// Hook procs — must be `extern "system"` with no captures
// ---------------------------------------------------------------------------

/// Low-level keyboard hook proc.
///
/// # Safety
/// Called by the OS with `nCode`, `wParam` (WM_KEY*), `lParam` (*KBDLLHOOKSTRUCT).
/// We must not panic, must not block, and must call `CallNextHookEx`.
unsafe extern "system" fn low_level_keyboard_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code == HC_ACTION as i32 {
        let vk_msg = w_param.0 as u32;
        let is_down = vk_msg == WM_KEYDOWN || vk_msg == WM_SYSKEYDOWN;
        let is_up = vk_msg == WM_KEYUP || vk_msg == WM_SYSKEYUP;

        if is_down || is_up {
            // SAFETY: lParam is a valid *KBDLLHOOKSTRUCT for HC_ACTION.
            let info = unsafe { std::ptr::read_unaligned(l_param.0 as *const KbdLlHookStruct) };
            let vk = info.vk_code as i32;
            let scan = info.scan_code;

            let event = if is_down {
                CapturedEvent::KeyDown { vk, scan_code: scan }
            } else {
                CapturedEvent::KeyUp { vk, scan_code: scan }
            };

            // Try to forward — never block. If receiver is gone or full, drop.
            if let Ok(guard) = hook_sender().lock() {
                if let Some(sender) = guard.as_ref() {
                    let _ = sender.send(event);
                }
            }
        }
    }

    // SAFETY: CallNextHookEx with null HHOOK walks the chain.
    unsafe { CallNextHookEx(HHOOK::default(), n_code, w_param, l_param) }
}

unsafe extern "system" fn low_level_mouse_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code == HC_ACTION as i32 {
        let msg = w_param.0 as u32;
        // SAFETY: lParam is *MSLLHOOKSTRUCT for HC_ACTION
        let info = unsafe { std::ptr::read_unaligned(l_param.0 as *const MsLlHookStruct) };
        let x = info.pt.x;
        let y = info.pt.y;

        let event_opt: Option<CapturedEvent> = match msg {
            m if m == WM_MOUSEMOVE => Some(CapturedEvent::MouseMove { x, y }),
            m if m == WM_LBUTTONDOWN => Some(CapturedEvent::MouseDown {
                button: crate::model::MouseButton::Left,
                x,
                y,
            }),
            m if m == WM_LBUTTONUP => Some(CapturedEvent::MouseUp {
                button: crate::model::MouseButton::Left,
                x,
                y,
            }),
            m if m == WM_RBUTTONDOWN => Some(CapturedEvent::MouseDown {
                button: crate::model::MouseButton::Right,
                x,
                y,
            }),
            m if m == WM_RBUTTONUP => Some(CapturedEvent::MouseUp {
                button: crate::model::MouseButton::Right,
                x,
                y,
            }),
            m if m == WM_MBUTTONDOWN => Some(CapturedEvent::MouseDown {
                button: crate::model::MouseButton::Middle,
                x,
                y,
            }),
            m if m == WM_MBUTTONUP => Some(CapturedEvent::MouseUp {
                button: crate::model::MouseButton::Middle,
                x,
                y,
            }),
            m if m == WM_XBUTTONDOWN || m == WM_XBUTTONUP => {
                let xbutton = (info.mouse_data >> 16) as u16;
                let btn_opt = match xbutton {
                    1 => Some(crate::model::MouseButton::X1),
                    2 => Some(crate::model::MouseButton::X2),
                    _ => None,
                };
                match btn_opt {
                    Some(btn) => {
                        if msg == WM_XBUTTONDOWN {
                            Some(CapturedEvent::MouseDown { button: btn, x, y })
                        } else {
                            Some(CapturedEvent::MouseUp { button: btn, x, y })
                        }
                    }
                    None => None,
                }
            }
            m if m == WM_MOUSEWHEEL => {
                let delta = ((info.mouse_data >> 16) as u16) as i16 as i32;
                Some(CapturedEvent::MouseWheel { delta })
            }
            _ => None,
        };

        if let Some(ev) = event_opt {
            if let Ok(guard) = hook_sender().lock() {
                if let Some(sender) = guard.as_ref() {
                    let _ = sender.send(ev);
                }
            }
        }
    }

    unsafe { CallNextHookEx(HHOOK::default(), n_code, w_param, l_param) }
}

// ---------------------------------------------------------------------------
// Panic triple-Tap Esc — global emergency stop (WH_KEYBOARD_LL, hardcoded)
// ---------------------------------------------------------------------------

static ESC_PRESS_TIMES: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
static PANIC_TRIGGERED: AtomicBool = AtomicBool::new(false);

const TRIPLE_TAP_WINDOW_MS: u32 = 600;
const VK_ESCAPE: i32 = 0x1B;

fn esc_press_times() -> &'static Mutex<Vec<u32>> {
    ESC_PRESS_TIMES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Check if Esc has been pressed 3x within TRIPLE_TAP_WINDOW_MS.
/// Must be called ONLY on key-DOWN for VK_ESCAPE. Returns true if panic triggered.
fn check_panic_triple_tap() -> bool {
    let now = tick_count();
    let mut times = esc_press_times().lock().unwrap();
    times.push(now);
    times.retain(|&t| now.wrapping_sub(t) <= TRIPLE_TAP_WINDOW_MS);
    if times.len() >= 3 {
        times.clear();
        true
    } else {
        false
    }
}

/// Called by App::update each frame to drain the panic flag (global, tab-independent).
pub fn take_panic_triggered() -> bool {
    PANIC_TRIGGERED.swap(false, Ordering::Relaxed)
}

fn trigger_panic() {
    PANIC_TRIGGERED.store(true, Ordering::Relaxed);
    log::warn!("⚠ Emergency stop triggered (triple Esc)");
}

/// Low-level keyboard proc SOLELY for panic detection (global, lifetime = app).
/// Does NOT forward to channel, does NOT swallow Esc — always CallNextHookEx.
unsafe extern "system" fn panic_keyboard_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code == HC_ACTION as i32 {
        let vk_msg = w_param.0 as u32;
        let is_down = vk_msg == WM_KEYDOWN || vk_msg == WM_SYSKEYDOWN;
        if is_down {
            let info = unsafe { std::ptr::read_unaligned(l_param.0 as *const KbdLlHookStruct) };
            if info.vk_code as i32 == VK_ESCAPE {
                if check_panic_triple_tap() {
                    trigger_panic();
                }
            }
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), n_code, w_param, l_param) }
}

/// RAII guard for panic hook (lifetime = app).
pub struct PanicHookGuard {
    hook: Option<HHOOK>,
}
unsafe impl Send for PanicHookGuard {}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(h) = self.hook.take() {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }
}

/// Install global panic hook (WH_KEYBOARD_LL) once at startup. Must be held for app lifetime.
pub fn install_panic_hook() -> Result<PanicHookGuard, String> {
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(panic_keyboard_proc), HINSTANCE::default(), 0) }
        .map_err(|e| format!("panic hook SetWindowsHookEx failed: {:?}", e))?;
    log::info!("panic hook installed: {:?}", hook);
    Ok(PanicHookGuard { hook: Some(hook) })
}

// Helpers exposed for tests (pure logic, no hook needed)
#[cfg(test)]
pub fn panic_test_push_and_check(now: u32) -> bool {
    let mut times = esc_press_times().lock().unwrap();
    times.push(now);
    times.retain(|&t| now.wrapping_sub(t) <= TRIPLE_TAP_WINDOW_MS);
    if times.len() >= 3 {
        times.clear();
        true
    } else {
        false
    }
}

#[cfg(test)]
pub fn panic_test_clear() {
    if let Some(m) = ESC_PRESS_TIMES.get() {
        if let Ok(mut g) = m.lock() {
            g.clear();
        }
    }
    PANIC_TRIGGERED.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// RAII handle — unhooks on drop
// ---------------------------------------------------------------------------

/// RAII guard for the two low-level hooks. Drop unhooks automatically.
///
/// Install via `install_hooks()` which returns `(HookHandles, Receiver)`.
pub struct HookHandles {
    mouse: Option<HHOOK>,
    keyboard: Option<HHOOK>,
}

// HHOOK is an opaque handle; safe to send between threads for Unhook.
unsafe impl Send for HookHandles {}

impl Drop for HookHandles {
    fn drop(&mut self) {
        // SAFETY: UnhookWindowsHookEx is unsafe because it dereferences the handle.
        // We own the handles and only unhook once. Failure is ignored.
        unsafe {
            if let Some(h) = self.mouse.take() {
                let _ = UnhookWindowsHookEx(h);
            }
            if let Some(h) = self.keyboard.take() {
                let _ = UnhookWindowsHookEx(h);
            }
        }
        // Clear the global sender so late hook callbacks (racing drop) just drop events.
        if let Some(m) = HOOK_SENDER.get() {
            if let Ok(mut g) = m.lock() {
                *g = None;
            }
        }
    }
}

/// Install both low-level hooks and return the RAII guard plus the event receiver.
///
/// The returned `Receiver<CapturedEvent>` yields every raw input sample; the
/// `recorder.rs` layer does filtering/dedup/suppression. The hooks stay active
/// until `HookHandles` is dropped.
///
/// # Errors
/// Returns a string if either `SetWindowsHookExW` fails (e.g. UIPI block).
pub fn install_hooks() -> Result<(HookHandles, Receiver<CapturedEvent>), String> {
    // Prevent double-install which would orphan previous sender/receiver.
    {
        let guard = hook_sender().lock().map_err(|_| "hook sender mutex poisoned".to_string())?;
        if guard.is_some() {
            return Err("hooks already installed — drop previous HookHandles first".to_string());
        }
    }
    let (tx, rx) = mpsc::channel();

    // Publish the sender globally so hook procs can find it.
    {
        let mut guard = hook_sender().lock().map_err(|_| "hook sender mutex poisoned".to_string())?;
        *guard = Some(tx);
    }

    // SAFETY: SetWindowsHookExW is unsafe because it stores the function pointer
    // and will call it from the OS. We pass WH_MOUSE_LL / WH_KEYBOARD_LL, our
    // `extern "system"` procs, hMod = null, threadId = 0 (global).
    let mouse_res = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), HINSTANCE::default(), 0) };
    let keyboard_res = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), HINSTANCE::default(), 0) };

    match (mouse_res, keyboard_res) {
        (Ok(mouse_hook), Ok(keyboard_hook)) => Ok((
            HookHandles {
                mouse: Some(mouse_hook),
                keyboard: Some(keyboard_hook),
            },
            rx,
        )),
        (mouse_res, keyboard_res) => {
            // Partial failure: unhook survivor if any.
            if let Ok(h) = mouse_res {
                unsafe { let _ = UnhookWindowsHookEx(h); }
            }
            if let Ok(h) = keyboard_res {
                unsafe { let _ = UnhookWindowsHookEx(h); }
            }
            if let Ok(mut g) = hook_sender().lock() {
                *g = None;
            }
            let err = std::io::Error::last_os_error();
            Err(format!("SetWindowsHookEx failed: {}", err))
        }
    }
}

/// Convenience alias — installs hooks (global low-level hooks dispatch regardless of pump
/// on modern Windows, so this is same as `install_hooks` for now).
pub fn install_hooks_with_pump() -> Result<(HookHandles, Receiver<CapturedEvent>), String> {
    install_hooks()
}

// ---------------------------------------------------------------------------
// Shared input/window helpers — reused by player.rs and static_clicker.rs
// No duplicated P/Invoke: all via `windows` crate.
// ---------------------------------------------------------------------------

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetCursorPos, GetWindowTextW, IsWindow, PostMessageW, WindowFromPoint, GA_ROOT,
};

/// Safe HWND wrapper for Send across threads (HWND is *mut c_void !Send).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeHwnd(pub HWND);
unsafe impl Send for SafeHwnd {}
unsafe impl Sync for SafeHwnd {}

impl SafeHwnd {
    pub fn is_valid(self) -> bool {
        unsafe { IsWindow(self.0).as_bool() }
    }
    pub fn title(self) -> String {
        get_window_title(self.0)
    }
    pub fn as_hwnd(self) -> HWND {
        self.0
    }
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    let ok = unsafe { GetCursorPos(&mut pt) };
    if ok.is_ok() {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

pub fn absolute_coords(screen_x: i32, screen_y: i32) -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let (v_left, v_top, v_width, v_height) = unsafe {
        let l = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let t = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (l, t, w.max(1), h.max(1))
    };
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

pub fn send_mouse_move_absolute(screen_x: i32, screen_y: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    };
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
    unsafe {
        let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Send a complete click sequence (move to target + down + up [+ down + up for double])
/// in a SINGLE SendInput call. This prevents the restore-move from racing with the up-event.
pub fn send_mouse_click_sequence(button: crate::model::MouseButton, is_double: bool, target_x: i32, target_y: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    };
    use crate::model::MouseButton as MB;

    if button == MB::None {
        return;
    }

    let (dx, dy) = absolute_coords(target_x, target_y);

    let (down_flag, up_flag, xbutton_data) = match button {
        MB::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        MB::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        MB::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        MB::X1 | MB::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, if button == MB::X1 { 1 } else { 2 }),
        MB::None => unreachable!(),
    };

    let base_flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

    // Stack array — zero heap allocation per tick (was Vec::with_capacity,
    // ~1000 allocs/sec at 1ms interval). Layout: MOVE + DOWN + UP [+ DOWN + UP].
    let mut inputs = [INPUT::default(); 5];
    let mut n = 0usize;

    // 1. Move to target
    inputs[n] = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: base_flags | MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    n += 1;

    // 2. Down
    inputs[n] = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: xbutton_data,
                dwFlags: base_flags | down_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    n += 1;

    // 3. Up
    inputs[n] = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: xbutton_data,
                dwFlags: base_flags | up_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    n += 1;

    // 4-5. Double click: down + up again
    if is_double {
        inputs[n] = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: xbutton_data,
                    dwFlags: base_flags | down_flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        n += 1;
        inputs[n] = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: xbutton_data,
                    dwFlags: base_flags | up_flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        n += 1;
    }

    // Single SendInput call with the entire sequence
    unsafe {
        let _ = SendInput(&inputs[..n], std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn window_from_point(x: i32, y: i32) -> HWND {
    unsafe { WindowFromPoint(POINT { x, y }) }
}

pub fn ancestor_root(hwnd: HWND) -> HWND {
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

pub fn screen_to_client(hwnd: HWND, x: i32, y: i32) -> Option<(i32, i32)> {
    let mut pt = POINT { x, y };
    let ok = unsafe { ScreenToClient(hwnd, &mut pt) };
    if ok.as_bool() {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

pub fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd).as_bool() }
}

pub fn get_window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len == 0 {
        format!("HWND 0x{:X}", hwnd.0 as usize)
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

pub fn post_click(hwnd: HWND, button: crate::model::MouseButton, client_x: i32, client_y: i32, down: bool) -> bool {
    // wParam should carry MK_* flag for down, 0 for up — some apps check this
    let (msg, wparam_val) = match (button, down) {
        (crate::model::MouseButton::Left, true) => (WM_LBUTTONDOWN, 0x0001), // MK_LBUTTON
        (crate::model::MouseButton::Left, false) => (WM_LBUTTONUP, 0x0000),
        (crate::model::MouseButton::Right, true) => (WM_RBUTTONDOWN, 0x0002), // MK_RBUTTON
        (crate::model::MouseButton::Right, false) => (WM_RBUTTONUP, 0x0000),
        (crate::model::MouseButton::Middle, true) => (WM_MBUTTONDOWN, 0x0010), // MK_MBUTTON
        (crate::model::MouseButton::Middle, false) => (WM_MBUTTONUP, 0x0000),
        _ => return false,
    };
    let lparam = ((client_y as u32) << 16) | (client_x as u16 as u32);
    let wparam = windows::Win32::Foundation::WPARAM(wparam_val);
    let lparam = windows::Win32::Foundation::LPARAM(lparam as isize);
    let ok = unsafe { PostMessageW(hwnd, msg, wparam, lparam) };
    ok.is_ok()
}

pub fn send_click_message(hwnd: HWND, button: crate::model::MouseButton, client_x: i32, client_y: i32, down: bool) -> bool {
    // Synchronous SendMessage fallback — some windows only handle SendMessage, not Post
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let (msg, wparam_val) = match (button, down) {
        (crate::model::MouseButton::Left, true) => (WM_LBUTTONDOWN, 0x0001),
        (crate::model::MouseButton::Left, false) => (WM_LBUTTONUP, 0x0000),
        (crate::model::MouseButton::Right, true) => (WM_RBUTTONDOWN, 0x0002),
        (crate::model::MouseButton::Right, false) => (WM_RBUTTONUP, 0x0000),
        (crate::model::MouseButton::Middle, true) => (WM_MBUTTONDOWN, 0x0010),
        (crate::model::MouseButton::Middle, false) => (WM_MBUTTONUP, 0x0000),
        _ => return false,
    };
    let lparam = ((client_y as u32) << 16) | (client_x as u16 as u32);
    unsafe {
        SendMessageW(hwnd, msg, windows::Win32::Foundation::WPARAM(wparam_val), windows::Win32::Foundation::LPARAM(lparam as isize));
    }
    true
}

pub fn is_hotkey_pressed(combo: &crate::config::HotkeyCombo) -> bool {
    if combo.key == 0 {
        return false;
    }
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        let check = |vk: i32| GetAsyncKeyState(vk) as u16 & 0x8000 != 0;
        let ctrl = check(0x11) || check(0xA2) || check(0xA3);
        let shift = check(0x10) || check(0xA0) || check(0xA1);
        let alt = check(0x12) || check(0xA4) || check(0xA5);
        let win = check(0x5B) || check(0x5C);
        let need = |flag: i32, down: bool| {
            if combo.modifiers & flag != 0 { down } else { !down }
        };
        if !need(crate::config::mod_flag::CONTROL, ctrl) { return false; }
        if !need(crate::config::mod_flag::SHIFT, shift) { return false; }
        if !need(crate::config::mod_flag::ALT, alt) { return false; }
        if !need(crate::config::mod_flag::WIN, win) { return false; }
        check(combo.key)
    }
}

pub fn find_tinytask_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::w;
    unsafe {
        let hwnd = FindWindowW(None, w!("Clickei")).unwrap_or_default();
        if hwnd.0.is_null() { None } else { Some(hwnd) }
    }
}

pub fn is_point_in_tinytask(x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    if let Some(hwnd) = find_tinytask_window() {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
        }
    }
    false
}

pub fn find_sequence_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::w;
    unsafe {
        let hwnd = FindWindowW(None, w!("Sequence Clicking")).unwrap_or_default();
        if hwnd.0.is_null() { None } else { Some(hwnd) }
    }
}

pub fn is_point_in_sequence_window(x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    if let Some(hwnd) = find_sequence_window() {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
        }
    }
    false
}

pub fn is_point_in_app_window(x: i32, y: i32) -> bool {
    is_point_in_tinytask(x, y) || is_point_in_sequence_window(x, y)
}

pub fn find_window_by_title(title: &str) -> Option<SafeHwnd> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::{HSTRING, PCWSTR};
    if title.is_empty() {
        return None;
    }
    let htitle = HSTRING::from(title);
    let hwnd = unsafe { FindWindowW(None, PCWSTR(htitle.as_ptr())).unwrap_or_default() };
    if hwnd.0.is_null() {
        None
    } else {
        Some(SafeHwnd(hwnd))
    }
}

pub fn get_foreground_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() { None } else { Some(hwnd) }
}

pub fn set_foreground_window(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    unsafe { SetForegroundWindow(hwnd).as_bool() }
}

// ---------------------------------------------------------------------------
// Collision guard — real (hardware) input vs our injected SendInput.
// SendInput ALSO refreshes GetLastInputInfo, so we track the tick-count window
// of our own injections and discount it when asking "did the user act recently?".
// ---------------------------------------------------------------------------

use std::sync::atomic::AtomicU32;

static OWN_INJECT_START: AtomicU32 = AtomicU32::new(0);
static OWN_INJECT_END: AtomicU32 = AtomicU32::new(0);

pub fn tick_count() -> u32 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    unsafe { GetTickCount() }
}

/// Record [start, end] GetTickCount window covering an injection burst
/// (click sequence + restore move) so `real_input_recently` can ignore it.
pub fn note_own_injection(start: u32, end: u32) {
    OWN_INJECT_START.store(start, Ordering::Relaxed);
    OWN_INJECT_END.store(end, Ordering::Relaxed);
}

/// True if the given mouse button is PHYSICALLY held down by the user right now.
/// Note: injected SendInput also flips this, but our own down+up completes inside
/// one call before we ever check again, so at guard time only real presses show.
pub fn is_real_button_down(button: crate::model::MouseButton) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use crate::model::MouseButton as MB;
    let vk: i32 = match button {
        MB::Left => 0x01,    // VK_LBUTTON
        MB::Right => 0x02,   // VK_RBUTTON
        MB::Middle => 0x04,  // VK_MBUTTON
        MB::X1 => 0x05,      // VK_XBUTTON1
        MB::X2 => 0x06,      // VK_XBUTTON2
        MB::None => return false,
    };
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

/// True if NON-injected input happened within `threshold_ms`.
///
/// GetLastInputInfo covers ALL input including our own SendInput, so a recent
/// timestamp falling inside our recorded injection window is treated as ours,
/// not the user's. Unreadable state returns false (never block clicking).
pub fn real_input_recently(threshold_ms: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    let input_ts = unsafe {
        if !GetLastInputInfo(&mut info).as_bool() {
            return false;
        }
        info.dwTime
    };

    let now = tick_count();
    let idle = now.wrapping_sub(input_ts);
    if idle >= threshold_ms {
        return false; // system idle long enough — safe to inject
    }

    // Recent input seen. Was it our own injection? (window + 5ms timer slack)
    let s = OWN_INJECT_START.load(Ordering::Relaxed);
    let e = OWN_INJECT_END.load(Ordering::Relaxed);
    if e != 0 {
        let rel = input_ts.wrapping_sub(s) as i64;
        let span = e.wrapping_sub(s) as i64;
        if rel >= 0 && rel <= span + 5 {
            return false; // that was us — user is idle
        }
    }
    true // recent REAL input — user is active
}

#[cfg(test)]
mod panic_tests {
    use super::*;

    #[test]
    fn triple_tap_within_window_triggers() {
        panic_test_clear();
        assert!(!panic_test_push_and_check(1000));
        assert!(!panic_test_push_and_check(1200));
        // 3rd within 600ms from first (1000 -> 1300 = 300ms)
        assert!(panic_test_push_and_check(1300));
        // after trigger, buffer cleared so next single should not trigger
        assert!(!panic_test_push_and_check(1350));
        panic_test_clear();
    }

    #[test]
    fn slow_taps_do_not_trigger() {
        panic_test_clear();
        assert!(!panic_test_push_and_check(0));
        assert!(!panic_test_push_and_check(700)); // >600 from 0, so 0 evicted
        assert!(!panic_test_push_and_check(1400)); // again >600
        // only 1 in window, not 3
        assert!(!panic_test_push_and_check(2000));
        panic_test_clear();
    }

    #[test]
    fn wrapping_tick_count() {
        panic_test_clear();
        let near_max = u32::MAX - 200;
        assert!(!panic_test_push_and_check(near_max));
        assert!(!panic_test_push_and_check(near_max.wrapping_add(200))); // +200
        assert!(panic_test_push_and_check(near_max.wrapping_add(400))); // within 600 of first via wrapping
        panic_test_clear();
    }

    #[test]
    fn take_panic_triggered_drains() {
        PANIC_TRIGGERED.store(false, Ordering::Relaxed);
        trigger_panic();
        assert!(take_panic_triggered());
        assert!(!take_panic_triggered());
        panic_test_clear();
    }
}
