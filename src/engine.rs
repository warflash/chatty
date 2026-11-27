//! The engine thread: owns the speech model, the VAD and the cleanup model,
//! drains the microphone ring while recording, streams or previews, then
//! runs the text pipeline and pastes. It reports "finished" to the
//! coordinator only after the paste (and clipboard restore) completed, so an
//! Enter tap always lands after the text.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use transcribe_cpp::{
    Backend, CancelToken, Feature, Model, ModelOptions, ParakeetBufferedStreamOptions, ParakeetStreamOptions,
    RunOptions, Session, StreamExtension, StreamOptions, VadSession, VoxtralRealtimeStreamOptions,
};

use crate::audio::{self, Capture, Resampler};
use crate::cleanup::Cleaner;
use crate::config::{self, Config};
use crate::coordinator::Input;
use crate::hook::KeyEvent;
use crate::models::{self, AsrModel, Streaming};
use crate::overlay;
use crate::stats::{self, Dictation};
use crate::text;
use crate::tray::{self, TrayState};

pub enum Cmd {
    Begin { pressed_at: Instant, started_at: Instant },
    Finish { released_at: Instant },
    Cancel,
    Config(Arc<Config>),
    Downloaded { id: &'static str, ok: bool },
    /// Fetch a model without switching to it (from the Models page).
    Download(&'static str),
    /// Load (or download) what the config selects.
    Warm,
}

#[derive(Clone)]
pub struct Handle {
    tx: Sender<Cmd>,
    /// Cancelled by the coordinator the moment the key is released, so a
    /// running preview never delays the final transcript.
    pub preview_cancel: CancelToken,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

struct Loaded {
    spec: &'static AsrModel,
    _model: Model,
    session: Session,
    backend: String,
    vocabulary: bool,
}

struct Worker {
    rx: Receiver<Cmd>,
    self_tx: Sender<Cmd>,
    cfg: config::Shared,
    coord: Sender<KeyEvent>,
    stats: stats::Writer,
    audio: audio::Control,
    capture: Capture,
    asr: Option<Loaded>,
    vad: Option<(Model, VadSession)>,
    cleaner: Option<Cleaner>,
    preview_cancel: CancelToken,
    downloading: HashSet<&'static str>,
    last_used: Instant,
    /// Model load time to attribute to the next dictation (cold start).
    pending_load_ms: Option<f64>,
}

pub fn spawn(
    cfg: config::Shared,
    coord: Sender<KeyEvent>,
    stats: stats::Writer,
    audio: audio::Control,
    capture: Capture,
) -> Handle {
    let (tx, rx) = mpsc::channel();
    let preview_cancel = CancelToken::new();
    let handle = Handle { tx: tx.clone(), preview_cancel: preview_cancel.clone() };
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            let mut w = Worker {
                rx,
                self_tx: tx,
                cfg,
                coord,
                stats,
                audio,
                capture,
                asr: None,
                vad: None,
                cleaner: None,
                preview_cancel,
                downloading: HashSet::new(),
                last_used: Instant::now(),
                pending_load_ms: None,
            };
            w.run();
        })
        .expect("spawn engine");
    handle
}

/// What ended a recording.
enum End {
    Finish(Instant),
    Cancel,
}

/// Drains the ring, resamples to 16 kHz and feeds the VAD.
struct Pump {
    resampler: Resampler,
    raw: Vec<f32>,
    pcm: Vec<f32>,
    vad_probs: Vec<f32>,
    vad_ms: f64,
}

impl Pump {
    fn new(rate: u32) -> Self {
        Pump {
            resampler: Resampler::new(rate),
            raw: Vec::with_capacity(8192),
            pcm: Vec::with_capacity(audio::RATE as usize * 30),
            vad_probs: Vec::new(),
            vad_ms: 0.0,
        }
    }

    /// Returns the index where the new samples start.
    fn pull(&mut self, capture: &mut Capture, vad: Option<&mut VadSession>, flush: bool) -> usize {
        self.raw.clear();
        capture.drain(&mut self.raw);
        let start = self.pcm.len();
        self.resampler.process(&self.raw, &mut self.pcm);
        if !flush && self.pcm.len() > start {
            let new = &self.pcm[start..];
            let rms = (new.iter().map(|s| s * s).sum::<f32>() / new.len() as f32).sqrt();
            crate::status::set_level(meter_level(rms, new.len() as f32 / audio::RATE as f32));
        }
        if flush {
            self.resampler.flush(&mut self.pcm);
        }
        if let Some(v) = vad {
            let t = Instant::now();
            if self.pcm.len() > start {
                if let Ok(r) = v.stream_feed(&self.pcm[start..]) {
                    self.vad_probs.extend_from_slice(&r.probs);
                }
            }
            if flush {
                if let Ok(r) = v.stream_flush() {
                    self.vad_probs.extend_from_slice(&r.probs);
                }
            }
            self.vad_ms += t.elapsed().as_secs_f64() * 1000.0;
        }
        start
    }

