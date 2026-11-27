//! Statistics window: plain Win32 + GDI, created on the main thread (which
//! pumps messages anyway). The report is computed on a worker thread; the
//! window lays it out into a flat list of text/fill/button items and paints
//! them double-buffered with a vertical scroll offset.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetScrollInfo;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_END, VK_F5, VK_HOME, VK_NEXT, VK_PRIOR};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::config;
use crate::stats::{self, Report};

const WM_REPORT: u32 = WM_APP + 20;
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static STANDALONE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
const BG: u32 = rgb(27, 27, 29);
const PANEL: u32 = rgb(38, 38, 42);
const TEXT: u32 = rgb(232, 232, 232);
const WEAK: u32 = rgb(150, 150, 155);
const ACCENT: u32 = rgb(70, 140, 200);
const WARN: u32 = rgb(230, 160, 60);
const STRIPE: u32 = rgb(33, 33, 36);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Font {
    Title,
    Heading,
    Body,
    Strong,
    Small,
    Big,
}

#[derive(Clone)]
enum Action {
    Refresh,
    Play(String),
    Copy(String),
}

enum Kind {
    Text { text: String, font: Font, color: u32, wrap: bool, right: bool },
    Fill(u32),
    Button(String, Action),
}

struct Item {
    rect: RECT,
    kind: Kind,
}

struct State {
    cfg: config::Shared,
    report: Option<Result<Report, String>>,
    loading: bool,
    items: Vec<Item>,
    content_h: i32,
    scroll: i32,
    /// HFONT handles as integers (GDI handles are process-global).
    fonts: Vec<(Font, isize)>,
    dpi: u32,
}

impl State {
    fn font(&self, f: Font) -> HFONT {
        self.fonts.iter().find(|(k, _)| *k == f).map_or_else(HFONT::default, |(_, h)| HFONT(*h as *mut _))
    }
}

