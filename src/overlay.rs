//! Borderless, click-through, topmost window at the bottom of the screen.
//! It never takes focus (WS_EX_NOACTIVATE + SW_SHOWNOACTIVATE). Any thread
//! updates the shared view and posts a message; the main thread paints.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DT_CALCRECT, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect, GetMonitorInfoW,
    HFONT, HGDIOBJ, InvalidateRect, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, SelectObject,
    SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const WM_UPDATE: u32 = WM_APP + 1;
const TIMER_HIDE: usize = 1;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
pub static ENABLED: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Hidden,
    Live(String),
    Transcribing,
    Notice(String),
    Progress(String, f32),
}

static VIEW: Mutex<(Mode, String)> = Mutex::new((Mode::Hidden, String::new()));

fn set(mode: Mode) {
    {
        let mut v = VIEW.lock().unwrap();
        if let Mode::Live(t) = &mode {
            v.1 = t.clone();
        }
        if v.0 == mode {
            return;
        }
        v.0 = mode;
    }
    let hwnd = HWND_RAW.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Recording; `text` is the live transcript so far.
pub fn live(text: &str) {
    set(Mode::Live(text.to_string()));
}

pub fn transcribing() {
    set(Mode::Transcribing);
}

pub fn hide() {
    let mut v = VIEW.lock().unwrap();
    // Leave notices up until their timer expires.
    if matches!(v.0, Mode::Live(_) | Mode::Transcribing) {
        v.0 = Mode::Hidden;
        v.1.clear();
        drop(v);
        let hwnd = HWND_RAW.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_UPDATE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// A short message shown for a few seconds.
pub fn notice(text: &str) {
    log::info!("notice: {text}");
    set(Mode::Notice(text.to_string()));
}

pub fn progress(text: &str, fraction: f32) {
    set(Mode::Progress(text.to_string(), (fraction * 100.0).floor() / 100.0));
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

struct Paint {
    hwnd: HWND,
    scale: f32,
}

impl Paint {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }
}

fn font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
            windows::Win32::Graphics::Gdi::OUT_DEFAULT_PRECIS,
            windows::Win32::Graphics::Gdi::CLIP_DEFAULT_PRECIS,
            windows::Win32::Graphics::Gdi::CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        )
    }
}

/// (dot color, main text, secondary text)
fn content(mode: &Mode, live_text: &str) -> Option<(u32, String, bool)> {
    match mode {
        Mode::Hidden => None,
        Mode::Live(t) if t.trim().is_empty() => Some((0x3B3BE8, "Listening…".into(), true)),
        Mode::Live(t) => Some((0x3B3BE8, t.trim().to_string(), false)),
        Mode::Transcribing => Some((
            0x1EA5F5,
            if live_text.trim().is_empty() { "Transcribing…".into() } else { live_text.trim().to_string() },
            true,
        )),
        Mode::Notice(t) => Some((0xD08A3B, t.clone(), false)),
        Mode::Progress(t, f) => Some((0xD08A3B, format!("{t}  {:.0}%", f * 100.0), false)),
    }
}

const MAX_LINES: i32 = 3;

/// Drops leading words until `text` fits in `MAX_LINES` lines of `width`.
fn fit_tail(hdc: windows::Win32::Graphics::Gdi::HDC, text: &str, width: i32, line_h: i32) -> (String, i32) {
    let mut start = 0;
    let words: Vec<(usize, &str)> = text.split_whitespace().map(|w| (w.as_ptr() as usize - text.as_ptr() as usize, w)).collect();
    loop {
        let s = if start == 0 { text.to_string() } else { format!("…{}", &text[words[start].0..]) };
        let mut buf = wide(&s);
        let mut r = RECT { left: 0, top: 0, right: width, bottom: 0 };
        unsafe { DrawTextW(hdc, &mut buf, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX) };
        let lines = ((r.bottom + line_h / 2) / line_h).max(1);
        if lines <= MAX_LINES || start + 1 >= words.len() {
            return (s, lines.min(MAX_LINES));
        }
        start += 1;
    }
}

