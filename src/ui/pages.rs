//! Page content for the main window.

use super::gfx::{Align, Color, Font, Rect};
use super::{Action, Kind, Page, TextField, Ui, icon, theme};
use crate::models;
use crate::stats::Report;
use crate::status::{self, Phase};

pub struct PageState<'a> {
    pub report: Option<&'a Result<Report, String>>,
    pub loading: bool,
    pub devices: &'a [String],
    pub search: &'a mut TextField,
    pub new_word: &'a mut TextField,
    pub new_filler: &'a mut TextField,
    pub filler_lang: &'a mut usize,
    pub expanded: &'a mut Option<i64>,
}

/// Vertical layout cursor.
struct Col {
    x: f32,
    y: f32,
    w: f32,
}

impl Col {
    fn take(&mut self, h: f32) -> Rect {
        let r = Rect::new(self.x, self.y, self.w, h);
        self.y += h;
        r
    }
    fn gap(&mut self, h: f32) {
        self.y += h;
    }
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

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{} {}", fmt_int(n), if n == 1 { one } else { many })
}

fn cfg() -> std::sync::Arc<crate::config::Config> {
    super::ctx().cfg.get()
}

fn set(ui: &mut Ui, key: &str, v: impl Into<toml_edit::Value>) {
    ui.actions.push(Action::Config(vec![(key.to_string(), v.into())]));
}

pub fn draw(ui: &mut Ui, page: Page, area: Rect, st: &mut PageState, nav: &mut Option<Page>) -> f32 {
    let w = (area.w - 96.0).min(1040.0);
    let mut c = Col { x: area.x + (area.w - w) / 2.0, y: area.y + 40.0, w };
    match page {
        Page::Overview => overview(ui, &mut c, st, nav),
        Page::History => history(ui, &mut c, st),
        Page::Stats => statistics(ui, &mut c, st),
        Page::Models => models_page(ui, &mut c, st),
        Page::Words => words(ui, &mut c, st),
        Page::Settings => settings(ui, &mut c, st),
    }
    c.y + 48.0
}

fn header(ui: &mut Ui, c: &mut Col, title: &str, subtitle: &str) {
    let r = c.take(40.0);
    ui.g.label(title, Font::display(30.0), r, theme::TEXT, Align::Left);
    if !subtitle.is_empty() {
        c.gap(6.0);
        let h = ui.g.text(subtitle, Font::text(14.0), Rect::new(c.x, c.y, c.w.min(720.0), 100.0), theme::TEXT2, true, Align::Left);
        c.gap(h);
    }
    c.gap(28.0);
}

fn section(ui: &mut Ui, c: &mut Col, title: &str) {
    c.gap(22.0);
    let r = c.take(18.0);
    ui.caps(title, r, theme::TEXT3);
    c.gap(10.0);
}

fn report_or_message<'a>(ui: &mut Ui, c: &mut Col, st: &PageState<'a>) -> Option<&'a Report> {
    match st.report {
        Some(Ok(r)) => Some(r),
        Some(Err(e)) => {
            let r = c.take(60.0);
            ui.card(r);
            ui.g.label(&format!("Statistics unavailable: {e}"), Font::text(13.0), r.inset(16.0, 0.0), theme::RED, Align::Left);
            None
        }
        None => {
            let r = c.take(60.0);
            ui.card(r);
            ui.g.label("Loading…", Font::text(13.0), r.inset(16.0, 0.0), theme::TEXT3, Align::Left);
            None
        }
    }
}

fn tile(ui: &mut Ui, r: Rect, label: &str, value: &str, sub: &str, accent: Option<Color>) {
    ui.card(r);
    let x = r.x + 18.0;
    let w = r.w - 36.0;
    ui.caps(label, Rect::new(x, r.y + 16.0, w, 16.0), theme::TEXT3);
    ui.g.label(value, Font::display(28.0), Rect::new(x, r.y + 36.0, w, 38.0), accent.unwrap_or(theme::TEXT), Align::Left);
    ui.g.label(sub, Font::text(12.0), Rect::new(x, r.y + 78.0, w, 18.0), theme::TEXT2, Align::Left);
}

/// Four per row on normal widths, two when narrow: never a lone tile.
fn tiles(ui: &mut Ui, c: &mut Col, items: &[(&str, String, String, Option<Color>)]) {
    let gap = 14.0;
    let per_row = if c.w >= 700.0 { 4 } else { 2 };
    let tw = (c.w - gap * (per_row as f32 - 1.0)) / per_row as f32;
    let th = 108.0;
    for (i, (label, value, sub, accent)) in items.iter().enumerate() {
        let (col, row) = (i % per_row, i / per_row);
        let r = Rect::new(c.x + col as f32 * (tw + gap), c.y + row as f32 * (th + gap), tw, th);
        tile(ui, r, label, value, sub, *accent);
    }
    let rows = items.len().div_ceil(per_row);
    c.gap(rows as f32 * (th + gap) - gap);
}

// --------------------------------------------------------------- overview --