    fn heard_speech(&self, threshold: f32) -> bool {
        self.vad_probs.iter().any(|p| *p >= threshold)
    }
}

/// Speech statistics from Silero frame probabilities (512 samples = 32 ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speech {
    pub speech_ms: i64,
    /// Sample range covering the speech, padded.
    pub range: Option<(usize, usize)>,
}

pub fn analyze_speech(probs: &[f32], threshold: f32, total_samples: usize) -> Speech {
    const FRAME: usize = 512;
    let speech_frames = probs.iter().filter(|p| **p >= threshold).count();
    let first = probs.iter().position(|p| *p >= threshold);
    let last = probs.iter().rposition(|p| *p >= threshold);
    // Fewer than three speech frames (~100 ms) is a click or a breath.
    let range = match (first, last) {
        (Some(a), Some(b)) if speech_frames >= 3 => {
            let pad_before = 200 * audio::RATE as usize / 1000;
            let pad_after = 300 * audio::RATE as usize / 1000;
            Some(((a * FRAME).saturating_sub(pad_before), ((b + 1) * FRAME + pad_after).min(total_samples)))
        }
        _ => None,
    };
    Speech { speech_ms: (speech_frames * FRAME * 1000 / audio::RATE as usize) as i64, range }
}

fn stream_options(spec: &AsrModel) -> StreamOptions {
    let family = match spec.streaming {
        Streaming::No => None,
        Streaming::Buffered { left_ms, chunk_ms, right_ms } => {
            Some(StreamExtension::ParakeetBuffered(ParakeetBufferedStreamOptions {
                left_ms: Some(left_ms),
                chunk_ms: Some(chunk_ms),
                right_ms: Some(right_ms),
            }))
        }
        Streaming::CacheAware { att_right } => Some(StreamExtension::ParakeetStream(ParakeetStreamOptions {
            att_context_right: Some(att_right),
        })),
        Streaming::Voxtral { delay_tokens, min_decode_interval_ms } => {
            Some(StreamExtension::VoxtralRealtime(VoxtralRealtimeStreamOptions {
                num_delay_tokens: Some(delay_tokens),
                min_decode_interval_ms: Some(min_decode_interval_ms),
            }))
        }
    };
    StreamOptions { family, ..Default::default() }
}

/// First compute allocates buffers and compiles GPU pipelines (seconds on
/// Vulkan). Run both paths once on a few seconds of low noise so the first
/// dictation does not pay for it.
pub fn warm_up(session: &mut Session, spec: &AsrModel, run: &RunOptions) {
    let mut seed = 0x2545_f491u32;
    let noise: Vec<f32> = (0..audio::RATE as usize * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32 - 0.5) * 0.02
        })
        .collect();
    if let Err(e) = session.run(&noise, run) {
        log::debug!("warm-up run: {e}");
    }
    if spec.is_streaming() {
        if let Ok(mut s) = session.stream(run, &stream_options(spec)) {
            for chunk in noise.chunks(320) {
                let _ = s.feed(chunk);
            }
            let _ = s.finalize();
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Waits up to `timeout` for a command that ends the recording; other
/// commands are kept for later.
fn wait(rx: &Receiver<Cmd>, timeout: Duration, deferred: &mut Vec<Cmd>) -> Option<End> {
    match rx.recv_timeout(timeout) {
        Ok(Cmd::Finish { released_at }) => Some(End::Finish(released_at)),
        Ok(Cmd::Cancel) => Some(End::Cancel),
        Ok(Cmd::Begin { .. }) => None,
        Ok(other) => {
            deferred.push(other);
            None
        }
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(End::Cancel),
    }
}

/// Transcription result of one recording.
struct Heard {
    text: String,
    language: Option<String>,
    /// Compute after the key was released (stream finalize or offline run).
    final_ms: f64,
    /// All model compute, including streaming feeds / previews.
    compute_ms: f64,
}

enum Outcome {
    Cancelled,
    Empty { released_at: Instant },
    Heard { released_at: Instant, heard: Heard, drained_at: Instant },
}

struct Parts<'a> {
    rx: &'a Receiver<Cmd>,
    capture: &'a mut Capture,
    audio: &'a audio::Control,
    vad: Option<&'a mut VadSession>,
    pump: &'a mut Pump,
    deferred: &'a mut Vec<Cmd>,
    cfg: &'a Config,
}

const TICK: Duration = Duration::from_millis(20);

fn record_streaming(session: &mut Session, spec: &AsrModel, run: &RunOptions, p: Parts) -> Result<Outcome, String> {
    let Parts { rx, capture, audio, mut vad, pump, deferred, .. } = p;
    let mut stream = session.stream(run, &stream_options(spec)).map_err(|e| format!("stream: {e}"))?;
    let mut shown = String::new();
    let mut compute = 0.0;
    let end = loop {
        if let Some(end) = wait(rx, TICK, deferred) {
            break end;
        }
        let start = pump.pull(capture, vad.as_deref_mut(), false);
        if pump.pcm.len() > start {
            let t = Instant::now();
            let update = stream.feed(&pump.pcm[start..]).map_err(|e| format!("stream feed: {e}"))?;
            compute += ms(t.elapsed());
            if update.result_changed {
                let text = stream.text().display();
                if text != shown {
                    overlay::live(&text, "");
                    shown = text;
                }
            }
        }
    };
    let released_at = match end {
        End::Cancel => {
            audio.stop();
            return Ok(Outcome::Cancelled);
        }
        End::Finish(t) => t,
    };
    audio.stop();
    let start = pump.pull(capture, vad, true);
    let drained_at = Instant::now();
    let t = Instant::now();
    if pump.pcm.len() > start {
        stream.feed(&pump.pcm[start..]).map_err(|e| format!("stream feed: {e}"))?;
    }
    stream.finalize().map_err(|e| format!("stream finalize: {e}"))?;
    let final_ms = ms(t.elapsed());
    let text = stream.text();
    let text = if text.full.trim().is_empty() { text.display() } else { text.full };
    let language = stream.snapshot().language;
    Ok(Outcome::Heard {
        released_at,
        heard: Heard { text, language, final_ms, compute_ms: compute + final_ms },
        drained_at,
    })
}

/// Loudest recent level in dBFS, kept across recordings so the meter
/// remembers the microphone. Stored as f32 bits.
static METER_PEAK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xC220_0000); // -40.0

