//! The main window: one place for status, history, statistics, models,
//! words and settings. Immediate-mode widgets drawn with Direct2D on the
//! main thread; the Windows 11 Mica backdrop shows through the background.
//!
//! Every input event repaints one frame. Widgets decide hover/click from the
//! frame's input; side effects are queued as `Action`s and executed after the
//! frame, outside the window state borrow (Win32 calls may re-enter the
//! window procedure).

pub mod gfx;
mod pages;

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, TRACKMOUSEEVENT, TME_LEAVE, TrackMouseEvent, VK_CONTROL};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

use crate::config;
use crate::stats::{self, Report};
use gfx::{Align, Color, Font, Gfx, Rect};

// ------------------------------------------------------------------ theme --

pub mod theme {
    use super::gfx::Color;
    // Surfaces, darkest to lightest.
    pub const SIDEBAR: Color = Color::rgb(12, 12, 14);
    pub const BG: Color = Color::rgb(17, 17, 20);
    pub const CARD: Color = Color::rgb(24, 24, 28);
    pub const CARD_HOVER: Color = Color::rgb(29, 29, 34);
    pub const CARD_STROKE: Color = Color(1.0, 1.0, 1.0, 0.065);
    pub const CONTROL: Color = Color::rgb(36, 36, 42);
    pub const CONTROL_HOVER: Color = Color::rgb(44, 44, 51);
    pub const CONTROL_DOWN: Color = Color::rgb(31, 31, 36);
    pub const HAIRLINE: Color = Color(1.0, 1.0, 1.0, 0.06);
    // Text.
    pub const TEXT: Color = Color::rgb(237, 237, 240);
    pub const TEXT2: Color = Color::rgb(161, 161, 170);
    pub const TEXT3: Color = Color::rgb(113, 113, 122);
    // Accent and states.
    pub const ACCENT: Color = Color::rgb(130, 132, 255);
    pub const ACCENT2: Color = Color::rgb(167, 139, 250);
    pub const ACCENT_TEXT: Color = Color::rgb(255, 255, 255);
    pub const RED: Color = Color::rgb(255, 92, 102);
    pub const AMBER: Color = Color::rgb(251, 191, 36);
    pub const GREEN: Color = Color::rgb(74, 222, 128);
    pub const BLUE: Color = Color::rgb(96, 165, 250);
    pub const RADIUS: f32 = 12.0;
}

// Segoe Fluent Icons / MDL2 code points.
pub mod icon {
    pub const HOME: char = '\u{E80F}';
    pub const HISTORY: char = '\u{E81C}';
    pub const CHART: char = '\u{E9D2}';
    pub const MODELS: char = '\u{E8F1}';
    pub const WORDS: char = '\u{E8D2}';
    pub const SETTINGS: char = '\u{E713}';
    pub const MIC: char = '\u{E720}';
    pub const PLAY: char = '\u{E768}';
    pub const COPY: char = '\u{E8C8}';
    pub const DOWNLOAD: char = '\u{E896}';
    pub const DELETE: char = '\u{E74D}';
    pub const CHECK: char = '\u{E73E}';
    pub const REFRESH: char = '\u{E72C}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const ADD: char = '\u{E710}';
    pub const REMOVE: char = '\u{E738}';
    pub const SEARCH: char = '\u{E721}';
    pub const OPEN: char = '\u{E8A7}';
    pub const FOLDER: char = '\u{E838}';
    pub const CLOCK: char = '\u{E823}';
    pub const KEYBOARD: char = '\u{E765}';
    pub const SPARKLE: char = '\u{E945}';
    pub const WARNING: char = '\u{E7BA}';
}

// ---------------------------------------------------------------- context --

pub struct Ctx {
    pub cfg: config::Shared,
    pub engine: Option<crate::engine::Handle>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static NOTIFY_PENDING: AtomicBool = AtomicBool::new(false);
static STANDALONE: AtomicBool = AtomicBool::new(false);

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_STATUS: u32 = WM_APP + 30;
const WM_STATS: u32 = WM_APP + 31;
const TIMER_ANIM: usize = 1;

pub fn init(cfg: config::Shared, engine: Option<crate::engine::Handle>) {
    let _ = CTX.set(Ctx { cfg, engine });
}

fn ctx() -> &'static Ctx {
    CTX.get().expect("ui::init")
}

fn hwnd() -> Option<HWND> {
    let h = HWND_RAW.load(Ordering::Relaxed);
    (h != 0).then(|| HWND(h as *mut _))
}

fn visible() -> bool {
    hwnd().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() })
}

/// Live status changed (any thread).
pub fn notify() {
    if let Some(h) = hwnd() {
        if visible() && !NOTIFY_PENDING.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(Some(h), WM_STATUS, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// The statistics database changed (any thread).
pub fn stats_changed() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_STATS, WPARAM(0), LPARAM(0));
        }
    }
}

// ------------------------------------------------------------------ input --

#[derive(Default)]
pub struct Input {
    pub mouse: (f32, f32),
    pub down: bool,
    pub pressed: bool,
    pub released: bool,
    pub wheel: f32,
    pub chars: Vec<char>,
    pub keys: Vec<u16>,
}