fn overview(ui: &mut Ui, c: &mut Col, st: &mut PageState, nav: &mut Option<Page>) {
    let cfg = cfg();
    header(ui, c, "Overview", "Hold Ctrl + Numpad / to dictate into any window. A short tap presses Enter; Escape cancels.");
    let s = status::get();
    let (color, word) = super::phase_style(s.phase);

    // Hero status card.
    let live = matches!(s.phase, Phase::Recording | Phase::Transcribing) && !s.live.trim().is_empty();
    let text_w = c.w - 140.0;
    let live_h = if live { ui.g.measure(s.live.trim(), Font::text(16.0), text_w, true).1.min(96.0) } else { 0.0 };
    let hero = c.take(140.0 + live_h);
    ui.g.shadow(hero, 16.0, 0.14);
    ui.g.gradient(hero, 16.0, theme::CARD.mix(color, 0.07), theme::CARD);
    ui.g.push_clip(hero);
    ui.g.glow(hero.x + 70.0, hero.y + 70.0, 160.0, color.alpha(0.10));
    ui.g.pop_clip();
    ui.g.stroke(hero, 16.0, color.alpha(0.18), 1.0);
    let (cx, cy) = (hero.x + 70.0, hero.y + 70.0);
    if s.phase == Phase::Recording {
        let pulse = (ui.t * 3.0).sin() * 0.5 + 0.5;
        ui.g.circle(cx, cy, 36.0 + pulse * 6.0 + s.level * 12.0, color.alpha(0.08 + 0.10 * pulse));
    }
    ui.g.glow(cx, cy, 60.0, color.alpha(0.25));
    ui.g.circle(cx, cy, 30.0, color.alpha(0.16));
    ui.g.gradient(Rect::new(cx - 22.0, cy - 22.0, 44.0, 44.0), 22.0, color.mix(Color::white(1.0), 0.15), color);
    ui.icon(icon::MIC, Rect::new(cx - 20.0, cy - 20.0, 40.0, 40.0), 19.0, Color::rgb(16, 16, 20));
    let tx = hero.x + 130.0;
    let title = match s.phase {
        Phase::Recording => {
            let secs = s.phase_since.elapsed().as_secs();
            format!("Listening  {}:{:02}", secs / 60, secs % 60)
        }
        Phase::Transcribing => "Transcribing…".into(),
        Phase::Loading => "Loading model…".into(),
        Phase::Idle => "Ready".into(),
        Phase::Error => word.into(),
    };
    ui.g.label(&title, Font::display(24.0), Rect::new(tx, hero.y + 26.0, text_w, 32.0), theme::TEXT, Align::Left);
    let model = models::asr(&cfg.model).map_or(cfg.model.clone(), |m| m.name.split(" (").next().unwrap_or(m.name).to_string());
    let backend = s.loaded.as_ref().filter(|(id, _)| id == &cfg.model).map_or("not loaded".to_string(), |(_, b)| b.clone());
    let mic = if cfg.microphone.is_empty() { "default microphone".to_string() } else { cfg.microphone.clone() };
    let sub = format!("{model}  ·  {backend}  ·  {mic}");
    ui.g.label(&sub, Font::text(13.0), Rect::new(tx, hero.y + 60.0, text_w, 20.0), theme::TEXT2, Align::Left);
    let mut chips_x = tx;
    let lang = match cfg.language.as_str() {
        "auto" | "" => "Auto language".to_string(),
        "en" => "English".to_string(),
        "de" => "German".to_string(),
        l => l.to_string(),
    };
    let cy_chips = hero.y + 92.0;
    chips_x += ui.chip(&lang, chips_x, cy_chips, theme::ACCENT) + 8.0;
    chips_x += ui.chip(if cfg.cleanup.enabled { "Cleanup on" } else { "Cleanup off" }, chips_x, cy_chips, if cfg.cleanup.enabled { theme::GREEN } else { theme::TEXT2 }) + 8.0;
    chips_x += ui.chip(&format!("Hold {} ms", cfg.hold_threshold_ms), chips_x, cy_chips, theme::TEXT2) + 8.0;
    ui.chip(if cfg.decode == "stream" { "Streaming" } else { "Full-context pass" }, chips_x, cy_chips, theme::TEXT2);
    if live {
        ui.g.push_clip(Rect::new(tx, hero.y + 128.0, text_w, live_h));
        // Show the tail when the text is long.
        let full_h = ui.g.measure(s.live.trim(), Font::text(16.0), text_w, true).1;
        ui.g.text(s.live.trim(), Font::text(16.0), Rect::new(tx, hero.y + 128.0 - (full_h - live_h), text_w, full_h), theme::TEXT, true, Align::Left);
        ui.g.pop_clip();
    }
    for (file, f) in &s.downloads {
        c.gap(12.0);
        let r = c.take(64.0);
        ui.card(r);
        ui.icon(icon::DOWNLOAD, Rect::new(r.x + 16.0, r.y + 12.0, 20.0, 20.0), 14.0, theme::ACCENT);
        ui.g.label(&format!("Downloading {file}"), Font::text(13.0), Rect::new(r.x + 44.0, r.y + 12.0, r.w - 120.0, 20.0), theme::TEXT, Align::Left);
        ui.g.label(&format!("{:.0}%", f * 100.0), Font::semibold(13.0), Rect::new(r.right() - 70.0, r.y + 12.0, 54.0, 20.0), theme::TEXT, Align::Right);
        ui.progress(Rect::new(r.x + 44.0, r.y + 42.0, r.w - 60.0, 4.0), *f, theme::ACCENT);
    }
    c.gap(16.0);

    let Some(r) = report_or_message(ui, c, st) else { return };
    tiles(
        ui,
        c,
        &[
            ("Words today", fmt_int(r.words.today), plural(r.dictations.today, "dictation", "dictations"), None),
            ("Release to paste", format!("{:.0} ms", r.avg_latency), format!("p95 {:.0} ms", r.p95_latency), None),
            ("Time saved", fmt_minutes(r.saved_minutes), format!("vs typing at {:.0} WPM", cfg.typing_wpm), Some(theme::GREEN)),
            ("Streak", plural(r.streak_days, "day", "days"), format!("longest {}", plural(r.longest_streak, "day", "days")), None),
        ],
    );

    if let Some(h) = r.history.iter().find(|h| h.status == "ok") {
        section(ui, c, "Last dictation");
        let tw = c.w - 40.0;
        let full_h = ui.g.measure(&h.final_text, Font::text(15.0), tw, true).1;
        // At most four lines; the full text is in History.
        let line = ui.g.measure("Ag", Font::text(15.0), tw, true).1;
        let text_h = full_h.min(line * 4.0);
        let card = c.take(text_h + 160.0);
        ui.card(card);
        let meta = format!("{}  ·  {}  ·  {} words", h.at, h.app.trim_end_matches(".exe"), h.words);
        ui.g.label(&meta, Font::text(12.0), Rect::new(card.x + 20.0, card.y + 18.0, card.w - 40.0, 18.0), theme::TEXT3, Align::Left);
        ui.g.push_clip(Rect::new(card.x + 20.0, card.y + 44.0, tw, text_h));
        ui.g.text(&h.final_text, Font::text(15.0), Rect::new(card.x + 20.0, card.y + 44.0, tw, full_h), theme::TEXT, true, Align::Left);
        ui.g.pop_clip();
        if full_h > text_h {
            ui.g.gradient(Rect::new(card.x + 20.0, card.y + 44.0 + text_h - line, tw, line), 0.0, theme::CARD.alpha(0.0), theme::CARD);
        }
        latency_bar(ui, Rect::new(card.x + 20.0, card.y + 64.0 + text_h, card.w - 40.0, 80.0), h);
    }

    section(ui, c, "Recent");
    let recent: Vec<_> = r.history.iter().take(5).collect();
    if recent.is_empty() {
        let r = c.take(64.0);
        ui.card(r);
        ui.g.label("Nothing yet. Hold the button behind the scroll wheel and speak.", Font::text(13.0), r.inset(16.0, 0.0), theme::TEXT2, Align::Left);
    }
    if !recent.is_empty() {
        // One card, rows separated by hairlines.
        let row_h = 52.0;
        let card = c.take(row_h * recent.len() as f32 + 56.0);
        ui.card(card);
        for (i, h) in recent.iter().enumerate() {
            let r = Rect::new(card.x, card.y + i as f32 * row_h, card.w, row_h);
            if i > 0 {
                ui.g.line(r.x + 18.0, r.y, r.right() - 18.0, r.y, theme::HAIRLINE, 1.0);
            }
            if ui.hovered(r) {
                ui.g.fill(r.inset(6.0, 4.0), 8.0, Color::white(0.025));
            }
            let (badge, bc) = status_badge(&h.status);
            let mut x = r.x + 18.0;
            let time = h.at.get(11..16).unwrap_or(&h.at).to_string();
            ui.g.label(&time, Font::semibold(12.5), Rect::new(x, r.y, 46.0, r.h), theme::TEXT2, Align::Left);
            x += 54.0;
            if let Some(b) = badge {
                x += ui.chip(b, x, r.y + 15.0, bc) + 10.0;
            }
            let text = if h.final_text.is_empty() { "No speech" } else { h.final_text.as_str() };
            ui.g.label(text, Font::text(13.5), Rect::new(x, r.y, r.right() - x - 170.0, r.h), theme::TEXT, Align::Left);
            let meta = match h.latency_ms {
                Some(l) => format!("{}  ·  {l:.0} ms", h.app.trim_end_matches(".exe")),
                None => h.app.trim_end_matches(".exe").to_string(),
            };
            ui.g.label(&meta, Font::text(12.0), Rect::new(r.right() - 168.0, r.y, 150.0, r.h), theme::TEXT3, Align::Right);
        }
        let foot = Rect::new(card.x, card.bottom() - 56.0, card.w, 56.0);
        ui.g.line(foot.x + 18.0, foot.y, foot.right() - 18.0, foot.y, theme::HAIRLINE, 1.0);
        let bw = ui.button_width("Open history", false);
        if ui.button("ov/history", "Open history", None, Kind::Subtle, Rect::new(foot.x + 10.0, foot.y + 12.0, bw, 32.0)) {
            *nav = Some(Page::History);
        }
    }
}