fn make_fonts(scale: f32) -> Vec<(Font, isize)> {
    let mk = |px: f32, weight: i32| unsafe {
        CreateFontW(
            -((px * scale).round() as i32),
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
            w!("Segoe UI"),
        )
        .0 as isize
    };
    vec![
        (Font::Title, mk(22.0, 600)),
        (Font::Heading, mk(17.0, 600)),
        (Font::Body, mk(14.0, 400)),
        (Font::Strong, mk(14.0, 600)),
        (Font::Small, mk(12.0, 400)),
        (Font::Big, mk(24.0, 600)),
    ]
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn fmt_int(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn fmt_minutes(m: f64) -> String {
    if m.abs() >= 90.0 { format!("{:.1} h", m / 60.0) } else { format!("{m:.0} min") }
}

fn days(n: i64) -> String {
    if n == 1 { "1 day".into() } else { format!("{n} days") }
}

/// Layout builder: a cursor moving down the page.
struct L<'a> {
    hdc: HDC,
    st: &'a State,
    scale: f32,
    x0: i32,
    width: i32,
    y: i32,
    items: Vec<Item>,
}

impl L<'_> {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn measure(&self, text: &str, font: Font, width: i32) -> i32 {
        unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(self.st.font(font).0));
            let mut r = RECT { left: 0, top: 0, right: width.max(1), bottom: 0 };
            let mut buf = wide(if text.is_empty() { " " } else { text });
            DrawTextW(self.hdc, &mut buf, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
            SelectObject(self.hdc, old);
            r.bottom
        }
    }

    fn line_h(&self, font: Font) -> i32 {
        self.measure("Ag", font, 1000)
    }

    fn text_at(&mut self, x: i32, y: i32, w: i32, text: &str, font: Font, color: u32) -> i32 {
        let h = self.line_h(font);
        self.items.push(Item {
            rect: RECT { left: x, top: y, right: x + w, bottom: y + h },
            kind: Kind::Text { text: text.to_string(), font, color, wrap: false, right: false },
        });
        h
    }

    fn para(&mut self, text: &str, font: Font, color: u32) {
        let h = self.measure(text, font, self.width);
        self.items.push(Item {
            rect: RECT { left: self.x0, top: self.y, right: self.x0 + self.width, bottom: self.y + h },
            kind: Kind::Text { text: text.to_string(), font, color, wrap: true, right: false },
        });
        self.y += h + self.px(2.0);
    }

    fn fill(&mut self, rect: RECT, color: u32) {
        self.items.push(Item { rect, kind: Kind::Fill(color) });
    }

    fn button(&mut self, x: i32, y: i32, label: &str, action: Action) -> i32 {
        let w = unsafe {
            let old = SelectObject(self.hdc, HGDIOBJ(self.st.font(Font::Small).0));
            let mut size = windows::Win32::Foundation::SIZE::default();
            let buf = wide(label);
            let _ = GetTextExtentPoint32W(self.hdc, &buf, &mut size);
            SelectObject(self.hdc, old);
            size.cx
        } + self.px(16.0);
        let h = self.line_h(Font::Small) + self.px(6.0);
        self.items.push(Item { rect: RECT { left: x, top: y, right: x + w, bottom: y + h }, kind: Kind::Button(label.into(), action) });
        w
    }

    fn heading(&mut self, title: &str) {
        self.y += self.px(18.0);
        let h = self.text_at(self.x0, self.y, self.width, title, Font::Heading, TEXT);
        self.y += h + self.px(4.0);
        self.fill(RECT { left: self.x0, top: self.y, right: self.x0 + self.width, bottom: self.y + 1 }, PANEL);
        self.y += self.px(8.0);
    }

    fn tiles(&mut self, tiles: &[(&str, String, String)]) {
        let (tw, th, gap) = (self.px(196.0), self.px(76.0), self.px(10.0));
        let per_row = ((self.width + gap) / (tw + gap)).max(1);
        for (i, (label, value, sub)) in tiles.iter().enumerate() {
            let (col, row) = (i as i32 % per_row, i as i32 / per_row);
            let x = self.x0 + col * (tw + gap);
            let y = self.y + row * (th + gap);
            self.fill(RECT { left: x, top: y, right: x + tw, bottom: y + th }, PANEL);
            let pad = self.px(10.0);
            let mut yy = y + self.px(6.0);
            yy += self.text_at(x + pad, yy, tw - 2 * pad, label, Font::Small, WEAK);
            yy += self.text_at(x + pad, yy, tw - 2 * pad, value, Font::Big, TEXT);
            self.text_at(x + pad, yy, tw - 2 * pad, sub, Font::Small, WEAK);
        }
        let rows = (tiles.len() as i32 + per_row - 1) / per_row;
        self.y += rows * (th + gap);
    }

    /// Columns are (header, width in dip, right-aligned). `bar` draws a bar
    /// after the last column, sized by the value in column `bar.0`.
    fn table(&mut self, cols: &[(&str, f32, bool)], rows: &[Vec<String>], bar: Option<(usize, f64)>, warn: impl Fn(usize, usize) -> bool) {
        let lh = self.line_h(Font::Body) + self.px(4.0);
        let mut x = self.x0;
        for (h, w, right) in cols {
            let w = self.px(*w);
            self.items.push(Item {
                rect: RECT { left: x, top: self.y, right: x + w - self.px(8.0), bottom: self.y + lh },
                kind: Kind::Text { text: h.to_string(), font: Font::Strong, color: TEXT, wrap: false, right: *right },
            });
            x += w;
        }
        self.y += lh;
        for (ri, row) in rows.iter().enumerate() {
            if ri % 2 == 1 {
                self.fill(RECT { left: self.x0, top: self.y, right: x, bottom: self.y + lh }, STRIPE);
            }
            let mut x = self.x0;
            for (ci, ((_, w, right), cell)) in cols.iter().zip(row).enumerate() {
                let w = self.px(*w);
                let color = if warn(ri, ci) { WARN } else { TEXT };
                self.items.push(Item {
                    rect: RECT { left: x, top: self.y + self.px(2.0), right: x + w - self.px(8.0), bottom: self.y + lh },
                    kind: Kind::Text { text: cell.clone(), font: Font::Body, color, wrap: false, right: *right },
                });
                x += w;
            }
            if let Some((col, max)) = bar {
                let v: f64 = row.get(col).and_then(|c| c.replace(',', "").parse().ok()).unwrap_or(0.0);
                let bw = (self.px(160.0) as f64 * v / max.max(1.0)) as i32;
                let top = self.y + lh / 2 - self.px(4.0);
                self.fill(RECT { left: x, top, right: x + bw.max(1), bottom: top + self.px(8.0) }, ACCENT);
            }
            self.y += lh;
        }
    }

    fn heatmap(&mut self, data: &[[i64; 24]]) {
        const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let max = data.iter().flat_map(|d| d.iter()).copied().max().unwrap_or(0).max(1) as f32;
        let (cw, ch, label_w) = (self.px(28.0), self.px(20.0), self.px(40.0));
        for h in (0..24).step_by(3) {
            self.text_at(self.x0 + label_w + h * cw, self.y, cw * 3, &format!("{h:02}"), Font::Small, WEAK);
        }
        self.y += self.line_h(Font::Small) + self.px(2.0);
        for (d, row) in data.iter().enumerate() {
            self.text_at(self.x0, self.y + self.px(2.0), label_w, DAYS[d], Font::Small, WEAK);
            for (h, &v) in row.iter().enumerate() {
                let t = (v as f32 / max).sqrt();
                let color = if v == 0 {
                    PANEL
                } else {
                    rgb((40.0 + 30.0 * t) as u8, (80.0 + 120.0 * t) as u8, (90.0 + 160.0 * t) as u8)
                };
                let x = self.x0 + label_w + h as i32 * cw;
                self.fill(RECT { left: x, top: self.y, right: x + cw - self.px(3.0), bottom: self.y + ch - self.px(3.0) }, color);
            }
            self.y += ch;
        }
    }
}

