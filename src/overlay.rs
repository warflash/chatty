//! Borderless, topmost window at the bottom of the screen. It never takes
//! focus (WS_EX_NOACTIVATE + SW_SHOWNOACTIVATE) and is click-through except
//! in card mode, where a transcript that could not be pasted waits with a
//! Copy button. Any thread updates the shared view and posts a message; the
//! main thread lays out and paints, double-buffered.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
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
    /// A transcript that was not pasted, waiting to be copied.
    Card { text: String, copied: bool },
}

struct View {
    mode: Mode,
    live: String,
    /// Height in pixels while a recording is shown: never shrinks, so the
    /// window does not jump as words wrap.
    min_h: i32,
    /// Current window geometry; moves only when it changes.
    geom: (i32, i32, i32, i32),
    copy_btn: RECT,
    close_btn: RECT,
}

static VIEW: Mutex<View> = Mutex::new(View {
    mode: Mode::Hidden,
    live: String::new(),
    min_h: 0,
    geom: (0, 0, 0, 0),
    copy_btn: RECT { left: 0, top: 0, right: 0, bottom: 0 },
    close_btn: RECT { left: 0, top: 0, right: 0, bottom: 0 },
});

fn post() {
    let hwnd = HWND_RAW.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

fn set(mode: Mode) {
    {
        let mut v = VIEW.lock().unwrap();
        if let Mode::Live(t) = &mode {
            v.live = t.clone();
        }
        if v.mode == mode {
            return;
        }
        v.mode = mode;
    }
    post();
}

/// Recording; `text` is the live transcript so far.
pub fn live(text: &str) {
    let t = text.to_string();
    crate::status::update(|s| s.live = t);
    set(Mode::Live(text.to_string()));
}

pub fn transcribing() {
    set(Mode::Transcribing);
}

pub fn hide() {
    let mut v = VIEW.lock().unwrap();
    // Notices and cards stay until their timer or the user dismisses them.
    if matches!(v.mode, Mode::Live(_) | Mode::Transcribing) {
        v.mode = Mode::Hidden;
        v.live.clear();
        drop(v);
        post();
    }
}

/// A short message shown for a few seconds.
pub fn notice(text: &str) {
    log::info!("notice: {text}");
    let t = text.to_string();
    crate::status::update(|s| s.notice = Some((t, std::time::Instant::now())));
    set(Mode::Notice(text.to_string()));
}

pub fn progress(text: &str, fraction: f32) {
    set(Mode::Progress(text.to_string(), (fraction * 100.0).floor() / 100.0));
}

/// Shows a transcript that was not pasted, with a Copy button.
pub fn card(text: &str) {
    set(Mode::Card { text: text.to_string(), copied: false });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
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
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI Variable Text"),
        )
    }
}

const BG: u32 = 0x00201C1A;
const RED: u32 = 0x003B3BE8;
const AMBER: u32 = 0x001EA5F5;
const BLUE: u32 = 0x00D08A3B;
const TEXT: u32 = 0x00F2F2F2;
const DIM: u32 = 0x00A8A8A8;

/// (dot color, text, dim)
fn content(mode: &Mode, live_text: &str) -> Option<(u32, String, bool)> {
    match mode {
        Mode::Hidden => None,
        Mode::Live(t) if t.trim().is_empty() => Some((RED, "Listening…".into(), true)),
        Mode::Live(t) => Some((RED, t.trim().to_string(), false)),
        Mode::Transcribing => Some((
            AMBER,
            if live_text.trim().is_empty() { "Transcribing…".into() } else { live_text.trim().to_string() },
            true,
        )),
        Mode::Notice(t) => Some((BLUE, t.clone(), false)),
        Mode::Progress(t, f) => Some((BLUE, format!("{t}  {:.0}%", f * 100.0), false)),
        Mode::Card { text, .. } => Some((BLUE, text.clone(), false)),
    }
}

struct Metrics {
    scale: f32,
}

impl Metrics {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }
}

