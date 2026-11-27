//! Statistics database (`%LOCALAPPDATA%\chatty\stats.db`). A writer thread
//! owns the connection; recording a dictation never blocks the pipeline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::sqlite::Db;
use crate::text::Correction;

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS dictations(
    id INTEGER PRIMARY KEY,
    pressed_at INTEGER NOT NULL,      -- unix ms, key down
    local_day TEXT,                   -- YYYY-MM-DD in local time (DST-correct)
    local_time TEXT,                  -- YYYY-MM-DD HH:MM:SS
    local_hour INTEGER,
    local_weekday INTEGER,            -- 0 = Monday
    started_at INTEGER,               -- recording started (threshold)
    released_at INTEGER,
    pasted_at INTEGER,
    recording_ms INTEGER,             -- captured audio
    speech_ms INTEGER,                -- VAD speech
    silence_ratio REAL,
    words INTEGER, chars INTEGER,
    wpm_speech REAL,                  -- words / speech time
    wpm_total REAL,                   -- words / (key down -> paste)
    latency_ms REAL,                  -- release -> Ctrl+V sent
    transcribe_ms REAL,               -- compute after release
    compute_ms REAL,                  -- all model compute incl. streaming/preview
    rtf REAL,                         -- compute / audio
    mic_open_ms REAL, mic_start_ms REAL, model_load_ms REAL,
    drain_ms REAL, vad_ms REAL, post_ms REAL, cleanup_ms REAL,
    paste_wait_ms REAL, paste_ms REAL,
    model TEXT, backend TEXT, language TEXT, app TEXT,
    status TEXT NOT NULL,             -- ok | cancelled | empty | error | blocked
    error TEXT,
    raw_text TEXT, final_text TEXT,
    cleaned_text TEXT, cleanup_model TEXT, cleanup_used INTEGER, cleanup_note TEXT,
    cleanup_length_ratio REAL, cleanup_edit_ratio REAL,
    fillers_removed INTEGER, corrections INTEGER,
    audio_path TEXT
);
CREATE INDEX IF NOT EXISTS dictations_pressed ON dictations(pressed_at);
CREATE TABLE IF NOT EXISTS corrections(
    id INTEGER PRIMARY KEY, dictation_id INTEGER, at INTEGER,
    word TEXT, original TEXT, score REAL, required REAL
);
CREATE INDEX IF NOT EXISTS corrections_word ON corrections(word);
CREATE TABLE IF NOT EXISTS fillers(dictation_id INTEGER, at INTEGER, word TEXT);
CREATE TABLE IF NOT EXISTS enter_taps(
    id INTEGER PRIMARY KEY, at INTEGER, local_day TEXT, app TEXT, latency_ms REAL, waited_ms REAL
);
"#;

/// Local calendar fields of a unix-ms timestamp. Computed here rather than
/// in SQL: Windows' winsqlite3 returns NULL for the 'localtime' modifier.
#[derive(Debug, Clone, PartialEq)]
pub struct Local {
    pub day: String,
    pub time: String,
    pub hour: i64,
    /// 0 = Monday.
    pub weekday: i64,
}

pub fn local(epoch_ms: i64) -> Local {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME: 100 ns ticks since 1601-01-01.
    let ticks = (epoch_ms as u64 + 11_644_473_600_000) * 10_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut t = SYSTEMTIME::default();
    unsafe {
        let _ = FileTimeToSystemTime(&ft, &mut utc);
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut t).is_err() {
            t = utc;
        }
    }
    Local {
        day: format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay),
        time: format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond),
        hour: t.wHour as i64,
        weekday: (t.wDayOfWeek as i64 + 6) % 7,
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Days since 1970-01-01 of a civil date, and back (H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn parse_day(s: &str) -> Option<i64> {
    let mut it = s.split('-').map(|p| p.parse::<i64>().ok());
    Some(days_from_civil(it.next()??, it.next()??, it.next()??))
}

/// (today, Monday of this week, first of this month) as YYYY-MM-DD.
pub fn calendar(now: &Local) -> (String, String, String) {
    let today = parse_day(&now.day).unwrap_or(0);
    let (y, m, d) = civil_from_days(today - now.weekday);
    (now.day.clone(), format!("{y:04}-{m:02}-{d:02}"), format!("{}-01", &now.day[..7]))
}

pub fn epoch_ms(at: Instant) -> i64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    now - Instant::now().saturating_duration_since(at).as_millis() as i64
}