fn layout(hwnd: HWND, st: &State) -> (Vec<Item>, i32) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let scale = st.dpi as f32 / 96.0;
        let hdc = GetDC(Some(hwnd));
        let margin = (16.0 * scale) as i32;
        let mut l = L { hdc, st, scale, x0: margin, width: (rc.right - 2 * margin).max(200), y: margin, items: Vec::new() };
        let h = l.text_at(margin, l.y, l.px(200.0), "chatty", Font::Title, TEXT);
        let bx = margin + l.px(90.0);
        l.button(bx, l.y + l.px(4.0), "Refresh (F5)", Action::Refresh);
        if st.loading {
            l.text_at(bx + l.px(110.0), l.y + l.px(6.0), l.px(200.0), "loading…", Font::Small, WEAK);
        }
        l.y += h + l.px(8.0);
        match &st.report {
            None => l.para("Loading…", Font::Body, WEAK),
            Some(Err(e)) => l.para(e, Font::Body, WARN),
            Some(Ok(r)) => build(&mut l, r),
        }
        let content_h = l.y + margin;
        let items = std::mem::take(&mut l.items);
        ReleaseDC(Some(hwnd), hdc);
        (items, content_h)
    }
}

fn build(l: &mut L, r: &Report) {
    l.tiles(&[
        ("Words today", fmt_int(r.words.today), format!("{} dictations", r.dictations.today)),
        ("This week", fmt_int(r.words.week), format!("{} dictations", r.dictations.week)),
        ("This month", fmt_int(r.words.month), format!("{} dictations", r.dictations.month)),
        ("All time", fmt_int(r.words.all), format!("{} dictations", r.dictations.all)),
        ("Streak", days(r.streak_days), format!("longest {}", days(r.longest_streak))),
        (
            "Time saved",
            fmt_minutes(r.saved_minutes),
            format!("{} typing vs {} speaking", fmt_minutes(r.typing_minutes), fmt_minutes(r.speaking_minutes)),
        ),
        ("Average WPM", format!("{:.0}", r.avg_wpm), format!("{:.0} incl. pauses and latency", r.avg_wpm_total)),
        ("Best WPM", format!("{:.0}", r.best_wpm), "at least 3 s of speech".into()),
        ("Release to paste", format!("{:.0} ms", r.avg_latency), format!("p95 {:.0} ms", r.p95_latency)),
        ("Enter taps", fmt_int(r.enter_taps), format!("{} today, {:.1} ms tap to Enter", r.enter_taps_today, r.enter_latency)),
        ("Longest dictation", format!("{:.1} s", r.longest_ms as f64 / 1000.0), format!("{} words", r.longest_words)),
        ("Vocabulary", fmt_int(r.vocabulary as i64), "distinct words".into()),
    ]);

    l.heading("When you dictate (words by weekday and hour)");
    l.heatmap(&r.heatmap);

    let breakdown = |rows: &[stats::Breakdown]| -> (Vec<Vec<String>>, f64) {
        let max = rows.iter().map(|b| b.words).max().unwrap_or(1) as f64;
        (rows.iter().take(15).map(|b| vec![b.name.clone(), fmt_int(b.words), fmt_int(b.dictations)]).collect(), max)
    };
    let cols = [("", 220.0, false), ("words", 80.0, true), ("dictations", 90.0, true)];
    l.heading("By app");
    let (rows, max) = breakdown(&r.apps);
    l.table(&cols, &rows, Some((1, max)), |_, _| false);
    l.heading("By language");
    let (rows, max) = breakdown(&r.languages);
    l.table(&cols, &rows, Some((1, max)), |_, _| false);

    l.heading("Models");
    let rows: Vec<Vec<String>> = r
        .models
        .iter()
        .map(|m| {
            vec![
                m.model.clone(),
                m.backend.clone(),
                fmt_int(m.dictations),
                fmt_int(m.words),
                format!("{:.0} ms", m.avg_latency),
                format!("{:.0} ms", m.p50_latency),
                format!("{:.0} ms", m.p95_latency),
                format!("{:.0} ms", m.avg_transcribe),
                format!("{:.3}", m.avg_rtf),
                format!("{:.2}", m.corrections_per_100_words),
                format!("{:.0}", m.avg_wpm),
            ]
        })
        .collect();
    l.table(
        &[
            ("model", 250.0, false),
            ("backend", 80.0, false),
            ("dictations", 80.0, true),
            ("words", 70.0, true),
            ("latency", 70.0, true),
            ("p50", 70.0, true),
            ("p95", 70.0, true),
            ("transcribe", 80.0, true),
            ("RTF", 60.0, true),
            ("corr./100 w", 90.0, true),
            ("WPM", 50.0, true),
        ],
        &rows,
        None,
        |_, _| false,
    );

    l.heading("Custom words (low scores hint at false matches)");
    if r.custom_words.is_empty() {
        l.para("No corrections yet.", Font::Body, WEAK);
    } else {
        let rows: Vec<Vec<String>> = r
            .custom_words
            .iter()
            .map(|w| {
                vec![w.word.clone(), fmt_int(w.fired), format!("{:.2}", w.avg_score), format!("{:.2}", w.min_score), w.originals.clone()]
            })
            .collect();
        let low: Vec<bool> = r.custom_words.iter().map(|w| w.min_score < 0.9).collect();
        l.table(
            &[("word", 180.0, false), ("fired", 60.0, true), ("avg score", 80.0, true), ("min score", 80.0, true), ("heard as", 420.0, false)],
            &rows,
            None,
            |ri, ci| ci == 3 && low[ri],
        );
    }

    l.heading("Most used words");
    let rows: Vec<Vec<String>> = r.top_words.iter().take(20).map(|(w, n)| vec![w.clone(), fmt_int(*n)]).collect();
    let max = r.top_words.first().map_or(1.0, |t| t.1 as f64);
    l.table(&[("", 180.0, false), ("uses", 70.0, true)], &rows, Some((1, max)), |_, _| false);

    l.heading("Quality");
    let rows: Vec<Vec<String>> = [
        ("Cancelled", fmt_int(r.cancelled)),
        ("Empty (no speech)", fmt_int(r.empty)),
        ("Errors", fmt_int(r.errors)),
        ("Blocked (elevated window)", fmt_int(r.blocked)),
        ("Average silence", format!("{:.0}%", r.avg_silence * 100.0)),
        ("Fillers removed", fmt_int(r.fillers_removed)),
        ("Top fillers", r.top_fillers.iter().map(|(w, n)| format!("{w} ({n})")).collect::<Vec<_>>().join(", ")),
        ("Cleanup runs", fmt_int(r.cleanup_runs)),
        ("Cleanup rejected (drift)", fmt_int(r.cleanup_rejected)),
        ("Cleanup average", format!("{:.0} ms", r.cleanup_avg_ms)),
    ]
    .into_iter()
    .map(|(k, v)| vec![k.to_string(), v])
    .collect();
    l.table(&[("", 240.0, false), ("", 400.0, false)], &rows, None, |_, _| false);

    l.heading("Recent dictations");
    for h in &r.history {
        let top = l.y;
        let mut meta = format!("{}   {} · {} · {}", h.at, h.app, h.model, h.status);
        if let Some(ms) = h.latency_ms {
            meta.push_str(&format!(" · {ms:.0} ms"));
        }
        let lh = l.text_at(l.x0 + l.px(8.0), l.y + l.px(6.0), l.width - l.px(220.0), &meta, Font::Small, WEAK);
        let mut bx = l.x0 + l.width - l.px(150.0);
        if let Some(a) = &h.audio {
            bx += l.button(bx, l.y + l.px(4.0), "Play", Action::Play(a.clone())) + l.px(6.0);
        }
        if !h.final_text.is_empty() {
            l.button(bx, l.y + l.px(4.0), "Copy", Action::Copy(h.final_text.clone()));
        }
        l.y += lh + l.px(10.0);
        let x0 = l.x0;
        l.x0 += l.px(8.0);
        l.width -= l.px(16.0);
        if !h.final_text.is_empty() {
            l.para(&h.final_text, Font::Body, TEXT);
        }
        if h.raw != h.final_text && !h.raw.is_empty() {
            l.para(&format!("raw: {}", h.raw), Font::Small, WEAK);
        }
        if let Some(c) = &h.cleaned {
            let used = if h.cleanup_used == Some(true) { "used" } else { "rejected" };
            l.para(&format!("cleanup ({used}): {c}"), Font::Small, WEAK);
        }
        l.x0 = x0;
        l.width += l.px(16.0);
        l.y += l.px(6.0);
        // Panel behind the entry: insert before its contents.
        let panel = Item { rect: RECT { left: l.x0, top, right: l.x0 + l.width, bottom: l.y }, kind: Kind::Fill(PANEL) };
        let first = l.items.iter().rposition(|i| i.rect.top < top).map_or(0, |p| p + 1);
        l.items.insert(first, panel);
        l.y += l.px(8.0);
    }
}