fn status_badge(status: &str) -> (Option<&'static str>, Color) {
    match status {
        "ok" => (None, theme::GREEN),
        "empty" => (Some("No speech"), theme::TEXT3),
        "cancelled" => (Some("Cancelled"), theme::AMBER),
        "blocked" => (Some("Clipboard only"), theme::AMBER),
        "card" => (Some("No text field"), theme::BLUE),
        _ => (Some("Error"), theme::RED),
    }
}

fn latency_bar(ui: &mut Ui, r: Rect, h: &crate::stats::HistoryRow) {
    let Some(total) = h.latency_ms else {
        ui.g.label("Not pasted.", Font::text(12.0), Rect::new(r.x, r.y, r.w, 18.0), theme::TEXT3, Align::Left);
        return;
    };
    let parts = [
        ("Transcribe", h.transcribe_ms, theme::ACCENT),
        ("Text pipeline", h.post_ms, theme::GREEN),
        ("Cleanup", h.cleanup_ms, theme::BLUE),
        ("Key release", h.paste_wait_ms, theme::AMBER),
    ];
    let known: f64 = parts.iter().map(|p| p.1).sum();
    let rest = (total - known).max(0.0);
    ui.g.label(
        &format!("{total:.0} ms from release to paste"),
        Font::semibold(13.0),
        Rect::new(r.x, r.y, r.w, 18.0),
        theme::TEXT,
        Align::Left,
    );
    let bar = Rect::new(r.x, r.y + 26.0, r.w, 8.0);
    ui.g.fill(bar, 4.0, Color::white(0.08));
    let mut x = bar.x;
    let all = parts.iter().map(|p| (p.0, p.1, p.2)).chain([("Paste delay", rest, Color::white(0.16))]);
    let mut legend_x = r.x;
    for (label, ms, color) in all {
        let wseg = (ms / total.max(1.0)) as f32 * bar.w;
        if wseg >= 1.0 {
            ui.g.fill(Rect::new(x, bar.y, wseg, bar.h), 2.0, color);
            x += wseg;
        }
        if ms >= 0.5 {
            ui.g.circle(legend_x + 4.0, r.y + 52.0, 4.0, color);
            let t = format!("{label} {ms:.0} ms");
            let (tw, _) = ui.g.measure(&t, Font::text(12.0), 1000.0, false);
            ui.g.label(&t, Font::text(12.0), Rect::new(legend_x + 14.0, r.y + 43.0, tw + 4.0, 18.0), theme::TEXT2, Align::Left);
            legend_x += tw + 34.0;
        }
    }
}

// ---------------------------------------------------------------- history --

fn history(ui: &mut Ui, c: &mut Col, st: &mut PageState) {
    let cfg = cfg();
    header(
        ui,
        c,
        "History",
        &format!("Your recent dictations with the raw transcript. Audio is kept for the newest {}.", cfg.history_size),
    );
    let r = Rect::new(c.x, c.y, 360.0, 34.0);
    ui.text_input("history/search", st.search, "Search dictations", Some(icon::SEARCH), r);
    c.gap(50.0);
    let Some(rep) = report_or_message(ui, c, st) else { return };
    let q = st.search.text.to_lowercase();
    let rows: Vec<_> = rep
        .history
        .iter()
        .filter(|h| q.is_empty() || h.final_text.to_lowercase().contains(&q) || h.raw.to_lowercase().contains(&q) || h.app.to_lowercase().contains(&q))
        .collect();
    if rows.is_empty() {
        let r = c.take(64.0);
        ui.card(r);
        ui.g.label(if q.is_empty() { "No dictations yet." } else { "No match." }, Font::text(13.0), r.inset(16.0, 0.0), theme::TEXT2, Align::Left);
        return;
    }
    for h in rows {
        let inner = c.w - 32.0;
        let text = if h.final_text.is_empty() { "(no text)".to_string() } else { h.final_text.clone() };
        let text_h = ui.g.measure(&text, Font::text(14.0), inner, true).1;
        let mut extra = Vec::new();
        if !h.raw.is_empty() && h.raw != h.final_text {
            extra.push(format!("Heard: {}", h.raw));
        }
        if let Some(cl) = &h.cleaned {
            extra.push(format!("Cleanup {}: {cl}", if h.cleanup_used == Some(true) { "(used)" } else { "(rejected)" }));
        }
        let extra_h: f32 = extra.iter().map(|e| ui.g.measure(e, Font::text(12.0), inner, true).1 + 4.0).sum();
        let card = c.take(56.0 + text_h + extra_h + 12.0);
        ui.card(card);
        let mut x = card.x + 16.0;
        let (badge, bc) = status_badge(&h.status);
        if let Some(b) = badge {
            x += ui.chip(b, x, card.y + 13.0, bc) + 10.0;
        }
        let mut meta = format!("{}  ·  {}  ·  {}", h.at, h.app, h.model);
        if !h.language.is_empty() {
            meta.push_str(&format!("  ·  {}", h.language));
        }
        if let Some(l) = h.latency_ms {
            meta.push_str(&format!("  ·  {l:.0} ms"));
        }
        ui.g.label(&meta, Font::text(12.0), Rect::new(x, card.y + 12.0, card.right() - x - 100.0, 22.0), theme::TEXT3, Align::Left);
        let mut bx = card.right() - 16.0;
        if !h.final_text.is_empty() {
            bx -= 32.0;
            if ui.button(&format!("hist/copy/{}", h.id), "", Some(icon::COPY), Kind::Subtle, Rect::new(bx, card.y + 8.0, 32.0, 30.0)) {
                ui.actions.push(Action::Copy(h.final_text.clone()));
            }
        }
        if let Some(a) = &h.audio {
            bx -= 36.0;
            if ui.button(&format!("hist/play/{}", h.id), "", Some(icon::PLAY), Kind::Subtle, Rect::new(bx, card.y + 8.0, 32.0, 30.0)) {
                ui.actions.push(Action::Play(a.clone()));
            }
        }
        let mut y = card.y + 44.0;
        y += ui.g.text(&text, Font::text(14.0), Rect::new(card.x + 16.0, y, inner, text_h), if h.final_text.is_empty() { theme::TEXT3 } else { theme::TEXT }, true, Align::Left);
        y += 6.0;
        for e in &extra {
            y += ui.g.text(e, Font::text(12.0), Rect::new(card.x + 16.0, y, inner, 400.0), theme::TEXT3, true, Align::Left) + 4.0;
        }
        c.gap(8.0);
    }
}

// ------------------------------------------------------------- statistics --

