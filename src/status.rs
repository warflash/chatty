//! Live application status shared by the engine, the overlay, the tray and
//! the main window. Writers update it through `update`; the window repaints
//! when something changed.

use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording,
    Transcribing,
    Loading,
    Error,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub phase: Phase,
    pub phase_since: Instant,
    /// Live transcript of the current recording.
    pub live: String,
    /// Microphone level 0..1 (peak-ish RMS of the last 20 ms), while recording.
    pub level: f32,
    /// (file name, fraction) of running downloads.
    pub downloads: Vec<(String, f32)>,
    /// Model id and backend of the loaded speech model.
    pub loaded: Option<(String, String)>,
    /// Last notice shown in the overlay.
    pub notice: Option<(String, Instant)>,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Status) -> R) -> R {
    let mut g = STATUS.lock().unwrap();
    let s = g.get_or_insert_with(|| Status {
        phase: Phase::Loading,
        phase_since: Instant::now(),
        live: String::new(),
        level: 0.0,
        downloads: Vec::new(),
        loaded: None,
        notice: None,
    });
    f(s)
}

pub fn get() -> Status {
    with(|s| s.clone())
}

pub fn update(f: impl FnOnce(&mut Status)) {
    with(f);
    crate::ui::notify();
}

pub fn set_phase(p: Phase) {
    update(|s| {
        if s.phase != p {
            s.phase = p;
            s.phase_since = Instant::now();
            if p != Phase::Recording {
                s.level = 0.0;
            }
            if p == Phase::Idle {
                s.live.clear();
            }
        }
    });
}

pub fn set_level(level: f32) {
    // Level changes 50 times a second; only the overlay animates on it, so
    // skip the window notification.
    with(|s| s.level = level);
}

pub fn set_download(file: &str, fraction: Option<f32>) {
    update(|s| {
        s.downloads.retain(|(f, _)| f != file);
        if let Some(f) = fraction {
            s.downloads.push((file.to_string(), f));
        }
    });
}
