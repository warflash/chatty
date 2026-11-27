//! Floating card at the bottom of the screen: live transcript, transcribing,
//! notices, download progress, and transcripts that could not be pasted
//! (with Copy). Drawn with Direct2D into a per-pixel-alpha layered window
//! (anti-aliased corners, soft shadow, the main window's fonts and colors).
//!
//! It never takes focus (WS_EX_NOACTIVATE) and is click-through except as a
//! card. Any thread updates the shared view and posts a message; the main
//! thread lays out and draws. An animation timer runs only while something
//! moves (level meter, transcribing, fades), so idle CPU stays at zero.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

use crate::ui::gfx::{Align, Color, Font, Gfx, Rect};
use crate::ui::{icon, theme};

const WM_UPDATE: u32 = WM_APP + 1;
const TIMER_HIDE: usize = 1;
const TIMER_ANIM: usize = 2;
const FRAME_MS: u32 = 33;

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
    Card { text: String, reason: String, copied: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Copy,
    Close,
}

struct View {
    mode: Mode,
    live: String,
    /// Card height (DIPs) while a recording is shown: never shrinks, so the
    /// card does not jump as words wrap.
    min_h: f32,
    /// Button rects in window pixels (card mode).
    copy_btn: RECT,
    close_btn: RECT,
    hover: Option<Button>,
}

const ZERO: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };

static VIEW: Mutex<View> =
    Mutex::new(View { mode: Mode::Hidden, live: String::new(), min_h: 0.0, copy_btn: ZERO, close_btn: ZERO, hover: None });

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
    crate::status::update(|s| s.notice = Some((t, Instant::now())));
    set(Mode::Notice(text.to_string()));
}

pub fn progress(text: &str, fraction: f32) {
    set(Mode::Progress(text.to_string(), (fraction * 100.0).floor() / 100.0));
}

/// Shows a transcript that was not pasted, with a Copy button. `reason`
/// says why, in a few words.
pub fn card(text: &str, reason: &str) {
    set(Mode::Card { text: text.to_string(), reason: reason.to_string(), copied: false });
}

// ----------------------------------------------------------------- layout --

/// Room around the card for the shadow (DIPs).
const MARGIN: f32 = 28.0;
const CARD_W: f32 = 640.0;
const PAD_X: f32 = 18.0;
const PAD_Y: f32 = 15.0;
const LEAD_W: f32 = 26.0;
const GAP: f32 = 12.0;
const META_W: f32 = 44.0;
const RADIUS: f32 = 18.0;
const LIVE_LINES: u32 = 3;
const CARD_LINES: u32 = 6;
const FOOTER_H: f32 = 34.0;
/// Card bottom above the work area's bottom edge (DIPs).
const LIFT: f32 = 40.0;
const FADE_IN: f32 = 0.16;
const FADE_OUT: f32 = 0.14;

const FILL: Color = Color(0.082, 0.082, 0.098, 0.97);
const FONT: Font = Font::text(15.5);
const META: Font = Font::text(12.5);

/// Everything one frame needs.
#[derive(Clone)]
struct Frame {
    mode: Mode,
    live: String,
    level: f32,
    elapsed: f32,
    /// Seconds since the overlay appeared (animations).
    t: f32,
    hover: Option<Button>,
}

fn is_warning(s: &str) -> bool {
    let l = s.to_lowercase();
    ["fail", "unavailable", "error", "no microphone", "not found", "elevated"].iter().any(|k| l.contains(k))
}

fn body(f: &Frame) -> (String, Color) {
    match &f.mode {
        Mode::Live(t) if t.trim().is_empty() => ("Listening…".into(), theme::TEXT3),
        Mode::Live(t) => (t.trim().into(), theme::TEXT),
        Mode::Transcribing if f.live.trim().is_empty() => ("Transcribing…".into(), theme::TEXT3),
        Mode::Transcribing => (f.live.trim().into(), theme::TEXT2),
        Mode::Notice(t) => (t.clone(), theme::TEXT),
        Mode::Progress(t, _) => (t.clone(), theme::TEXT),
        Mode::Card { text, .. } => (text.clone(), theme::TEXT),
        Mode::Hidden => (String::new(), theme::TEXT),
    }
}

fn text_width(f: &Frame) -> f32 {
    let meta = matches!(f.mode, Mode::Live(_) | Mode::Transcribing | Mode::Progress(..));
    CARD_W - 2.0 * PAD_X - LEAD_W - GAP - if meta { META_W + 8.0 } else { 0.0 }
}