#[derive(Debug, Default, Clone)]
pub struct Dictation {
    pub pressed_at: i64,
    pub started_at: i64,
    pub released_at: Option<i64>,
    pub pasted_at: Option<i64>,
    pressed_instant: Option<Instant>,
    released_instant: Option<Instant>,
    pub recording_ms: i64,
    pub speech_ms: i64,
    pub words: i64,
    pub chars: i64,
    pub latency_ms: Option<f64>,
    pub total_ms: Option<f64>,
    pub transcribe_ms: f64,
    pub compute_ms: f64,
    pub mic_open_ms: f64,
    pub mic_start_ms: f64,
    pub model_load_ms: f64,
    pub drain_ms: f64,
    pub vad_ms: f64,
    pub post_ms: f64,
    pub cleanup_ms: f64,
    pub paste_wait_ms: f64,
    pub paste_ms: f64,
    pub model: String,
    pub backend: String,
    pub language: String,
    pub app: String,
    pub status: &'static str,
    pub error: Option<String>,
    pub raw_text: String,
    pub final_text: String,
    pub cleaned_text: Option<String>,
    pub cleanup_model: Option<String>,
    pub cleanup_used: Option<bool>,
    pub cleanup_note: Option<String>,
    pub cleanup_length_ratio: Option<f64>,
    pub cleanup_edit_ratio: Option<f64>,
    pub fillers: Vec<String>,
    pub corrections: Vec<Correction>,
}

impl Dictation {
    pub fn new(pressed_at: Instant, started_at: Instant) -> Self {
        Dictation {
            pressed_at: epoch_ms(pressed_at),
            started_at: epoch_ms(started_at),
            pressed_instant: Some(pressed_at),
            status: "ok",
            ..Default::default()
        }
    }

    pub fn set_released(&mut self, at: Instant) {
        self.released_at = Some(epoch_ms(at));
        self.released_instant = Some(at);
    }

    pub fn set_pasted(&mut self, at: Instant) {
        self.pasted_at = Some(epoch_ms(at));
        if let Some(r) = self.released_instant {
            self.latency_ms = Some(at.saturating_duration_since(r).as_secs_f64() * 1000.0);
        }
        if let Some(p) = self.pressed_instant {
            self.total_ms = Some(at.saturating_duration_since(p).as_secs_f64() * 1000.0);
        }
    }

    pub fn rtf(&self) -> f64 {
        if self.recording_ms > 0 { self.compute_ms / self.recording_ms as f64 } else { 0.0 }
    }

    fn wpm_speech(&self) -> Option<f64> {
        (self.speech_ms > 0 && self.words > 0).then(|| self.words as f64 / (self.speech_ms as f64 / 60_000.0))
    }

    fn wpm_total(&self) -> Option<f64> {
        self.total_ms.filter(|t| *t > 0.0 && self.words > 0).map(|t| self.words as f64 / (t / 60_000.0))
    }
}

enum Msg {
    Dictation(Box<Dictation>, Option<Vec<f32>>),
    Enter { at: i64, app: String, latency_ms: f64, waited_ms: f64 },
}

#[derive(Clone)]
pub struct Writer {
    tx: Sender<Msg>,
}

impl Writer {
    pub fn dictation(&self, d: Dictation, audio: Option<Vec<f32>>) {
        let _ = self.tx.send(Msg::Dictation(Box::new(d), audio));
    }

    pub fn enter(&self, at: Instant, app: String, latency_ms: f64, waited_ms: f64) {
        let _ = self.tx.send(Msg::Enter { at: epoch_ms(at), app, latency_ms, waited_ms });
    }
}

pub fn open() -> Result<Db, String> {
    let path = crate::paths::db_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let db = Db::open(&path)?;
    db.exec_batch(SCHEMA)?;
    Ok(db)
}

