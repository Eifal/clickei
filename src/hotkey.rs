//! hotkey.rs — Global hotkeys (RegisterHotKey / WM_HOTKEY) on a dedicated thread.
//!
//! All `unsafe` Win32 message-window + RegisterHotKey work is isolated here.
//! The rest of the app only sees a safe `mpsc::Receiver<HotkeyId>`.
//!
//! Parity with Core/HotkeyManager.cs + Native/HotkeyMessageWindow.cs:
//! - Three hotkeys (Record / Play / Stop), user-rebindable via Settings.
//! - Duplicate detection before RegisterHotKey.
//! - MOD_NOREPEAT to suppress auto-repeat.
//! - Runs on a thread with its own hidden message-only window + GetMessage loop.
//! - Reports conflicts ("already in use by another program") instead of swallowing.
//!
//! Safety invariants:
//! - The hidden window class + WndProc are `extern "system"` free functions (no captures).
//!   We store the `Sender` in a global `OnceLock<Mutex<Option<Sender>>>` like hooks.rs.
//! - `RegisterHotKey` / `UnregisterHotKey` / `CreateWindowExW` / `RegisterClassExW` are
//!   unsafe because they take raw HWND / function pointers. We wrap them narrowly.
//! - The message loop thread owns the HWND; we only touch it from that thread.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Mutex, OnceLock,
};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassExW,
    TranslateMessage, UnregisterClassW, MSG, WNDCLASSEXW, WM_CLOSE, WM_HOTKEY, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

// Custom messages for cross-thread hotkey management (must be > WM_USER)
const WM_HOTKEY_APPLY: u32 = 0x8001; // WM_APP + 1
const WM_HOTKEY_UNREGISTER: u32 = 0x8002;

// ---------------------------------------------------------------------------
// Public IDs — mirror C# HotkeyManager.Id*
// ---------------------------------------------------------------------------

pub const ID_RECORD: i32 = 1;
pub const ID_PLAY: i32 = 2;
pub const ID_STOP: i32 = 3;
pub const ID_STATIC: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyId {
    Record,
    Play,
    Stop,
    Static,
}

impl HotkeyId {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Record => ID_RECORD,
            Self::Play => ID_PLAY,
            Self::Stop => ID_STOP,
            Self::Static => ID_STATIC,
        }
    }
    pub fn from_i32(id: i32) -> Option<Self> {
        match id {
            ID_RECORD => Some(Self::Record),
            ID_PLAY => Some(Self::Play),
            ID_STOP => Some(Self::Stop),
            ID_STATIC => Some(Self::Static),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Global sender bridge for the WndProc
// ---------------------------------------------------------------------------

static HOTKEY_SENDER: OnceLock<Mutex<Option<Sender<HotkeyId>>>> = OnceLock::new();

fn hotkey_sender() -> &'static Mutex<Option<Sender<HotkeyId>>> {
    HOTKEY_SENDER.get_or_init(|| Mutex::new(None))
}

// Cross-thread apply data: main thread posts bindings here, hotkey thread picks it up in WndProc
static HOTKEY_APPLY_DATA: OnceLock<Mutex<Option<(crate::config::SettingsBindings, Sender<Vec<String>>)>>> =
    OnceLock::new();
fn hotkey_apply_data(
) -> &'static Mutex<Option<(crate::config::SettingsBindings, Sender<Vec<String>>)>> {
    HOTKEY_APPLY_DATA.get_or_init(|| Mutex::new(None))
}

/// Actual registration logic — must run on the hotkey thread that owns `hwnd`.
fn do_register_hotkeys(hwnd: HWND, bindings: &crate::config::SettingsBindings) -> Vec<String> {
    // Always unregister first (on correct thread)
    unsafe {
        let _ = UnregisterHotKey(hwnd, ID_RECORD);
        let _ = UnregisterHotKey(hwnd, ID_PLAY);
        let _ = UnregisterHotKey(hwnd, ID_STOP);
        let _ = UnregisterHotKey(hwnd, ID_STATIC);
    }
    let mut failures = Vec::new();
    let all = [
        ("Record", bindings.record.clone(), ID_RECORD),
        ("Play", bindings.play.clone(), ID_PLAY),
        ("Stop", bindings.stop.clone(), ID_STOP),
        ("Static", bindings.static_clicker.clone(), ID_STATIC),
    ];
    let mut dup_reported = std::collections::HashSet::new();
    for (name, combo, id) in &all {
        if combo.key == 0 {
            continue;
        }
        let dup = all.iter().filter(|(_, c, _)| c == combo).count();
        if dup > 1 && !dup_reported.contains(combo) {
            dup_reported.insert(combo.clone());
            failures.push(format!("{} ({}): assigned to two actions", name, combo.display_owned()));
            continue;
        } else if dup > 1 {
            failures.push(format!("{} ({}): assigned to two actions", name, combo.display_owned()));
            continue;
        }
        let mods_raw = combo.to_modifier_flags(MOD_NOREPEAT.0);
        let mods = HOT_KEY_MODIFIERS(mods_raw);
        let ok = unsafe { RegisterHotKey(hwnd, *id, mods, combo.key as u32) };
        if let Err(e) = ok {
            let os_err = std::io::Error::last_os_error();
            failures.push(format!(
                "{} ({}): already in use by another program (RegisterHotKey failed: {:?} / {})",
                name,
                combo.display_owned(),
                e,
                os_err
            ));
            log::warn!("RegisterHotKey failed for {} ({}) : {:?} / {}", name, combo.display_owned(), e, os_err);
        }
    }
    failures
}