/// (card height, line height, lines shown, lines total) in DIPs.
fn measure(g: &mut Gfx, f: &Frame) -> (f32, f32, u32, u32) {
    let (s, _) = body(f);
    let (_, lh) = g.wrapped("Ag", FONT, 1000.0);
    let (lines, _) = g.wrapped(if s.is_empty() { "Ag" } else { &s }, FONT, text_width(f));
    let max = if matches!(f.mode, Mode::Card { .. }) { CARD_LINES } else { LIVE_LINES };
    let shown = lines.clamp(1, max);
    let mut h = 2.0 * PAD_Y + shown as f32 * lh;
    if matches!(f.mode, Mode::Card { .. }) {
        h += 10.0 + FOOTER_H;
    }
    if matches!(f.mode, Mode::Progress(..)) {
        h += 12.0;
    }
    (h, lh, shown, lines)
}

/// Draws the card at (MARGIN, MARGIN). Returns the button rects (DIPs).
fn draw(g: &mut Gfx, f: &Frame, card_h: f32, lh: f32, shown: u32, total: u32) -> Option<(Rect, Rect)> {
    let card = Rect::new(MARGIN, MARGIN, CARD_W, card_h);
    // Soft shadow: stacked translucent rounded rects.
    for i in 0..10 {
        let d = 1.0 + i as f32 * 1.7;
        let r = Rect::new(card.x - d, card.y - d + 5.0, card.w + 2.0 * d, card.h + 2.0 * d);
        g.fill(r, RADIUS + d, Color(0.0, 0.0, 0.0, 0.05 * (1.0 - i as f32 / 10.0)));
    }
    g.fill(card, RADIUS, FILL);
    // Hairline border.
    g.stroke(card.inset(0.5, 0.5), RADIUS - 0.5, Color::white(0.085), 1.0);

    let x0 = card.x + PAD_X;
    let ty = card.y + PAD_Y;
    let cy = ty + lh / 2.0;
    let lead_cx = x0 + LEAD_W / 2.0;

    // Leading indicator.
    match &f.mode {
        Mode::Live(_) => level_meter(g, lead_cx, cy, f.level, f.t),
        Mode::Transcribing => dots(g, lead_cx, cy, f.t),
        Mode::Notice(t) => {
            let (ic, c) = if is_warning(t) { (icon::WARNING, theme::AMBER) } else { ('\u{E946}', theme::BLUE) };
            g.label(&ic.to_string(), Font::icon(15.0), Rect::new(x0, cy - 10.0, LEAD_W, 20.0), c, Align::Center);
        }
        Mode::Progress(..) => {
            g.label(&icon::DOWNLOAD.to_string(), Font::icon(15.0), Rect::new(x0, cy - 10.0, LEAD_W, 20.0), theme::ACCENT, Align::Center)
        }
        Mode::Card { copied, .. } => {
            let c = if *copied { theme::GREEN } else { theme::ACCENT };
            g.glow(lead_cx, cy, 16.0, c.alpha(0.18));
            g.label(&'\u{E77F}'.to_string(), Font::icon(15.0), Rect::new(x0, cy - 10.0, LEAD_W, 20.0), c, Align::Center);
        }
        Mode::Hidden => {}
    }

    // Body text: live text keeps the newest lines (scrolls up, top fades);
    // cards keep the first lines (bottom fades).
    let (s, color) = body(f);
    let tx = x0 + LEAD_W + GAP;
    let tw = text_width(f);
    let visible = Rect::new(tx, ty, tw, shown as f32 * lh);
    let is_card = matches!(f.mode, Mode::Card { .. });
    let overflow = total > shown;
    let fade = if overflow { lh * 0.9 } else { 0.0 };
    let (top_fade, bottom_fade) = if is_card { (0.0, fade) } else { (fade, 0.0) };
    g.push_fade(Rect::new(visible.x - 2.0, visible.y, visible.w + 4.0, visible.h + 2.0), top_fade, bottom_fade);
    let y = if overflow && !is_card { ty - (total - shown) as f32 * lh } else { ty };
    g.text(&s, FONT, Rect::new(tx, y, tw, total as f32 * lh + lh), color, true, Align::Left);
    g.pop_fade();

    // Right meta: recording time or percentage.
    let meta = Rect::new(card.right() - PAD_X - META_W, cy - 10.0, META_W, 20.0);
    match &f.mode {
        Mode::Live(_) => {
            let secs = f.elapsed as u32;
            g.label(&format!("{}:{:02}", secs / 60, secs % 60), META, meta, theme::TEXT3, Align::Right);
        }
        Mode::Progress(_, p) => g.label(&format!("{:.0}%", p * 100.0), META, meta, theme::TEXT2, Align::Right),
        _ => {}
    }

    // Bottom accents.
    let track = Rect::new(card.x + RADIUS, card.bottom() - 2.5, card.w - 2.0 * RADIUS, 1.5);
    match &f.mode {
        Mode::Transcribing => shimmer(g, track, f.t),
        Mode::Progress(_, p) => {
            let bar = Rect::new(tx, ty + shown as f32 * lh + 8.0, tw + META_W + 8.0, 3.0);
            g.fill(bar, 1.5, Color::white(0.08));
            g.hgradient(Rect::new(bar.x, bar.y, (bar.w * p.clamp(0.0, 1.0)).max(3.0), bar.h), 1.5, theme::ACCENT, theme::ACCENT2);
        }
        _ => {}
    }

    // Card footer: reason and buttons.
    if let Mode::Card { reason, copied, .. } = &f.mode {
        let fy = card.bottom() - PAD_Y - FOOTER_H + 2.0;
        let close = Rect::new(card.right() - PAD_X - 32.0, fy, 32.0, 32.0);
        let copy = Rect::new(close.x - 8.0 - 98.0, fy, 98.0, 32.0);
        g.label(reason, META, Rect::new(tx, fy, copy.x - tx - 12.0, 32.0), theme::TEXT3, Align::Left);
        let hover_copy = f.hover == Some(Button::Copy);
        if *copied {
            g.fill(copy, 16.0, theme::GREEN.alpha(0.16));
            g.stroke(copy.inset(0.5, 0.5), 15.5, theme::GREEN.alpha(0.5), 1.0);
            button_label(g, copy, icon::CHECK, "Copied", theme::GREEN);
        } else {
            let lift = if hover_copy { 0.08 } else { 0.0 };
            g.glow(copy.x + copy.w / 2.0, copy.y + copy.h / 2.0, 60.0, theme::ACCENT.alpha(if hover_copy { 0.16 } else { 0.08 }));
            g.gradient(copy, 16.0, theme::ACCENT.mix(Color::white(1.0), lift), theme::ACCENT2.mix(Color::white(1.0), lift));
            g.stroke(copy.inset(0.5, 0.5), 15.5, Color::white(0.18), 1.0);
            button_label(g, copy, icon::COPY, "Copy", theme::ACCENT_TEXT);
        }
        let hover_close = f.hover == Some(Button::Close);
        g.fill(close, 16.0, if hover_close { theme::CONTROL_HOVER } else { theme::CONTROL });
        g.label(
            &'\u{E711}'.to_string(),
            Font::icon(11.0),
            close,
            if hover_close { theme::TEXT } else { theme::TEXT2 },
            Align::Center,
        );
        return Some((copy, close));
    }
    None
}