/// `history_size` is read per write so config changes apply.
pub fn spawn_writer(cfg: crate::config::Shared) -> Writer {
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::Builder::new()
        .name("stats".into())
        .spawn(move || {
            let db = match open() {
                Ok(db) => db,
                Err(e) => {
                    log::error!("stats database: {e}");
                    return;
                }
            };
            for msg in rx {
                let result = match msg {
                    Msg::Dictation(d, audio) => insert(&db, &d, audio, cfg.get().history_size),
                    Msg::Enter { at, app, latency_ms, waited_ms } => db
                        .execute(
                            "INSERT INTO enter_taps(at, local_day, app, latency_ms, waited_ms) VALUES (?, ?, ?, ?, ?)",
                            &[at.into(), local(at).day.into(), app.into(), latency_ms.into(), waited_ms.into()],
                        )
                        .map(|_| ()),
                };
                if let Err(e) = result {
                    log::error!("stats: {e}");
                }
            }
        })
        .expect("spawn stats writer");
    Writer { tx }
}

fn insert(db: &Db, d: &Dictation, audio: Option<Vec<f32>>, history: usize) -> Result<(), String> {
    let silence_ratio =
        (d.recording_ms > 0).then(|| 1.0 - (d.speech_ms as f64 / d.recording_ms as f64).clamp(0.0, 1.0));
    let lt = local(d.pressed_at);
    let id = db.execute(
        "INSERT INTO dictations(local_day, local_time, local_hour, local_weekday, pressed_at, started_at, released_at, pasted_at, recording_ms, speech_ms, silence_ratio,
            words, chars, wpm_speech, wpm_total, latency_ms, transcribe_ms, compute_ms, rtf,
            mic_open_ms, mic_start_ms, model_load_ms, drain_ms, vad_ms, post_ms, cleanup_ms, paste_wait_ms, paste_ms,
            model, backend, language, app, status, error, raw_text, final_text,
            cleaned_text, cleanup_model, cleanup_used, cleanup_note, cleanup_length_ratio, cleanup_edit_ratio,
            fillers_removed, corrections)
         VALUES (?,?,?,?, ?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,?,?,?,?,?, ?,?)",
        &[
            lt.day.into(),
            lt.time.into(),
            lt.hour.into(),
            lt.weekday.into(),
            d.pressed_at.into(),
            d.started_at.into(),
            d.released_at.into(),
            d.pasted_at.into(),
            d.recording_ms.into(),
            d.speech_ms.into(),
            silence_ratio.into(),
            d.words.into(),
            d.chars.into(),
            d.wpm_speech().into(),
            d.wpm_total().into(),
            d.latency_ms.into(),
            d.transcribe_ms.into(),
            d.compute_ms.into(),
            d.rtf().into(),
            d.mic_open_ms.into(),
            d.mic_start_ms.into(),
            d.model_load_ms.into(),
            d.drain_ms.into(),
            d.vad_ms.into(),
            d.post_ms.into(),
            d.cleanup_ms.into(),
            d.paste_wait_ms.into(),
            d.paste_ms.into(),
            d.model.as_str().into(),
            d.backend.as_str().into(),
            d.language.as_str().into(),
            d.app.as_str().into(),
            d.status.into(),
            d.error.clone().into(),
            d.raw_text.as_str().into(),
            d.final_text.as_str().into(),
            d.cleaned_text.clone().into(),
            d.cleanup_model.clone().into(),
            d.cleanup_used.map(|b| b as i64).into(),
            d.cleanup_note.clone().into(),
            d.cleanup_length_ratio.into(),
            d.cleanup_edit_ratio.into(),
            (d.fillers.len() as i64).into(),
            (d.corrections.len() as i64).into(),
        ],
    )?;
    for c in &d.corrections {
        db.execute(
            "INSERT INTO corrections(dictation_id, at, word, original, score, required) VALUES (?,?,?,?,?,?)",
            &[
                id.into(),
                d.pressed_at.into(),
                c.word.as_str().into(),
                c.original.as_str().into(),
                (c.score as f64).into(),
                (c.required as f64).into(),
            ],
        )?;
    }
    for f in &d.fillers {
        db.execute("INSERT INTO fillers(dictation_id, at, word) VALUES (?,?,?)", &[id.into(), d.pressed_at.into(), f.as_str().into()])?;
    }
    if let Some(pcm) = audio.filter(|a| !a.is_empty() && history > 0) {
        let dir = crate::paths::history_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{id}.wav"));
        crate::wav::write(&path, &pcm, crate::audio::RATE).map_err(|e| e.to_string())?;
        db.execute("UPDATE dictations SET audio_path = ? WHERE id = ?", &[path.to_string_lossy().as_ref().into(), id.into()])?;
    }
    // Keep audio for the newest `history` dictations only.
    let old = db.query(
        "SELECT id, audio_path FROM dictations WHERE audio_path IS NOT NULL ORDER BY id DESC LIMIT -1 OFFSET ?",
        &[(history as i64).into()],
    )?;
    for row in old {
        let _ = std::fs::remove_file(PathBuf::from(row[1].text()));
        db.execute("UPDATE dictations SET audio_path = NULL WHERE id = ?", &[row[0].clone()])?;
    }
    Ok(())
}

