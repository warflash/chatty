//! Floating card at the bottom of the screen: live transcript, transcribing,
//! notices, download progress, and transcripts that could not be pasted
//! (with Copy). Drawn with the main window's renderer into a per-pixel-alpha
//! window (anti-aliased corners, soft shadow, the same fonts and colors):
//! a layered window on Windows, a borderless panel on macOS (`host`).
//!
//! It never takes focus and is click-through except as a card. Any thread
//! updates the shared view and posts to the main thread, which lays out and
//! draws. An animation timer runs only while something
//! moves (level meter, transcribing, fades), so idle CPU stays at zero.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::ui::gfx::{Align, Color, Font, Gfx, Rect};
use crate::ui::{icon, theme};

#[cfg_attr(windows, path = "overlay/win.rs")]
#[cfg_attr(target_os = "macos", path = "overlay/mac.rs")]
mod host;
pub use host::{create, render_png, selftest, shutdown};

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
    /// Bytes of `live` that are settled.
    split: usize,
    /// Button rects in window pixels (card mode).
    copy_btn: Px,
    close_btn: Px,
    hover: Option<Button>,
}

/// A rectangle in window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Px {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

const ZERO: Px = Px { left: 0, top: 0, right: 0, bottom: 0 };

static VIEW: Mutex<View> =
    Mutex::new(View { mode: Mode::Hidden, live: String::new(), split: 0, copy_btn: ZERO, close_btn: ZERO, hover: None });