#[derive(Default, Clone)]
pub struct TextField {
    pub text: String,
    /// Caret as a char index.
    pub caret: usize,
}

#[derive(Clone)]
struct Popup {
    id: u64,
    anchor: Rect,
    options: Vec<String>,
    selected: usize,
}

#[derive(Default)]
struct Memory {
    active: u64,
    focus: u64,
    popup: Option<Popup>,
    popup_rect: Option<Rect>,
    /// Popup choice made last frame: (select id, option index).
    chosen: Option<(u64, usize)>,
    hand: bool,
    ibeam: bool,
}

/// Side effects queued during a frame.
pub enum Action {
    Config(Vec<(String, toml_edit::Value)>),
    Open(std::path::PathBuf),
    Copy(String),
    Play(String),
    Download(&'static str),
    DeleteModel(&'static crate::models::Asset),
    ReloadStats,
    ListDevices,
}

pub fn id(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Subtle,
}

pub struct Ui<'a> {
    pub g: &'a mut Gfx,
    input: &'a Input,
    mem: &'a mut Memory,
    pub actions: &'a mut Vec<Action>,
    /// Seconds since the window was created (animations).
    pub t: f32,
    /// Visible region for hit-testing (content viewport).
    pub clip: Rect,
    blocked: bool,
    pub animate: bool,
    focus_claimed: bool,
}

impl Ui<'_> {
    fn hovered(&self, r: Rect) -> bool {
        !self.blocked && self.clip.contains(self.input.mouse) && r.contains(self.input.mouse)
    }

    /// (hovered, held, clicked)
    fn interact(&mut self, id: u64, r: Rect) -> (bool, bool, bool) {
        let hovered = self.hovered(r);
        if hovered {
            self.mem.hand = true;
        }
        if self.input.pressed && hovered {
            self.mem.active = id;
        }
        let held = self.mem.active == id && self.input.down && hovered;
        let clicked = self.input.released && self.mem.active == id && hovered;
        (hovered, held, clicked)
    }

    pub fn card(&mut self, r: Rect) {
        self.g.shadow(r, theme::RADIUS, 0.10);
        self.g.gradient(r, theme::RADIUS, theme::CARD.mix(Color::white(1.0), 0.015), theme::CARD);
        self.g.stroke(r, theme::RADIUS, theme::CARD_STROKE, 1.0);
    }

    pub fn icon(&mut self, glyph: char, r: Rect, size: f32, c: Color) {
        let s = glyph.to_string();
        self.g.label(&s, Font::icon(size), r, c, Align::Center);
    }

    pub fn button(&mut self, key: &str, label: &str, glyph: Option<char>, kind: Kind, r: Rect) -> bool {
        let (hover, held, clicked) = self.interact(id(key), r);
        let fg = match kind {
            Kind::Primary => {
                let (a, b) = (theme::ACCENT, theme::ACCENT2);
                let lift = if held { -0.08 } else if hover { 0.10 } else { 0.0 };
                let tint = |c: Color| if lift >= 0.0 { c.mix(Color::white(1.0), lift) } else { c.mix(Color(0.0, 0.0, 0.0, 1.0), -lift) };
                self.g.gradient(r, 8.0, tint(a), tint(b));
                self.g.stroke(r, 8.0, Color::white(0.16), 1.0);
                theme::ACCENT_TEXT
            }
            Kind::Secondary => {
                let bg = if held { theme::CONTROL_DOWN } else if hover { theme::CONTROL_HOVER } else { theme::CONTROL };
                self.g.fill(r, 8.0, bg);
                self.g.stroke(r, 8.0, Color::white(0.07), 1.0);
                theme::TEXT
            }
            Kind::Subtle => {
                if held || hover {
                    self.g.fill(r, 8.0, if held { theme::CONTROL_DOWN } else { theme::CONTROL });
                }
                if hover { theme::TEXT } else { theme::TEXT2 }
            }
        };
        let font = Font::semibold(13.0);
        match (glyph, label.is_empty()) {
            (Some(gl), true) => self.icon(gl, r, 14.0, fg),
            (Some(gl), false) => {
                let (tw, _) = self.g.measure(label, font, 1000.0, false);
                let total = 14.0 + 8.0 + tw;
                let x = r.x + (r.w - total) / 2.0;
                self.icon(gl, Rect::new(x, r.y, 14.0, r.h), 13.0, fg);
                self.g.label(label, font, Rect::new(x + 22.0, r.y, tw + 2.0, r.h), fg, Align::Left);
            }
            (None, _) => self.g.label(label, font, r, fg, Align::Center),
        }
        clicked
    }

    /// Width of a button with this label.
    pub fn button_width(&mut self, label: &str, glyph: bool) -> f32 {
        let (tw, _) = self.g.measure(label, Font::semibold(13.0), 1000.0, false);
        tw + if glyph { 22.0 } else { 0.0 } + 32.0
    }