/// Text that fits in `max_lines`: the tail for live text, the head (with an
/// ellipsis) for cards. Returns (text, lines).
unsafe fn fit(hdc: HDC, text: &str, width: i32, line_h: i32, max_lines: i32, tail: bool) -> (String, i32) {
    let measure = |s: &str| -> i32 {
        let mut buf = wide(s);
        let mut r = RECT { left: 0, top: 0, right: width, bottom: 0 };
        unsafe { DrawTextW(hdc, &mut buf, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX) };
        ((r.bottom + line_h / 2) / line_h).max(1)
    };
    let words: Vec<(usize, &str)> = text.split_whitespace().map(|w| (w.as_ptr() as usize - text.as_ptr() as usize, w)).collect();
    let lines = measure(text);
    if lines <= max_lines || words.len() <= 1 {
        return (text.to_string(), lines.min(max_lines));
    }
    // Binary search on the number of words kept.
    let (mut lo, mut hi) = (1usize, words.len());
    let candidate = |n: usize| -> String {
        if tail {
            format!("…{}", &text[words[words.len() - n].0..])
        } else {
            let (off, w) = words[n - 1];
            format!("{}…", &text[..off + w.len()])
        }
    };
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&candidate(mid)) <= max_lines { lo = mid } else { hi = mid - 1 }
    }
    (candidate(lo), max_lines)
}

