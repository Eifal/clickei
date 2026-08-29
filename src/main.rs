//! Clickei — Rust entry point (egui + native hooks).
//! Single-instance guard prevents two recorders fighting over hotkeys.
//! One window, two tabs (Recording / Static Clicker) — mutually exclusive.
// Hide console window in release builds; keep it in debug for RUST_LOG output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

use clickei::ui::main_window::MainWindow;
use clickei::ui::static_clicker_window::StaticClickerWindow;
use clickei::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppScreen {
    Recording,
    StaticClicker,
}

impl AppScreen {
    fn label(self) -> &'static str {
        match self {
            AppScreen::Recording => "Recording",
            AppScreen::StaticClicker => "Static Clicker",
        }
    }
    /// Viewport size that fits this tab's content snugly (no dead space).
    fn viewport_size(self) -> egui::Vec2 {
        match self {
            AppScreen::Recording => egui::vec2(440.0, 300.0),
            AppScreen::StaticClicker => egui::vec2(520.0, 460.0),
        }
    }
}

struct App {
    screen: AppScreen,
    recorder: MainWindow,
    clicker: StaticClickerWindow,
    /// Tracks whether global hotkeys are currently suspended for the hotkey dialog
    hk_dialog_suspended: bool,
    /// Global panic hook (WH_KEYBOARD_LL triple-Esc) — lifetime = app
    _panic_hook: Option<clickei::hooks::PanicHookGuard>,
}

impl Default for App {
    fn default() -> Self {
        let panic_hook = match clickei::hooks::install_panic_hook() {
            Ok(g) => Some(g),
            Err(e) => {
                log::warn!("panic hook install failed: {}", e);
                None
            }
        };
        Self {
            screen: AppScreen::StaticClicker,
            recorder: MainWindow::default(),
            clicker: StaticClickerWindow::default(),
            hk_dialog_suspended: false,
            _panic_hook: panic_hook,
        }
    }
}

impl App {
    /// Leave current screen: stop whatever runs so only ONE mode is ever active.
    fn leave_current(&mut self) {
        match self.screen {
            AppScreen::Recording => self.recorder.on_leave(),
            AppScreen::StaticClicker => self.clicker.on_leave(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        theme::apply_dark_theme(ctx);

        // ---- Global panic hotkey (triple-Esc) — must be checked app-wide, not per-tab
        // Hook is WH_KEYBOARD_LL installed once at startup (see _panic_hook), so this
        // works regardless of which tab/window is focused or even when minimized.
        if clickei::hooks::take_panic_triggered() {
            self.recorder.emergency_stop();
            self.clicker.emergency_stop();
        }

        // Suspend global hotkeys while the Static Clicker hotkey dialog is open;
        // on close, re-register from config (picks up a newly saved key).
        let dialog_open = self.clicker.hotkey_dialog_open();
        if dialog_open != self.hk_dialog_suspended {
            self.hk_dialog_suspended = dialog_open;
            if dialog_open {
                self.recorder.suspend_hotkeys();
            } else {
                self.recorder.reapply_hotkeys_from_config();
            }
        }

        // Keep the UI awake even with zero input, so global hotkey events are
        // drained promptly (WM_HOTKEY goes to a hidden window — it does NOT
        // wake egui by itself)
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        // Global hotkey routing — single poller to avoid double-consume.
        // Only the ACTIVE tab's hotkeys are handled. (No Vec — zero alloc per frame)
        while let Some(id) = self.recorder.try_recv_hotkey() {
            match (self.screen, id) {
                (AppScreen::Recording, clickei::hotkey::HotkeyId::Record)
                | (AppScreen::Recording, clickei::hotkey::HotkeyId::Play)
                | (AppScreen::Recording, clickei::hotkey::HotkeyId::Stop) => {
                    self.recorder.handle_hotkey(id);
                }
                (AppScreen::StaticClicker, clickei::hotkey::HotkeyId::Static) => {
                    self.clicker.handle_hotkey();
                }
                _ => {
                    // Hotkey belonging to inactive tab — ignore
                }
            }
        }

        // ---- SidePanel BEFORE TopBottomPanel (fixes tab header width) ----
        // Egui layout: SidePanel claims its width first, then TopBottomPanel/CentralPanel
        // fill the remaining width (window_width - panel_width). If TopBottomPanel is
        // added before SidePanel, it spans the full window including the side panel area,
        // causing Recording/Static Clicker tabs to stretch when Sequence panel is expanded.
        // Outer guard: only when StaticClicker so Recording tab never reserves width.
        // Inner guard (is_docked_panel_active() == MultiTarget && !popped_out) for
        // SidePanel::right and window-widen is inside show_docked_side_panel() itself
        // (static_clicker_window.rs:191 / 172) — so widen/panel don't run when mode != MultiTarget.
        if self.screen == AppScreen::StaticClicker {
            self.clicker.show_docked_side_panel(ctx);
        }

        // ---- Shared tab bar (replaces the old launcher) ----
        let mut switch_to: Option<AppScreen> = None;
        egui::TopBottomPanel::top("mode_tabs").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let w = (ui.available_width() - 6.0) / 2.0;
                for screen in [AppScreen::Recording, AppScreen::StaticClicker] {
                    let selected = self.screen == screen;
                    let text = egui::RichText::new(screen.label()).strong().color(if selected {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_SECONDARY
                    });
                    let resp = ui.add_sized([w, 22.0], egui::SelectableLabel::new(selected, text));
                    if resp.clicked() && !selected {
                        switch_to = Some(screen);
                    }
                }
            });
            ui.add_space(3.0);
        });

        // ---- Handle tab switch: stop the other mode first (mutual exclusion) ----
        if let Some(target) = switch_to {
            self.leave_current();
            self.screen = target;
            let size = target.viewport_size();
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }

        // ---- Active tab content ----
        match self.screen {
            AppScreen::Recording => {
                // Delegate to MainWindow update (toolbar + console fill the rest)
                self.recorder.update(ctx, frame);
            }
            AppScreen::StaticClicker => {
                self.clicker.show(ctx);
            }
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.clicker.on_leave();
        self.recorder.on_exit(gl);
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // --- Single-instance guard (Windows named mutex) ---
    #[cfg(windows)]
    let _instance_guard = match create_single_instance_guard() {
        Ok(g) => g,
        Err(msg) => {
            let _ = rfd::MessageDialog::new()
                .set_title("Clickei")
                .set_description(&msg)
                .set_level(rfd::MessageLevel::Info)
                .show();
            eprintln!("{}", msg);
            std::process::exit(0);
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 460.0])
            .with_min_inner_size([340.0, 300.0])
            .with_resizable(true)
            .with_decorations(true),
        ..Default::default()
    };

    eframe::run_native(
        "Clickei",
        options,
        Box::new(|cc| {
            // PNG icon loaders for Sequence Clicking / Pop out (src/icons/*.png)
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

#[cfg(windows)]
fn create_single_instance_guard() -> Result<InstanceGuard, String> {
    use windows::Win32::Foundation::{CloseHandle, WIN32_ERROR};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let name: Vec<u16> = "Clickei_SingleInstance_Mutex\0".encode_utf16().collect();
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
        .map_err(|e| format!("CreateMutexW failed: {:?}", e))?;

    let err = unsafe { windows::Win32::Foundation::GetLastError() };
    const ERROR_ALREADY_EXISTS: WIN32_ERROR = WIN32_ERROR(183);
    if err == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err("Clickei is already running.".to_string());
    }

    Ok(InstanceGuard(handle))
}

#[cfg(windows)]
struct InstanceGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