    /// Returns the new state when clicked.
    pub fn toggle(&mut self, key: &str, on: bool, x: f32, y: f32) -> Option<bool> {
        let r = Rect::new(x, y, 42.0, 24.0);
        let (hover, _, clicked) = self.interact(id(key), r.inset(-4.0, -6.0));
        if on {
            let lift = if hover { 0.1 } else { 0.0 };
            self.g.gradient(r, 12.0, theme::ACCENT.mix(Color::white(1.0), lift), theme::ACCENT2.mix(Color::white(1.0), lift));
            self.g.circle(r.right() - 12.0, r.y + 12.0, 8.0, Color::white(1.0));
        } else {
            self.g.fill(r, 12.0, if hover { theme::CONTROL_HOVER } else { theme::CONTROL });
            self.g.stroke(r, 12.0, Color::white(0.08), 1.0);
            self.g.circle(r.x + 12.0, r.y + 12.0, 8.0, theme::TEXT3);
        }
        clicked.then_some(!on)
    }

    pub fn segmented(&mut self, key: &str, options: &[&str], selected: usize, r: Rect) -> Option<usize> {
        self.g.fill(r, 9.0, theme::CONTROL_DOWN);
        self.g.stroke(r, 9.0, Color::white(0.06), 1.0);
        let w = r.w / options.len() as f32;
        let mut out = None;
        for (i, o) in options.iter().enumerate() {
            let cell = Rect::new(r.x + i as f32 * w, r.y, w, r.h);
            let (hover, _, clicked) = self.interact(id(&format!("{key}/{i}")), cell);
            if i == selected {
                let thumb = cell.inset(3.0, 3.0);
                self.g.fill(thumb, 7.0, theme::CONTROL_HOVER);
                self.g.stroke(thumb, 7.0, Color::white(0.08), 1.0);
            } else if hover {
                self.g.fill(cell.inset(3.0, 3.0), 7.0, theme::CONTROL);
            }
            let c = if i == selected { theme::TEXT } else { theme::TEXT2 };
            let f = if i == selected { Font::semibold(13.0) } else { Font::text(13.0) };
            self.g.label(o, f, cell, c, Align::Center);
            if clicked && i != selected {
                out = Some(i);
            }
        }
        out
    }

    /// Returns -1 / +1 when a side was clicked.
    pub fn stepper(&mut self, key: &str, value: &str, r: Rect) -> i32 {
        self.g.fill(r, 9.0, theme::CONTROL_DOWN);
        self.g.stroke(r, 9.0, Color::white(0.06), 1.0);
        let minus = Rect::new(r.x, r.y, r.h, r.h);
        let plus = Rect::new(r.right() - r.h, r.y, r.h, r.h);
        let mut out = 0;
        for (side, rect, glyph, d) in [("-", minus, icon::REMOVE, -1), ("+", plus, icon::ADD, 1)] {
            let (hover, held, clicked) = self.interact(id(&format!("{key}{side}")), rect);
            if hover {
                self.g.fill(rect.inset(3.0, 3.0), 7.0, if held { theme::CONTROL } else { theme::CONTROL_HOVER });
            }
            self.icon(glyph, rect, 11.0, theme::TEXT2);
            if clicked {
                out = d;
            }
        }
        self.g.label(value, Font::text(13.0), Rect::new(r.x + r.h, r.y, r.w - 2.0 * r.h, r.h), theme::TEXT, Align::Center);
        out
    }

    /// Dropdown; the list opens over everything else.
    pub fn select(&mut self, key: &str, options: &[String], selected: usize, r: Rect) -> Option<usize> {
        let k = id(key);
        if let Some((cid, i)) = self.mem.chosen {
            if cid == k {
                self.mem.chosen = None;
                return Some(i);
            }
        }
        let (hover, held, clicked) = self.interact(k, r);
        let bg = if held { theme::CONTROL_DOWN } else if hover { theme::CONTROL_HOVER } else { theme::CONTROL };
        self.g.fill(r, 8.0, bg);
        self.g.stroke(r, 8.0, Color::white(0.07), 1.0);
        let label = options.get(selected).map(String::as_str).unwrap_or("");
        self.g.label(label, Font::text(13.0), Rect::new(r.x + 12.0, r.y, r.w - 40.0, r.h), theme::TEXT, Align::Left);
        self.icon(icon::CHEVRON_DOWN, Rect::new(r.right() - 32.0, r.y, 24.0, r.h), 10.0, theme::TEXT2);
        if clicked {
            // The popup is drawn in screen coordinates at the end of the frame.
            self.mem.popup = Some(Popup { id: k, anchor: r, options: options.to_vec(), selected });
        }
        None
    }