fn button_label(g: &mut Gfx, r: Rect, ic: char, label: &str, c: Color) {
    let font = Font::semibold(13.0);
    let (lw, _) = g.measure(label, font, 200.0, false);
    let total = 14.0 + 7.0 + lw;
    let x = r.x + (r.w - total) / 2.0;
    g.label(&ic.to_string(), Font::icon(13.0), Rect::new(x, r.y, 14.0, r.h), c, Align::Center);
    g.label(label, font, Rect::new(x + 21.0, r.y, lw + 2.0, r.h), c, Align::Left);
}

/// Five rounded bars driven by the microphone level, with a gentle wave so
/// they stay alive between syllables.
fn level_meter(g: &mut Gfx, cx: f32, cy: f32, level: f32, t: f32) {
    const SHAPE: [f32; 5] = [0.5, 0.78, 1.0, 0.78, 0.5];
    let (bw, gap, max_h, min_h) = (3.0, 2.5, 18.0, 3.0);
    let total = 5.0 * bw + 4.0 * gap;
    g.glow(cx, cy, 20.0, theme::ACCENT.alpha(0.10 + 0.22 * level));
    for (i, s) in SHAPE.iter().enumerate() {
        let wave = 0.72 + 0.28 * (t * 8.5 + i as f32 * 1.25).sin();
        let h = min_h + (max_h - min_h) * (level * s * wave).clamp(0.0, 1.0);
        let x = cx - total / 2.0 + i as f32 * (bw + gap);
        let c = theme::ACCENT.mix(theme::ACCENT2, i as f32 / 4.0);
        g.fill(Rect::new(x, cy - h / 2.0, bw, h), bw / 2.0, c);
    }
}