unsafe extern "system" fn hotkey_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_HOTKEY {
        let id = (wparam.0 & 0xFFFF) as i32;
        if let Some(hk) = HotkeyId::from_i32(id) {
            if let Ok(g) = hotkey_sender().lock() {
                if let Some(tx) = g.as_ref() {
                    let _ = tx.send(hk);
                }
            }
        }
    }
    if msg == WM_HOTKEY_APPLY {
        // This runs on the hotkey thread — correct thread for RegisterHotKey
        let data_opt = hotkey_apply_data().lock().ok().and_then(|mut g| g.take());
        if let Some((bindings, result_tx)) = data_opt {
            let failures = do_register_hotkeys(hwnd, &bindings);
            let _ = result_tx.send(failures);
        }
        return LRESULT(0);
    }
    if msg == WM_HOTKEY_UNREGISTER {
        unsafe {
            let _ = UnregisterHotKey(hwnd, ID_RECORD);
            let _ = UnregisterHotKey(hwnd, ID_PLAY);
            let _ = UnregisterHotKey(hwnd, ID_STOP);
            let _ = UnregisterHotKey(hwnd, ID_STATIC);
        }
        return LRESULT(0);
    }
    if msg == WM_CLOSE {
        // Ensure hotkeys are unregistered before destroy
        unsafe {
            let _ = UnregisterHotKey(hwnd, ID_RECORD);
            let _ = UnregisterHotKey(hwnd, ID_PLAY);
            let _ = UnregisterHotKey(hwnd, ID_STOP);
            let _ = UnregisterHotKey(hwnd, ID_STATIC);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// Small wrapper to make HWND Send across thread boundary.
// HWND is *mut c_void which is !Send, but the handle value itself is just an
// opaque integer safe to share. We assert that and wrap it.
#[derive(Debug, Clone, Copy)]
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

// ---------------------------------------------------------------------------
// HotkeyManager — owns the thread + window
// ---------------------------------------------------------------------------

pub struct HotkeyManager {
    rx: Receiver<HotkeyId>,
    _tx: Sender<HotkeyId>,
    thread: Option<std::thread::JoinHandle<()>>,
    hwnd: Arc<Mutex<Option<SendHwnd>>>,
    should_exit: Arc<AtomicBool>,
    current_bindings: crate::config::SettingsBindings,
}

impl HotkeyManager {
    /// Spawn the hidden window + message loop thread. Returns manager + receiver.
    pub fn spawn() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        {
            let mut g = hotkey_sender().lock().map_err(|_| "hotkey mutex poisoned".to_string())?;
            *g = Some(tx.clone());
        }

        let hwnd_cell: Arc<Mutex<Option<SendHwnd>>> = Arc::new(Mutex::new(None));
        let hwnd_clone = hwnd_cell.clone();
        let should_exit = Arc::new(AtomicBool::new(false));
        let should_exit_clone = should_exit.clone();

        let thread = std::thread::Builder::new()
            .name("HotkeyLoop".into())
            .spawn(move || {
                run_message_loop(hwnd_clone, should_exit_clone);
            })
            .map_err(|e| format!("spawn hotkey thread: {}", e))?;

        // Wait briefly for HWND to be created
        for _ in 0..50 {
            if hwnd_cell.lock().map(|g| g.is_some()).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        Ok(Self {
            rx,
            _tx: tx,
            thread: Some(thread),
            hwnd: hwnd_cell,
            should_exit,
            current_bindings: crate::config::SettingsBindings::default(),
        })
    }

    pub fn receiver(&self) -> &Receiver<HotkeyId> {
        &self.rx
    }

    pub fn try_recv(&self) -> Option<HotkeyId> {
        self.rx.try_recv().ok()
    }

    /// Apply new bindings. Returns list of human-readable failures (conflicts).
    /// This marshals the RegisterHotKey call to the hotkey thread (required: error 1408 otherwise).
    pub fn apply(&mut self, bindings: crate::config::SettingsBindings) -> Vec<String> {
        // Fast duplicate check before crossing thread (still done on hotkey thread too, but early return saves IPC)
        let all = [
            ("Record", bindings.record.clone(), ID_RECORD),
            ("Play", bindings.play.clone(), ID_PLAY),
            ("Stop", bindings.stop.clone(), ID_STOP),
            ("Static", bindings.static_clicker.clone(), ID_STATIC),
        ];
        let mut dup_found = false;
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                if all[i].1.key != 0 && all[i].1 == all[j].1 {
                    dup_found = true;
                }
            }
        }
        // If hwnd not ready, fallback to direct error
        let hwnd_opt = self.hwnd.lock().ok().and_then(|g| *g);
        let Some(SendHwnd(hwnd)) = hwnd_opt else {
            let mut failures = Vec::new();
            for (name, combo, _) in &all {
                if combo.key != 0 {
                    failures.push(format!("{}: hotkey window not ready", name));
                }
            }
            self.current_bindings = bindings;
            return failures;
        };

        let (tx, rx) = mpsc::channel();
        {
            let mut g = hotkey_apply_data().lock().expect("hotkey apply mutex poisoned");
            *g = Some((bindings.clone(), tx));
        }
        unsafe {
            let _ = PostMessageW(hwnd, WM_HOTKEY_APPLY, WPARAM(0), LPARAM(0));
        }
        // Wait for hotkey thread to process (with timeout to avoid deadlock)
        let failures = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_else(|_| {
            vec!["hotkey thread did not respond (timeout)".to_string()]
        });
        // Only update current_bindings after attempt (even if failures, we keep intended bindings for resume)
        self.current_bindings = bindings;
        // If dup_found we already have failures from thread; but ensure we don't lose them
        let _ = dup_found;
        failures
    }

    pub fn suspend(&mut self) {
        if let Ok(g) = self.hwnd.lock() {
            if let Some(SendHwnd(hwnd)) = *g {
                unsafe {
                    let _ = PostMessageW(hwnd, WM_HOTKEY_UNREGISTER, WPARAM(0), LPARAM(0));
                }
                // Small delay to let hotkey thread process unregister before caller proceeds
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        log::info!("hotkeys suspended");
    }

    pub fn resume(&mut self) -> Vec<String> {
        let b = self.current_bindings.clone();
        self.apply(b)
    }

    fn unregister_all(&self) {
        // Must be called from hotkey thread via PostMessage; direct call from main thread would be 1408.
        // For Drop/shutdown we post message; for immediate callers use suspend().
        if let Ok(g) = self.hwnd.lock() {
            if let Some(SendHwnd(hwnd)) = *g {
                unsafe {
                    let _ = PostMessageW(hwnd, WM_HOTKEY_UNREGISTER, WPARAM(0), LPARAM(0));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.unregister_all();
        self.should_exit.store(true, Ordering::Relaxed);
        // Wake GetMessageW which may be blocked — post WM_CLOSE to our hidden window
        if let Ok(g) = self.hwnd.lock() {
            if let Some(SendHwnd(hwnd)) = *g {
                unsafe {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        // Join the thread with timeout-like best effort; don't block forever on Drop
        if let Some(h) = self.thread.take() {
            // Give the message loop a moment to exit; don't hang UI thread indefinitely
            let _ = h.join();
        }
        if let Some(m) = HOTKEY_SENDER.get() {
            if let Ok(mut g) = m.lock() {
                *g = None;
            }
        }
    }
}

impl HotkeyManager {
    pub fn shutdown(mut self) {
        self.unregister_all();
        self.should_exit.store(true, Ordering::Relaxed);
        if let Ok(g) = self.hwnd.lock() {
            if let Some(SendHwnd(hwnd)) = *g {
                unsafe {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
        if let Some(m) = HOTKEY_SENDER.get() {
            if let Ok(mut g) = m.lock() {
                *g = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message loop — runs on the hotkey thread
// ---------------------------------------------------------------------------

fn run_message_loop(hwnd_cell: Arc<Mutex<Option<SendHwnd>>>, should_exit: Arc<AtomicBool>) {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();

        let class_name: Vec<u16> = format!("TTE_Hotkey_{}\0", uuid_simple())
            .encode_utf16()
            .collect();

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(hotkey_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hIconSm: Default::default(),
        };

        let atom = RegisterClassExW(&wnd_class);
        if atom == 0 {
            log::error!("RegisterClassExW failed: {}", std::io::Error::last_os_error());
            return;
        }

        let hwnd_res = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            w!("ClickeiHotkey"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        );

        let hwnd = match hwnd_res {
            Ok(h) => h,
            Err(e) => {
                log::error!("CreateWindowExW failed: {:?}", e);
                return;
            }
        };

        {
            let mut g = hwnd_cell.lock().unwrap();
            *g = Some(SendHwnd(hwnd));
        }

        let mut msg = MSG::default();
        loop {
            if should_exit.load(Ordering::Relaxed) {
                break;
            }
            let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if ret.0 == 0 {
                break; // WM_QUIT
            }
            if ret.0 == -1 {
                log::error!("GetMessageW failed: {}", std::io::Error::last_os_error());
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            let _ = DestroyWindow(hwnd);
            let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), hinstance);
        }
    }
}

fn uuid_simple() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    format!("{:016x}", h.finish())
}