fn statistics(ui: &mut Ui, c: &mut Col, st: &mut PageState) {
    let cfg = cfg();
    header(ui, c, "Statistics", "Everything measured on every dictation, stored locally.");
    let Some(r) = report_or_message(ui, c, st) else { return };
    tiles(
        ui,
        c,
        &[
            ("Words today", fmt_int(r.words.today), plural(r.dictations.today, "dictation", "dictations"), None),
            ("This week", fmt_int(r.words.week), plural(r.dictations.week, "dictation", "dictations"), None),
            ("This month", fmt_int(r.words.month), plural(r.dictations.month, "dictation", "dictations"), None),
            ("All time", fmt_int(r.words.all), plural(r.dictations.all, "dictation", "dictations"), None),
            ("Average WPM", format!("{:.0}", r.avg_wpm), format!("{:.0} including pauses", r.avg_wpm_total), None),
            ("Best WPM", format!("{:.0}", r.best_wpm), "at least 3 s of speech".into(), None),
            ("Time saved", fmt_minutes(r.saved_minutes), format!("typing at {:.0} WPM", cfg.typing_wpm), Some(theme::GREEN)),
            ("Streak", plural(r.streak_days, "day", "days"), format!("longest {}", plural(r.longest_streak, "day", "days")), None),
            ("Release to paste", format!("{:.0} ms", r.avg_latency), format!("p95 {:.0} ms", r.p95_latency), None),
            ("Enter taps", fmt_int(r.enter_taps), format!("{} today · {:.1} ms", r.enter_taps_today, r.enter_latency), None),
            ("Longest dictation", format!("{:.1} s", r.longest_ms as f64 / 1000.0), plural(r.longest_words, "word", "words"), None),
            ("Vocabulary", fmt_int(r.vocabulary as i64), "distinct words".into(), None),
        ],
    );

    section(ui, c, "Words per day");
    let card = c.take(220.0);
    ui.card(card);
    bar_chart(ui, card.inset(20.0, 20.0), &r.daily);

    section(ui, c, "When you dictate");
    let card = c.take(232.0);
    ui.card(card);
    heatmap(ui, card.inset(20.0, 18.0), &r.heatmap);

    let half = (c.w - 12.0) / 2.0;
    section(ui, c, "By app and language");
    let rows = r.apps.len().max(r.languages.len()).clamp(1, 8) as f32;
    let h = 52.0 + rows * 30.0;
    let left = Rect::new(c.x, c.y, half, h);
    let right = Rect::new(c.x + half + 12.0, c.y, half, h);
    breakdown(ui, left, "Apps", &r.apps);
    breakdown(ui, right, "Languages", &r.languages);
    c.gap(h);

    section(ui, c, "Latency distribution");
    let card = c.take(190.0);
    ui.card(card);
    histogram(ui, card.inset(20.0, 18.0), &r.latencies);

    section(ui, c, "Models");
    let cols: [(&str, f32, Align); 8] = [
        ("Model", 0.30, Align::Left),
        ("Dictations", 0.10, Align::Right),
        ("Words", 0.09, Align::Right),
        ("Median", 0.10, Align::Right),
        ("p95", 0.09, Align::Right),
        ("Transcribe", 0.11, Align::Right),
        ("RTF", 0.09, Align::Right),
        ("Fixes /100w", 0.12, Align::Right),
    ];
    let rows: Vec<Vec<String>> = r
        .models
        .iter()
        .map(|m| {
            vec![
                format!("{} ({})", m.model, m.backend),
                fmt_int(m.dictations),
                fmt_int(m.words),
                format!("{:.0} ms", m.p50_latency),
                format!("{:.0} ms", m.p95_latency),
                format!("{:.0} ms", m.avg_transcribe),
                format!("{:.3}", m.avg_rtf),
                format!("{:.2}", m.corrections_per_100_words),
            ]
        })
        .collect();
    table(ui, c, &cols, &rows);

    section(ui, c, "Quality");
    let q = [
        ("Cancelled", fmt_int(r.cancelled)),
        ("No speech detected", fmt_int(r.empty)),
        ("Errors", fmt_int(r.errors)),
        ("Elevated window (clipboard only)", fmt_int(r.blocked)),
        ("Average silence in recordings", format!("{:.0}%", r.avg_silence * 100.0)),
        ("Filler words removed", fmt_int(r.fillers_removed)),
        ("Cleanup runs / rejected", format!("{} / {}", fmt_int(r.cleanup_runs), fmt_int(r.cleanup_rejected))),
        ("Cleanup average", if r.cleanup_runs > 0 { format!("{:.0} ms", r.cleanup_avg_ms) } else { "–".into() }),
    ];
    let rows: Vec<Vec<String>> = q.iter().map(|(k, v)| vec![k.to_string(), v.clone()]).collect();
    table(ui, c, &[("", 0.7, Align::Left), ("", 0.3, Align::Right)], &rows);
}

fn bar_chart(ui: &mut Ui, r: Rect, daily: &[(String, i64)]) {
    if daily.is_empty() {
        return;
    }
    let max = daily.iter().map(|d| d.1).max().unwrap_or(0).max(1) as f32;
    let label_h = 20.0;
    let ch = r.h - label_h - 18.0;
    let n = daily.len() as f32;
    let bw = r.w / n;
    ui.g.label(&fmt_int(max as i64), Font::text(11.0), Rect::new(r.x, r.y, 60.0, 14.0), theme::TEXT3, Align::Left);
    let base = r.y + 18.0 + ch;
    ui.g.line(r.x, base, r.right(), base, Color::white(0.10), 1.0);
    let mut tip = None;
    for (i, (day, v)) in daily.iter().enumerate() {
        let x = r.x + i as f32 * bw;
        let bh = (*v as f32 / max) * ch;
        let col = Rect::new(x, r.y + 18.0, bw, ch);
        let hovered = ui.hovered(col);
        let color = if hovered { theme::ACCENT } else { theme::ACCENT.alpha(0.7) };
        if *v > 0 {
            ui.g.gradient(Rect::new(x + bw * 0.18, base - bh, bw * 0.64, bh), 4.0, color.mix(theme::ACCENT2, 0.4), color.alpha(0.55));
        }
        if i % 5 == 4 || i + 1 == daily.len() {
            ui.g.label(day, Font::text(11.0), Rect::new(x - 20.0, base + 4.0, bw + 40.0, 14.0), theme::TEXT3, Align::Center);
        }
        if hovered {
            tip = Some((x + bw / 2.0, base - bh, format!("{day}: {} words", fmt_int(*v))));
        }
    }
    if let Some((x, y, t)) = tip {
        tooltip(ui, x, y - 6.0, &t);
    }
}

fn tooltip(ui: &mut Ui, x: f32, y: f32, t: &str) {
    let (tw, _) = ui.g.measure(t, Font::text(12.0), 1000.0, false);
    let r = Rect::new(x - tw / 2.0 - 10.0, y - 28.0, tw + 20.0, 26.0);
    ui.g.fill(r, 5.0, Color::rgb(52, 52, 58));
    ui.g.stroke(r, 5.0, Color::white(0.10), 1.0);
    ui.g.label(t, Font::text(12.0), r, theme::TEXT, Align::Center);
}

fn heatmap(ui: &mut Ui, r: Rect, data: &[[i64; 24]]) {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let max = data.iter().flat_map(|d| d.iter()).copied().max().unwrap_or(0).max(1) as f32;
    let label_w = 40.0;
    let cw = ((r.w - label_w) / 24.0).min(34.0);
    let ch = ((r.h - 20.0) / 7.0).min(26.0);
    for h in (0..24).step_by(3) {
        ui.g.label(&format!("{h:02}"), Font::text(11.0), Rect::new(r.x + label_w + h as f32 * cw, r.y, cw * 2.0, 14.0), theme::TEXT3, Align::Left);
    }
    let mut tip = None;
    for (d, row) in data.iter().enumerate() {
        let y = r.y + 20.0 + d as f32 * ch;
        ui.g.label(DAYS[d], Font::text(12.0), Rect::new(r.x, y, label_w, ch - 4.0), theme::TEXT3, Align::Left);
        for (h, &v) in row.iter().enumerate() {
            let cell = Rect::new(r.x + label_w + h as f32 * cw, y, cw - 4.0, ch - 4.0);
            let t = (v as f32 / max).sqrt();
            let color = if v == 0 { Color::white(0.05) } else { theme::ACCENT.alpha(0.2 + 0.8 * t) };
            ui.g.fill(cell, 4.0, color);
            if ui.hovered(cell) {
                ui.g.stroke(cell, 4.0, Color::white(0.6), 1.0);
                tip = Some((cell.x + cell.w / 2.0, cell.y, format!("{} {:02}:00 · {} words", DAYS[d], h, fmt_int(v))));
            }
        }
    }
    if let Some((x, y, t)) = tip {
        tooltip(ui, x, y - 4.0, &t);
    }
}

