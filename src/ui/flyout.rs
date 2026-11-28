//! Tray flyout: the panel a right click on the tray icon opens. Status, the
//! last transcript (click to copy), quick settings and shortcuts, drawn with
//! the main window's widgets.
//!
//! Like a menu it never takes the focus, not even on a click: taking it
//! closes the Windows 11 tray overflow the icon may sit in. It closes on a
//! click outside, Alt+Tab or the Windows key, after an action that leaves
//! it, and when a recording starts.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_LBUTTON, VK_LWIN,
    VK_MBUTTON, VK_MENU, VK_RBUTTON, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::gfx::{Align, Color, Font, Gfx, Rect};
use super::{Action, Input, Memory, Page, Ui, icon, id, theme};
use crate::models;

const W: f32 = 300.0;
const HEADER_H: f32 = 44.0;
const LAST_H: f32 = 94.0;
const ROW_H: f32 = 40.0;
const ROWS: usize = 4;
/// Space above and below the settings rows.
const ROWS_PAD: f32 = 6.0;
const FOOTER_H: f32 = 44.0;
/// Height in DIPs; sections are separated by 1-DIP lines.
const H: f32 = HEADER_H + 1.0 + LAST_H + 1.0 + ROWS_PAD * 2.0 + ROW_H * ROWS as f32 + 1.0 + FOOTER_H;
/// Left/right inset of text and controls.
const INSET: f32 = 16.0;
/// Distance from the taskbar and the screen edges.
const MARGIN: f32 = 12.0;
const OPEN_SECS: f32 = 0.15;
/// How long "Copied" shows before the flyout closes.
const COPIED_SECS: f32 = 0.6;
/// A tray click this soon after the flyout closed was the click that closed it.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

const WM_STATUS: u32 = WM_APP + 40;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_ANIM: usize = 1;
/// Checks for outside clicks and app switches while open.
const TIMER_WATCH: usize = 2;

const GLYPH_EDIT: char = '\u{E70F}';
const GLYPH_LOG: char = '\u{E8A5}';
const GLYPH_POWER: char = '\u{E7E8}';

/// A step above the main window's background.
const BG: Color = Color::rgb(30, 30, 35);

enum Cmd {
    Window(Option<Page>),
    Open(PathBuf),
    Quit,
    Hide,
}

struct Flyout {
    gfx: Option<Gfx>,
    input: Input,
    mem: Memory,
    shown: Instant,
    copied: Option<Instant>,
    animating: bool,
}

thread_local! {
    static FLY: RefCell<Option<Flyout>> = const { RefCell::new(None) };
}

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static PENDING: AtomicBool = AtomicBool::new(false);
static HIDDEN_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn hwnd() -> Option<HWND> {
    let h = HWND_RAW.load(Ordering::Relaxed);
    (h != 0).then(|| HWND(h as *mut _))
}

fn visible() -> bool {
    hwnd().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() })
}