/// Three pulsing dots.
fn dots(g: &mut Gfx, cx: f32, cy: f32, t: f32) {
    for i in 0..3 {
        let phase = (t * 5.0 - i as f32 * 0.7).sin() * 0.5 + 0.5;
        let c = theme::ACCENT.mix(theme::ACCENT2, i as f32 / 2.0).alpha(0.3 + 0.7 * phase);
        g.circle(cx + (i as f32 - 1.0) * 7.0, cy - phase * 1.5, 2.6, c);
    }
}

/// A highlight sweeping along the card's bottom edge.
fn shimmer(g: &mut Gfx, track: Rect, t: f32) {
    let seg = track.w * 0.28;
    let p = (t * 0.9).fract();
    let x = track.x - seg + p * (track.w + seg);
    let a = Rect::new(x.max(track.x), track.y, (x + seg / 2.0).min(track.right()) - x.max(track.x), track.h);
    let b = Rect::new((x + seg / 2.0).max(track.x), track.y, (x + seg).min(track.right()) - (x + seg / 2.0).max(track.x), track.h);
    if a.w > 0.0 {
        g.hgradient(a, 0.0, theme::ACCENT.alpha(0.0), theme::ACCENT2.alpha(0.9));
    }
    if b.w > 0.0 {
        g.hgradient(b, 0.0, theme::ACCENT2.alpha(0.9), theme::ACCENT.alpha(0.0));
    }
}

fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

// --------------------------------------------------------------- renderer --

struct Surface {
    rt: ID2D1DCRenderTarget,
    g: Gfx,
    mem: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: (i32, i32),
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.mem);
        }
    }
}

struct Anim {
    surface: Option<Surface>,
    visible: bool,
    shown_at: Instant,
    hiding_since: Option<Instant>,
    last: Option<Frame>,
    level: f32,
    timer: bool,
}

thread_local! {
    static ANIM: RefCell<Anim> = RefCell::new(Anim {
        surface: None,
        visible: false,
        shown_at: Instant::now(),
        hiding_since: None,
        last: None,
        level: 0.0,
        timer: false,
    });
}

fn dc_target(dpi: f32) -> windows::core::Result<ID2D1DCRenderTarget> {
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        factory.CreateDCRenderTarget(&props)
    }
}

fn surface(anim: &mut Anim, w: i32, h: i32, dpi: f32) -> Option<&mut Surface> {
    let fits = anim.surface.as_ref().is_some_and(|s| s.size == (w, h));
    if !fits {
        let keep = anim.surface.take();
        let rt = match keep {
            Some(s) => s.rt.clone(),
            None => dc_target(dpi).ok()?,
        };
        unsafe {
            let mem = CreateCompatibleDC(None);
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let target: ID2D1RenderTarget = rt.cast().ok()?;
            let g = Gfx::from_target(target, dpi).ok()?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            anim.surface = Some(Surface { rt, g, mem, bmp, old, bits: bits as *mut u8, size: (w, h) });
        }
    }
    let s = anim.surface.as_mut()?;
    if s.g.dpi != dpi {
        s.g.set_dpi(dpi);
    }
    Some(s)
}