fn breakdown(ui: &mut Ui, r: Rect, title: &str, rows: &[crate::stats::Breakdown]) {
    ui.card(r);
    ui.g.label(title, Font::semibold(13.0), Rect::new(r.x + 16.0, r.y + 12.0, r.w - 32.0, 20.0), theme::TEXT, Align::Left);
    let max = rows.iter().map(|b| b.words).max().unwrap_or(1).max(1) as f32;
    let mut y = r.y + 44.0;
    if rows.is_empty() {
        ui.g.label("No data yet", Font::text(12.0), Rect::new(r.x + 16.0, y, r.w, 20.0), theme::TEXT3, Align::Left);
    }
    for b in rows.iter().take(8) {
        let name_w = r.w * 0.36;
        ui.g.label(&b.name, Font::text(13.0), Rect::new(r.x + 16.0, y, name_w - 8.0, 24.0), theme::TEXT, Align::Left);
        let bar_x = r.x + 16.0 + name_w;
        let bar_w = r.w - name_w - 110.0;
        ui.g.fill(Rect::new(bar_x, y + 9.0, bar_w, 6.0), 3.0, Color::white(0.06));
        ui.g.fill(Rect::new(bar_x, y + 9.0, (bar_w * b.words as f32 / max).max(3.0), 6.0), 3.0, theme::ACCENT);
        ui.g.label(&fmt_int(b.words), Font::text(12.0), Rect::new(r.right() - 90.0, y, 74.0, 24.0), theme::TEXT2, Align::Right);
        y += 30.0;
    }
}

fn histogram(ui: &mut Ui, r: Rect, lat: &[f64]) {
    let edges = [0.0, 75.0, 100.0, 125.0, 150.0, 200.0, 300.0, 500.0, f64::INFINITY];
    let labels = ["<75", "75", "100", "125", "150", "200", "300", "500+"];
    let mut counts = [0i64; 8];
    for &l in lat {
        if let Some(i) = edges.windows(2).position(|e| l >= e[0] && l < e[1]) {
            counts[i] += 1;
        }
    }
    let max = *counts.iter().max().unwrap_or(&1).max(&1) as f32;
    let bw = r.w / 8.0;
    let ch = r.h - 40.0;
    let base = r.y + 18.0 + ch;
    for (i, &n) in counts.iter().enumerate() {
        let x = r.x + i as f32 * bw;
        let bh = n as f32 / max * ch;
        let color = if edges[i + 1] <= 150.0 { theme::GREEN } else if edges[i] < 300.0 { theme::AMBER } else { theme::RED };
        if n > 0 {
            ui.g.fill(Rect::new(x + bw * 0.15, base - bh, bw * 0.7, bh), 3.0, color.alpha(0.8));
            ui.g.label(&n.to_string(), Font::text(11.0), Rect::new(x, base - bh - 16.0, bw, 14.0), theme::TEXT2, Align::Center);
        }
        ui.g.label(labels[i], Font::text(11.0), Rect::new(x, base + 4.0, bw, 14.0), theme::TEXT3, Align::Center);
    }
    ui.g.label("ms from release to paste", Font::text(11.0), Rect::new(r.x, r.bottom() - 12.0, r.w, 14.0), theme::TEXT3, Align::Right);
}

fn table(ui: &mut Ui, c: &mut Col, cols: &[(&str, f32, Align)], rows: &[Vec<String>]) {
    let has_header = cols.iter().any(|c| !c.0.is_empty());
    let row_h = 34.0;
    let h = (rows.len().max(1) as f32 + if has_header { 1.0 } else { 0.0 }) * row_h + 12.0;
    let card = c.take(h);
    ui.card(card);
    let inner = card.inset(16.0, 6.0);
    let mut y = inner.y;
    let cell = |i: usize, y: f32| -> Rect {
        let x0: f32 = cols[..i].iter().map(|c| c.1).sum::<f32>() * inner.w;
        Rect::new(inner.x + x0, y, cols[i].1 * inner.w - 8.0, row_h)
    };
    if has_header {
        for (i, (h, _, a)) in cols.iter().enumerate() {
            ui.g.label(&h.to_uppercase(), Font::semibold(11.0), cell(i, y), theme::TEXT3, *a);
        }
        y += row_h;
    }
    if rows.is_empty() {
        ui.g.label("No data yet", Font::text(13.0), Rect::new(inner.x, y, inner.w, row_h), theme::TEXT3, Align::Left);
    }
    for row in rows {
        ui.g.line(inner.x, y, inner.right(), y, Color::white(0.05), 1.0);
        for (i, v) in row.iter().enumerate().take(cols.len()) {
            ui.g.label(v, Font::text(13.0), cell(i, y), if i == 0 { theme::TEXT } else { theme::TEXT2 }, cols[i].2);
        }
        y += row_h;
    }
}

// ----------------------------------------------------------------- models --

