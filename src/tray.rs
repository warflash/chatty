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

/// The window/taskbar icon: the exe's embedded icon (build.rs, resource 1).
pub fn app_icon() -> HICON {
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        LoadIconW(Some(instance.into()), windows::core::PCWSTR(1 as *const u16)).unwrap_or_default()
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

/// Tray icon: the app icon at the small-icon size, with a status dot in the
/// bottom-right corner while not idle (greyed out on error). Rebuilt when the
/// small-icon size changes (DPI change).
fn icon_for(s: TrayState) -> HICON {
    static ICONS: Mutex<(i32, [isize; 5])> = Mutex::new((0, [0; 5]));
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let mut icons = ICONS.lock().unwrap();
    if icons.0 != n {
        for h in icons.1 {
            if h != 0 {
                unsafe {
                    let _ = DestroyIcon(HICON(h as *mut _));
                }
            }
        }
        let base = app_pixels(n);
        let make = |badge: Option<(u8, u8, u8)>, grey: bool| match &base {
            Some(px) => badged_icon(px, n as usize, badge, grey).0 as isize,
            None => 0,
        };
        *icons = (
            n,
            [
                make(None, false),
                make(Some((232, 59, 59)), false),
                make(Some((245, 165, 30)), false),
                make(Some((59, 138, 208)), false),
                make(Some((232, 59, 59)), true),
            ],
        );
    }
    HICON(icons.1[s as usize] as *mut _)
}

/// The embedded app icon's `n` x `n` frame as straight-alpha BGRA rows, top
/// first.
fn app_pixels(n: i32) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).ok()?;
        let h = LoadImageW(Some(instance.into()), windows::core::PCWSTR(1 as *const u16), IMAGE_ICON, n, n, LR_DEFAULTCOLOR).ok()?;
        let icon = HICON(h.0);
        let mut info = ICONINFO::default();
        let got = GetIconInfo(icon, &mut info);
        let _ = DestroyIcon(icon);
        got.ok()?;
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (n * n * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, info.hbmColor, 0, n as u32, Some(px.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        (lines == n).then_some(px)
    }
}

/// Copies `base`, optionally greys it, and paints a status dot with a
/// transparent gap around it into the bottom-right corner.
fn badged_icon(base: &[u8], n: usize, badge: Option<(u8, u8, u8)>, grey: bool) -> HICON {
    let mut px = base.to_vec();
    if grey {
        for p in px.chunks_exact_mut(4) {
            let l = (0.114 * p[0] as f32 + 0.587 * p[1] as f32 + 0.299 * p[2] as f32) * 0.8;
            p[0] = l as u8;
            p[1] = l as u8;
            p[2] = l as u8;
        }
    }
    if let Some(rgb) = badge {
        let nf = n as f32;
        let r = nf * 0.22;
        let gap = r + (nf / 16.0).max(1.0);
        let c = nf - r - 0.5;
        for y in 0..n {
            for x in 0..n {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                let cover = |rad: f32| (rad - d + 0.5).clamp(0.0, 1.0);
                let p = &mut px[(y * n + x) * 4..][..4];
                // Cut the gap, then lay the dot over it (straight alpha).
                let da = p[3] as f32 / 255.0 * (1.0 - cover(gap));
                let sa = cover(r);
                let oa = sa + da * (1.0 - sa);
                if oa > 0.0 {
                    for (ch, sc) in [(0, rgb.2), (1, rgb.1), (2, rgb.0)] {
                        p[ch] = ((sc as f32 * sa + p[ch] as f32 * da * (1.0 - sa)) / oa).round() as u8;
                    }
                }
                p[3] = (oa * 255.0).round() as u8;
            }
        }
    }
    let and_mask = vec![0u8; n.div_ceil(32) * 4 * n];
    unsafe { CreateIcon(None, n as i32, n as i32, 1, 32, and_mask.as_ptr(), px.as_ptr()).unwrap_or_default() }
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