fn with_fly<R>(f: impl FnOnce(&mut Flyout) -> R) -> Option<R> {
    FLY.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Live status changed (any thread).
pub fn notify() {
    if let Some(h) = hwnd() {
        if visible() && !PENDING.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(Some(h), WM_STATUS, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// ----------------------------------------------------------------- drawing --

fn frame(f: &mut Flyout, w: f32, h: f32, dpi: f32) -> (Vec<Action>, Option<Cmd>) {
    let mut actions = Vec::new();
    let mut cmd = None;
    let g = f.gfx.as_mut().unwrap();
    if (g.dpi - dpi).abs() > 0.1 {
        g.set_dpi(dpi);
    }
    let open = (f.shown.elapsed().as_secs_f32() / OPEN_SECS).min(1.0);
    let ease = 1.0 - (1.0 - open).powi(3);
    g.begin();
    g.clear(BG);
    // Slides up into place while it fades in.
    g.set_offset(0.0, (1.0 - ease) * 6.0);
    let blocked = f.mem.popup_rect.is_some_and(|r| r.contains(f.input.mouse));
    f.mem.hand = false;
    let mut tip: Option<(Rect, &'static str)> = None;
    let animate;
    {
        let mut ui = Ui {
            g: &mut *g,
            input: &f.input,
            mem: &mut f.mem,
            actions: &mut actions,
            t: 0.0,
            clip: Rect::new(0.0, 0.0, w, h),
            blocked,
            animate: false,
            focus_claimed: false,
        };
        let cfg = super::ctx().cfg.get();
        let mut y = 0.0;
        header(&mut ui, Rect::new(0.0, y, w, HEADER_H), &mut cmd, &mut tip);
        y += HEADER_H;
        separator(&mut ui, y, w);
        y += 1.0;
        if let Some(text) = last_transcript(&mut ui, Rect::new(0.0, y, w, LAST_H), f.copied) {
            ui.actions.push(Action::Copy(text));
            f.copied = Some(Instant::now());
        }
        y += LAST_H;
        separator(&mut ui, y, w);
        y += 1.0 + ROWS_PAD;
        settings(&mut ui, Rect::new(0.0, y, w, ROW_H * ROWS as f32), &cfg);
        y += ROW_H * ROWS as f32 + ROWS_PAD;
        separator(&mut ui, y, w);
        y += 1.0;
        footer(&mut ui, Rect::new(0.0, y, w, FOOTER_H), &mut cmd, &mut tip);

        ui.clip = Rect::new(0.0, 0.0, w, h);
        ui.blocked = false;
        super::popup(&mut ui, w, h);
        if let Some((r, text)) = tip.filter(|_| ui.mem.popup.is_none()) {
            tooltip(&mut ui, r, text, w);
        }
        animate = ui.animate;
    }
    g.set_offset(0.0, 0.0);
    if ease < 1.0 {
        g.fill(Rect::new(0.0, 0.0, w, h), 0.0, BG.alpha(1.0 - ease));
    }
    if f.input.released {
        f.mem.active = 0;
    }
    if !g.end() {
        f.gfx = None;
    }
    let copying = f.copied.is_some_and(|c| c.elapsed().as_secs_f32() < COPIED_SECS);
    if f.copied.is_some() && !copying && cmd.is_none() {
        cmd = Some(Cmd::Hide);
    }
    f.animating = animate || open < 1.0 || copying || f.mem.popup.is_some();
    f.input.pressed = false;
    f.input.released = false;
    (actions, cmd)
}

fn separator(ui: &mut Ui, y: f32, w: f32) {
    ui.g.fill(Rect::new(0.0, y, w, 1.0), 0.0, theme::HAIRLINE);
}

fn hover_tip(ui: &Ui, r: Rect, text: &'static str, tip: &mut Option<(Rect, &'static str)>) {
    if ui.hovered(r) {
        *tip = Some((r, text));
    }
}

/// Square icon button; `danger` turns it red on hover.
fn icon_button(ui: &mut Ui, key: &str, glyph: char, r: Rect, danger: bool) -> bool {
    let (hover, held, clicked) = ui.interact(id(key), r);
    if hover {
        let c = if danger { theme::RED.alpha(if held { 0.20 } else { 0.13 }) } else { Color::white(if held { 0.05 } else { 0.08 }) };
        ui.g.fill(r, 6.0, c);
    }
    let fg = match (hover, danger) {
        (true, true) => theme::RED,
        (true, false) => theme::TEXT,
        _ => theme::TEXT2,
    };
    ui.icon(glyph, r, 13.0, fg);
    clicked
}

fn header(ui: &mut Ui, r: Rect, cmd: &mut Option<Cmd>, tip: &mut Option<(Rect, &'static str)>) {
    let (dot, word) = super::phase_style(crate::status::get().phase);
    let cy = r.y + r.h / 2.0;
    ui.g.circle(r.x + INSET + 4.0, cy, 4.0, dot);
    let x = r.x + INSET + 16.0;
    let title = Font::semibold(13.0);
    let (tw, _) = ui.g.measure("chatty", title, 200.0, false);
    ui.g.label("chatty", title, Rect::new(x, r.y, tw + 2.0, r.h), theme::TEXT, Align::Left);
    ui.g.label(word, Font::text(12.0), Rect::new(x + tw + 8.0, r.y, 160.0, r.h), theme::TEXT3, Align::Left);
    let b = Rect::new(r.right() - 8.0 - 32.0, cy - 16.0, 32.0, 32.0);
    hover_tip(ui, b, "Open chatty", tip);
    if icon_button(ui, "fly/open", icon::OPEN, b, false) {
        *cmd = Some(Cmd::Window(None));
    }
}

/// Whitespace collapsed to single spaces; long text cut early (two lines
/// show at most).
fn preview(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if out.len() > 600 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn ago(at_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - at_ms).max(0) / 1000;
    match s {
        0..45 => "just now".into(),
        45..90 => "1 min ago".into(),
        90..3600 => format!("{} min ago", (s + 30) / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// The last transcript, two lines of it; a click copies it. Returns the text
/// to copy when clicked.
fn last_transcript(ui: &mut Ui, r: Rect, copied: Option<Instant>) -> Option<String> {
    let last = crate::status::get().last;
    let hit = r.inset(6.0, 6.0);
    let (hover, held, clicked) = if last.is_some() { ui.interact(id("fly/last"), hit) } else { (false, false, false) };
    if hover {
        ui.g.fill(hit, 6.0, Color::white(if held { 0.035 } else { 0.05 }));
    }
    let x = r.x + INSET;
    let inner_w = r.w - 2.0 * INSET;
    ui.caps("Last transcript", Rect::new(x, r.y + 12.0, 160.0, 16.0), theme::TEXT3);
    let Some((text, at_ms)) = last else {
        ui.g.label("Nothing yet", Font::text(12.0), Rect::new(x, r.y + 34.0, inner_w, 16.0), theme::TEXT2, Align::Left);
        return None;
    };
    // "Copy" / "Copied", right-aligned with the label.
    let (glyph, label, fg) = match (copied.is_some(), hover) {
        (true, _) => (icon::CHECK, "Copied", theme::GREEN),
        (false, true) => (icon::COPY, "Copy", theme::ACCENT),
        (false, false) => (icon::COPY, "Copy", theme::TEXT3),
    };
    let font = Font::text(11.5);
    let (tw, _) = ui.g.measure(label, font, 200.0, false);
    let lx = r.right() - INSET - tw;
    ui.g.label(label, font, Rect::new(lx, r.y + 12.0, tw + 2.0, 16.0), fg, Align::Left);
    ui.icon(glyph, Rect::new(lx - 18.0, r.y + 12.0, 14.0, 16.0), 10.5, fg);

    let line_h = 16.0;
    let color = if hover { theme::TEXT } else { theme::TEXT2 };
    ui.g.text_lines(&preview(&text), Font::text(12.0), Rect::new(x, r.y + 33.0, inner_w, line_h * 2.0), line_h, 2, color);
    let now = crate::stats::epoch_ms(Instant::now());
    ui.g.label(&ago(at_ms, now), Font::text(11.0), Rect::new(x, r.y + 72.0, inner_w, 14.0), theme::TEXT3, Align::Left);

    (clicked && copied.is_none()).then_some(text)
}

fn set(ui: &mut Ui, key: &str, v: impl Into<toml_edit::Value>) {
    ui.actions.push(Action::Config(vec![(key.to_string(), v.into())]));
}

fn settings(ui: &mut Ui, r: Rect, cfg: &crate::config::Config) {
    let row = |i: usize| Rect::new(r.x, r.y + i as f32 * ROW_H, r.w, ROW_H);
    let label = |ui: &mut Ui, rr: Rect, text: &str| {
        ui.g.label(text, Font::text(13.0), Rect::new(rr.x + INSET, rr.y, 110.0, rr.h), theme::TEXT, Align::Left);
    };
    let control = |rr: Rect, left: f32| Rect::new(rr.x + left, rr.y + (rr.h - 28.0) / 2.0, rr.w - left - 12.0, 28.0);

    let rr = row(0);
    label(ui, rr, "Model");
    let options: Vec<String> = models::ASR
        .iter()
        .map(|m| if m.asset.is_ready() { m.name.to_string() } else { format!("{}  ·  {} MB", m.name, m.asset.size >> 20) })
        .collect();
    let selected = models::ASR.iter().position(|m| m.id == cfg.model).unwrap_or(0);
    // Model names are long: the widest control.
    if let Some(i) = ui.select("fly/model", &options, selected, false, control(rr, 96.0)) {
        set(ui, "model", models::ASR[i].id);
    }

    let rr = row(1);
    label(ui, rr, "Language");
    let langs = ["auto", "en", "de"];
    let li = langs.iter().position(|l| *l == cfg.language).unwrap_or(0);
    if let Some(i) = ui.segmented("fly/lang", &["Auto", "EN", "DE"], li, control(rr, 138.0)) {
        set(ui, "language", langs[i]);
    }

    let toggle_x = r.right() - 12.0 - 42.0;
    for (i, name, key, on) in [(2, "Live overlay", "overlay", cfg.overlay), (3, "LLM cleanup", "cleanup.enabled", cfg.cleanup.enabled)] {
        let rr = row(i);
        label(ui, rr, name);
        if let Some(on) = ui.toggle(&format!("fly/{key}"), on, toggle_x, rr.y + (rr.h - 24.0) / 2.0) {
            set(ui, key, on);
        }
    }
}

fn footer(ui: &mut Ui, r: Rect, cmd: &mut Option<Cmd>, tip: &mut Option<(Rect, &'static str)>) {
    let items: [(char, &'static str, Cmd); 6] = [
        (icon::HISTORY, "History", Cmd::Window(Some(Page::History))),
        (icon::CHART, "Statistics", Cmd::Window(Some(Page::Stats))),
        (icon::SETTINGS, "Settings", Cmd::Window(Some(Page::Settings))),
        (GLYPH_EDIT, "Edit config.toml", Cmd::Open(super::ctx().cfg.path.to_path_buf())),
        (GLYPH_LOG, "Open log", Cmd::Open(crate::paths::log_path())),
        (icon::FOLDER, "Open data folder", Cmd::Open(crate::paths::local_dir())),
    ];
    let size = 32.0;
    let y = r.y + (r.h - size) / 2.0;
    for (i, (glyph, label, c)) in items.into_iter().enumerate() {
        let b = Rect::new(r.x + 8.0 + i as f32 * (size + 2.0), y, size, size);
        hover_tip(ui, b, label, tip);
        if icon_button(ui, &format!("fly/foot/{i}"), glyph, b, false) {
            *cmd = Some(c);
        }
    }
    let b = Rect::new(r.right() - 8.0 - size, y, size, size);
    hover_tip(ui, b, "Quit chatty", tip);
    if icon_button(ui, "fly/quit", GLYPH_POWER, b, true) {
        *cmd = Some(Cmd::Quit);
    }
}

/// Small label above `anchor`, kept inside the window.
fn tooltip(ui: &mut Ui, anchor: Rect, text: &str, w: f32) {
    let font = Font::text(11.5);
    let (tw, _) = ui.g.measure(text, font, 300.0, false);
    let tr_w = tw + 16.0;
    let x = (anchor.x + anchor.w / 2.0 - tr_w / 2.0).clamp(6.0, w - tr_w - 6.0);
    let r = Rect::new(x, anchor.y - 28.0, tr_w, 22.0);
    ui.g.fill(r, 5.0, Color::rgb(44, 44, 50));
    ui.g.stroke(r, 5.0, Color::white(0.08), 1.0);
    ui.g.label(text, font, r, theme::TEXT, Align::Center);
}

// ------------------------------------------------------------------ window --

/// Window rect in pixels next to `anchor` (the tray icon), or the cursor.
fn place(anchor: Option<RECT>) -> (i32, i32, i32, i32) {
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let pt = anchor.map_or(cursor, |r| POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 });
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx.max(96) as f32 / 96.0;
        let (w, h) = ((W * scale).round() as i32, (H * scale).round() as i32);
        let m = (MARGIN * scale) as i32;
        let work = mi.rcWork;
        let mut x = pt.x - w / 2;
        let mut y = pt.y - h - m;
        if let Some(ov) = overflow_rect(pt) {
            // Above the tray overflow panel, which stays open.
            y = ov.top - h - m / 2;
        } else if pt.y >= work.bottom {
            // The icon sits in the taskbar, outside the work area: open away
            // from whichever edge it is on.
            y = work.bottom - h - m;
        } else if pt.y < work.top {
            y = work.top + m;
        }
        if pt.x >= work.right {
            x = work.right - w - m;
        } else if pt.x < work.left {
            x = work.left + m;
        }
        x = x.clamp(work.left + m, (work.right - w - m).max(work.left + m));
        y = y.clamp(work.top + m, (work.bottom - h - m).max(work.top + m));
        (x, y, w, h)
    }
}

/// The visible Windows 11 tray overflow panel containing `pt`, if any.
fn overflow_rect(pt: POINT) -> Option<RECT> {
    let h = unsafe { FindWindowW(w!("TopLevelWindowForOverflowXamlIsland"), None) }.ok()?;
    let mut r = RECT::default();
    unsafe { IsWindowVisible(h).as_bool() && GetWindowRect(h, &mut r).is_ok() }
        .then_some(r)
        .filter(|r| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom)
}

/// Shows the flyout next to `anchor`, or hides it if it is open. Main thread
/// only.
pub fn toggle(anchor: Option<RECT>) {
    if visible() {
        hide();
        return;
    }
    if HIDDEN_AT.lock().unwrap().is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
        return;
    }
    let Some(hwnd) = ensure_window() else { return };
    with_fly(|f| {
        f.shown = Instant::now();
        f.copied = None;
        f.mem = Memory::default();
        f.input = Input { mouse: (-1.0, -1.0), ..Default::default() };
    });
    let (x, y, w, h) = place(anchor);
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
        SetTimer(Some(hwnd), TIMER_WATCH, 50, None);
    }
    invalidate(hwnd);
}

fn hide() {
    let Some(h) = hwnd() else { return };
    unsafe {
        if IsWindowVisible(h).as_bool() {
            *HIDDEN_AT.lock().unwrap() = Some(Instant::now());
            let _ = KillTimer(Some(h), TIMER_ANIM);
            let _ = KillTimer(Some(h), TIMER_WATCH);
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// Every 50 ms while open: closes on a mouse button pressed outside the
/// flyout, or on Alt (+Tab) / the Windows key.
fn watch(hwnd: HWND) {
    let mut pt = POINT::default();
    let mut rc = RECT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
        let _ = GetWindowRect(hwnd, &mut rc);
    }
    let inside = pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom;
    let clicked_outside = !inside && [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].into_iter().any(key_down);
    if clicked_outside || [VK_MENU, VK_LWIN, VK_RWIN].into_iter().any(key_down) {
        hide();
    }
}

fn run(cmd: Cmd) {
    hide();
    match cmd {
        Cmd::Hide => {}
        Cmd::Window(page) => super::open(page),
        Cmd::Open(p) => super::run_actions(vec![Action::Open(p)]),
        Cmd::Quit => crate::tray::quit(),
    }
}

fn ensure_window() -> Option<HWND> {
    if let Some(h) = hwnd() {
        return Some(h);
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("chatty_flyout"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        FLY.with(|a| {
            *a.borrow_mut() = Some(Flyout {
                gfx: None,
                input: Input::default(),
                mem: Memory::default(),
                shown: Instant::now(),
                copied: None,
                animating: false,
            })
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("chatty_flyout"),
            w!("chatty"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .inspect_err(|e| log::error!("flyout window: {e}"))
        .ok()?;
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        Some(hwnd)
    }
}

fn paint(hwnd: HWND) -> Option<(Vec<Action>, Option<Cmd>, bool)> {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
    with_fly(|f| {
        if f.gfx.is_none() {
            match Gfx::new(hwnd, rc.right as u32, rc.bottom as u32, dpi) {
                Ok(g) => f.gfx = Some(g),
                Err(e) => {
                    log::error!("Direct2D: {e}");
                    return (Vec::new(), None, false);
                }
            }
        }
        let scale = dpi / 96.0;
        let (actions, cmd) = frame(f, rc.right as f32 / scale, rc.bottom as f32 / scale, dpi);
        (actions, cmd, f.animating)
    })
}

fn set_mouse(hwnd: HWND, lparam: LPARAM, button: Option<bool>) {
    let p = super::mouse_pos(hwnd, lparam);
    with_fly(|f| {
        f.input.mouse = p;
        if let Some(down) = button {
            f.input.down = down;
            f.input.pressed |= down;
            f.input.released |= !down;
        }
    });
    invalidate(hwnd);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let result = paint(hwnd);
                let _ = EndPaint(hwnd, &ps);
                if let Some((actions, cmd, animating)) = result {
                    let had_actions = !actions.is_empty();
                    super::run_actions(actions);
                    if let Some(c) = cmd {
                        run(c);
                    } else if animating || had_actions {
                        SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
                    } else {
                        let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ANIM => {
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_WATCH => {
                watch(hwnd);
                LRESULT(0)
            }
            WM_STATUS => {
                PENDING.store(false, Ordering::Release);
                // A recording pastes into the window behind: get out of the way.
                if crate::status::get().phase == crate::status::Phase::Recording {
                    hide();
                } else {
                    invalidate(hwnd);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (wpx, hpx) = ((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32);
                with_fly(|f| {
                    if let Some(g) = f.gfx.as_mut() {
                        g.resize(wpx, hpx);
                    }
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                set_mouse(hwnd, lparam, None);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with_fly(|f| f.input.mouse = (-1.0, -1.0));
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                SetCapture(hwnd);
                set_mouse(hwnd, lparam, Some(true));
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                set_mouse(hwnd, lparam, Some(false));
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let hand = with_fly(|f| f.mem.hand).unwrap_or(false);
                SetCursor(LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ago, preview};

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(preview("  hello\n\n world \t again "), "hello world again");
        assert_eq!(preview(""), "");
    }

    #[test]
    fn ago_buckets() {
        let m = 60_000;
        assert_eq!(ago(0, 10_000), "just now");
        assert_eq!(ago(0, m), "1 min ago");
        assert_eq!(ago(0, 7 * m), "7 min ago");
        assert_eq!(ago(0, 3 * 60 * m), "3 h ago");
        assert_eq!(ago(0, 30 * 60 * m), "yesterday");
        assert_eq!(ago(0, 5 * 24 * 60 * m), "5 days ago");
        assert_eq!(ago(10, 0), "just now");
    }
}