fn models_page(ui: &mut Ui, c: &mut Col, st: &mut PageState) {
    let cfg = cfg();
    let s = status::get();
    header(
        ui,
        c,
        "Models",
        "Everything runs on this PC. Files are pinned to a revision, checked with SHA-256 and stored in your local app data.",
    );
    let usage = st.report.and_then(|r| r.as_ref().ok()).map(|r| r.models.clone()).unwrap_or_default();
    section(ui, c, "Speech recognition");
    for m in models::ASR {
        let active = m.id == cfg.model;
        let card = c.take(104.0);
        ui.card(card);
        if active {
            ui.g.stroke(card, theme::RADIUS, theme::ACCENT.alpha(0.6), 1.5);
        }
        ui.g.label(m.name, Font::semibold(15.0), Rect::new(card.x + 18.0, card.y + 14.0, card.w - 260.0, 22.0), theme::TEXT, Align::Left);
        ui.g.label(m.note, Font::text(12.0), Rect::new(card.x + 18.0, card.y + 38.0, card.w - 260.0, 18.0), theme::TEXT2, Align::Left);
        let mut x = card.x + 18.0;
        let y = card.y + 66.0;
        x += ui.chip(if m.is_streaming() { "Streaming" } else { "Offline" }, x, y, if m.is_streaming() { theme::ACCENT } else { theme::TEXT2 }) + 6.0;
        let langs = if m.languages.len() == 1 { "English".to_string() } else { format!("{} languages", m.languages.len()) };
        x += ui.chip(&langs, x, y, theme::BLUE) + 6.0;
        x += ui.chip(&format!("{} MB", m.asset.size >> 20), x, y, theme::TEXT3) + 6.0;
        if let Some(u) = usage.iter().find(|u| u.model == m.id) {
            ui.chip(&format!("{} dictations · median {:.0} ms", fmt_int(u.dictations), u.p50_latency), x, y, theme::TEXT3);
        }
        model_actions(ui, card, &m.asset, m.id, active, &s, |ui| set(ui, "model", m.id));
        c.gap(10.0);
    }

    section(ui, c, "Cleanup (optional small LLM)");
    let row = c.take(64.0);
    ui.card(row);
    ui.g.label("Clean up punctuation and phrasing after transcription", Font::text(14.0), Rect::new(row.x + 18.0, row.y + 12.0, row.w - 120.0, 20.0), theme::TEXT, Align::Left);
    ui.g.label(
        "Adds ~80–100 ms. Output that drifts from what you said is discarded.",
        Font::text(12.0),
        Rect::new(row.x + 18.0, row.y + 34.0, row.w - 120.0, 18.0),
        theme::TEXT3,
        Align::Left,
    );
    if let Some(v) = ui.toggle("models/cleanup", cfg.cleanup.enabled, row.right() - 64.0, row.y + 22.0) {
        set(ui, "cleanup.enabled", v);
    }
    c.gap(10.0);
    for m in models::CLEANUP {
        let active = m.id == cfg.cleanup.model;
        let card = c.take(84.0);
        ui.card(card);
        if active {
            ui.g.stroke(card, theme::RADIUS, theme::ACCENT.alpha(if cfg.cleanup.enabled { 0.6 } else { 0.25 }), 1.5);
        }
        ui.g.label(m.name, Font::semibold(15.0), Rect::new(card.x + 18.0, card.y + 14.0, card.w - 260.0, 22.0), theme::TEXT, Align::Left);
        let mut x = card.x + 18.0;
        x += ui.chip(&format!("{} MB", m.asset.size >> 20), x, card.y + 46.0, theme::TEXT3) + 6.0;
        if m.prompt == models::Prompt::S1 {
            ui.chip("English, trained for transcript cleanup", x, card.y + 46.0, theme::GREEN);
        }
        model_actions(ui, card, &m.asset, m.id, active, &s, |ui| set(ui, "cleanup.model", m.id));
        c.gap(10.0);
    }

    section(ui, c, "Voice activity detection");
    let card = c.take(64.0);
    ui.card(card);
    ui.g.label("Silero VAD v6.2", Font::semibold(15.0), Rect::new(card.x + 18.0, card.y + 10.0, 300.0, 22.0), theme::TEXT, Align::Left);
    ui.g.label("Trims silence and detects empty recordings. Required, 1 MB.", Font::text(12.0), Rect::new(card.x + 18.0, card.y + 34.0, card.w - 200.0, 18.0), theme::TEXT2, Align::Left);
    let ready = models::VAD.is_ready();
    ui.chip(if ready { "Installed" } else { "Missing" }, card.right() - 110.0, card.y + 22.0, if ready { theme::GREEN } else { theme::AMBER });
    c.gap(18.0);

    let used: u64 = std::fs::read_dir(crate::paths::models_dir())
        .map(|d| d.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0);
    let r = Rect::new(c.x, c.y, c.w, 34.0);
    ui.g.label(&format!("{:.2} GB on disk", used as f64 / 1e9), Font::text(13.0), Rect::new(r.x, r.y, 300.0, r.h), theme::TEXT2, Align::Left);
    let bw = ui.button_width("Open models folder", true);
    if ui.button("models/folder", "Open models folder", Some(icon::FOLDER), Kind::Secondary, Rect::new(r.right() - bw, r.y, bw, 32.0)) {
        ui.actions.push(Action::Open(crate::paths::models_dir()));
    }
    c.gap(34.0);
}

fn model_actions(
    ui: &mut Ui,
    card: Rect,
    asset: &'static models::Asset,
    id: &'static str,
    active: bool,
    s: &status::Status,
    use_it: impl FnOnce(&mut Ui),
) {
    let right = card.right() - 18.0;
    let y = card.y + (card.h - 32.0) / 2.0;
    if let Some((_, f)) = s.downloads.iter().find(|(file, _)| file == asset.file) {
        ui.g.label(&format!("{:.0}%", f * 100.0), Font::semibold(13.0), Rect::new(right - 200.0, y - 10.0, 200.0, 20.0), theme::TEXT, Align::Right);
        ui.progress(Rect::new(right - 200.0, y + 18.0, 200.0, 4.0), *f, theme::ACCENT);
        ui.animate = true;
        return;
    }
    if !asset.is_ready() {
        let label = format!("Download {} MB", asset.size >> 20);
        let bw = ui.button_width(&label, true);
        if ui.button(&format!("dl/{id}"), &label, Some(icon::DOWNLOAD), Kind::Secondary, Rect::new(right - bw, y, bw, 32.0)) {
            ui.actions.push(Action::Download(id));
        }
        return;
    }
    if active {
        ui.chip("In use", right - 64.0, y + 6.0, theme::GREEN);
        return;
    }
    let del = Rect::new(right - 36.0, y, 36.0, 32.0);
    if ui.button(&format!("del/{id}"), "", Some(icon::DELETE), Kind::Subtle, del) {
        ui.actions.push(Action::DeleteModel(asset));
    }
    let r = Rect::new(right - 36.0 - 8.0 - 80.0, y, 80.0, 32.0);
    if ui.button(&format!("use/{id}"), "Use", None, Kind::Primary, r) {
        use_it(ui);
    }
}

// ------------------------------------------------------------------ words --

fn words_value(words: &[crate::config::CustomWord]) -> toml_edit::Value {
    let mut arr = toml_edit::Array::new();
    for w in words {
        match w.strictness {
            None => arr.push(w.word.as_str()),
            Some(s) => {
                let mut t = toml_edit::InlineTable::new();
                t.insert("word", w.word.as_str().into());
                t.insert("strictness", ((s * 100.0).round() as f64 / 100.0).into());
                arr.push(t);
            }
        }
    }
    toml_edit::Value::Array(arr)
}