    /// Single-line text input. Returns true when Enter was pressed.
    pub fn text_input(&mut self, key: &str, field: &mut TextField, placeholder: &str, glyph: Option<char>, r: Rect) -> bool {
        let k = id(key);
        let hovered = self.hovered(r);
        if hovered {
            self.mem.ibeam = true;
        }
        if self.input.pressed && hovered {
            self.mem.focus = k;
            self.focus_claimed = true;
            field.caret = field.text.chars().count();
        }
        let focused = self.mem.focus == k;
        let mut submit = false;
        if focused {
            for &c in &self.input.chars {
                match c {
                    '\u{16}' => {
                        // Ctrl+V
                        if let Some(t) = crate::clipboard::get_text() {
                            let t: String = t.chars().filter(|c| !c.is_control()).collect();
                            let byte = byte_at(&field.text, field.caret);
                            field.text.insert_str(byte, &t);
                            field.caret += t.chars().count();
                        }
                    }
                    c if !c.is_control() => {
                        let byte = byte_at(&field.text, field.caret);
                        field.text.insert(byte, c);
                        field.caret += 1;
                    }
                    _ => {}
                }
            }
            let n = field.text.chars().count();
            for &k in &self.input.keys {
                match k {
                    0x08 if field.caret > 0 => {
                        let b = byte_at(&field.text, field.caret - 1);
                        field.text.remove(b);
                        field.caret -= 1;
                    }
                    0x2E if field.caret < n => {
                        let b = byte_at(&field.text, field.caret);
                        field.text.remove(b);
                    }
                    0x25 => field.caret = field.caret.saturating_sub(1),
                    0x27 => field.caret = (field.caret + 1).min(field.text.chars().count()),
                    0x24 => field.caret = 0,
                    0x23 => field.caret = field.text.chars().count(),
                    0x0D => submit = true,
                    0x1B => {
                        field.text.clear();
                        field.caret = 0;
                    }
                    _ => {}
                }
            }
        }
        self.g.fill(r, 8.0, if focused { theme::CONTROL_DOWN } else if hovered { theme::CONTROL_HOVER } else { theme::CONTROL });
        if focused {
            self.g.stroke(r, 8.0, theme::ACCENT.alpha(0.8), 1.5);
        } else {
            self.g.stroke(r, 8.0, Color::white(0.07), 1.0);
        }
        let mut x = r.x + 12.0;
        if let Some(gl) = glyph {
            self.icon(gl, Rect::new(r.x + 8.0, r.y, 20.0, r.h), 12.0, theme::TEXT3);
            x += 22.0;
        }
        let font = Font::text(13.0);
        let tr = Rect::new(x, r.y, r.right() - x - 10.0, r.h);
        if field.text.is_empty() {
            self.g.label(placeholder, font, tr, theme::TEXT3, Align::Left);
        } else {
            self.g.label(&field.text, font, tr, theme::TEXT, Align::Left);
        }
        if focused && (self.t * 2.0).fract() < 0.6 {
            let utf16: usize = field.text.chars().take(field.caret).map(char::len_utf16).sum();
            let cx = x + if field.text.is_empty() { 0.0 } else { self.g.caret_x(&field.text, font, utf16) };
            self.g.fill(Rect::new(cx, r.y + 8.0, 1.0, r.h - 16.0), 0.0, theme::TEXT);
        }
        if focused {
            self.animate = true;
        }
        submit
    }

    pub fn progress(&mut self, r: Rect, f: f32, c: Color) {
        self.g.fill(r, r.h / 2.0, Color::white(0.08));
        let w = r.w * f.clamp(0.0, 1.0);
        if w >= 1.0 {
            self.g.gradient(Rect::new(r.x, r.y, w, r.h), r.h / 2.0, c, c.mix(theme::ACCENT2, 0.5));
        }
    }

    /// Small rounded label. Returns its width.
    pub fn chip(&mut self, text: &str, x: f32, y: f32, c: Color) -> f32 {
        let font = Font::semibold(11.5);
        let (tw, _) = self.g.measure(text, font, 1000.0, false);
        let r = Rect::new(x, y, tw + 18.0, 22.0);
        self.g.fill(r, 11.0, c.alpha(0.12));
        self.g.stroke(r, 11.0, c.alpha(0.22), 1.0);
        self.g.label(text, font, r, c, Align::Center);
        r.w
    }

    /// Uppercase section label.
    pub fn caps(&mut self, text: &str, r: Rect, c: Color) {
        self.g.label(&text.to_uppercase(), Font::semibold(11.0), r, c, Align::Left);
    }
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(b, _)| b)
}

// ------------------------------------------------------------------- app --

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Overview,
    History,
    Stats,
    Models,
    Words,
    Settings,
}

const PAGES: [(Page, &str, char); 6] = [
    (Page::Overview, "Overview", icon::HOME),
    (Page::History, "History", icon::HISTORY),
    (Page::Stats, "Statistics", icon::CHART),
    (Page::Models, "Models", icon::MODELS),
    (Page::Words, "Words", icon::WORDS),
    (Page::Settings, "Settings", icon::SETTINGS),
];

