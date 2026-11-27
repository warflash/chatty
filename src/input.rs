//! Synthesized keys and foreground-window queries.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL,
    VK_RETURN, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::PWSTR;

/// `dwExtraInfo` tag on keys chatty sends, so its own hook can tell them apart.
pub const OUR_INPUT: usize = 0x6368_6174; // "chat"

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: OUR_INPUT,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() { Ok(()) } else { Err(format!("SendInput sent {sent}/{}", inputs.len())) }
}

pub fn ctrl_v() -> Result<(), String> {
    send(&[key(VK_CONTROL, false), key(VK_V, false), key(VK_V, true), key(VK_CONTROL, true)])
}

pub fn enter() -> Result<(), String> {
    send(&[key(VK_RETURN, false), key(VK_RETURN, true)])
}

/// Waits until neither Ctrl nor the trigger key is physically down, so a
/// synthesized Ctrl+V or Enter never combines with the trigger's modifiers.
/// Returns how long it waited; gives up after `limit`.
pub fn wait_trigger_released(limit: Duration) -> Duration {
    let start = Instant::now();
    while crate::hook::trigger_keys_down() && start.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(2));
    }
    if crate::hook::trigger_keys_down() {
        log::warn!("trigger keys still down after {limit:?}; sending anyway");
    }
    start.elapsed()
}

fn foreground_process() -> Option<HANDLE> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()
    }
}

/// Executable name of the foreground window's process ("Code.exe").
pub fn foreground_app() -> String {
    let Some(process) = foreground_process() else { return String::new() };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let name = unsafe {
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    };
    name.rsplit('\\').next().unwrap_or("").to_string()
}

fn is_elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok.then_some(elevation.TokenIsElevated != 0)
    }
}

/// UIPI: an unelevated process cannot send keys to an elevated window, and
/// Windows drops them silently. Ask before pasting.
pub fn foreground_blocks_input() -> bool {
    if is_elevated(unsafe { GetCurrentProcess() }) == Some(true) {
        return false;
    }
    let Some(process) = foreground_process() else { return false };
    // An elevated process's token cannot be opened from medium integrity.
    let elevated = is_elevated(process).unwrap_or(true);
    unsafe {
        let _ = CloseHandle(process);
    }
    elevated
}
