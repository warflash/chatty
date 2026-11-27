//! Statistics window. egui on its own thread, started on first open. winit
//! allows one event loop per process, so closing hides the window and the
//! next open shows it again; while hidden the loop sleeps (no CPU).

use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui::{self, Color32, RichText, Sense, Vec2};

use crate::config;
use crate::stats::{self, Report};

static CTX: OnceLock<egui::Context> = OnceLock::new();
static OPENING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn open(cfg: config::Shared) {
    if let Some(ctx) = CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
        return;
    }
    if OPENING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("stats-ui".into())
        .spawn(move || {
            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_title("chatty statistics")
                    .with_inner_size([1100.0, 800.0])
                    .with_min_inner_size([700.0, 400.0]),
                event_loop_builder: Some(Box::new(|b| {
                    use winit::platform::windows::EventLoopBuilderExtWindows;
                    b.with_any_thread(true);
                })),
                ..Default::default()
            };
            let result = eframe::run_native(
                "chatty-stats",
                options,
                Box::new(move |cc| {
                    let _ = CTX.set(cc.egui_ctx.clone());
                    Ok(Box::new(StatsApp::new(cfg, cc.egui_ctx.clone())))
                }),
            );
            if let Err(e) = result {
                log::error!("stats window: {e}");
            }
        })
        .expect("spawn stats window");
}

struct StatsApp {
    cfg: config::Shared,
    report: Arc<Mutex<Option<Result<Report, String>>>>,
    loading: Arc<std::sync::atomic::AtomicBool>,
    was_visible: bool,
    ctx: egui::Context,
}

impl StatsApp {
    fn new(cfg: config::Shared, ctx: egui::Context) -> Self {
        let app = StatsApp {
            cfg,
            report: Arc::new(Mutex::new(None)),
            loading: Arc::new(false.into()),
            was_visible: true,
            ctx,
        };
        app.refresh();
        app
    }

    fn refresh(&self) {
        if self.loading.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let slot = self.report.clone();
        let loading = self.loading.clone();
        let ctx = self.ctx.clone();
        let cfg = self.cfg.get();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let r = stats::open().and_then(|db| stats::report(&db, cfg.typing_wpm as f64, cfg.history_size.max(20)));
            log::info!("stats report in {:?}", t.elapsed());
            *slot.lock().unwrap() = Some(r);
            loading.store(false, std::sync::atomic::Ordering::SeqCst);
            ctx.request_repaint();
        });
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

fn tile(ui: &mut egui::Ui, label: &str, value: String, sub: Option<String>) {
    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
        ui.set_min_width(150.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(label).small().weak());
            ui.label(RichText::new(value).size(22.0).strong());
            if let Some(s) = sub {
                ui.label(RichText::new(s).small().weak());
            }
        });
    });
}

fn heatmap(ui: &mut egui::Ui, data: &[[i64; 24]]) {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let max = data.iter().flat_map(|d| d.iter()).copied().max().unwrap_or(0).max(1) as f32;
    let cell = Vec2::new(26.0, 18.0);
    ui.horizontal(|ui| {
        ui.add_space(36.0);
        for h in 0..24 {
            ui.allocate_ui(Vec2::new(cell.x, 14.0), |ui| {
                if h % 3 == 0 {
                    ui.label(RichText::new(format!("{h:02}")).small().weak());
                }
            });
        }
    });
    for (d, row) in data.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.allocate_ui(Vec2::new(32.0, cell.y), |ui| ui.label(RichText::new(DAYS[d]).small()));
            for (h, &v) in row.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(cell - Vec2::splat(2.0), Sense::hover());
                let t = (v as f32 / max).sqrt();
                let color = if v == 0 {
                    ui.visuals().faint_bg_color
                } else {
                    Color32::from_rgb((40.0 + 30.0 * t) as u8, (80.0 + 120.0 * t) as u8, (90.0 + 160.0 * t) as u8)
                };
                ui.painter().rect_filled(rect, 3.0, color);
                resp.on_hover_text(format!("{} {:02}:00 – {} words", DAYS[d], h, fmt_int(v)));
            }
        });
    }
}