/// Level meter 0..1 on a dB scale relative to the loudest recent speech:
/// the peak maps to 1, `RANGE` dB below it to 0. The peak follows louder
/// input at once and falls back 1.5 dB/s, but never below -52 dBFS, so
/// silence stays at 0 even on a quiet microphone.
fn meter_level(rms: f32, secs: f32) -> f32 {
    use std::sync::atomic::Ordering;
    const RANGE: f32 = 26.0;
    const FLOOR: f32 = -52.0;
    const RELEASE_DB_PER_S: f32 = 1.5;
    let db = 20.0 * rms.max(1e-6).log10();
    let mut peak = f32::from_bits(METER_PEAK.load(Ordering::Relaxed));
    peak = if db > peak { db } else { (peak - RELEASE_DB_PER_S * secs).max(FLOOR) };
    METER_PEAK.store(peak.to_bits(), Ordering::Relaxed);
    ((db - (peak - RANGE)) / RANGE).clamp(0.0, 1.0)
}

const VAD_FRAME: usize = 512;
/// Segments are closed at a pause once they are at least this long...
const MIN_SEGMENT: usize = audio::RATE as usize * 4;
/// ...and forced closed at the quietest point beyond this.
const MAX_SEGMENT: usize = audio::RATE as usize * 20;
/// Pause that closes a segment: 10 frames = 320 ms without speech.
const PAUSE_FRAMES: usize = 10;
/// Speech up to this long gets one full-context final pass.
const FULL_PASS_MAX: usize = audio::RATE as usize * 40;

/// Speech range inside samples [a, b), padded, or None if it holds no speech.
fn speech_in(probs: &[f32], threshold: f32, a: usize, b: usize) -> Option<(usize, usize)> {
    let fa = a / VAD_FRAME;
    let fb = b.div_ceil(VAD_FRAME).min(probs.len());
    if fa >= fb {
        return None;
    }
    let base = fa * VAD_FRAME;
    analyze_speech(&probs[fa..fb], threshold, b - base).range.map(|(x, y)| (base + x, base + y))
}

/// Full-context transcription in segments.
///
/// While recording, the current segment is re-transcribed every
/// `preview_interval_ms` over all of its audio, so the preview corrects
/// itself as context arrives. At a pause (or after 20 s) the segment is
/// finalized in the background and becomes fixed text. On release only the
/// last segment still needs a pass. Every pass sees only the VAD-detected
/// speech, so noise before the first word (mic start, button click) is not
/// transcribed.
/// Segment state for full-context decoding (shared with `chatty --bench`).
#[derive(Default)]
pub struct Segmenter {
    pub committed: Vec<String>,
    pub language: Option<String>,
    seg_start: usize,
    last_len: usize,
    pub current: String,
    pub compute_ms: f64,
    /// Number of segments finalized while recording.
    pub closed: usize,
}

impl Segmenter {
    /// Committed text plus the live tail.
    pub fn text(&self) -> String {
        let mut t = self.committed.join(" ");
        if !self.current.is_empty() {
            if !t.is_empty() {
                t.push(' ');
            }
            t.push_str(&self.current);
        }
        t
    }