unsafe fn layout_and_show(hwnd: HWND) {
    let mut v = VIEW.lock().unwrap();
    let show = match &v.mode {
        Mode::Live(_) | Mode::Transcribing => ENABLED.load(Ordering::Relaxed),
        Mode::Hidden => false,
        _ => true,
    };
    unsafe {
        if !show {
            v.min_h = 0;
            v.geom = (0, 0, 0, 0);
            drop(v);
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }
        let card = matches!(v.mode, Mode::Card { .. });
        let Some((_, text, _)) = content(&v.mode, &v.live) else { return };
        // Clickable only as a card.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = if card { ex & !(WS_EX_TRANSPARENT.0 as isize) } else { ex | WS_EX_TRANSPARENT.0 as isize };
        if want != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        }
        let fg = GetForegroundWindow();
        let monitor = MonitorFromWindow(if fg.is_invalid() { hwnd } else { fg }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let m = Metrics { scale: dpi.max(96) as f32 / 96.0 };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work = mi.rcWork;
        let width = m.px(680.0).min(work.right - work.left - m.px(80.0));
        let line_h = m.px(22.0);
        let hdc = GetDC(Some(hwnd));
        let f = font(m.px(16.0), 400);
        let old = SelectObject(hdc, HGDIOBJ(f.0));
        let max_lines = if card { 6 } else { 3 };
        let (_, lines) = fit(hdc, &text, width - m.px(56.0), line_h, max_lines, !card);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(f.0));
        ReleaseDC(Some(hwnd), hdc);
        let mut height = lines * line_h + m.px(22.0);
        if card {
            height += m.px(44.0);
        }
        if matches!(v.mode, Mode::Live(_) | Mode::Transcribing) {
            height = height.max(v.min_h);
            v.min_h = height;
        } else {
            v.min_h = 0;
        }
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.bottom - height - m.px(56.0);
        let geom = (x, y, width, height);
        if geom != v.geom {
            v.geom = geom;
            let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, m.px(18.0), m.px(18.0));
            SetWindowRgn(hwnd, Some(rgn), false);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOREDRAW);
        } else if !IsWindowVisible(hwnd).as_bool() {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        let is_notice = matches!(v.mode, Mode::Notice(_));
        let copied = matches!(v.mode, Mode::Card { copied: true, .. });
        drop(v);
        let _ = KillTimer(Some(hwnd), TIMER_HIDE);
        if is_notice {
            SetTimer(Some(hwnd), TIMER_HIDE, 3500, None);
        } else if copied {
            SetTimer(Some(hwnd), TIMER_HIDE, 1200, None);
        } else if card {
            SetTimer(Some(hwnd), TIMER_HIDE, 120_000, None);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let screen = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        // Double buffer: draw into memory, copy once (no flicker).
        let hdc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, rc.right.max(1), rc.bottom.max(1));
        let old_bmp = SelectObject(hdc, HGDIOBJ(bmp.0));
        let bg = CreateSolidBrush(COLORREF(BG));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let mut v = VIEW.lock().unwrap();
        if let Some((dot, text, dim)) = content(&v.mode, &v.live) {
            let m = Metrics { scale: GetDpiForWindow(hwnd).max(96) as f32 / 96.0 };
            let card = matches!(v.mode, Mode::Card { .. });
            let line_h = m.px(22.0);
            SetBkMode(hdc, TRANSPARENT);
            let top = m.px(11.0);
            // Status dot.
            let brush = CreateSolidBrush(COLORREF(dot));
            let pen_old = SelectObject(hdc, GetStockObject(NULL_PEN));
            let br_old = SelectObject(hdc, HGDIOBJ(brush.0));
            let cy = top + line_h / 2;
            let _ = Ellipse(hdc, m.px(18.0), cy - m.px(5.0), m.px(28.0), cy + m.px(5.0));
            SelectObject(hdc, br_old);
            SelectObject(hdc, pen_old);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let f = font(m.px(16.0), 400);
            let old = SelectObject(hdc, HGDIOBJ(f.0));
            SetTextColor(hdc, COLORREF(if dim { DIM } else { TEXT }));
            let text_w = rc.right - m.px(56.0);
            let (s, _) = fit(hdc, &text, text_w, line_h, if card { 6 } else { 3 }, !card);
            let mut buf = wide(&s);
            let bottom = if card { rc.bottom - m.px(50.0) } else { rc.bottom - m.px(8.0) };
            let mut r = RECT { left: m.px(40.0), top, right: rc.right - m.px(16.0), bottom };
            DrawTextW(hdc, &mut buf, &mut r, DT_WORDBREAK | DT_NOPREFIX);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(f.0));
            if card {
                let copied = matches!(v.mode, Mode::Card { copied: true, .. });
                let small = font(m.px(13.0), 400);
                let semibold = font(m.px(13.0), 600);
                let old = SelectObject(hdc, HGDIOBJ(small.0));
                SetTextColor(hdc, COLORREF(DIM));
                let mut hint = wide("Not pasted: no text field had focus");
                let mut hr = RECT { left: m.px(40.0), top: rc.bottom - m.px(42.0), right: rc.right - m.px(200.0), bottom: rc.bottom - m.px(10.0) };
                DrawTextW(hdc, &mut hint, &mut hr, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
                // Buttons.
                let bh = m.px(30.0);
                let by = rc.bottom - m.px(42.0);
                let close = RECT { left: rc.right - m.px(48.0), top: by, right: rc.right - m.px(14.0), bottom: by + bh };
                let copy = RECT { left: close.left - m.px(108.0), top: by, right: close.left - m.px(8.0), bottom: by + bh };
                let accent = CreateSolidBrush(COLORREF(if copied { 0x005FCB6C } else { 0x00FFCD60 }));
                let rgn_copy = CreateRoundRectRgn(copy.left, copy.top, copy.right, copy.bottom, m.px(8.0), m.px(8.0));
                let _ = FillRgn(hdc, rgn_copy, accent);
                let _ = DeleteObject(HGDIOBJ(rgn_copy.0));
                let _ = DeleteObject(HGDIOBJ(accent.0));
                let ghost = CreateSolidBrush(COLORREF(0x00353030));
                let rgn_close = CreateRoundRectRgn(close.left, close.top, close.right, close.bottom, m.px(8.0), m.px(8.0));
                let _ = FillRgn(hdc, rgn_close, ghost);
                let _ = DeleteObject(HGDIOBJ(rgn_close.0));
                let _ = DeleteObject(HGDIOBJ(ghost.0));
                SelectObject(hdc, HGDIOBJ(semibold.0));
                SetTextColor(hdc, COLORREF(0x00101010));
                let mut label = wide(if copied { "Copied" } else { "Copy" });
                let mut cr = copy;
                DrawTextW(hdc, &mut label, &mut cr, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
                SetTextColor(hdc, COLORREF(TEXT));
                let mut x = wide("\u{2715}");
                let mut xr = close;
                DrawTextW(hdc, &mut x, &mut xr, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(small.0));
                let _ = DeleteObject(HGDIOBJ(semibold.0));
                v.copy_btn = copy;
                v.close_btn = close;
            }
        }
        drop(v);
        let _ = BitBlt(screen, 0, 0, rc.right, rc.bottom, Some(hdc), 0, 0, SRCCOPY);
        SelectObject(hdc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn click(x: i32, y: i32) {
    let mut v = VIEW.lock().unwrap();
    let Mode::Card { text, .. } = &v.mode else { return };
    if inside(&v.copy_btn, x, y) {
        let text = text.clone();
        v.mode = Mode::Card { text: text.clone(), copied: true };
        drop(v);
        if let Err(e) = crate::clipboard::set_text(&text) {
            log::warn!("copy: {e}");
        }
        post();
    } else if inside(&v.close_btn, x, y) {
        v.mode = Mode::Hidden;
        drop(v);
        post();
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
                if matches!(v.mode, Mode::Notice(_) | Mode::Card { .. }) {
                    v.mode = Mode::Hidden;
                    drop(v);
                    layout_and_show(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                click(x, y);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p);
                let v = VIEW.lock().unwrap();
                let over = inside(&v.copy_btn, p.x, p.y) || inside(&v.close_btn, p.x, p.y);
                drop(v);
                SetCursor(LoadCursorW(None, if over { IDC_HAND } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
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
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 242, LWA_ALPHA);
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
    }
}