fn bar_table(ui: &mut egui::Ui, id: &str, rows: &[stats::Breakdown]) {
    let max = rows.iter().map(|r| r.words).max().unwrap_or(1).max(1) as f32;
    egui::Grid::new(id).striped(true).num_columns(4).show(ui, |ui| {
        ui.label(RichText::new("").strong());
        ui.label(RichText::new("words").strong());
        ui.label(RichText::new("dictations").strong());
        ui.label("");
        ui.end_row();
        for r in rows.iter().take(15) {
            ui.label(&r.name);
            ui.label(fmt_int(r.words));
            ui.label(fmt_int(r.dictations));
            let (rect, _) = ui.allocate_exact_size(Vec2::new(160.0 * r.words as f32 / max, 10.0), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(70, 140, 200));
            ui.end_row();
        }
    });
}

fn play(path: &str) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    unsafe {
        let _ = PlaySoundW(&windows::core::HSTRING::from(path), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.heading(title);
    ui.separator();
}

fn show_report(ui: &mut egui::Ui, r: &Report) {
    ui.horizontal_wrapped(|ui| {
        tile(ui, "Words today", fmt_int(r.words.today), Some(format!("{} dictations", r.dictations.today)));
        tile(ui, "This week", fmt_int(r.words.week), Some(format!("{} dictations", r.dictations.week)));
        tile(ui, "This month", fmt_int(r.words.month), Some(format!("{} dictations", r.dictations.month)));
        tile(ui, "All time", fmt_int(r.words.all), Some(format!("{} dictations", r.dictations.all)));
        tile(ui, "Streak", format!("{} days", r.streak_days), Some(format!("longest {} days", r.longest_streak)));
        tile(
            ui,
            "Time saved",
            fmt_minutes(r.saved_minutes),
            Some(format!("{} typing vs {} speaking", fmt_minutes(r.typing_minutes), fmt_minutes(r.speaking_minutes))),
        );
        tile(ui, "Average WPM", format!("{:.0}", r.avg_wpm), Some(format!("{:.0} incl. pauses & latency", r.avg_wpm_total)));
        tile(ui, "Best WPM", format!("{:.0}", r.best_wpm), Some("≥ 3 s of speech".into()));
        tile(ui, "Release → paste", format!("{:.0} ms", r.avg_latency), Some(format!("p95 {:.0} ms", r.p95_latency)));
        tile(
            ui,
            "Enter taps",
            fmt_int(r.enter_taps),
            Some(format!("{} today · {:.0} ms tap → Enter", r.enter_taps_today, r.enter_latency)),
        );
        tile(
            ui,
            "Longest dictation",
            format!("{:.1} s", r.longest_ms as f64 / 1000.0),
            Some(format!("{} words", r.longest_words)),
        );
        tile(ui, "Vocabulary", fmt_int(r.vocabulary as i64), Some("distinct words".into()));
    });

    section(ui, "When you dictate (words by weekday and hour)");
    heatmap(ui, &r.heatmap);

    ui.columns(2, |cols| {
        section(&mut cols[0], "By app");
        bar_table(&mut cols[0], "apps", &r.apps);
        section(&mut cols[1], "By language");
        bar_table(&mut cols[1], "langs", &r.languages);
    });

    section(ui, "Models");
    egui::Grid::new("models").striped(true).show(ui, |ui| {
        for h in ["model", "backend", "dictations", "words", "latency avg", "p50", "p95", "transcribe", "RTF", "corrections/100 words", "WPM"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        for m in &r.models {
            ui.label(&m.model);
            ui.label(&m.backend);
            ui.label(fmt_int(m.dictations));
            ui.label(fmt_int(m.words));
            ui.label(format!("{:.0} ms", m.avg_latency));
            ui.label(format!("{:.0} ms", m.p50_latency));
            ui.label(format!("{:.0} ms", m.p95_latency));
            ui.label(format!("{:.0} ms", m.avg_transcribe));
            ui.label(format!("{:.3}", m.avg_rtf));
            ui.label(format!("{:.2}", m.corrections_per_100_words));
            ui.label(format!("{:.0}", m.avg_wpm));
            ui.end_row();
        }
    });

    section(ui, "Custom words (how often each fired; low scores hint at false matches)");
    if r.custom_words.is_empty() {
        ui.label(RichText::new("No corrections yet.").weak());
    }
    egui::Grid::new("words").striped(true).show(ui, |ui| {
        for h in ["word", "fired", "avg score", "min score", "heard as"] {
            ui.label(RichText::new(h).strong());
        }
        ui.end_row();
        for w in &r.custom_words {
            ui.label(&w.word);
            ui.label(fmt_int(w.fired));
            ui.label(format!("{:.2}", w.avg_score));
            let min = RichText::new(format!("{:.2}", w.min_score));
            ui.label(if w.min_score < 0.9 { min.color(Color32::from_rgb(230, 160, 60)) } else { min });
            ui.label(&w.originals);
            ui.end_row();
        }
    });

    ui.columns(2, |cols| {
        section(&mut cols[0], "Most used words");
        egui::Grid::new("top").striped(true).show(&mut cols[0], |ui| {
            for (w, n) in r.top_words.iter().take(20) {
                ui.label(w);
                ui.label(fmt_int(*n));
                ui.end_row();
            }
        });
        section(&mut cols[1], "Quality");
        egui::Grid::new("quality").striped(true).show(&mut cols[1], |ui| {
            let rows = [
                ("Cancelled", fmt_int(r.cancelled)),
                ("Empty (no speech)", fmt_int(r.empty)),
                ("Errors", fmt_int(r.errors)),
                ("Blocked (elevated window)", fmt_int(r.blocked)),
                ("Average silence", format!("{:.0}%", r.avg_silence * 100.0)),
                ("Fillers removed", fmt_int(r.fillers_removed)),
                (
                    "Top fillers",
                    r.top_fillers.iter().map(|(w, n)| format!("{w} ({n})")).collect::<Vec<_>>().join(", "),
                ),
                ("Cleanup runs", fmt_int(r.cleanup_runs)),
                ("Cleanup rejected (drift)", fmt_int(r.cleanup_rejected)),
                ("Cleanup avg time", format!("{:.0} ms", r.cleanup_avg_ms)),
            ];
            for (k, v) in rows {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            }
        });
    });

    section(ui, "Recent dictations");
    for h in &r.history {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&h.at).strong());
                ui.label(RichText::new(format!("{} · {} · {}", h.app, h.model, h.status)).weak());
                if let Some(l) = h.latency_ms {
                    ui.label(RichText::new(format!("{l:.0} ms")).weak());
                }
                if let Some(a) = &h.audio {
                    if ui.small_button("▶ audio").clicked() {
                        play(a);
                    }
                }
                if !h.final_text.is_empty() && ui.small_button("copy").clicked() {
                    ui.ctx().copy_text(h.final_text.clone());
                }
            });
            if !h.final_text.is_empty() {
                ui.label(&h.final_text);
            }
            if h.raw != h.final_text && !h.raw.is_empty() {
                ui.label(RichText::new(format!("raw: {}", h.raw)).small().weak());
            }
            if let Some(c) = &h.cleaned {
                let used = if h.cleanup_used == Some(true) { "used" } else { "rejected" };
                ui.label(RichText::new(format!("cleanup ({used}): {c}")).small().weak());
            }
        });
    }
}

impl eframe::App for StatsApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.was_visible = false;
            return;
        }
        let visible = ctx.input(|i| i.viewport().inner_rect.is_some());
        if visible && !self.was_visible {
            self.refresh();
        }
        self.was_visible = visible;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.was_visible {
            self.was_visible = true;
            self.refresh();
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("chatty");
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
                if self.loading.load(std::sync::atomic::Ordering::SeqCst) {
                    ui.spinner();
                }
            });
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let guard = self.report.lock().unwrap();
                match guard.as_ref() {
                    None => {
                        ui.label("Loading…");
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::RED, e);
                    }
                    Some(Ok(r)) => show_report(ui, r),
                }
            });
        });
    }
}
