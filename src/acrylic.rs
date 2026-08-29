//! acrylic.rs — DWM acrylic / blur-behind helper.
//!
//! Ports Native/NativeAcrylic.cs. All `unsafe` is isolated here; the rest of the
//! codebase calls the safe wrappers `try_enable_acrylic` / `try_enable_blur_behind`
//! / `try_enable_dark_title_bar` which degrade gracefully on older Windows.
//!
//! # Safety invariants
//! - `hwnd` must be a valid top-level window handle (`HWND`) created on the calling
//!   thread. We never dereference it, only pass it to Win32.
//! - `AccentPolicy` and `WindowCompositionAttributeData` are `#[repr(C)]` and match
//!   the undocumented `SetWindowCompositionAttribute` ABI exactly (validated against
//!   the C# layout and Win32 headers). We allocate them on the stack and pass
//!   pointers with correct `SizeOfData`.
//! - `SetWindowCompositionAttribute` is undocumented but present since Windows 10.
//!   We resolve it dynamically via `GetProcAddress` so missing exports (Wine, very
//!   old Windows) simply return `false` instead of link failure.
//! - `DwmSetWindowAttribute` is loaded from `dwmapi.dll` — also optional.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

// ---------------------------------------------------------------------------
// Raw undocumented structs — must stay #[repr(C)] and match C# exactly.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AccentPolicy {
    accent_state: AccentState,
    accent_flags: i32,
    gradient_color: u32, // ABGR
    animation_id: i32,
}

#[allow(dead_code)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccentState {
    Disabled = 0,
    EnableGradient = 1,
    EnableTransparentGradient = 2,
    EnableBlurBehind = 3,
    EnableAcrylicBlurBehind = 4,
    EnableHostBackdrop = 5,
    InvalidState = 6,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy)]
enum WindowCompositionAttribute {
    WcaAccentPolicy = 19,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: WindowCompositionAttribute,
    data: *mut std::ffi::c_void,
    size_of_data: usize,
}

// Undocumented function signature in user32.dll
type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> i32;

// DWM constants (from dwmapi.h / winuser.h) — stable across Windows 10/11.
const DWMWA_USE_IMMERSIVE_DARK_MODE_BEFORE_20H1: i32 = 19;
const DWMWA_USE_IMMERSIVE_DARK_MODE: i32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: i32 = 33;
const DWMWCP_ROUND: i32 = 2;

/// Try to enable Acrylic blur behind `hwnd` with a tint color.
///
/// `tint_rgb` is 0xRRGGBB, `opacity` is 0..255 (175 is the C# default).
/// Returns `true` on success, `false` on any failure (window handle invalid,
/// OS too old, DWM disabled, etc.). Never panics.
pub fn try_enable_acrylic(hwnd: HWND, tint_rgb: u32, opacity: u8) -> bool {
    if hwnd.0.is_null() {
        return false;
    }

    // ABGR packing for GradientColor: A << 24 | B << 16 | G << 8 | R
    let r = tint_rgb & 0xFF;
    let g = (tint_rgb >> 8) & 0xFF;
    let b = (tint_rgb >> 16) & 0xFF;
    let abgr = ((opacity as u32) << 24) | (b << 16) | (g << 8) | r;

    let mut policy = AccentPolicy {
        accent_state: AccentState::EnableAcrylicBlurBehind,
        accent_flags: 2, // draw all borders + acrylic blend
        gradient_color: abgr,
        animation_id: 0,
    };

    // SAFETY:
    // - `policy` lives on the stack for the duration of the call.
    // - `data` points at `policy` with correct size.
    // - `SetWindowCompositionAttribute` is called with a valid HWND; if the
    //   function pointer is missing we just return false.
    let ok = unsafe { call_set_window_composition_attribute(hwnd, &mut policy) };

    if ok {
        // Best-effort: also enable dark title bar + rounded corners.
        let _ = try_enable_dark_title_bar(hwnd);
        return true;
    }

    // Fallback to plain blur-behind if acrylic failed (older build).
    try_enable_blur_behind(hwnd)
}

/// Fallback: plain blur-behind (no tint) for older Windows 10 builds.
pub fn try_enable_blur_behind(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut policy = AccentPolicy {
        accent_state: AccentState::EnableBlurBehind,
        accent_flags: 0,
        gradient_color: 0,
        animation_id: 0,
    };
    // SAFETY: same invariants as above.
    let ok = unsafe { call_set_window_composition_attribute(hwnd, &mut policy) };
    if ok {
        let _ = try_enable_dark_title_bar(hwnd);
    }
    ok
}

/// Enable immersive dark title bar + rounded corners (Windows 10 20H1+ / 11).
/// Gracefully ignores failures (e.g. Windows 7/8 where dwmapi is missing).
pub fn try_enable_dark_title_bar(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }

    // SAFETY: DwmSetWindowAttribute takes a pointer to an `i32` and its size.
    // We pass a stack `i32` with `size_of::<i32>()`. The HWND is validated above.
    // Failure (non-zero HRESULT) is not treated as panic — we try the older enum
    // value as fallback.
    unsafe {
        let mut use_dark: i32 = 1;
        let res = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE),
            &mut use_dark as *mut i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        if res.is_err() {
            // Try pre-20H1 value (19) before giving up.
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE_BEFORE_20H1),
                &mut use_dark as *mut i32 as *const std::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }

        // Rounded corners on Windows 11 — ignore if not supported.
        let mut corner: i32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE),
            &mut corner as *mut i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
    true
}

// ---------------------------------------------------------------------------
// Internal helper — all dynamic-link unsafe work isolated here
// ---------------------------------------------------------------------------

/// Dynamically resolve `SetWindowCompositionAttribute` and invoke it.
///
/// # Safety
/// Caller must ensure `policy` outlives the call and `hwnd` is valid.
/// This function is `unsafe` because it dereferences raw function pointers and
/// passes raw pointers to Win32.
unsafe fn call_set_window_composition_attribute(hwnd: HWND, policy: &mut AccentPolicy) -> bool {
    // user32.dll is always loaded for GUI apps; GetModuleHandle is cheaper and
    // does not leak a reference like LoadLibrary.
    let hmod = match unsafe { GetModuleHandleW(windows::core::w!("user32.dll")) } {
        Ok(h) => h,
        Err(_) => return false,
    };
    if hmod.0.is_null() {
        return false;
    }

    // SAFETY: proc name is a static nul-terminated C string, valid for the call.
    let proc_name = windows::core::s!("SetWindowCompositionAttribute");
    let func_ptr = unsafe { GetProcAddress(hmod, proc_name) };
    let Some(addr) = func_ptr else {
        return false;
    };

    // SAFETY: we transmute the FARPROC to the correct signature. The function
    // is `extern "system"` and takes (HWND, *mut WCA_DATA) -> BOOL (i32).
    // If the signature mismatched the stack would corrupt — but this matches
    // the documented/observed ABI and the C# P/Invoke.
    let func: SetWindowCompositionAttributeFn = unsafe { std::mem::transmute(addr) };

    let mut data = WindowCompositionAttributeData {
        attribute: WindowCompositionAttribute::WcaAccentPolicy,
        data: policy as *mut AccentPolicy as *mut std::ffi::c_void,
        size_of_data: std::mem::size_of::<AccentPolicy>(),
    };

    // SAFETY: `data.data` points at live `policy`, `size_of_data` is correct,
    // `hwnd` is the target window. The OS copies the policy synchronously.
    let result = unsafe { func(hwnd, &mut data) };
    result != 0
}