fn words(ui: &mut Ui, c: &mut Col, st: &mut PageState) {
    let cfg = cfg();
    header(
        ui,
        c,
        "Words",
        "Custom words fix how names and jargon are spelled. Words of four letters or fewer only match exactly; raise the strictness of a word that fires on ordinary speech.",
    );
    section(ui, c, "Custom words");
    let r = Rect::new(c.x, c.y, 300.0, 34.0);
    let submit = ui.text_input("words/new", st.new_word, "Add a word, e.g. Supabase", Some(icon::ADD), r);
    let add = ui.button("words/add", "Add", None, Kind::Primary, Rect::new(r.right() + 8.0, r.y, 72.0, 34.0));
    let new = st.new_word.text.trim().to_string();
    if (submit || add) && !new.is_empty() && !cfg.custom_words.iter().any(|w| w.word == new) {
        let mut list = cfg.custom_words.clone();
        list.push(crate::config::CustomWord { word: new, strictness: None });
        ui.actions.push(Action::Config(vec![("custom_words".into(), words_value(&list))]));
        st.new_word.text.clear();
        st.new_word.caret = 0;
    }
    c.gap(48.0);
    let stats = st.report.and_then(|r| r.as_ref().ok()).map(|r| r.custom_words.clone()).unwrap_or_default();
    let list_h = (cfg.custom_words.len().max(1) as f32) * 52.0 + 44.0;
    let card = c.take(list_h);
    ui.card(card);
    let cols = [("Word", 0.28), ("Strictness", 0.26), ("Fired", 0.12), ("Avg score", 0.14), ("", 0.20)];
    let inner = card.inset(18.0, 8.0);
    let mut x = inner.x;
    for (h, f) in cols {
        ui.g.label(h, Font::semibold(12.0), Rect::new(x, inner.y, inner.w * f, 28.0), theme::TEXT2, Align::Left);
        x += inner.w * f;
    }
    let mut y = inner.y + 36.0;
    let mut changed: Option<Vec<crate::config::CustomWord>> = None;
    if cfg.custom_words.is_empty() {
        ui.g.label("No custom words yet.", Font::text(13.0), Rect::new(inner.x, y, inner.w, 40.0), theme::TEXT3, Align::Left);
    }
    for (i, w) in cfg.custom_words.iter().enumerate() {
        ui.g.line(inner.x, y - 4.0, inner.right(), y - 4.0, Color::white(0.05), 1.0);
        let mut x = inner.x;
        ui.g.label(&w.word, Font::semibold(14.0), Rect::new(x, y, inner.w * 0.28 - 8.0, 40.0), theme::TEXT, Align::Left);
        x += inner.w * 0.28;
        let key: String = w.word.chars().filter(|c| c.is_alphanumeric()).collect();
        let default = crate::text::default_required(key.chars().count());
        let current = w.strictness.unwrap_or(default);
        let label = match w.strictness {
            None => format!("Auto ({current:.2})"),
            Some(s) => format!("{s:.2}"),
        };
        let d = ui.stepper(&format!("words/strict/{i}"), &label, Rect::new(x, y + 4.0, 170.0, 32.0));
        if d != 0 {
            let mut list = cfg.custom_words.clone();
            let v = ((current + d as f32 * 0.05) * 100.0).round() / 100.0;
            list[i].strictness = Some(v.clamp(0.5, 1.0));
            changed = Some(list);
        }
        x += inner.w * 0.26;
        let s = stats.iter().find(|s| s.word == w.word);
        ui.g.label(&s.map_or("0".into(), |s| fmt_int(s.fired)), Font::text(13.0), Rect::new(x, y, inner.w * 0.12, 40.0), theme::TEXT2, Align::Left);
        x += inner.w * 0.12;
        let score = s.map_or("–".to_string(), |s| format!("{:.2}", s.avg_score));
        let warn = s.is_some_and(|s| s.min_score < 0.9);
        ui.g.label(&score, Font::text(13.0), Rect::new(x, y, inner.w * 0.14, 40.0), if warn { theme::AMBER } else { theme::TEXT2 }, Align::Left);
        if ui.button(&format!("words/del/{i}"), "", Some(icon::DELETE), Kind::Subtle, Rect::new(inner.right() - 36.0, y + 4.0, 36.0, 32.0)) {
            let mut list = cfg.custom_words.clone();
            list.remove(i);
            changed = Some(list);
        }
        if w.strictness.is_some() && ui.button(&format!("words/auto/{i}"), "Auto", None, Kind::Subtle, Rect::new(inner.right() - 100.0, y + 4.0, 56.0, 32.0)) {
            let mut list = cfg.custom_words.clone();
            list[i].strictness = None;
            changed = Some(list);
        }
        y += 52.0;
    }
    if let Some(list) = changed {
        ui.actions.push(Action::Config(vec![("custom_words".into(), words_value(&list))]));
    }

    section(ui, c, "Recent corrections");
    let corr = st.report.and_then(|r| r.as_ref().ok()).map(|r| r.corrections.clone()).unwrap_or_default();
    let rows: Vec<Vec<String>> = corr
        .iter()
        .take(15)
        .map(|k| vec![k.at.clone(), format!("{}  →  {}", k.original, k.word), format!("{:.2}", k.score)])
        .collect();
    table(ui, c, &[("When", 0.25, Align::Left), ("Heard → written", 0.6, Align::Left), ("Score", 0.15, Align::Right)], &rows);

    section(ui, c, "Filler words");
    let row = c.take(64.0);
    ui.card(row);
    ui.g.label("Remove filler words", Font::text(14.0), Rect::new(row.x + 18.0, row.y + 12.0, row.w, 20.0), theme::TEXT, Align::Left);
    ui.g.label(
        "Language lists apply only when the language is known, so German \u{201C}um\u{201D} stays a word.",
        Font::text(12.0),
        Rect::new(row.x + 18.0, row.y + 34.0, row.w - 120.0, 18.0),
        theme::TEXT3,
        Align::Left,
    );
    if let Some(v) = ui.toggle("words/fillers", cfg.remove_fillers, row.right() - 64.0, row.y + 22.0) {
        set(ui, "remove_fillers", v);
    }
    c.gap(10.0);
    let langs = ["Any language", "English", "German"];
    let keys = ["any", "en", "de"];
    if let Some(i) = ui.segmented("words/lang", &langs, *st.filler_lang, Rect::new(c.x, c.y, 360.0, 34.0)) {
        *st.filler_lang = i;
    }
    c.gap(46.0);
    let key = keys[*st.filler_lang];
    let list: Vec<String> = if key == "any" {
        cfg.filler_words.any.clone()
    } else {
        cfg.filler_words.by_lang.get(key).cloned().unwrap_or_default()
    };
    let card = c.take(96.0);
    ui.card(card);
    let mut x = card.x + 16.0;
    let mut remove = None;
    for (i, wd) in list.iter().enumerate() {
        let font = Font::text(13.0);
        let (tw, _) = ui.g.measure(wd, font, 1000.0, false);
        let chip = Rect::new(x, card.y + 16.0, tw + 40.0, 30.0);
        if chip.right() > card.right() - 16.0 {
            break;
        }
        ui.g.fill(chip, 15.0, Color::white(0.08));
        ui.g.label(wd, font, Rect::new(chip.x + 12.0, chip.y, tw + 2.0, chip.h), theme::TEXT, Align::Left);
        if ui.button(&format!("filler/del/{key}/{i}"), "", Some(icon::REMOVE), Kind::Subtle, Rect::new(chip.right() - 26.0, chip.y + 3.0, 24.0, 24.0)) {
            remove = Some(i);
        }
        x = chip.right() + 8.0;
    }
    let ir = Rect::new(card.x + 16.0, card.y + 54.0, 220.0, 30.0);
    let submit = ui.text_input("filler/new", st.new_filler, "Add filler", Some(icon::ADD), ir);
    let new = st.new_filler.text.trim().to_lowercase();
    if (submit && !new.is_empty() && !list.contains(&new)) || remove.is_some() {
        let mut l = list.clone();
        if let Some(i) = remove {
            l.remove(i);
        } else {
            l.push(new);
            st.new_filler.text.clear();
            st.new_filler.caret = 0;
        }
        let arr: toml_edit::Array = l.iter().map(String::as_str).collect();
        ui.actions.push(Action::Config(vec![(format!("filler_words.{key}"), toml_edit::Value::Array(arr))]));
    }
}

// --------------------------------------------------------------- settings --

/// A settings row: title + description on the left; returns the control area.
fn row(ui: &mut Ui, c: &mut Col, title: &str, desc: &str, control_w: f32) -> Rect {
    let r = c.take(68.0);
    ui.card(r);
    ui.g.label(title, Font::text(14.0), Rect::new(r.x + 18.0, r.y + 13.0, r.w - control_w - 60.0, 20.0), theme::TEXT, Align::Left);
    ui.g.label(desc, Font::text(12.0), Rect::new(r.x + 18.0, r.y + 35.0, r.w - control_w - 60.0, 18.0), theme::TEXT3, Align::Left);
    c.gap(4.0);
    Rect::new(r.right() - control_w - 18.0, r.y + (r.h - 32.0) / 2.0, control_w, 32.0)
}

fn toggle_row(ui: &mut Ui, c: &mut Col, title: &str, desc: &str, key: &str, on: bool) {
    let r = row(ui, c, title, desc, 40.0);
    if let Some(v) = ui.toggle(&format!("set/{key}"), on, r.x, r.y + 6.0) {
        set(ui, key, v);
    }
}