// ---------------------------------------------------------------- report --

#[derive(Debug, Default, Clone)]
pub struct Totals {
    pub today: i64,
    pub week: i64,
    pub month: i64,
    pub all: i64,
}

#[derive(Debug, Default, Clone)]
pub struct Breakdown {
    pub name: String,
    pub dictations: i64,
    pub words: i64,
}

#[derive(Debug, Default, Clone)]
pub struct ModelRow {
    pub model: String,
    pub backend: String,
    pub dictations: i64,
    pub words: i64,
    pub avg_latency: f64,
    pub p50_latency: f64,
    pub p95_latency: f64,
    pub avg_transcribe: f64,
    pub avg_rtf: f64,
    pub corrections_per_100_words: f64,
    pub avg_wpm: f64,
}

#[derive(Debug, Default, Clone)]
pub struct WordRow {
    pub word: String,
    pub fired: i64,
    pub avg_score: f64,
    pub min_score: f64,
    pub originals: String,
}

#[derive(Debug, Default, Clone)]
pub struct HistoryRow {
    pub at: String,
    pub app: String,
    pub model: String,
    pub status: String,
    pub latency_ms: Option<f64>,
    pub raw: String,
    pub final_text: String,
    pub cleaned: Option<String>,
    pub cleanup_used: Option<bool>,
    pub audio: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub words: Totals,
    pub dictations: Totals,
    pub streak_days: i64,
    pub longest_streak: i64,
    /// [weekday 0=Mon][hour] -> words
    pub heatmap: Vec<[i64; 24]>,
    pub avg_wpm: f64,
    pub best_wpm: f64,
    pub avg_wpm_total: f64,
    pub longest_ms: i64,
    pub longest_words: i64,
    pub vocabulary: usize,
    pub top_words: Vec<(String, i64)>,
    pub apps: Vec<Breakdown>,
    pub languages: Vec<Breakdown>,
    pub speaking_minutes: f64,
    pub typing_minutes: f64,
    pub saved_minutes: f64,
    pub models: Vec<ModelRow>,
    pub custom_words: Vec<WordRow>,
    pub cancelled: i64,
    pub empty: i64,
    pub errors: i64,
    pub blocked: i64,
    pub enter_taps: i64,
    pub enter_taps_today: i64,
    pub enter_latency: f64,
    pub avg_latency: f64,
    pub p95_latency: f64,
    pub avg_silence: f64,
    pub fillers_removed: i64,
    pub top_fillers: Vec<(String, i64)>,
    pub cleanup_runs: i64,
    pub cleanup_rejected: i64,
    pub cleanup_avg_ms: f64,
    pub history: Vec<HistoryRow>,
}

const OK: &str = "status = 'ok'";

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

/// Most common English and German function words, left out of "most used".
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "is", "it", "i", "that", "this", "for", "with", "be", "we",
    "you", "so", "but", "if", "at", "as", "are", "was", "not", "have", "do", "just", "can", "my", "me", "der", "die",
    "das", "und", "ist", "ich", "nicht", "zu", "es", "ein", "eine", "den", "mit", "auf", "für", "sie", "wir", "dass",
    "auch", "im", "von", "du", "dann", "also", "aber",
];