pub struct App {
    gfx: Option<Gfx>,
    pub page: Page,
    input: Input,
    mem: Memory,
    pub report: Arc<Mutex<Option<Result<Report, String>>>>,
    loading: Arc<AtomicBool>,
    scroll: f32,
    scroll_target: f32,
    content_h: f32,
    viewport_h: f32,
    pub devices: Vec<String>,
    pub search: TextField,
    pub new_word: TextField,
    pub new_filler: TextField,
    pub filler_lang: usize,
    pub expanded: Option<i64>,
    created: Instant,
    animating: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

impl App {
    fn new() -> App {
        App {
            gfx: None,
            page: Page::Overview,
            input: Input::default(),
            mem: Memory::default(),
            report: Arc::new(Mutex::new(None)),
            loading: Arc::new(AtomicBool::new(false)),
            scroll: 0.0,
            scroll_target: 0.0,
            content_h: 0.0,
            viewport_h: 0.0,
            devices: Vec::new(),
            search: TextField::default(),
            new_word: TextField::default(),
            new_filler: TextField::default(),
            filler_lang: 1,
            expanded: None,
            created: Instant::now(),
            animating: false,
        }
    }
}

fn reload_report(report: Arc<Mutex<Option<Result<Report, String>>>>, loading: Arc<AtomicBool>) {
    if loading.swap(true, Ordering::AcqRel) {
        return;
    }
    let cfg = ctx().cfg.get();
    std::thread::Builder::new()
        .name("stats-report".into())
        .spawn(move || {
            let t = Instant::now();
            let r = stats::open().and_then(|db| stats::report(&db, cfg.typing_wpm as f64, 60));
            log::debug!("stats report in {:?}", t.elapsed());
            *report.lock().unwrap() = Some(r);
            loading.store(false, Ordering::Release);
            if let Some(h) = hwnd() {
                unsafe {
                    let _ = InvalidateRect(Some(h), None, false);
                }
            }
        })
        .expect("spawn report");
}

const SIDEBAR_W: f32 = 232.0;

fn frame_window(app: &mut App, hwnd: HWND) -> Vec<Action> {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
    if app.gfx.is_none() {
        match Gfx::new(hwnd, rc.right as u32, rc.bottom as u32, dpi) {
            Ok(g) => app.gfx = Some(g),
            Err(e) => {
                log::error!("Direct2D: {e}");
                return Vec::new();
            }
        }
    }
    let scale = dpi / 96.0;
    frame(app, rc.right as f32 / scale, rc.bottom as f32 / scale, dpi)
}

/// Draws one frame of `w` x `h` DIPs into `app.gfx`.
fn frame(app: &mut App, w: f32, h: f32, dpi: f32) -> Vec<Action> {
    let mut actions = Vec::new();
    // Smooth scrolling.
    if app.input.wheel != 0.0 && app.mem.popup.is_none() {
        app.scroll_target -= app.input.wheel * 1.2;
    }
    let max_scroll = (app.content_h - app.viewport_h).max(0.0);
    app.scroll_target = app.scroll_target.clamp(0.0, max_scroll);
    let diff = app.scroll_target - app.scroll;
    app.scroll = if diff.abs() < 0.5 { app.scroll_target } else { app.scroll + diff * 0.35 };
    let scrolling = app.scroll != app.scroll_target;

    let g = app.gfx.as_mut().unwrap();
    if (g.dpi - dpi).abs() > 0.1 {
        g.set_dpi(dpi);
    }
    g.begin();
    g.clear(theme::BG);
    let blocked = app.mem.popup_rect.is_some_and(|r| r.contains(app.input.mouse));
    app.mem.hand = false;
    app.mem.ibeam = false;
    let t = app.created.elapsed().as_secs_f32();
    let content = Rect::new(SIDEBAR_W, 0.0, w - SIDEBAR_W, h);
    let mut nav_to = None;
    let content_h;
    let focus_claimed;
    let animate;
    {
        let mut ui = Ui {
            g,
            input: &app.input,
            mem: &mut app.mem,
            actions: &mut actions,
            t,
            clip: Rect::new(0.0, 0.0, w, h),
            blocked,
            animate: false,
            focus_claimed: false,
        };
        // Sidebar.
        sidebar(&mut ui, app.page, h, &mut nav_to);
        // Content layer.
        // A faint accent glow behind the page header.
        ui.g.glow(content.x + content.w * 0.35, -120.0, 520.0, theme::ACCENT.alpha(0.07));
        ui.g.line(content.x + 0.5, 0.0, content.x + 0.5, h, theme::HAIRLINE, 1.0);
        ui.clip = content;
        ui.g.push_clip(content);
        let report = app.report.lock().unwrap().clone();
        let mut page_state = pages::PageState {
            report: report.as_ref(),
            loading: app.loading.load(Ordering::Acquire),
            devices: &app.devices,
            search: &mut app.search,
            new_word: &mut app.new_word,
            new_filler: &mut app.new_filler,
            filler_lang: &mut app.filler_lang,
            expanded: &mut app.expanded,
        };
        let top = content.y - app.scroll;
        let end_y = pages::draw(&mut ui, app.page, Rect::new(content.x, top, content.w, h), &mut page_state, &mut nav_to);
        content_h = end_y - top;
        ui.g.pop_clip();
        // Scroll indicator.
        if content_h > h + 1.0 {
            let frac = h / content_h;
            let bar_h = (h * frac).max(40.0);
            let y = (app.scroll / (content_h - h)) * (h - bar_h);
            ui.g.fill(Rect::new(w - 7.0, y + 4.0, 4.0, bar_h - 8.0), 2.0, Color::white(0.14));
        }
        ui.clip = Rect::new(0.0, 0.0, w, h);
        ui.blocked = false;
        popup(&mut ui, w, h);
        focus_claimed = ui.focus_claimed;
        animate = ui.animate;
    }
    app.content_h = content_h;
    app.viewport_h = h;
    if app.input.pressed && !focus_claimed {
        app.mem.focus = 0;
    }
    if app.input.released {
        app.mem.active = 0;
    }
    let ok = app.gfx.as_ref().unwrap().end();
    if !ok {
        app.gfx = None;
    }
    if let Some(p) = nav_to {
        if p != app.page {
            app.page = p;
            app.scroll = 0.0;
            app.scroll_target = 0.0;
            app.mem.popup = None;
            app.mem.focus = 0;
            actions.push(Action::ReloadStats);
            if p == Page::Settings {
                actions.push(Action::ListDevices);
            }
        }
    }
    let status = crate::status::get();
    let live = matches!(status.phase, crate::status::Phase::Recording | crate::status::Phase::Transcribing | crate::status::Phase::Loading)
        || !status.downloads.is_empty();
    app.animating = scrolling || animate || (live && app.page == Page::Overview) || app.loading.load(Ordering::Acquire);
    app.input.pressed = false;
    app.input.released = false;
    app.input.wheel = 0.0;
    app.input.chars.clear();
    app.input.keys.clear();
    actions
}

fn sidebar(ui: &mut Ui, current: Page, h: f32, nav_to: &mut Option<Page>) {
    let status = crate::status::get();
    ui.g.fill(Rect::new(0.0, 0.0, SIDEBAR_W, h), 0.0, theme::SIDEBAR);
    // Brand: gradient tile with the mic glyph.
    let logo = Rect::new(20.0, 22.0, 34.0, 34.0);
    ui.g.glow(logo.x + 17.0, logo.y + 17.0, 40.0, theme::ACCENT.alpha(0.18));
    ui.g.gradient(logo, 10.0, theme::ACCENT, theme::ACCENT2);
    ui.icon(icon::MIC, logo, 16.0, Color::white(1.0));
    ui.g.label("chatty", Font::display(19.0), Rect::new(66.0, 20.0, 150.0, 24.0), theme::TEXT, Align::Left);
    let (dot, word) = phase_style(status.phase);
    ui.g.circle(70.0, 49.0, 3.5, dot);
    ui.g.label(word, Font::text(12.0), Rect::new(80.0, 40.0, 150.0, 18.0), theme::TEXT2, Align::Left);

    ui.caps("Menu", Rect::new(24.0, 82.0, 120.0, 16.0), theme::TEXT3);
    let mut y = 104.0;
    for (page, label, glyph) in PAGES {
        let r = Rect::new(12.0, y, SIDEBAR_W - 24.0, 38.0);
        let (hover, held, clicked) = ui.interact(id(&format!("nav/{label}")), r);
        let selected = page == current;
        if selected {
            ui.g.fill(r, 9.0, Color::white(0.065));
            ui.g.stroke(r, 9.0, Color::white(0.05), 1.0);
        } else if hover {
            ui.g.fill(r, 9.0, if held { Color::white(0.025) } else { Color::white(0.04) });
        }
        let fg = if selected { theme::TEXT } else if hover { theme::TEXT } else { theme::TEXT2 };
        let ic = if selected { theme::ACCENT } else { fg };
        ui.icon(glyph, Rect::new(r.x + 10.0, r.y, 24.0, r.h), 15.0, ic);
        let f = if selected { Font::semibold(13.5) } else { Font::text(13.5) };
        ui.g.label(label, f, Rect::new(r.x + 44.0, r.y, 160.0, r.h), fg, Align::Left);
        if clicked {
            *nav_to = Some(page);
        }
        y += 42.0;
    }

    // Footer card: model, backend, microphone.
    let cfg = ctx().cfg.get();
    let model = crate::models::asr(&cfg.model).map_or(cfg.model.as_str(), |m| m.id);
    let backend = status.loaded.as_ref().filter(|(id, _)| id == &cfg.model).map(|(_, b)| b.clone());
    let card = Rect::new(12.0, h - 106.0, SIDEBAR_W - 24.0, 92.0);
    ui.g.fill(card, 12.0, theme::CARD);
    ui.g.stroke(card, 12.0, theme::CARD_STROKE, 1.0);
    ui.caps("Model", Rect::new(card.x + 14.0, card.y + 10.0, 100.0, 16.0), theme::TEXT3);
    ui.g.label(model, Font::semibold(12.5), Rect::new(card.x + 14.0, card.y + 28.0, card.w - 28.0, 18.0), theme::TEXT, Align::Left);
    let mic = if cfg.microphone.is_empty() { "Default microphone".to_string() } else { cfg.microphone.clone() };
    let line = match backend {
        Some(b) => format!("{b} · {mic}"),
        None => format!("not loaded · {mic}"),
    };
    ui.icon(icon::MIC, Rect::new(card.x + 12.0, card.y + 54.0, 16.0, 26.0), 11.0, theme::TEXT3);
    ui.g.label(&line, Font::text(11.5), Rect::new(card.x + 30.0, card.y + 54.0, card.w - 44.0, 26.0), theme::TEXT3, Align::Left);
}

pub fn phase_style(p: crate::status::Phase) -> (Color, &'static str) {
    use crate::status::Phase::*;
    match p {
        Idle => (theme::GREEN, "Ready"),
        Recording => (theme::RED, "Listening"),
        Transcribing => (theme::AMBER, "Transcribing"),
        Loading => (theme::BLUE, "Loading model"),
        Error => (theme::RED, "Needs attention"),
    }
}

fn popup(ui: &mut Ui, w: f32, h: f32) {
    let Some(p) = ui.mem.popup.clone() else {
        ui.mem.popup_rect = None;
        return;
    };
    let row = 34.0;
    let ph = (p.options.len() as f32 * row + 8.0).min(h - 40.0);
    let mut y = p.anchor.bottom() + 4.0;
    if y + ph > h - 8.0 {
        y = (p.anchor.y - ph - 4.0).max(8.0);
    }
    let r = Rect::new(p.anchor.x, y, p.anchor.w.max(220.0).min(w - p.anchor.x - 8.0), ph);
    ui.g.shadow(r, 10.0, 0.22);
    ui.g.fill(r, 10.0, Color::rgb(32, 32, 38));
    ui.g.stroke(r, 10.0, Color::white(0.10), 1.0);
    ui.mem.popup_rect = Some(r);
    let mut chosen = None;
    for (i, o) in p.options.iter().enumerate() {
        let rr = Rect::new(r.x + 4.0, r.y + 4.0 + i as f32 * row, r.w - 8.0, row - 2.0);
        if rr.bottom() > r.bottom() {
            break;
        }
        let (hover, _, clicked) = ui.interact(id(&format!("popup/{i}")), rr);
        if hover || i == p.selected {
            ui.g.fill(rr, 4.0, Color::white(if hover { 0.08 } else { 0.05 }));
        }
        if i == p.selected {
            ui.g.fill(Rect::new(rr.x, rr.y + 9.0, 3.0, rr.h - 18.0), 1.5, theme::ACCENT);
        }
        ui.g.label(o, Font::text(13.0), Rect::new(rr.x + 14.0, rr.y, rr.w - 20.0, rr.h), theme::TEXT, Align::Left);
        if clicked {
            chosen = Some(i);
        }
    }
    if let Some(i) = chosen {
        ui.mem.chosen = Some((p.id, i));
        ui.mem.popup = None;
        ui.animate = true; // one more frame delivers the choice
    } else if ui.input.pressed && !r.contains(ui.input.mouse) {
        ui.mem.popup = None;
    }
}

fn run_actions(actions: Vec<Action>) {
    for a in actions {
        match a {
            Action::Config(values) => {
                let refs: Vec<(&str, toml_edit::Value)> = values.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                ctx().cfg.set(&refs);
            }
            Action::Open(p) => unsafe {
                ShellExecuteW(None, w!("open"), &HSTRING::from(p.as_os_str()), None, None, SW_SHOWNORMAL);
            },
            Action::Copy(t) => {
                if let Err(e) = crate::clipboard::set_text(&t) {
                    log::warn!("copy: {e}");
                }
            }
            Action::Play(p) => {
                use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
                unsafe {
                    let _ = PlaySoundW(&HSTRING::from(p.as_str()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
                }
            }
            Action::Download(id) => {
                if let Some(e) = &ctx().engine {
                    e.send(crate::engine::Cmd::Download(id));
                }
            }
            Action::DeleteModel(asset) => {
                if let Err(e) = crate::models::delete(asset) {
                    log::warn!("delete {}: {e}", asset.file);
                }
            }
            Action::ReloadStats => APP.with(|a| {
                if let Ok(a) = a.try_borrow() {
                    if let Some(a) = a.as_ref() {
                        reload_report(a.report.clone(), a.loading.clone());
                    }
                }
            }),
            Action::ListDevices => {
                let names = crate::audio::device_names();
                APP.with(|a| {
                    if let Ok(mut a) = a.try_borrow_mut() {
                        if let Some(a) = a.as_mut() {
                            a.devices = names;
                        }
                    }
                });
            }
        }
    }
}

fn mouse_pos(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    (x / scale, y / scale)
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let result = with_app(|a| {
                    let actions = frame_window(a, hwnd);
                    (actions, a.animating, a.mem.hand, a.mem.ibeam)
                });
                let _ = EndPaint(hwnd, &ps);
                if let Some((actions, animating, _, _)) = result {
                    let had_actions = !actions.is_empty();
                    run_actions(actions);
                    if animating || had_actions {
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
            WM_STATUS => {
                NOTIFY_PENDING.store(false, Ordering::Release);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_STATS => {
                let r = with_app(|a| (a.report.clone(), a.loading.clone()));
                if let Some((r, l)) = r {
                    reload_report(r, l);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (wpx, hpx) = ((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32);
                with_app(|a| {
                    if let Some(g) = a.gfx.as_mut() {
                        g.resize(wpx, hpx);
                    }
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| a.input.mouse = p);
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with_app(|a| a.input.mouse = (-1.0, -1.0));
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| {
                    a.input.mouse = p;
                    a.input.down = true;
                    a.input.pressed = true;
                });
                SetCapture(hwnd);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let p = mouse_pos(hwnd, lparam);
                with_app(|a| {
                    a.input.mouse = p;
                    a.input.down = false;
                    a.input.released = true;
                });
                let _ = ReleaseCapture();
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
                with_app(|a| a.input.wheel += delta);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wparam.0 as u32) {
                    with_app(|a| a.input.chars.push(c));
                    invalidate(hwnd);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u16;
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                with_app(|a| {
                    a.input.keys.push(vk);
                    if a.mem.focus == 0 {
                        // Page navigation keys.
                        let page = match vk {
                            0x74 => {
                                a.input.keys.clear();
                                Some(a.page) // F5 reloads
                            }
                            _ => None,
                        };
                        if page.is_some() {
                            reload_report(a.report.clone(), a.loading.clone());
                        }
                        match vk {
                            0x21 => a.scroll_target -= a.viewport_h * 0.9,
                            0x22 => a.scroll_target += a.viewport_h * 0.9,
                            0x24 if !ctrl => a.scroll_target = 0.0,
                            0x23 if !ctrl => a.scroll_target = a.content_h,
                            _ => {}
                        }
                    }
                });
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let (hand, ibeam) = with_app(|a| (a.mem.hand, a.mem.ibeam)).unwrap_or((false, false));
                let cursor = if ibeam { IDC_IBEAM } else if hand { IDC_HAND } else { IDC_ARROW };
                SetCursor(LoadCursorW(None, cursor).ok());
                LRESULT(1)
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                info.ptMinTrackSize.x = (860.0 * scale) as i32;
                info.ptMinTrackSize.y = (560.0 * scale) as i32;
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                if STANDALONE.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                } else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    // Free the GPU surface while hidden.
                    with_app(|a| a.gfx = None);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                if STANDALONE.load(Ordering::Relaxed) {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn apply_backdrop(hwnd: HWND) {
    unsafe {
        // Opaque, custom-painted content; the title bar matches the sidebar.
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        let caption: u32 = 0x000E0C0C; // COLORREF of theme::SIDEBAR
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as *const _, 4);
        let border: u32 = 0x00262222;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
    }
}

/// Shows the window (creating it on first use) on `page`. Main thread only.
pub fn open(page: Option<Page>) {
    if let Some(h) = hwnd() {
        with_app(|a| {
            if let Some(p) = page {
                a.page = p;
                a.scroll = 0.0;
                a.scroll_target = 0.0;
            }
            reload_report(a.report.clone(), a.loading.clone());
        });
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(h);
        }
        invalidate(h);
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("chatty_window"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::tray::app_icon(),
            ..Default::default()
        };
        RegisterClassW(&class);
        APP.with(|a| *a.borrow_mut() = Some(App::new()));
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("chatty_window"),
            w!("chatty"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1200,
            820,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::error!("window: {e}");
                return;
            }
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        apply_backdrop(hwnd);
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let _ = SetWindowPos(hwnd, None, 0, 0, (1200.0 * scale) as i32, (820.0 * scale) as i32, SWP_NOMOVE | SWP_NOZORDER);
        if let Some(p) = page {
            with_app(|a| a.page = p);
        }
        with_app(|a| reload_report(a.report.clone(), a.loading.clone()));
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// `chatty --render <page> <out.png> [w h scale]`: draws a page offscreen
/// (Direct2D into a WIC bitmap) with the real statistics, no window.
pub fn render_png(cfg: config::Shared, page: Page, path: &std::path::Path, w: u32, h: u32, scale: f32) -> Result<(), String> {
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::core::Interface;
    let e = |e: windows::core::Error| e.to_string();
    init(cfg.clone(), None);
    crate::status::set_phase(crate::status::Phase::Idle);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(e)?;
        let (pw, ph) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
        let bmp = wic.CreateBitmap(pw, ph, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand).map_err(e)?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(e)?;
        let dpi = 96.0 * scale;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            ..Default::default()
        };
        let rt = factory.CreateWicBitmapRenderTarget(&bmp, &props).map_err(e)?;
        let mut app = App::new();
        app.gfx = Some(Gfx::from_target(rt.cast().map_err(e)?, dpi).map_err(e)?);
        app.page = page;
        let c = cfg.get();
        app.report = Arc::new(Mutex::new(Some(stats::open().and_then(|db| stats::report(&db, c.typing_wpm as f64, 60)))));
        app.devices = crate::audio::device_names();
        let _ = frame(&mut app, w as f32, h as f32, dpi);

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

/// `chatty --stats`: the window is the whole process; closing exits.
pub fn run_standalone(cfg: config::Shared, page: Page) -> Result<(), String> {
    STANDALONE.store(true, Ordering::Relaxed);
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    init(cfg, None);
    crate::status::set_phase(crate::status::Phase::Idle);
    open(Some(page));
    let Some(h) = hwnd() else { return Err("cannot create the window".into()) };
    crate::clipboard::OWNER.store(h.0 as isize, Ordering::Relaxed);
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