fn post() {
    host::post();
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

/// Recording: `settled` is text that won't change any more, `tail` the
/// part still being re-transcribed (shown dimmer).
pub fn live(settled: &str, tail: &str) {
    let mut text = settled.to_string();
    if !settled.is_empty() && !tail.is_empty() {
        text.push(' ');
    }
    let split = text.len();
    text.push_str(tail);
    let t = text.clone();
    crate::status::update(|s| s.live = t);
    VIEW.lock().unwrap().split = if tail.is_empty() { text.len() } else { split };
    set(Mode::Live(text));
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
const MARGIN: f32 = 40.0;
const MAX_W: f32 = 640.0;
const MIN_W: f32 = 340.0;
const PAD_X: f32 = 14.0;
const PAD_Y: f32 = 13.0;
const BADGE: f32 = 34.0;
const GAP: f32 = 12.0;
const META_W: f32 = 40.0;
const RADIUS: f32 = 22.0;
const LINE_H: f32 = 23.0;
const LIVE_LINES: u32 = 3;
const CARD_LINES: u32 = 6;
const FOOTER_H: f32 = 34.0;
/// Card bottom above the work area's bottom edge (DIPs).
const LIFT: f32 = 36.0;
const FADE_IN: f32 = 0.18;
const FADE_OUT: f32 = 0.16;

const FONT: Font = Font::text(15.5);
const META: Font = Font::text(12.5);

/// Everything one frame needs.
#[derive(Clone)]
struct Frame {
    mode: Mode,
    live: String,
    /// Bytes of the live text that are settled; the rest may still change.
    split: usize,
    level: f32,
    elapsed: f32,
    /// Seconds since the overlay appeared (animations).
    t: f32,
    hover: Option<Button>,
}

/// Card geometry in DIPs.
#[derive(Clone, Copy, Debug, Default)]
struct Geo {
    w: f32,
    h: f32,
    /// Text wrap width.
    tw: f32,
    shown: u32,
    total: u32,
    /// How far the text is scrolled up (live text keeps its newest lines).
    scroll: f32,
}

fn is_warning(s: &str) -> bool {
    let l = s.to_lowercase();
    ["fail", "unavailable", "error", "no microphone", "not found", "elevated"].iter().any(|k| l.contains(k))
}

/// (text, settled UTF-16 units, settled color, changing color)
fn body(f: &Frame) -> (String, usize, Color, Color) {
    let tentative = theme::TEXT.alpha(0.5);
    let units = |s: &str| s.encode_utf16().count();
    match &f.mode {
        Mode::Live(t) if t.trim().is_empty() => ("Listening…".into(), usize::MAX, theme::TEXT3, theme::TEXT3),
        Mode::Live(t) => {
            let split = f.split.min(t.len());
            let lead = t.len() - t.trim_start().len();
            (t.trim().to_string(), units(&t[lead.min(split)..split]), theme::TEXT, tentative)
        }
        Mode::Transcribing if f.live.trim().is_empty() => ("Transcribing…".into(), usize::MAX, theme::TEXT3, theme::TEXT3),
        Mode::Transcribing => (f.live.trim().into(), usize::MAX, theme::TEXT2, theme::TEXT2),
        Mode::Notice(t) | Mode::Progress(t, _) => (t.clone(), usize::MAX, theme::TEXT, theme::TEXT),
        Mode::Card { text, .. } => (text.clone(), usize::MAX, theme::TEXT, theme::TEXT),
        Mode::Hidden => (String::new(), usize::MAX, theme::TEXT, theme::TEXT),
    }
}

fn has_meta(m: &Mode) -> bool {
    // Transcribing keeps the timer's column so the text doesn't reflow.
    matches!(m, Mode::Live(_) | Mode::Transcribing | Mode::Progress(..))
}

/// Target geometry: the card hugs short text and wraps at `MAX_W`.
fn layout(g: &mut Gfx, f: &Frame) -> Geo {
    layout_at(g, f, 0.0)
}

/// `layout`, at least `min_w` wide.
fn layout_at(g: &mut Gfx, f: &Frame, min_w: f32) -> Geo {
    let (s, ..) = body(f);
    let s = if s.is_empty() { " ".to_string() } else { s };
    let card = matches!(f.mode, Mode::Card { .. });
    let chrome = 2.0 * PAD_X + BADGE + GAP + if has_meta(&f.mode) { META_W + 8.0 } else { 6.0 };
    let (one_line, _) = g.measure(&s, FONT, 10_000.0, false);
    let w = if card { MAX_W } else { (one_line + chrome + 4.0).max(min_w).clamp(MIN_W, MAX_W) };
    let tw = w - chrome;
    let total = g.line_count(&s, FONT, tw, LINE_H);
    let max = if card { CARD_LINES } else { LIVE_LINES };
    let shown = total.min(max);
    let mut h = (2.0 * PAD_Y + shown as f32 * LINE_H).max(2.0 * PAD_Y + BADGE - 6.0);
    if card {
        h += 8.0 + FOOTER_H;
    }
    if matches!(f.mode, Mode::Progress(..)) {
        h += 10.0;
    }
    let scroll = if card { 0.0 } else { (total - shown) as f32 * LINE_H };
    Geo { w, h, tw, shown, total, scroll }
}

/// Draws the card centered in the window. Returns the button rects (DIPs).
fn draw(g: &mut Gfx, f: &Frame, geo: &Geo) -> Option<(Rect, Rect)> {
    let card = Rect::new(MARGIN + (MAX_W - geo.w) / 2.0, MARGIN, geo.w, geo.h);
    let radius = RADIUS.min(card.h / 2.0);
    // Ambient and key shadows.
    for i in 0..14 {
        let d = 2.0 + i as f32 * 2.0;
        let r = Rect::new(card.x - d, card.y - d + 10.0, card.w + 2.0 * d, card.h + 2.0 * d);
        g.fill(r, radius + d, Color(0.0, 0.0, 0.0, 0.026 * (1.0 - i as f32 / 14.0)));
    }
    g.fill(Rect::new(card.x, card.y + 1.5, card.w, card.h), radius, Color(0.0, 0.0, 0.0, 0.22));
    // Body: a dark, slightly lifted surface with a light-catching edge.
    g.gradient(card, radius, Color(0.115, 0.115, 0.135, 0.975), Color(0.071, 0.071, 0.086, 0.975));
    g.stroke_vgradient(card.inset(0.5, 0.5), radius - 0.5, Color::white(0.16), Color::white(0.045), 1.0);

    let card_mode = matches!(f.mode, Mode::Card { .. });
    let text_top = card.y + PAD_Y;
    let block_h = geo.shown as f32 * LINE_H;
    let content_h = card.h - if card_mode { 8.0 + FOOTER_H } else { 0.0 } - if matches!(f.mode, Mode::Progress(..)) { 10.0 } else { 0.0 };
    // The badge is centered on the text block (the first line on cards).
    let cy = if card_mode { text_top + LINE_H / 2.0 } else { card.y + content_h / 2.0 };
    let bx = card.x + PAD_X;
    badge(g, f, bx + BADGE / 2.0, cy);

    // Text: settled words bright, words that may still change dimmer.
    // Live text keeps its newest lines (scrolls, top fades); cards keep the
    // first lines (bottom fades).
    let (s, split, c1, c2) = body(f);
    let tx = bx + BADGE + GAP;
    let ty = if card_mode { text_top } else { card.y + (content_h - block_h) / 2.0 };
    let visible = Rect::new(tx - 2.0, ty, (card.right() - PAD_X - tx + 2.0).max(1.0), block_h);
    let overflow = geo.total > geo.shown;
    let fade = if overflow { LINE_H * 0.9 } else { 0.0 };
    let (top_fade, bottom_fade) = if card_mode { (0.0, fade) } else { (fade.min(geo.scroll), 0.0) };
    g.push_fade(Rect::new(visible.x, visible.y, visible.w, visible.h + 3.0), top_fade, bottom_fade);
    g.text_two(&s, split, FONT, Rect::new(tx, ty - geo.scroll, geo.tw, geo.total as f32 * LINE_H + LINE_H), LINE_H, c1, c2);
    g.pop_fade();

    // Right meta: recording time or percentage, on the badge's line.
    let meta = Rect::new(card.right() - PAD_X - META_W, cy - 10.0, META_W, 20.0);
    match &f.mode {
        Mode::Live(_) if f.elapsed >= 1.0 => {
            let secs = f.elapsed as u32;
            g.label(&format!("{}:{:02}", secs / 60, secs % 60), META, meta, theme::TEXT3, Align::Right);
        }
        Mode::Progress(_, p) => g.label(&format!("{:.0}%", p * 100.0), META, meta, theme::TEXT2, Align::Right),
        _ => {}
    }

    if let Mode::Progress(_, p) = &f.mode {
        let bar = Rect::new(tx, ty + block_h + 7.0, card.right() - PAD_X - tx, 3.0);
        g.fill(bar, 1.5, Color::white(0.08));
        g.hgradient(Rect::new(bar.x, bar.y, (bar.w * p.clamp(0.0, 1.0)).max(3.0), bar.h), 1.5, theme::ACCENT, theme::ACCENT2);
    }

    // Card footer: reason and buttons.
    if let Mode::Card { reason, copied, .. } = &f.mode {
        let fy = card.bottom() - PAD_Y - FOOTER_H + 3.0;
        let close = Rect::new(card.right() - PAD_X - 32.0, fy, 32.0, 32.0);
        let copy = Rect::new(close.x - 8.0 - 98.0, fy, 98.0, 32.0);
        g.label(reason, META, Rect::new(tx, fy, copy.x - tx - 12.0, 32.0), theme::TEXT3, Align::Left);
        let hover_copy = f.hover == Some(Button::Copy);
        if *copied {
            g.fill(copy, 16.0, theme::GREEN.alpha(0.14));
            g.stroke(copy.inset(0.5, 0.5), 15.5, theme::GREEN.alpha(0.45), 1.0);
            button_label(g, copy, icon::CHECK, "Copied", theme::GREEN);
        } else {
            let lift = if hover_copy { 0.1 } else { 0.0 };
            g.gradient(copy, 16.0, theme::ACCENT.mix(Color::white(1.0), lift), theme::ACCENT2.mix(Color::white(1.0), lift * 0.6));
            g.stroke_vgradient(copy.inset(0.5, 0.5), 15.5, Color::white(0.32), Color::white(0.06), 1.0);
            button_label(g, copy, icon::COPY, "Copy", theme::ACCENT_TEXT);
        }
        let hover_close = f.hover == Some(Button::Close);
        g.fill(close, 16.0, if hover_close { Color::white(0.12) } else { Color::white(0.06) });
        g.label(&'\u{E711}'.to_string(), Font::icon(10.5), close, if hover_close { theme::TEXT } else { theme::TEXT2 }, Align::Center);
        return Some((copy, close));
    }
    None
}

/// The round badge on the left: what the overlay is doing.
fn badge(g: &mut Gfx, f: &Frame, cx: f32, cy: f32) {
    let r = BADGE / 2.0;
    let tint = |g: &mut Gfx, c: Color| {
        g.circle(cx, cy, r, c.alpha(0.13));
        g.ring(cx, cy, r - 0.5, c.alpha(0.22), 1.0);
    };
    let glyph = |g: &mut Gfx, ic: char, c: Color| {
        g.label(&ic.to_string(), Font::icon(14.0), Rect::new(cx - r, cy - r, BADGE, BADGE), c, Align::Center);
    };
    match &f.mode {
        Mode::Live(_) => {
            // A halo that breathes with the voice, and the level bars.
            let lv = f.level.clamp(0.0, 1.0);
            g.glow(cx, cy, r + 10.0, theme::ACCENT.alpha(0.10 + 0.25 * lv));
            g.circle(cx, cy, r, theme::ACCENT.alpha(0.14 + 0.06 * lv));
            g.ring(cx, cy, r + 2.0 + 4.0 * lv, theme::ACCENT2.alpha(0.32 * lv), 1.25);
            level_bars(g, cx, cy, lv, f.t);
        }
        Mode::Transcribing => {
            tint(g, theme::ACCENT);
            spinner(g, cx, cy, f.t);
        }
        Mode::Notice(t) if is_warning(t) => {
            tint(g, theme::AMBER);
            glyph(g, icon::WARNING, theme::AMBER);
        }
        Mode::Notice(_) => {
            tint(g, theme::BLUE);
            glyph(g, '\u{E946}', theme::BLUE);
        }
        Mode::Progress(..) => {
            tint(g, theme::ACCENT);
            glyph(g, icon::DOWNLOAD, theme::ACCENT);
        }
        Mode::Card { copied, .. } => {
            let c = if *copied { theme::GREEN } else { theme::ACCENT };
            tint(g, c);
            glyph(g, if *copied { icon::CHECK } else { '\u{E77F}' }, c);
        }
        Mode::Hidden => {}
    }
}

fn button_label(g: &mut Gfx, r: Rect, ic: char, label: &str, c: Color) {
    let font = Font::semibold(13.0);
    let (lw, _) = g.measure(label, font, 200.0, false);
    let total = 14.0 + 7.0 + lw;
    let x = r.x + (r.w - total) / 2.0;
    g.label(&ic.to_string(), Font::icon(13.0), Rect::new(x, r.y, 14.0, r.h), c, Align::Center);
    g.label(label, font, Rect::new(x + 21.0, r.y, lw + 2.0, r.h), c, Align::Left);
}

/// Five rounded bars, exactly centered on (cx, cy), driven by the
/// microphone level with a gentle wave so they stay alive between words.
fn level_bars(g: &mut Gfx, cx: f32, cy: f32, level: f32, t: f32) {
    const SHAPE: [f32; 5] = [0.45, 0.75, 1.0, 0.75, 0.45];
    let (bw, gap, max_h, min_h) = (2.5, 2.25, 16.0, 2.5);
    let span = 5.0 * bw + 4.0 * gap;
    for (i, s) in SHAPE.iter().enumerate() {
        let wave = 0.7 + 0.3 * (t * 8.0 + i as f32 * 1.1).sin();
        let h = min_h + (max_h - min_h) * (level * s * wave).clamp(0.0, 1.0);
        let x = cx - span / 2.0 + i as f32 * (bw + gap);
        let c = Color::white(0.94).mix(theme::ACCENT2, 0.2 + 0.12 * (i as f32 - 2.0).abs());
        g.fill(Rect::new(x, cy - h / 2.0, bw, h), bw / 2.0, c);
    }
}

/// Eight dots on a circle, brightness chasing around.
fn spinner(g: &mut Gfx, cx: f32, cy: f32, t: f32) {
    let head = t * 1.4;
    for i in 0..8 {
        let a = i as f32 / 8.0;
        let behind = (head - a).rem_euclid(1.0);
        let alpha = 0.22 + 0.78 * (1.0 - behind).powi(2);
        let ang = a * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        g.circle(cx + ang.cos() * 7.5, cy + ang.sin() * 7.5, 1.9, Color::white(0.9).mix(theme::ACCENT2, 0.35).alpha(alpha));
    }
}

fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

// ------------------------------------------------------------------ input --

fn inside(r: &Px, x: i32, y: i32) -> bool {
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