fn set_timer(hwnd: HWND, anim: &mut Anim, on: bool) {
    if on != anim.timer {
        anim.timer = on;
        unsafe {
            if on {
                SetTimer(Some(hwnd), TIMER_ANIM, FRAME_MS, None);
            } else {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
        }
    }
}

/// Lays out, draws and shows (or hides) the window. Main thread only.
unsafe fn render(hwnd: HWND) {
    ANIM.with(|cell| {
        let mut anim = cell.borrow_mut();
        let anim = &mut *anim;
        let (mode, live, hover, min_h) = {
            let v = VIEW.lock().unwrap();
            (v.mode.clone(), v.live.clone(), v.hover, v.min_h)
        };
        let wanted = match &mode {
            Mode::Live(_) | Mode::Transcribing => ENABLED.load(Ordering::Relaxed),
            Mode::Hidden => false,
            _ => true,
        };
        let now = Instant::now();
        let status = crate::status::get();

        let frame = if wanted {
            if !anim.visible || anim.hiding_since.is_some() {
                anim.shown_at = now;
                anim.hiding_since = None;
            }
            anim.visible = true;
            let target = if matches!(mode, Mode::Live(_)) { status.level } else { 0.0 };
            let k = if target > anim.level { 0.55 } else { 0.18 };
            anim.level += (target - anim.level) * k;
            let elapsed = if status.phase == crate::status::Phase::Recording { status.phase_since.elapsed().as_secs_f32() } else { 0.0 };
            let f = Frame { mode: mode.clone(), live, level: anim.level, elapsed, t: (now - anim.shown_at).as_secs_f32(), hover };
            anim.last = Some(f.clone());
            f
        } else {
            if !anim.visible {
                set_timer(hwnd, anim, false);
                return;
            }
            let since = *anim.hiding_since.get_or_insert(now);
            if (now - since).as_secs_f32() >= FADE_OUT || anim.last.is_none() {
                anim.visible = false;
                anim.hiding_since = None;
                anim.last = None;
                anim.level = 0.0;
                VIEW.lock().unwrap().min_h = 0.0;
                set_timer(hwnd, anim, false);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return;
            }
            let mut f = anim.last.clone().unwrap();
            f.t = (now - anim.shown_at).as_secs_f32();
            f
        };

        // Monitor of the foreground window, its DPI and work area.
        let (dpi, work) = unsafe {
            let fg = GetForegroundWindow();
            let monitor = MonitorFromWindow(if fg.is_invalid() { hwnd } else { fg }, MONITOR_DEFAULTTOPRIMARY);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut mi);
            (dx.max(96) as f32, mi.rcWork)
        };
        let scale = dpi / 96.0;
        let Some(s) = surface(anim, 1, 1, dpi).map(|s| s as *mut Surface) else { return };
        // Measure with the current surface, then size it.
        let (mut card_h, lh, shown, total) = measure(unsafe { &mut (*s).g }, &frame);
        if matches!(frame.mode, Mode::Live(_) | Mode::Transcribing) && wanted {
            card_h = card_h.max(min_h);
            VIEW.lock().unwrap().min_h = card_h;
        }
        let px = |v: f32| (v * scale).round() as i32;
        let (w, h) = (px(CARD_W + 2.0 * MARGIN), px(card_h + 2.0 * MARGIN));
        let Some(s) = surface(anim, w, h, dpi) else { return };
        let buttons = unsafe {
            let r = RECT { left: 0, top: 0, right: w, bottom: h };
            if s.rt.BindDC(s.mem, &r).is_err() {
                return;
            }
            s.g.begin();
            s.g.clear(Color(0.0, 0.0, 0.0, 0.0));
            let b = draw(&mut s.g, &frame, card_h, lh, shown, total);
            if !s.g.end() {
                anim.surface = None;
                return;
            }
            b
        };

        // Fade and slide.
        let fade = if let Some(since) = anim.hiding_since {
            1.0 - ease((now - since).as_secs_f32() / FADE_OUT)
        } else {
            ease((now - anim.shown_at).as_secs_f32() / FADE_IN)
        };
        let slide = px(8.0 * (1.0 - fade));
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.bottom - px(LIFT) - h + px(MARGIN) + slide;
        let s = anim.surface.as_ref().unwrap();
        unsafe {
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: (fade * 255.0).round() as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let screen = GetDC(None);
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&POINT { x, y }),
                Some(&SIZE { cx: w, cy: h }),
                Some(s.mem),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen);
            if !IsWindowVisible(hwnd).as_bool() {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            // Clickable only as a card.
            let card = matches!(frame.mode, Mode::Card { .. }) && anim.hiding_since.is_none();
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let want = if card { ex & !(WS_EX_TRANSPARENT.0 as isize) } else { ex | WS_EX_TRANSPARENT.0 as isize };
            if want != ex {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
            }
        }
        {
            let to_px = |r: Rect| RECT { left: px(r.x), top: px(r.y), right: px(r.right()), bottom: px(r.bottom()) };
            let mut v = VIEW.lock().unwrap();
            match buttons {
                Some((copy, close)) => {
                    v.copy_btn = to_px(copy);
                    v.close_btn = to_px(close);
                }
                None => {
                    v.copy_btn = ZERO;
                    v.close_btn = ZERO;
                }
            }
        }
        let moving = matches!(frame.mode, Mode::Live(_) | Mode::Transcribing | Mode::Progress(..))
            || anim.hiding_since.is_some()
            || fade < 1.0;
        set_timer(hwnd, anim, moving);
    });
}

