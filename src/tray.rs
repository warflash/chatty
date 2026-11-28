//! Tray icon and the hidden main window that owns it (and the clipboard).
//! Right-click opens the flyout (`ui::flyout`), left-click the main window.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::config;

const WM_TRAY: u32 = WM_APP + 10;
const WM_TRAY_UPDATE: u32 = WM_APP + 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrayState {
    Idle = 0,
    Recording = 1,
    Busy = 2,
    Loading = 3,
    Error = 4,
}

static STATE: AtomicU8 = AtomicU8::new(TrayState::Loading as u8);
static TOOLTIP: Mutex<String> = Mutex::new(String::new());
static MAIN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub struct Actions {
    pub cfg: config::Shared,
}

static ACTIONS: OnceLock<Actions> = OnceLock::new();

fn post_update() {
    let h = MAIN.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_TRAY_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn set_state(s: TrayState) {
    use crate::status::{Phase, set_phase};
    set_phase(match s {
        TrayState::Idle => Phase::Idle,
        TrayState::Recording => Phase::Recording,
        TrayState::Busy => Phase::Transcribing,
        TrayState::Loading => Phase::Loading,
        TrayState::Error => Phase::Error,
    });
    if STATE.swap(s as u8, Ordering::Relaxed) != s as u8 {
        TOOLTIP.lock().unwrap().clear();
        post_update();
    }
}

/// The window/taskbar icon: the exe's embedded icon (build.rs, resource 1),
/// or the idle tray glyph if that is missing.
pub fn app_icon() -> HICON {
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        LoadIconW(Some(instance.into()), windows::core::PCWSTR(1 as *const u16)).unwrap_or_else(|_| icon_for(TrayState::Idle))
    }
}

pub fn set_tooltip(s: &str) {
    *TOOLTIP.lock().unwrap() = s.to_string();
    post_update();
}

fn state() -> TrayState {
    match STATE.load(Ordering::Relaxed) {
        0 => TrayState::Idle,
        1 => TrayState::Recording,
        2 => TrayState::Busy,
        3 => TrayState::Loading,
        _ => TrayState::Error,
    }
}

/// 32x32 anti-aliased circle icon: filled while active, a ring with a dot
/// otherwise.
fn make_icon(rgb: (u8, u8, u8), filled: bool) -> HICON {
    const N: usize = 32;
    let mut bgra = vec![0u8; N * N * 4];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let cover = |r: f32| (r - d + 0.5).clamp(0.0, 1.0);
            let a = if filled {
                cover(13.0)
            } else {
                let ring = (cover(13.0) - cover(10.0)).max(0.0);
                (ring + cover(5.0)).min(1.0)
            };
            let i = (y * N + x) * 4;
            bgra[i] = rgb.2;
            bgra[i + 1] = rgb.1;
            bgra[i + 2] = rgb.0;
            bgra[i + 3] = (a * 255.0) as u8;
        }
    }
    let and_mask = vec![0u8; N * N / 8];
    unsafe {
        CreateIcon(None, N as i32, N as i32, 1, 32, and_mask.as_ptr(), bgra.as_ptr()).unwrap_or_default()
    }
}

fn icon_for(s: TrayState) -> HICON {
    static ICONS: OnceLock<[isize; 5]> = OnceLock::new();
    let icons = ICONS.get_or_init(|| {
        [
            make_icon((200, 200, 200), false).0 as isize,
            make_icon((232, 59, 59), true).0 as isize,
            make_icon((245, 165, 30), true).0 as isize,
            make_icon((59, 138, 208), false).0 as isize,
            make_icon((150, 40, 40), false).0 as isize,
        ]
    });
    HICON(icons[s as usize] as *mut _)
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let s = state();
    let mut tip = TOOLTIP.lock().unwrap().clone();
    if tip.is_empty() {
        let cfg = ACTIONS.get().map(|a| a.cfg.get());
        let model = cfg.as_ref().map(|c| c.model.clone()).unwrap_or_default();
        tip = match s {
            TrayState::Idle => format!("chatty: {model}"),
            TrayState::Recording => "chatty: recording".into(),
            TrayState::Busy => "chatty: transcribing".into(),
            TrayState::Loading => format!("chatty: loading {model}"),
            TrayState::Error => "chatty: error (see log)".into(),
        };
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon_for(s),
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    nid
}

fn add_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &notify_data(hwnd));
    }
}

pub fn remove_icon() {
    let h = MAIN.load(Ordering::Relaxed);
    if h != 0 {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(h as *mut _),
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

/// Screen rect of the tray icon (inside the overflow panel while that is
/// open).
fn icon_rect(hwnd: HWND) -> Option<RECT> {
    let id = NOTIFYICONIDENTIFIER { cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    unsafe { Shell_NotifyIconGetRect(&id) }.ok().filter(|r| r.right > r.left)
}

pub fn quit() {
    log::info!("quit");
    remove_icon();
    log::logger().flush();
    std::process::exit(0);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    let taskbar_created = *TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    if crate::clipboard::on_message(hwnd, msg, wparam.0 as u32) {
        return LRESULT(0);
    }
    unsafe {
        match msg {
            WM_TRAY => {
                match lparam.0 as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => crate::ui::flyout::toggle(icon_rect(hwnd)),
                    WM_LBUTTONUP => crate::ui::open(None),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TRAY_UPDATE => {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &notify_data(hwnd));
                LRESULT(0)
            }
            m if m == taskbar_created => {
                add_icon(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Creates the hidden main window and the tray icon. Main thread only.
pub fn create(instance: HINSTANCE, actions: Actions) -> HWND {
    let _ = ACTIONS.set(actions);
    unsafe {
        let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: w!("chatty_main"), ..Default::default() };
        RegisterClassW(&class);
        // A hidden top-level window (not message-only) so it receives
        // TaskbarCreated broadcasts after an Explorer restart.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("chatty_main"),
            w!("chatty"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("main window");
        MAIN.store(hwnd.0 as isize, Ordering::Relaxed);
        add_icon(hwnd);
        hwnd
    }
}
