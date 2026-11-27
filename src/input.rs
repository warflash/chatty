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

/// What has keyboard focus, as far as pasting is concerned.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Editable(String),
    /// Clearly not a text field (desktop, list, button, game window, ...).
    NotEditable(String),
    /// Could not tell (no UI Automation support, browser without its
    /// accessibility tree, timeout): paste anyway.
    Unknown(String),
}

fn window_class(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Terminals take Ctrl+V but expose no editable element.
const PASTE_CLASSES: &[&str] = &[
    "ConsoleWindowClass",
    "CASCADIA_HOSTING_WINDOW_CLASS",
    "mintty",
    "PuTTY",
    "VirtualConsoleClass",
    "org.wezfurlong.wezterm",
    "Alacritty",
];

/// Browser/Electron render surfaces: their accessibility tree may be off,
/// so the focused element says nothing about the caret.
const OPAQUE_CLASSES: &[&str] = &["Chrome_RenderWidgetHostHWND", "Chrome_WidgetWin_1", "MozillaWindowClass", "Intermediate D3D Window"];

thread_local! {
    static UIA: std::cell::RefCell<Option<windows::Win32::UI::Accessibility::IUIAutomation>> = const { std::cell::RefCell::new(None) };
}

/// Asks UI Automation whether the focused element takes text.
pub fn focus_target() -> Target {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Accessibility::*;
    use windows::core::Interface;

    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() {
        return Target::NotEditable("no foreground window".into());
    }
    let fg_class = window_class(fg);
    if PASTE_CLASSES.contains(&fg_class.as_str()) {
        return Target::Editable(format!("terminal {fg_class}"));
    }
    if matches!(fg_class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
        return Target::NotEditable(format!("desktop ({fg_class})"));
    }
    let uia = UIA.with(|u| {
        let mut u = u.borrow_mut();
        if u.is_none() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let created: windows::core::Result<IUIAutomation> = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER);
                if let Ok(a) = created {
                    if let Ok(a2) = a.cast::<IUIAutomation2>() {
                        let _ = a2.SetConnectionTimeout(200);
                        let _ = a2.SetTransactionTimeout(200);
                    }
                    *u = Some(a);
                }
            }
        }
        u.clone()
    });
    let Some(uia) = uia else { return Target::Unknown("UI Automation unavailable".into()) };
    unsafe {
        let Ok(el) = uia.GetFocusedElement() else { return Target::Unknown("no focused element".into()) };
        let ct = el.CurrentControlType().unwrap_or_default();
        let class = el.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
        let what = format!("control type {} class {class:?} in {fg_class}", ct.0);
        if OPAQUE_CLASSES.contains(&class.as_str()) {
            return Target::Unknown(what);
        }
        let read_only = el
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()
            .and_then(|vp| vp.CurrentIsReadOnly().ok())
            .map(|b| b.as_bool());
        if read_only == Some(false) {
            return Target::Editable(what);
        }
        if ct == UIA_EditControlTypeId || ct == UIA_DocumentControlTypeId {
            // A web page body is a read-only document.
            return if read_only == Some(true) { Target::NotEditable(format!("read-only {what}")) } else { Target::Editable(what) };
        }
        if el.GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId).is_ok() {
            return Target::Editable(what);
        }
        // Panes and windows are left out: older editors and games report
        // those for their text surface.
        const NOT_TEXT: &[UIA_CONTROLTYPE_ID] = &[
            UIA_ListControlTypeId,
            UIA_ListItemControlTypeId,
            UIA_TreeControlTypeId,
            UIA_TreeItemControlTypeId,
            UIA_ButtonControlTypeId,
            UIA_MenuItemControlTypeId,
            UIA_MenuControlTypeId,
            UIA_MenuBarControlTypeId,
            UIA_ImageControlTypeId,
            UIA_HyperlinkControlTypeId,
            UIA_TextControlTypeId,
            UIA_TabItemControlTypeId,
            UIA_TabControlTypeId,
            UIA_ToolBarControlTypeId,
            UIA_TitleBarControlTypeId,
            UIA_CheckBoxControlTypeId,
            UIA_RadioButtonControlTypeId,
            UIA_DataItemControlTypeId,
            UIA_TableControlTypeId,
            UIA_DataGridControlTypeId,
        ];
        if NOT_TEXT.contains(&ct) {
            return Target::NotEditable(what);
        }
        Target::Unknown(what)
    }
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