    /// One recording tick: close the segment at a pause (or when it gets
    /// long) and, when `preview_due`, re-transcribe the open segment.
    /// Returns true when the visible text changed.
    pub fn step(&mut self, session: &mut Session, run: &RunOptions, pcm: &[f32], probs: &[f32], thr: f32, preview_due: bool) -> bool {
        let mut changed = false;
        let frames = probs.len();
        let seg_len = (frames * VAD_FRAME).saturating_sub(self.seg_start);
        let paused = frames >= PAUSE_FRAMES && probs[frames - PAUSE_FRAMES..].iter().all(|p| *p < thr);
        if seg_len >= MIN_SEGMENT && (paused || seg_len >= MAX_SEGMENT) {
            let cut_frame = if paused {
                frames - PAUSE_FRAMES / 2
            } else {
                let from = frames.saturating_sub(62).max(self.seg_start / VAD_FRAME + 1);
                (from..frames).min_by(|a, b| probs[*a].total_cmp(&probs[*b])).unwrap_or(frames)
            };
            let cut = (cut_frame * VAD_FRAME).min(pcm.len());
            match speech_in(probs, thr, self.seg_start, cut) {
                Some((a, b)) => {
                    let t = Instant::now();
                    // An aborted run (key released) keeps the audio for the final pass.
                    if let Ok(tr) = session.run(&pcm[a..b], run) {
                        let text = tr.text.trim().to_string();
                        if !text.is_empty() {
                            self.committed.push(text);
                        }
                        self.language = tr.language.or(self.language.take());
                        self.seg_start = cut;
                        self.current.clear();
                        self.closed += 1;
                        changed = true;
                    }
                    self.compute_ms += ms(t.elapsed());
                }
                None => self.seg_start = cut,
            }
            self.last_len = 0;
        }
        if preview_due && pcm.len().saturating_sub(self.last_len) >= audio::RATE as usize / 5 {
            if let Some((a, b)) = speech_in(probs, thr, self.seg_start, pcm.len()) {
                let t = Instant::now();
                if let Ok(tr) = session.run(&pcm[a..b], run) {
                    let text = tr.text.trim().to_string();
                    if text != self.current {
                        self.current = text;
                        changed = true;
                    }
                }
                self.compute_ms += ms(t.elapsed());
            }
            self.last_len = pcm.len();
        }
        changed
    }

    /// The final pass. Up to `FULL_PASS_MAX` of speech it re-transcribes all
    /// of it in one run: one context, so no sentence breaks at segment
    /// boundaries (~5 ms per second of audio on a 3070). Longer recordings
    /// keep the segments finalized while recording and only finish the last
    /// one. Returns the compute time.
    pub fn finish(&mut self, session: &mut Session, run: &RunOptions, pcm: &[f32], probs: &[f32], thr: f32) -> Result<f64, String> {
        let t = Instant::now();
        self.current.clear();
        if let Some((a, b)) = speech_in(probs, thr, 0, pcm.len()).filter(|(a, b)| b - a <= FULL_PASS_MAX) {
            let tr = session.run(&pcm[a..b], run).map_err(|e| format!("transcribe: {e}"))?;
            self.committed = vec![tr.text.trim().to_string()];
            self.committed.retain(|s| !s.is_empty());
            self.language = tr.language.or(self.language.take());
            let ms = ms(t.elapsed());
            self.compute_ms += ms;
            return Ok(ms);
        }
        if let Some((a, b)) = speech_in(probs, thr, self.seg_start, pcm.len()) {
            let tr = session.run(&pcm[a..b], run).map_err(|e| format!("transcribe: {e}"))?;
            let text = tr.text.trim().to_string();
            if !text.is_empty() {
                self.committed.push(text);
            }
            self.language = tr.language.or(self.language.take());
        }
        let ms = ms(t.elapsed());
        self.compute_ms += ms;
        Ok(ms)
    }
}

fn record_segments(session: &mut Session, run: &RunOptions, preview_cancel: &CancelToken, p: Parts) -> Result<Outcome, String> {
    let Parts { rx, capture, audio, mut vad, pump, deferred, cfg } = p;
    let thr = cfg.vad_threshold;
    let interval = Duration::from_millis(cfg.preview_interval_ms.max(100));
    let mut seg = Segmenter::default();
    let mut last_preview = Instant::now();
    let end = loop {
        if let Some(end) = wait(rx, TICK, deferred) {
            break end;
        }
        pump.pull(capture, vad.as_deref_mut(), false);
        let due = last_preview.elapsed() >= interval;
        if seg.step(session, run, &pump.pcm, &pump.vad_probs, thr, due) {
            overlay::live(&seg.committed.join(" "), &seg.current);
        }
        if due {
            last_preview = Instant::now();
        }
    };
    let released_at = match end {
        End::Cancel => {
            audio.stop();
            return Ok(Outcome::Cancelled);
        }
        End::Finish(t) => t,
    };
    audio.stop();
    pump.pull(capture, vad, true);
    let drained_at = Instant::now();
    preview_cancel.reset();
    let final_ms = seg.finish(session, run, &pump.pcm, &pump.vad_probs, thr)?;
    if seg.committed.is_empty() {
        return Ok(Outcome::Empty { released_at });
    }
    log::info!("segments: {} closed while recording, final pass {final_ms:.0} ms", seg.closed);
    Ok(Outcome::Heard {
        released_at,
        heard: Heard { text: seg.committed.join(" "), language: seg.language, final_ms, compute_ms: seg.compute_ms },
        drained_at,
    })
}

/// Paste result.
struct Pasted {
    at: Option<Instant>,
    blocked: bool,
    /// Not pasted: shown as a copy card (no text field had focus, or no
    /// app read the clipboard after Ctrl+V).
    carded: bool,
    /// Ctrl+V was sent but nothing read the text.
    unconfirmed: bool,
    waited_ms: f64,
    paste_ms: f64,
}