/// Hide timers for notices and cards, restarted when the mode changes.
unsafe fn arm_hide(hwnd: HWND) {
    let v = VIEW.lock().unwrap();
    let ms = match &v.mode {
        Mode::Notice(_) => Some(3500),
        Mode::Card { copied: true, .. } => Some(1200),
        Mode::Card { .. } => Some(120_000),
        _ => None,
    };
    drop(v);
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_HIDE);
        if let Some(ms) = ms {
            SetTimer(Some(hwnd), TIMER_HIDE, ms, None);
        }
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn click(x: i32, y: i32) {
    let mut v = VIEW.lock().unwrap();
    let Mode::Card { text, reason, .. } = &v.mode else { return };
    if inside(&v.copy_btn, x, y) {
        let (text, reason) = (text.clone(), reason.clone());
        v.mode = Mode::Card { text: text.clone(), reason, copied: true };
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

fn hover_at(x: i32, y: i32) -> Option<Button> {
    let v = VIEW.lock().unwrap();
    if inside(&v.copy_btn, x, y) {
        Some(Button::Copy)
    } else if inside(&v.close_btn, x, y) {
        Some(Button::Close)
    } else {
        None
    }
}

fn set_hover(hwnd: HWND, h: Option<Button>) {
    let changed = {
        let mut v = VIEW.lock().unwrap();
        let c = v.hover != h;
        v.hover = h;
        c
    };
    if changed {
        unsafe { render(hwnd) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_UPDATE => {
                arm_hide(hwnd);
                render(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ANIM => {
                render(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_HIDE => {
                let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                let mut v = VIEW.lock().unwrap();
                if matches!(v.mode, Mode::Notice(_) | Mode::Card { .. }) {
                    v.mode = Mode::Hidden;
                    drop(v);
                    render(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                set_hover(hwnd, hover_at(x, y));
                LRESULT(0)
            }
            0x02A3 /* WM_MOUSELEAVE */ => {
                set_hover(hwnd, None);
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
                let _ = ScreenToClient(hwnd, &mut p);
                let over = hover_at(p.x, p.y).is_some();
                SetCursor(LoadCursorW(None, if over { IDC_HAND } else { IDC_ARROW }).ok());
                LRESULT(1)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Releases the Direct2D surface. Call on the main thread before exiting:
/// dropping it during process teardown corrupts the exit.
pub fn shutdown() {
    ANIM.with(|cell| cell.borrow_mut().surface = None);
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
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- preview --

/// Draws a card through the live path (DC render target into the DIB that
/// UpdateLayeredWindow would show) and checks the pixels. No window.
pub fn selftest() -> Result<(), String> {
    ANIM.with(|cell| {
        let mut anim = cell.borrow_mut();
        let frame = Frame {
            mode: Mode::Card { text: "Self-test".into(), reason: "Not pasted".into(), copied: false },
            live: String::new(),
            level: 0.0,
            elapsed: 0.0,
            t: 0.0,
            hover: None,
        };
        let dpi = 144.0;
        let s = surface(&mut anim, 1, 1, dpi).ok_or("surface")?;
        let (card_h, lh, shown, total) = measure(&mut s.g, &frame);
        let (w, h) = (((CARD_W + 2.0 * MARGIN) * 1.5) as i32, ((card_h + 2.0 * MARGIN) * 1.5) as i32);
        let s = surface(&mut anim, w, h, dpi).ok_or("surface")?;
        unsafe {
            s.rt.BindDC(s.mem, &RECT { left: 0, top: 0, right: w, bottom: h }).map_err(|e| e.to_string())?;
            s.g.begin();
            s.g.clear(Color(0.0, 0.0, 0.0, 0.0));
            draw(&mut s.g, &frame, card_h, lh, shown, total);
            if !s.g.end() {
                return Err("EndDraw failed".into());
            }
            let px = |x: i32, y: i32| {
                let p = s.bits.add(((y * w + x) * 4) as usize);
                [*p, *p.add(1), *p.add(2), *p.add(3)]
            };
            let corner = px(1, 1);
            let inside = px(w / 2, (MARGIN * 1.5) as i32 + 4);
            println!("corner bgra {corner:?}, card bgra {inside:?}, size {w}x{h}");
            if corner[3] != 0 || inside[3] < 240 {
                return Err("unexpected alpha".into());
            }
        }
        anim.surface = None;
        Ok(())
    })
}

/// Draws one overlay state to a PNG over a sample backdrop, for design
/// review: `chatty --render-overlay <live|long|transcribing|notice|warning|progress|card|copied> out.png [scale]`.
pub fn render_png(kind: &str, path: &std::path::Path, scale: f32) -> Result<(), String> {
    use windows::Win32::Graphics::Direct2D::*;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::core::HSTRING;
    let e = |e: windows::core::Error| e.to_string();
    let long = "So I was thinking we could move the review to Thursday afternoon, because the numbers from the \
                second test run won't be ready before Wednesday evening, and I'd rather walk everyone through the \
                final version than a draft that changes again the next morning.";
    let short = "Let's ship the overlay redesign today and";
    let (mode, live, level, elapsed, hover) = match kind {
        "live" => (Mode::Live(short.into()), short.to_string(), 0.7, 7.0, None),
        "listening" => (Mode::Live(String::new()), String::new(), 0.1, 1.0, None),
        "long" => (Mode::Live(long.into()), long.to_string(), 0.55, 23.0, None),
        "transcribing" => (Mode::Transcribing, long.to_string(), 0.0, 0.0, None),
        "notice" => (Mode::Notice("Downloaded Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "warning" => (Mode::Notice("Download failed: Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "progress" => (Mode::Progress("Downloading Parakeet Unified EN 0.6B".into(), 0.42), String::new(), 0.0, 0.0, None),
        "copied" => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: true },
            String::new(),
            0.0,
            0.0,
            None,
        ),
        _ => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: false },
            String::new(),
            0.0,
            0.0,
            Some(Button::Copy),
        ),
    };
    let frame = Frame { mode, live, level, elapsed, t: 1.3, hover };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(e)?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(e)?;
        let dpi = 96.0 * scale;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        // Measure first on a scratch target.
        let scratch = wic.CreateBitmap(4, 4, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let rt = factory.CreateWicBitmapRenderTarget(&scratch, &props).map_err(e)?;
        let mut g = Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?;
        let (card_h, lh, shown, total) = measure(&mut g, &frame);
        let (bw, bh) = (CARD_W + 2.0 * MARGIN + 80.0, card_h + 2.0 * MARGIN + 60.0);
        let (pw, ph) = ((bw * scale) as u32, (bh * scale) as u32);
        let bmp = wic.CreateBitmap(pw, ph, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let rt = factory.CreateWicBitmapRenderTarget(&bmp, &props).map_err(e)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let mut g = Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        g.begin();
        // A busy, light-and-dark backdrop to judge contrast and the shadow.
        g.gradient(Rect::new(0.0, 0.0, bw, bh), 0.0, Color::rgb(58, 74, 96), Color::rgb(214, 220, 228));
        g.fill(Rect::new(0.0, bh * 0.55, bw * 0.45, bh), 0.0, Color::rgb(245, 245, 247));
        g.rt.SetTransform(&windows_numerics::Matrix3x2::translation(40.0, 30.0));
        draw(&mut g, &frame, card_h, lh, shown, total);
        g.rt.SetTransform(&windows_numerics::Matrix3x2::identity());
        g.end();

        let stream = wic.CreateStream().map_err(e)?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), 0x4000_0000).map_err(e)?;
        let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).map_err(e)?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache).map_err(e)?;
        let mut fe: Option<IWICBitmapFrameEncode> = None;
        enc.CreateNewFrame(&mut fe, std::ptr::null_mut()).map_err(e)?;
        let fe = fe.ok_or("no frame")?;
        fe.Initialize(None).map_err(e)?;
        fe.SetSize(pw, ph).map_err(e)?;
        let mut fmt = GUID_WICPixelFormat32bppPBGRA;
        fe.SetPixelFormat(&mut fmt).map_err(e)?;
        fe.WriteSource(&bmp, std::ptr::null()).map_err(e)?;
        fe.Commit().map_err(e)?;
        enc.Commit().map_err(e)?;
    }
    Ok(())
}
