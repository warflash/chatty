//! Windows process plumbing: single instance, autostart, the message loop.

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage};
use windows::core::w;

use crate::{clipboard, config, engine, logi, overlay, tray, ui, update};

/// False when another chatty already runs. Also opts into per-monitor DPI.
pub fn single_instance() -> bool {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\chatty-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Closed so a caller that retries (after an update) can get it.
            if let Ok(h) = mutex {
                let _ = CloseHandle(h);
            }
            return false;
        }
        std::mem::forget(mutex);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    true
}

pub fn set_autostart(enabled: bool) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    // Never register a dev build, or a release build run from cargo's target dir.
    let exe = std::env::current_exe().unwrap_or_default();
    if cfg!(debug_assertions) || exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")) {
        return;
    }
    unsafe {
        if enabled {
            let exe = std::env::current_exe().unwrap_or_default();
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                w!("chatty"),
                REG_SZ.0,
                Some(value.as_ptr() as *const _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, w!("chatty"));
        }
    }
}

/// Creates the windows and runs the message loop on the main thread.
/// `updated`: started by an update.
pub fn run(cfg: config::Shared, engine: engine::Handle, updated: bool, t0: Instant) {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.map(|h| HINSTANCE(h.0)).unwrap_or_default();
    overlay::create(instance);
    ui::init(cfg.clone(), Some(engine.clone()));
    let main_hwnd = tray::create(instance, tray::Actions { cfg: cfg.clone() });
    clipboard::OWNER.store(main_hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
    engine.send(engine::Cmd::Warm);
    update::start(cfg.clone());
    if updated {
        overlay::notice(&format!("Updated to {}", env!("CARGO_PKG_VERSION")));
    }
    log::info!("ready in {:?}", t0.elapsed());

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    logi::shutdown();
    overlay::shutdown();
    tray::remove_icon();
}