fn paste(text: &str, cfg: &Config) -> Result<Pasted, String> {
    let t0 = Instant::now();
    let waited = crate::input::wait_trigger_released(Duration::from_secs(2));
    if crate::input::foreground_blocks_input() {
        // Keystrokes into an elevated window are dropped silently. Leave the
        // text on the clipboard instead.
        crate::clipboard::set_text(text)?;
        overlay::notice("Elevated window: text is on the clipboard (Ctrl+V)");
        return Ok(Pasted { at: None, blocked: true, carded: false, unconfirmed: false, waited_ms: ms(waited), paste_ms: ms(t0.elapsed()) });
    }
    if cfg.check_focus {
        let t = Instant::now();
        let target = crate::input::focus_target();
        log::info!("focus check in {:.1} ms: {target:?}", ms(t.elapsed()));
        if let crate::input::Target::NotEditable(_) = target {
            // Ctrl+V would go nowhere: keep the text one click away instead.
            overlay::card(text, "Not pasted · no text field had focus");
            return Ok(Pasted { at: None, blocked: false, carded: true, unconfirmed: false, waited_ms: ms(waited), paste_ms: ms(t0.elapsed()) });
        }
    }
    let saved = crate::clipboard::save().map_err(|e| log::warn!("clipboard save: {e}")).ok();
    let spaced;
    let text = if cfg.trailing_space && !cfg.auto_enter {
        spaced = format!("{text} ");
        spaced.as_str()
    } else {
        text
    };
    let Some(seq) = crate::clipboard::set_text_deferred(text)? else {
        // No owner window (should not happen): the old fixed-delay paste.
        let seq = crate::clipboard::set_text(text)?;
        std::thread::sleep(Duration::from_millis(cfg.paste_delay_before_ms));
        crate::input::ctrl_v()?;
        let at = Instant::now();
        std::thread::sleep(Duration::from_millis(cfg.paste_delay_after_ms));
        restore(saved, seq);
        return Ok(Pasted { at: Some(at), blocked: false, carded: false, unconfirmed: false, waited_ms: ms(waited), paste_ms: ms(t0.elapsed()) });
    };
    std::thread::sleep(Duration::from_millis(cfg.paste_delay_before_ms));
    let refused = crate::clipboard::arm();
    if !refused.is_empty() {
        let names: Vec<String> = refused.iter().map(|&p| process_name(p)).collect();
        log::debug!("clipboard monitors refused before Ctrl+V: {}", names.join(", "));
    }
    let sent = Instant::now();
    crate::input::ctrl_v()?;
    match crate::clipboard::wait_read(sent, Duration::from_millis(PASTE_CONFIRM_MS)) {
        Some(r) => {
            log::info!("paste read by {} {:.1} ms after Ctrl+V", process_name(r.pid), ms(r.at - sent));
            std::thread::sleep(Duration::from_millis(cfg.paste_delay_after_ms));
            restore(saved, seq);
            if cfg.auto_enter {
                crate::input::enter()?;
            }
            Ok(Pasted { at: Some(r.at), blocked: false, carded: false, unconfirmed: false, waited_ms: ms(waited), paste_ms: ms(t0.elapsed()) })
        }
        None => {
            // Nothing took the text. Keep it on the clipboard (a late Ctrl+V
            // still pastes it) and show it with a Copy button.
            let refused: Vec<String> = crate::clipboard::take_refused().iter().map(|&p| process_name(p)).collect();
            log::warn!(
                "paste not confirmed: no app read the clipboard within {PASTE_CONFIRM_MS} ms of Ctrl+V (refused as monitors: {refused:?})"
            );
            overlay::card(text, "Not pasted · the app didn't take it (it's on the clipboard)");
            Ok(Pasted { at: None, blocked: false, carded: true, unconfirmed: true, waited_ms: ms(waited), paste_ms: ms(t0.elapsed()) })
        }
    }
}

/// How long the target app gets to read the clipboard after Ctrl+V before
/// the paste counts as lost.
const PASTE_CONFIRM_MS: u64 = 1000;

fn restore(saved: Option<crate::clipboard::Saved>, seq: u32) {
    if let Some(saved) = saved {
        match crate::clipboard::restore(saved, seq) {
            Ok(false) => log::info!("clipboard changed during paste; not restored"),
            Err(e) => log::warn!("clipboard restore: {e}"),
            Ok(true) => {}
        }
    }
}

fn process_name(pid: u32) -> String {
    if pid == 0 {
        return "an unknown process".into();
    }
    crate::input::process_name(pid).unwrap_or_else(|| format!("pid {pid}"))
}