fn stepper_row(ui: &mut Ui, c: &mut Col, title: &str, desc: &str, key: &str, value: i64, label: String, steps: &[i64]) {
    let r = row(ui, c, title, desc, 170.0);
    let d = ui.stepper(&format!("set/{key}"), &label, r);
    if d != 0 {
        let pos = steps.iter().position(|&s| s >= value).unwrap_or(steps.len() - 1);
        let next = if d > 0 {
            steps.iter().copied().find(|&s| s > value).unwrap_or(*steps.last().unwrap())
        } else {
            steps[..pos].last().copied().unwrap_or(steps[0])
        };
        set(ui, key, next);
    }
}

fn settings(ui: &mut Ui, c: &mut Col, st: &mut PageState) {
    let cfg = cfg();
    header(ui, c, "Settings", "Changes save to config.toml and apply immediately.");

    section(ui, c, "Dictation");
    let r = row(ui, c, "Microphone", "Input device for dictation", 320.0);
    let mut options = vec!["System default".to_string()];
    options.extend(st.devices.iter().cloned());
    if !cfg.microphone.is_empty() && !options.contains(&cfg.microphone) {
        options.push(cfg.microphone.clone());
    }
    let selected = if cfg.microphone.is_empty() { 0 } else { options.iter().position(|o| o == &cfg.microphone).unwrap_or(0) };
    if let Some(i) = ui.select("set/mic", &options, selected, r) {
        set(ui, "microphone", if i == 0 { String::new() } else { options[i].clone() });
    }
    stepper_row(
        ui,
        c,
        "Hold threshold",
        "Hold at least this long to record; shorter taps press Enter",
        "hold_threshold_ms",
        cfg.hold_threshold_ms as i64,
        format!("{} ms", cfg.hold_threshold_ms),
        &(150..=800).step_by(25).collect::<Vec<i64>>(),
    );
    let r = row(ui, c, "Language", "Auto-detects on multilingual models", 300.0);
    let langs = ["auto", "en", "de"];
    let li = langs.iter().position(|l| *l == cfg.language).unwrap_or(0);
    if let Some(i) = ui.segmented("set/lang", &["Auto", "English", "German"], li, r) {
        set(ui, "language", langs[i]);
    }
    toggle_row(ui, c, "Live overlay", "Show the transcript at the bottom of the screen while you speak", "overlay", cfg.overlay);
    toggle_row(
        ui,
        c,
        "Only paste into text fields",
        "If nothing editable has focus, the transcript waits on screen with a Copy button",
        "check_focus",
        cfg.check_focus,
    );
    let r = row(ui, c, "Transcription", "Full context re-reads the whole sentence (more accurate); streaming is lighter on CPU", 300.0);
    let modes = ["segments", "stream"];
    let mi = modes.iter().position(|m| *m == cfg.decode).unwrap_or(0);
    if let Some(i) = ui.segmented("set/decode", &["Full context", "Streaming"], mi, r) {
        set(ui, "decode", modes[i]);
    }

    section(ui, c, "Speech engine");
    let r = row(ui, c, "Compute", "GPU uses Vulkan; CPU works everywhere", 300.0);
    let backs = ["auto", "vulkan", "cpu"];
    let bi = backs.iter().position(|b| *b == cfg.backend).unwrap_or(0);
    if let Some(i) = ui.segmented("set/backend", &["Auto", "GPU", "CPU"], bi, r) {
        set(ui, "backend", backs[i]);
    }
    stepper_row(
        ui,
        c,
        "Unload model when idle",
        "Frees memory after this long without dictation",
        "idle_unload_minutes",
        cfg.idle_unload_minutes as i64,
        if cfg.idle_unload_minutes == 0 { "Never".into() } else { format!("{} min", cfg.idle_unload_minutes) },
        &[0, 5, 15, 30, 60, 120, 240],
    );
    stepper_row(
        ui,
        c,
        "Keep microphone ready",
        "Reopening the device costs ~36 ms at the start of a recording",
        "mic_keep_open_secs",
        cfg.mic_keep_open_secs as i64,
        if cfg.mic_keep_open_secs == 0 { "Close at once".into() } else { format!("{} s", cfg.mic_keep_open_secs) },
        &[0, 30, 60, 120, 300, 600],
    );

    section(ui, c, "Pasting");
    let delays: Vec<i64> = (0..=200).step_by(10).collect();
    stepper_row(ui, c, "Delay before Ctrl+V", "Time for the clipboard to settle", "paste_delay_before_ms", cfg.paste_delay_before_ms as i64, format!("{} ms", cfg.paste_delay_before_ms), &delays);
    stepper_row(ui, c, "Delay before restoring the clipboard", "Time for the app to read the text", "paste_delay_after_ms", cfg.paste_delay_after_ms as i64, format!("{} ms", cfg.paste_delay_after_ms), &delays);

    section(ui, c, "Cleanup");
    toggle_row(ui, c, "Clean up with a local LLM", "Off by default; see Models to pick the model", "cleanup.enabled", cfg.cleanup.enabled);
    let r = row(ui, c, "Style", "How formal the cleaned text should be (S1-mini)", 420.0);
    let styles = ["casual", "semi-casual", "semi-formal", "formal"];
    let si = styles.iter().position(|s| *s == cfg.cleanup.styling).unwrap_or(2);
    if let Some(i) = ui.segmented("set/style", &["Casual", "Semi-casual", "Semi-formal", "Formal"], si, r) {
        set(ui, "cleanup.styling", styles[i]);
    }
    let r = row(ui, c, "Structure", "Lists turns enumerations into bullet points", 220.0);
    let structs = ["prose", "lists"];
    let sti = structs.iter().position(|s| *s == cfg.cleanup.structure).unwrap_or(0);
    if let Some(i) = ui.segmented("set/structure", &["Prose", "Lists"], sti, r) {
        set(ui, "cleanup.structure", structs[i]);
    }

    section(ui, c, "Statistics");
    stepper_row(ui, c, "Your typing speed", "Used to estimate time saved", "typing_wpm", cfg.typing_wpm as i64, format!("{:.0} WPM", cfg.typing_wpm), &(20..=150).step_by(5).collect::<Vec<i64>>());
    stepper_row(ui, c, "Keep audio of recent dictations", "Playable from History", "history_size", cfg.history_size as i64, format!("{}", cfg.history_size), &[0, 5, 10, 20, 50, 100]);

    section(ui, c, "System");
    toggle_row(ui, c, "Start with Windows", "Launch chatty when you sign in", "autostart", cfg.autostart);
    let r = row(ui, c, "Files", "Configuration, log and data folder", 420.0);
    let labels = [("Config", icon::OPEN, ctx_path(0)), ("Log", icon::OPEN, ctx_path(1)), ("Data folder", icon::FOLDER, ctx_path(2))];
    let mut x = r.right();
    for (i, (label, glyph, path)) in labels.into_iter().enumerate().rev() {
        let bw = ui.button_width(label, true);
        x -= bw;
        if ui.button(&format!("set/file/{i}"), label, Some(glyph), Kind::Secondary, Rect::new(x, r.y, bw, 32.0)) {
            ui.actions.push(Action::Open(path));
        }
        x -= 8.0;
    }
    let _ = &st.loading;
}

fn ctx_path(i: usize) -> std::path::PathBuf {
    match i {
        0 => super::ctx().cfg.path.as_ref().clone(),
        1 => crate::paths::log_path(),
        _ => crate::paths::local_dir(),
    }
}