/// Never call Win32 functions that can send messages (SetScrollInfo sends
/// WM_SIZE when the scrollbar appears) while holding STATE: the window
/// procedure would re-enter and deadlock on it.
fn relayout(hwnd: HWND) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let si = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else { return };
        let (items, h) = layout(hwnd, st);
        st.items = items;
        st.content_h = h;
        st.scroll = st.scroll.clamp(0, (h - rc.bottom).max(0));
        SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: h,
            nPage: rc.bottom.max(0) as u32,
            nPos: st.scroll,
            nTrackPos: 0,
        }
    };
    unsafe {
        SetScrollInfo(hwnd, SB_VERT, &si, true);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn scroll_to(hwnd: HWND, pos: i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let si = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else { return };
        st.scroll = pos.clamp(0, (st.content_h - rc.bottom).max(0));
        SCROLLINFO { cbSize: std::mem::size_of::<SCROLLINFO>() as u32, fMask: SIF_POS, nPos: st.scroll, ..Default::default() }
    };
    unsafe {
        SetScrollInfo(hwnd, SB_VERT, &si, true);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn current_scroll() -> i32 {
    STATE.lock().unwrap().as_ref().map_or(0, |s| s.scroll)
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, rc.right.max(1), rc.bottom.max(1));
        let old_bmp = SelectObject(mem, HGDIOBJ(bmp.0));
        let bg = CreateSolidBrush(COLORREF(BG));
        FillRect(mem, &rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        SetBkMode(mem, TRANSPARENT);
        if let Some(st) = STATE.lock().unwrap().as_ref() {
            let dy = -st.scroll;
            for item in &st.items {
                let mut r = item.rect;
                r.top += dy;
                r.bottom += dy;
                if r.bottom < 0 || r.top > rc.bottom {
                    continue;
                }
                match &item.kind {
                    Kind::Fill(c) => {
                        let b = CreateSolidBrush(COLORREF(*c));
                        FillRect(mem, &r, b);
                        let _ = DeleteObject(HGDIOBJ(b.0));
                    }
                    // DrawTextW faults on an empty (dangling-pointer) buffer.
                    Kind::Text { text, .. } if text.is_empty() => {}
                    Kind::Text { text, font, color, wrap, right } => {
                        let old = SelectObject(mem, HGDIOBJ(st.font(*font).0));
                        SetTextColor(mem, COLORREF(*color));
                        let mut buf = wide(text);
                        let mut flags = DT_NOPREFIX;
                        flags |= if *wrap { DT_WORDBREAK } else { DT_SINGLELINE | DT_END_ELLIPSIS };
                        if *right {
                            flags |= DT_RIGHT;
                        }
                        DrawTextW(mem, &mut buf, &mut r, flags);
                        SelectObject(mem, old);
                    }
                    Kind::Button(label, _) => {
                        let b = CreateSolidBrush(COLORREF(rgb(55, 55, 62)));
                        FillRect(mem, &r, b);
                        let _ = DeleteObject(HGDIOBJ(b.0));
                        let old = SelectObject(mem, HGDIOBJ(st.font(Font::Small).0));
                        SetTextColor(mem, COLORREF(TEXT));
                        let mut buf = wide(label);
                        DrawTextW(mem, &mut buf, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                        SelectObject(mem, old);
                    }
                }
            }
        }
        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn play(path: &str) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    unsafe {
        let _ = PlaySoundW(&HSTRING::from(path), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
    }
}

fn click(hwnd: HWND, x: i32, y: i32) {
    let action = {
        let guard = STATE.lock().unwrap();
        let Some(st) = guard.as_ref() else { return };
        let y = y + st.scroll;
        st.items.iter().find_map(|i| match &i.kind {
            Kind::Button(_, a) if x >= i.rect.left && x < i.rect.right && y >= i.rect.top && y < i.rect.bottom => {
                Some(a.clone())
            }
            _ => None,
        })
    };
    match action {
        Some(Action::Refresh) => refresh(hwnd),
        Some(Action::Play(p)) => play(&p),
        Some(Action::Copy(t)) => {
            if let Err(e) = crate::clipboard::set_text(&t) {
                log::warn!("copy: {e}");
            }
        }
        None => {}
    }
}

fn refresh(hwnd: HWND) {
    let cfg = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else { return };
        if st.loading {
            return;
        }
        st.loading = true;
        st.cfg.get()
    };
    relayout(hwnd);
    let raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("stats-report".into())
        .spawn(move || {
            let t = std::time::Instant::now();
            let r = stats::open().and_then(|db| stats::report(&db, cfg.typing_wpm as f64, cfg.history_size.max(20)));
            log::info!("stats report in {:?}", t.elapsed());
            if let Some(st) = STATE.lock().unwrap().as_mut() {
                st.report = Some(r);
                st.loading = false;
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_REPORT, WPARAM(0), LPARAM(0));
            }
        })
        .expect("spawn stats report");
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_REPORT | WM_SIZE => {
                relayout(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xFFFF) as u32;
                if let Some(st) = STATE.lock().unwrap().as_mut() {
                    st.dpi = dpi;
                    st.fonts = make_fonts(dpi as f32 / 96.0);
                }
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                relayout(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                scroll_to(hwnd, current_scroll() - delta * 3 / 2);
                LRESULT(0)
            }
            WM_VSCROLL => {
                let mut si = SCROLLINFO { cbSize: std::mem::size_of::<SCROLLINFO>() as u32, fMask: SIF_ALL, ..Default::default() };
                let _ = GetScrollInfo(hwnd, SB_VERT, &mut si);
                let page = si.nPage as i32;
                let pos = match SCROLLBAR_COMMAND((wparam.0 & 0xFFFF) as i32) {
                    SB_LINEUP => si.nPos - 40,
                    SB_LINEDOWN => si.nPos + 40,
                    SB_PAGEUP => si.nPos - page,
                    SB_PAGEDOWN => si.nPos + page,
                    SB_THUMBTRACK | SB_THUMBPOSITION => si.nTrackPos,
                    SB_TOP => 0,
                    SB_BOTTOM => i32::MAX / 2,
                    _ => si.nPos,
                };
                scroll_to(hwnd, pos);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                match windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(wparam.0 as u16) {
                    VK_F5 => refresh(hwnd),
                    VK_HOME => scroll_to(hwnd, 0),
                    VK_END => scroll_to(hwnd, i32::MAX / 2),
                    VK_PRIOR => scroll_to(hwnd, current_scroll() - rc.bottom),
                    VK_NEXT => scroll_to(hwnd, current_scroll() + rc.bottom),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                click(hwnd, x, y);
                LRESULT(0)
            }
            WM_CLOSE => {
                if STANDALONE.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                } else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
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

/// Shows the window, creating it on first use. Main thread only.
pub fn open(cfg: config::Shared) {
    let existing = HWND_RAW.load(Ordering::Relaxed);
    if existing != 0 {
        let hwnd = HWND(existing as *mut _);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
        }
        refresh(hwnd);
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("chatty_stats"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("chatty_stats"),
            w!("chatty statistics"),
            WS_OVERLAPPEDWINDOW | WS_VSCROLL,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100,
            820,
            None,
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::error!("stats window: {e}");
                return;
            }
        };
        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f32 / 96.0;
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (1100.0 * scale) as i32,
            (820.0 * scale) as i32,
            SWP_NOMOVE | SWP_NOZORDER,
        );
        *STATE.lock().unwrap() = Some(State {
            cfg,
            report: None,
            loading: false,
            items: Vec::new(),
            content_h: 0,
            scroll: 0,
            fonts: make_fonts(scale),
            dpi,
        });
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        refresh(hwnd);
    }
}

/// `chatty --stats`: the window is the whole process; closing exits.
pub fn run_standalone(cfg: config::Shared) -> Result<(), String> {
    STANDALONE.store(true, Ordering::Relaxed);
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    open(cfg);
    let hwnd = HWND_RAW.load(Ordering::Relaxed);
    if hwnd == 0 {
        return Err("cannot create the window".into());
    }
    crate::clipboard::OWNER.store(hwnd, Ordering::Relaxed);
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let _ = PCWSTR::null();
    Ok(())
}