impl Worker {
    fn run(&mut self) {
        let mut deferred: Vec<Cmd> = Vec::new();
        loop {
            let cmd = if let Some(c) = deferred.pop() {
                c
            } else {
                let cfg = self.cfg.get();
                let idle = Duration::from_secs(cfg.idle_unload_minutes * 60);
                let loaded = self.asr.is_some() || self.cleaner.is_some();
                if loaded && cfg.idle_unload_minutes > 0 {
                    let left = (self.last_used + idle).saturating_duration_since(Instant::now());
                    match self.rx.recv_timeout(left) {
                        Ok(c) => c,
                        Err(RecvTimeoutError::Timeout) => {
                            self.unload("idle");
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match self.rx.recv() {
                        Ok(c) => c,
                        Err(_) => return,
                    }
                }
            };
            match cmd {
                Cmd::Begin { pressed_at, started_at } => {
                    self.dictate(pressed_at, started_at, &mut deferred);
                    self.last_used = Instant::now();
                }
                // Stale: the recording already ended.
                Cmd::Finish { .. } | Cmd::Cancel => {}
                Cmd::Config(new) => self.apply_config(&new),
                Cmd::Warm => self.warm(),
                Cmd::Download(id) => {
                    let asset = models::asr(id).map(|m| &m.asset).or_else(|| models::cleanup(id).map(|m| &m.asset));
                    if let Some(a) = asset {
                        self.download(id, a);
                    }
                }
                Cmd::Downloaded { id, ok } => {
                    self.downloading.remove(id);
                    if ok {
                        self.warm();
                    } else {
                        tray::set_state(TrayState::Error);
                    }
                }
            }
        }
    }

    fn unload(&mut self, why: &str) {
        if self.asr.take().is_some() {
            log::info!("speech model unloaded ({why})");
            crate::status::update(|s| s.loaded = None);
        }
        if self.cleaner.take().is_some() {
            log::info!("cleanup model unloaded ({why})");
        }
        tray::set_state(TrayState::Idle);
    }

    fn apply_config(&mut self, cfg: &Config) {
        if self.asr.as_ref().is_some_and(|l| l.spec.id != cfg.model) {
            self.asr = None;
        }
        if self.cleaner.as_ref().is_some_and(|c| !cfg.cleanup.enabled || c.id() != cfg.cleanup.model) {
            self.cleaner = None;
            log::info!("cleanup model released");
        }
        self.warm();
    }

    /// Starts a background download; `Downloaded` arrives when done.
    fn download(&mut self, id: &'static str, asset: &'static models::Asset) {
        if !self.downloading.insert(id) {
            return;
        }
        tray::set_state(TrayState::Loading);
        let tx = self.self_tx.clone();
        std::thread::Builder::new()
            .name("download".into())
            .spawn(move || {
                let name = asset.file;
                crate::status::set_download(name, Some(0.0));
                let result = models::ensure(asset, |f| {
                    overlay::progress(&format!("Downloading {name}"), f);
                    crate::status::set_download(name, Some(f));
                    tray::set_tooltip(&format!("chatty: downloading {name} {:.0}%", f * 100.0));
                });
                crate::status::set_download(name, None);
                match &result {
                    Ok(_) => overlay::notice(&format!("Downloaded {name}")),
                    Err(e) => {
                        log::error!("{e}");
                        overlay::notice(&format!("Download failed: {name}"));
                    }
                }
                let _ = tx.send(Cmd::Downloaded { id, ok: result.is_ok() });
            })
            .expect("spawn download");
    }

    /// Loads what the config selects, downloading missing files first.
    fn warm(&mut self) {
        let cfg = self.cfg.get();
        if self.vad.is_none() {
            if models::VAD.is_ready() {
                match Model::load_with(models::VAD.path(), &ModelOptions { backend: Backend::Cpu, ..Default::default() })
                    .and_then(|m| m.vad_session().map(|s| (m, s)))
                {
                    Ok(v) => self.vad = Some(v),
                    Err(e) => log::error!("VAD load: {e}"),
                }
            } else {
                self.download("silero-vad", &models::VAD);
            }
        }
        if let Err(e) = self.ensure_asr(&cfg) {
            if !e.is_empty() {
                log::error!("{e}");
            }
        }
        if cfg.cleanup.enabled && self.cleaner.is_none() {
            match models::cleanup(&cfg.cleanup.model) {
                Some(spec) if spec.asset.is_ready() => match Cleaner::load(spec, &cfg.cleanup) {
                    Ok(c) => {
                        log::info!("cleanup model {} loaded in {:.0} ms", spec.id, c.load_ms);
                        self.cleaner = Some(c);
                    }
                    Err(e) => {
                        log::error!("cleanup load: {e}");
                        overlay::notice(&format!("Cleanup unavailable: {e}"));
                    }
                },
                Some(spec) => self.download(spec.id, &spec.asset),
                None => log::error!("unknown cleanup model {:?}", cfg.cleanup.model),
            }
        }
        if self.downloading.is_empty() {
            tray::set_state(TrayState::Idle);
        }
    }

    /// Ok when the configured model is loaded. Err("") while downloading.
    fn ensure_asr(&mut self, cfg: &Config) -> Result<(), String> {
        if self.asr.as_ref().is_some_and(|l| l.spec.id == cfg.model) {
            return Ok(());
        }
        let spec = models::asr(&cfg.model).ok_or_else(|| format!("unknown model {:?}", cfg.model))?;
        if !spec.asset.is_ready() {
            self.download(spec.id, &spec.asset);
            return Err(String::new());
        }
        self.asr = None;
        tray::set_state(TrayState::Loading);
        let t0 = Instant::now();
        let backend = match cfg.backend.as_str() {
            "cpu" => Backend::Cpu,
            "vulkan" => Backend::Vulkan,
            _ => Backend::Auto,
        };
        let model = Model::load_with(spec.asset.path(), &ModelOptions { backend, ..Default::default() })
            .map_err(|e| format!("load {}: {e}", spec.id))?;
        let mut session = model.session().map_err(|e| format!("session {}: {e}", spec.id))?;
        let backend_name = model.backend();
        let vocabulary = model.supports(Feature::Vocabulary);
        let run = RunOptions { language: spec.language_arg(&cfg.language), ..Default::default() };
        warm_up(&mut session, spec, &run);
        let load_ms = ms(t0.elapsed());
        log::info!("model {} loaded on {} in {:.0} ms (incl. warm-up)", spec.id, backend_name, load_ms);
        self.pending_load_ms = Some(load_ms);
        let loaded_info = (spec.id.to_string(), backend_name.clone());
        crate::status::update(|s| s.loaded = Some(loaded_info));
        self.asr = Some(Loaded { spec, _model: model, session, backend: backend_name, vocabulary });
        tray::set_state(TrayState::Idle);
        Ok(())
    }

    fn finished(&self) {
        let _ = self.coord.send(KeyEvent { input: Input::ProcessingFinished, at: Instant::now() });
    }

    fn fail_start(&self, msg: &str) {
        log::error!("{msg}");
        overlay::notice(msg);
        let _ = self.coord.send(KeyEvent { input: Input::StartFailed, at: Instant::now() });
    }

    fn dictate(&mut self, pressed_at: Instant, started_at: Instant, deferred: &mut Vec<Cmd>) {
        let cfg = self.cfg.get();
        self.capture.discard();
        let mic = match self.audio.start(&cfg.microphone, Duration::from_secs(cfg.mic_keep_open_secs)) {
            Ok(m) => m,
            Err(e) => return self.fail_start(&e),
        };
        let mic_ready = Instant::now();
        overlay::live("", "");
        // Loads while the microphone already buffers into the ring.
        if let Err(e) = self.ensure_asr(&cfg) {
            self.audio.stop();
            if e.is_empty() {
                return self.fail_start("Model is downloading");
            }
            return self.fail_start(&e);
        }
        let model_load_ms = self.pending_load_ms.take().unwrap_or(0.0);
        let mut rec = Dictation::new(pressed_at, started_at);
        rec.mic_open_ms = ms(mic.open);
        rec.mic_start_ms = ms(mic_ready.saturating_duration_since(started_at));
        rec.model_load_ms = model_load_ms;

        let Worker { rx, capture, audio, asr, vad, preview_cancel, .. } = self;
        let loaded = asr.as_mut().unwrap();
        let spec = loaded.spec;
        rec.model = spec.id.to_string();
        rec.backend = loaded.backend.clone();
        // Vocabulary boosting works on offline runs only (the streaming API
        // ignores it with a warning).
        let vocabulary = if loaded.vocabulary && !spec.is_streaming() {
            cfg.custom_words.iter().map(|w| w.word.clone()).collect()
        } else {
            Vec::new()
        };
        let run = RunOptions { language: spec.language_arg(&cfg.language), vocabulary, ..Default::default() };
        let mut pump = Pump::new(mic.rate);
        let vad_session = vad.as_mut().map(|(_, s)| {
            s.stream_reset();
            s
        });
        let parts = Parts { rx, capture, audio, vad: vad_session, pump: &mut pump, deferred, cfg: &cfg };
        let outcome = if spec.is_streaming() && cfg.decode == "stream" {
            loaded.session.clear_cancel_token();
            record_streaming(&mut loaded.session, spec, &run, parts)
        } else {
            preview_cancel.reset();
            loaded.session.set_cancel_token(preview_cancel);
            record_segments(&mut loaded.session, &run, preview_cancel, parts)
        };

        let total_samples = pump.pcm.len();
        rec.recording_ms = (total_samples * 1000 / audio::RATE as usize) as i64;
        let speech = analyze_speech(&pump.vad_probs, cfg.vad_threshold, total_samples);
        rec.speech_ms = if self.vad.is_some() { speech.speech_ms } else { rec.recording_ms };
        rec.vad_ms = pump.vad_ms;

        let (released_at, heard, drained_at) = match outcome {
            Err(e) => {
                log::error!("{e}");
                overlay::notice(&e);
                rec.status = "error";
                rec.error = Some(e);
                self.finished();
                self.stats.dictation(rec, None);
                return;
            }
            Ok(Outcome::Cancelled) => {
                log::info!("dictation cancelled after {} ms of audio", rec.recording_ms);
                overlay::hide();
                rec.status = "cancelled";
                self.stats.dictation(rec, None);
                return;
            }
            Ok(Outcome::Empty { released_at }) => {
                log::info!("no speech in {} ms of audio", rec.recording_ms);
                rec.set_released(released_at);
                rec.status = "empty";
                overlay::hide();
                self.finished();
                self.stats.dictation(rec, Some(pump.pcm));
                return;
            }
            Ok(Outcome::Heard { released_at, heard, drained_at }) => (released_at, heard, drained_at),
        };
        rec.set_released(released_at);
        rec.app = crate::input::foreground_app();
        rec.drain_ms = ms(drained_at.saturating_duration_since(released_at));
        rec.transcribe_ms = heard.final_ms;
        rec.compute_ms = heard.compute_ms;
        overlay::transcribing();

        // Text pipeline.
        let t_post = Instant::now();
        let raw = heard.text.trim().to_string();
        let language = heard
            .language
            .clone()
            .filter(|l| !l.is_empty() && l != "auto")
            .or_else(|| (cfg.language != "auto" && !cfg.language.is_empty()).then(|| cfg.language.clone()))
            .or_else(|| (spec.languages.len() == 1).then(|| spec.languages[0].to_string()));
        rec.language = language.clone().unwrap_or_default();
        let mut t = text::collapse_stutters(&raw);
        if cfg.remove_fillers {
            let (out, removed) = text::remove_fillers(&t, language.as_deref(), &cfg.filler_words);
            t = out;
            rec.fillers = removed;
        }
        let (corrected, corrections) = text::apply_custom_words(&t, &cfg.custom_words);
        rec.corrections = corrections;
        rec.post_ms = ms(t_post.elapsed());
        let mut final_text = corrected.clone();
        if cfg.cleanup.enabled && !corrected.is_empty() {
            if let Some(cleaner) = self.cleaner.as_mut() {
                let r = cleaner.clean(&corrected, language.as_deref(), &cfg.cleanup);
                log::info!(
                    "cleanup {} in {:.0} ms ({}): {:?}",
                    cleaner.id(),
                    r.ms,
                    if r.accepted { "accepted" } else { "rejected" },
                    r.note
                );
                rec.cleanup_model = Some(cleaner.id().to_string());
                rec.cleanup_ms = r.ms;
                rec.cleanup_used = Some(r.accepted);
                rec.cleanup_note = r.note.clone();
                rec.cleanup_length_ratio = r.length_ratio;
                rec.cleanup_edit_ratio = r.edit_ratio;
                rec.cleaned_text = Some(r.output.clone());
                if r.accepted {
                    final_text = r.output;
                }
            } else {
                log::warn!("cleanup enabled but its model is not loaded yet");
            }
        }
        rec.raw_text = raw;
        rec.words = text::word_count(&final_text) as i64;
        rec.chars = final_text.chars().count() as i64;

        if final_text.is_empty() {
            log::info!("empty transcript");
            rec.status = "empty";
            overlay::hide();
            self.finished();
            self.stats.dictation(rec, Some(pump.pcm));
            return;
        }

        match paste(&final_text, &cfg) {
            Ok(p) => {
                rec.paste_wait_ms = p.waited_ms;
                rec.paste_ms = p.paste_ms;
                if p.blocked {
                    rec.status = "blocked";
                }
                if p.carded {
                    rec.status = if p.unconfirmed { "unconfirmed" } else { "card" };
                }
                if let Some(at) = p.at {
                    rec.set_pasted(at);
                }
            }
            Err(e) => {
                log::error!("paste: {e}");
                overlay::notice(&format!("Paste failed: {e}"));
                rec.status = "error";
                rec.error = Some(format!("paste: {e}"));
            }
        }
        self.finished();
        if rec.status != "blocked" {
            overlay::hide();
        }
        rec.final_text = final_text;
        log::info!(
            "dictation: {} words, audio {} ms (speech {} ms), release->paste {:.0} ms [drain {:.1} transcribe {:.0} post {:.1} cleanup {:.0} wait-keys {:.0} paste-delay {}] rtf {:.3} model {} on {} app {:?}",
            rec.words,
            rec.recording_ms,
            rec.speech_ms,
            rec.latency_ms.unwrap_or(f64::NAN),
            rec.drain_ms,
            rec.transcribe_ms,
            rec.post_ms,
            rec.cleanup_ms,
            rec.paste_wait_ms,
            cfg.paste_delay_before_ms,
            rec.rtf(),
            rec.model,
            rec.backend,
            rec.app,
        );
        self.stats.dictation(rec, Some(pump.pcm));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_range_is_padded_and_clamped() {
        let mut probs = vec![0.0; 100];
        for p in &mut probs[10..20] {
            *p = 0.9;
        }
        let s = analyze_speech(&probs, 0.5, 100 * 512);
        assert_eq!(s.speech_ms, 10 * 32);
        let (a, b) = s.range.unwrap();
        assert_eq!(a, 10 * 512 - 3200);
        assert_eq!(b, 20 * 512 + 4800);
        // Speech at the very end is clamped to the buffer.
        let mut probs = vec![0.0; 10];
        for p in &mut probs[7..] {
            *p = 0.9;
        }
        let s = analyze_speech(&probs, 0.5, 10 * 512);
        assert_eq!(s.range.unwrap().1, 10 * 512);
    }

    #[test]
    fn clicks_are_not_speech() {
        let mut probs = vec![0.0; 50];
        probs[5] = 0.99;
        probs[6] = 0.99;
        assert_eq!(analyze_speech(&probs, 0.5, 50 * 512).range, None);
    }
}

pub fn stream_options_for_bench(spec: &AsrModel) -> StreamOptions {
    stream_options(spec)
}