/// Consecutive-day streaks from sorted distinct "YYYY-MM-DD" days.
pub fn streaks(days: &[String], today: &str) -> (i64, i64) {
    fn ordinal(d: &str) -> Option<i64> {
        // Days since 0000-03-01 (civil calendar), enough to compare adjacency.
        let mut it = d.split('-').map(|p| p.parse::<i64>().ok());
        let (y, m, day) = (it.next()??, it.next()??, it.next()??);
        let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        Some(365 * y + y / 4 - y / 100 + y / 400 + (153 * m + 2) / 5 + day - 1)
    }
    let ords: Vec<i64> = days.iter().filter_map(|d| ordinal(d)).collect();
    let mut longest = 0;
    let mut run = 0;
    let mut prev: Option<i64> = None;
    for &o in &ords {
        run = if prev == Some(o - 1) { run + 1 } else { 1 };
        longest = longest.max(run);
        prev = Some(o);
    }
    // The current streak is alive if it reaches today or yesterday.
    let current = match (ords.last(), ordinal(today)) {
        (Some(&last), Some(t)) if t - last <= 1 => run,
        _ => 0,
    };
    (current, longest)
}

pub fn report(db: &Db, typing_wpm: f64, history: usize) -> Result<Report, String> {
    let mut r = Report::default();
    let (today, week_start, month_start) = calendar(&local(now_ms()));
    let totals = |col: &str| -> Result<Totals, String> {
        let q = |filter: &str, day: &str| -> Result<i64, String> {
            let sql = format!("SELECT coalesce(sum({col}), 0) FROM dictations WHERE {OK} {filter}");
            let params: Vec<crate::sqlite::Value> = if day.is_empty() { vec![] } else { vec![day.into()] };
            Ok(db.scalar(&sql, &params)?.int())
        };
        Ok(Totals {
            today: q("AND local_day = ?", &today)?,
            week: q("AND local_day >= ?", &week_start)?,
            month: q("AND local_day >= ?", &month_start)?,
            all: q("", "")?,
        })
    };
    r.words = totals("words")?;
    r.dictations = totals("1")?;

    let days: Vec<String> = db
        .query(&format!("SELECT DISTINCT local_day FROM dictations WHERE {OK} ORDER BY 1"), &[])?
        .into_iter()
        .map(|row| row[0].text())
        .collect();
    (r.streak_days, r.longest_streak) = streaks(&days, &today);

    r.heatmap = vec![[0; 24]; 7];
    for row in db.query(
        &format!("SELECT local_weekday, local_hour, sum(words) FROM dictations WHERE {OK} GROUP BY 1, 2"),
        &[],
    )? {
        let (d, h) = (row[0].int() as usize, row[1].int() as usize);
        if d < 7 && h < 24 {
            r.heatmap[d][h] = row[2].int();
        }
    }

    let row = db.query(
        &format!(
            "SELECT avg(wpm_speech), max(CASE WHEN speech_ms >= 3000 THEN wpm_speech END), avg(wpm_total),
                    max(recording_ms), coalesce(sum(speech_ms), 0), coalesce(sum(words), 0), avg(silence_ratio),
                    coalesce(sum(coalesce(latency_ms, 0) + recording_ms), 0)
             FROM dictations WHERE {OK}"
        ),
        &[],
    )?;
    if let Some(row) = row.first() {
        r.avg_wpm = row[0].real();
        r.best_wpm = row[1].real();
        r.avg_wpm_total = row[2].real();
        r.longest_ms = row[3].int();
        r.avg_silence = row[6].real();
        let words = row[5].real();
        r.speaking_minutes = row[7].real() / 60_000.0;
        r.typing_minutes = if typing_wpm > 0.0 { words / typing_wpm } else { 0.0 };
        r.saved_minutes = r.typing_minutes - r.speaking_minutes;
    }
    r.longest_words = db
        .scalar(&format!("SELECT words FROM dictations WHERE {OK} ORDER BY recording_ms DESC LIMIT 1"), &[])?
        .int();

    let mut counts: HashMap<String, i64> = HashMap::new();
    for row in db.query(&format!("SELECT final_text FROM dictations WHERE {OK}"), &[])? {
        for w in crate::text::words(&row[0].text()) {
            *counts.entry(w).or_default() += 1;
        }
    }
    r.vocabulary = counts.len();
    let mut top: Vec<(String, i64)> =
        counts.into_iter().filter(|(w, _)| !STOP_WORDS.contains(&w.as_str()) && w.chars().count() > 1).collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(30);
    r.top_words = top;

    let breakdown = |col: &str| -> Result<Vec<Breakdown>, String> {
        Ok(db
            .query(
                &format!(
                    "SELECT coalesce(nullif({col}, ''), '?'), count(*), coalesce(sum(words), 0) FROM dictations
                     WHERE {OK} GROUP BY 1 ORDER BY 3 DESC"
                ),
                &[],
            )?
            .into_iter()
            .map(|row| Breakdown { name: row[0].text(), dictations: row[1].int(), words: row[2].int() })
            .collect())
    };
    r.apps = breakdown("app")?;
    r.languages = breakdown("language")?;

    for row in db.query(
        &format!(
            "SELECT model, group_concat(DISTINCT backend), count(*), coalesce(sum(words), 0), avg(latency_ms),
                    avg(transcribe_ms), avg(rtf), coalesce(sum(corrections), 0), avg(wpm_speech)
             FROM dictations WHERE {OK} GROUP BY model ORDER BY 3 DESC"
        ),
        &[],
    )? {
        let model = row[0].text();
        let mut lat: Vec<f64> = db
            .query(
                &format!("SELECT latency_ms FROM dictations WHERE {OK} AND model = ? AND latency_ms IS NOT NULL ORDER BY 1"),
                &[model.as_str().into()],
            )?
            .into_iter()
            .map(|r| r[0].real())
            .collect();
        lat.sort_by(f64::total_cmp);
        let words = row[3].int();
        r.models.push(ModelRow {
            model,
            backend: row[1].text(),
            dictations: row[2].int(),
            words,
            avg_latency: row[4].real(),
            p50_latency: percentile(&lat, 0.5),
            p95_latency: percentile(&lat, 0.95),
            avg_transcribe: row[5].real(),
            avg_rtf: row[6].real(),
            corrections_per_100_words: if words > 0 { row[7].real() * 100.0 / words as f64 } else { 0.0 },
            avg_wpm: row[8].real(),
        });
    }

    for row in db.query(
        "SELECT word, count(*), avg(score), min(score), group_concat(DISTINCT original) FROM corrections
         GROUP BY word ORDER BY 2 DESC",
        &[],
    )? {
        r.custom_words.push(WordRow {
            word: row[0].text(),
            fired: row[1].int(),
            avg_score: row[2].real(),
            min_score: row[3].real(),
            originals: row[4].text(),
        });
    }

    let status = |s: &str| db.scalar("SELECT count(*) FROM dictations WHERE status = ?", &[s.into()]).map(|v| v.int());
    r.cancelled = status("cancelled")?;
    r.empty = status("empty")?;
    r.errors = status("error")?;
    r.blocked = status("blocked")?;

    r.enter_taps = db.scalar("SELECT count(*) FROM enter_taps", &[])?.int();
    r.enter_taps_today = db
        .scalar("SELECT count(*) FROM enter_taps WHERE local_day = ?", &[today.as_str().into()])?
        .int();
    r.enter_latency = db.scalar("SELECT avg(latency_ms) FROM enter_taps", &[])?.real();

    let mut lat: Vec<f64> = db
        .query(&format!("SELECT latency_ms FROM dictations WHERE {OK} AND latency_ms IS NOT NULL"), &[])?
        .into_iter()
        .map(|r| r[0].real())
        .collect();
    lat.sort_by(f64::total_cmp);
    r.avg_latency = if lat.is_empty() { 0.0 } else { lat.iter().sum::<f64>() / lat.len() as f64 };
    r.p95_latency = percentile(&lat, 0.95);

    r.fillers_removed = db.scalar("SELECT count(*) FROM fillers", &[])?.int();
    r.top_fillers = db
        .query("SELECT word, count(*) FROM fillers GROUP BY word ORDER BY 2 DESC LIMIT 10", &[])?
        .into_iter()
        .map(|row| (row[0].text(), row[1].int()))
        .collect();

    if let Some(row) = db
        .query(
            "SELECT count(*), coalesce(sum(cleanup_used = 0), 0), avg(cleanup_ms) FROM dictations WHERE cleanup_model IS NOT NULL",
            &[],
        )?
        .first()
    {
        r.cleanup_runs = row[0].int();
        r.cleanup_rejected = row[1].int();
        r.cleanup_avg_ms = row[2].real();
    }

    for row in db.query(
        "SELECT id, local_time, app, model, status, latency_ms,
                raw_text, final_text, cleaned_text, cleanup_used, audio_path
         FROM dictations ORDER BY id DESC LIMIT ?",
        &[(history as i64).into()],
    )? {
        r.history.push(HistoryRow {
            at: row[1].text(),
            app: row[2].text(),
            model: row[3].text(),
            status: row[4].text(),
            latency_ms: (!row[5].is_null()).then(|| row[5].real()),
            raw: row[6].text(),
            final_text: row[7].text(),
            cleaned: (!row[8].is_null()).then(|| row[8].text()),
            cleanup_used: (!row[9].is_null()).then(|| row[9].int() != 0),
            audio: (!row[10].is_null()).then(|| row[10].text()),
        });
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_math() {
        for (y, m, d) in [(1970, 1, 1), (2024, 2, 29), (2026, 11, 27), (2000, 3, 1)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        // 2026-11-27 is a Friday (weekday 4 counting from Monday).
        let now = Local { day: "2026-11-27".into(), time: String::new(), hour: 12, weekday: 4 };
        assert_eq!(calendar(&now), ("2026-11-27".into(), "2026-11-23".into(), "2026-11-01".into()));
        let monday = Local { day: "2026-12-01".into(), time: String::new(), hour: 0, weekday: 1 };
        assert_eq!(calendar(&monday).1, "2026-11-30");
        // 2026-11-27 12:00 UTC.
        let l = local(1_795_780_800_000);
        assert!(l.day == "2026-11-27" || l.day == "2026-11-28" || l.day == "2026-11-26");
    }

    #[test]
    fn streak_counting() {
        let d = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(streaks(&d(&["2026-11-25", "2026-11-26", "2026-11-27"]), "2026-11-27"), (3, 3));
        assert_eq!(streaks(&d(&["2026-11-25", "2026-11-26"]), "2026-11-27"), (2, 2));
        assert_eq!(streaks(&d(&["2026-11-20", "2026-11-21", "2026-11-22", "2026-11-26"]), "2026-11-28"), (0, 3));
        // Month and leap-year boundaries.
        assert_eq!(streaks(&d(&["2024-02-28", "2024-02-29", "2024-03-01"]), "2024-03-01"), (3, 3));
        assert_eq!(streaks(&d(&["2026-12-31", "2027-01-01"]), "2027-01-01"), (2, 2));
        assert_eq!(streaks(&[], "2026-11-27"), (0, 0));
    }

    #[test]
    fn report_on_synthetic_data() {
        let path = std::env::temp_dir().join(format!("chatty-stats-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        db.exec_batch(SCHEMA).unwrap();
        let t0 = Instant::now();
        for i in 0..5 {
            let mut d = Dictation::new(t0, t0);
            d.recording_ms = 4000;
            d.speech_ms = 3000;
            d.words = 10 + i;
            d.final_text = "Nuxt and Tailwind and Vite".into();
            d.model = if i % 2 == 0 { "a".into() } else { "b".into() };
            d.app = "Code.exe".into();
            d.language = "en".into();
            d.latency_ms = Some(80.0 + i as f64);
            d.total_ms = Some(5000.0);
            d.corrections = vec![Correction { word: "Nuxt".into(), original: "nuxt".into(), score: 1.0, required: 1.0 }];
            d.fillers = vec!["um".into()];
            insert(&db, &d, None, 0).unwrap();
        }
        let mut c = Dictation::new(t0, t0);
        c.status = "cancelled";
        insert(&db, &c, None, 0).unwrap();
        let r = report(&db, 40.0, 10).unwrap();
        assert_eq!(r.words.all, 10 + 11 + 12 + 13 + 14);
        assert_eq!(r.words.today, r.words.all);
        assert_eq!(r.dictations.all, 5);
        assert_eq!(r.cancelled, 1);
        assert_eq!(r.streak_days, 1);
        assert_eq!(r.models.len(), 2);
        assert_eq!(r.custom_words[0].fired, 5);
        assert_eq!(r.apps[0].name, "Code.exe");
        assert_eq!(r.top_words[0].1, 5);
        assert_eq!(r.vocabulary, 4);
        assert!((r.avg_wpm - (12.0 / 0.05)).abs() < 1e-6);
        assert_eq!(r.history.len(), 6);
        assert_eq!(r.heatmap.iter().map(|d| d.iter().sum::<i64>()).sum::<i64>(), 60);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