unsafe fn layout_and_show(hwnd: HWND) {
    let (mode, live_text) = VIEW.lock().unwrap().clone();
    let visible_mode = match &mode {
        Mode::Live(_) | Mode::Transcribing => ENABLED.load(Ordering::Relaxed),
        Mode::Hidden => false,
        _ => true,
    };
    unsafe {
        if !visible_mode {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }
        let Some((_, text, _)) = content(&mode, &live_text) else { return };
        let dpi = GetDpiForWindow(GetForegroundWindow()).max(96);
        let p = Paint { hwnd, scale: dpi as f32 / 96.0 };
        let fg = GetForegroundWindow();
        let monitor = MonitorFromWindow(if fg.is_invalid() { hwnd } else { fg }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work = mi.rcWork;
        let width = p.px(680.0).min(work.right - work.left - p.px(80.0));
        let text_w = width - p.px(52.0);
        let line_h = p.px(22.0);
        let hdc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
        let f = font(p.px(16.0), 400);
        let old = SelectObject(hdc, HGDIOBJ(f.0));
        let (_, lines) = fit_tail(hdc, &text, text_w, line_h);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(f.0));
        windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), hdc);
        let height = lines * line_h + p.px(20.0);
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.bottom - height - p.px(56.0);
        let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, p.px(18.0), p.px(18.0));
        SetWindowRgn(hwnd, Some(rgn), true);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = InvalidateRect(Some(hwnd), None, true);
        let _ = KillTimer(Some(hwnd), TIMER_HIDE);
        if matches!(mode, Mode::Notice(_)) {
            SetTimer(Some(hwnd), TIMER_HIDE, 3500, None);
        }
        let _ = p.hwnd;
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let (mode, live_text) = VIEW.lock().unwrap().clone();
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let bg = CreateSolidBrush(COLORREF(0x201C1A));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        if let Some((dot, text, dim)) = content(&mode, &live_text) {
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            let px = |v: f32| (v * scale).round() as i32;
            // Status dot.
            let brush = CreateSolidBrush(COLORREF(dot));
            let pen_old = SelectObject(hdc, windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::NULL_PEN));
            let br_old = SelectObject(hdc, HGDIOBJ(brush.0));
            let cy = px(10.0) + px(11.0);
            let _ = Ellipse(hdc, px(18.0), cy - px(5.0), px(28.0), cy + px(5.0));
            SelectObject(hdc, br_old);
            SelectObject(hdc, pen_old);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            // Text.
            let f = font(px(16.0), 400);
            let old = SelectObject(hdc, HGDIOBJ(f.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(if dim { 0xA8A8A8 } else { 0xF2F2F2 }));
            let text_w = rc.right - px(52.0);
            let (s, lines) = fit_tail(hdc, &text, text_w, px(22.0));
            let mut buf = wide(&s);
            let mut r = RECT { left: px(40.0), top: px(10.0), right: rc.right - px(12.0), bottom: rc.bottom - px(10.0) };
            let flags = if lines <= 1 { DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS } else { DT_WORDBREAK | DT_END_ELLIPSIS };
            DrawTextW(hdc, &mut buf, &mut r, flags | DT_NOPREFIX);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_UPDATE => {
                layout_and_show(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER if wparam.0 == TIMER_HIDE => {
                let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                let mut v = VIEW.lock().unwrap();
                if matches!(v.0, Mode::Notice(_)) {
                    v.0 = Mode::Hidden;
                    drop(v);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Creates the window. Call on the main (message loop) thread.
pub fn create(instance: windows::Win32::Foundation::HINSTANCE) {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: w!("chatty_overlay"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("chatty_overlay"),
            w!("chatty"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("overlay window");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 238, LWA_ALPHA);
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
    }
}
