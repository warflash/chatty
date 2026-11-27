//! Hold-or-tap state machine for the trigger key.
//!
//! Pure: every decision is a function of the inputs and the timestamps they
//! carry, so tests drive it with a synthetic clock. The coordinator thread
//! (`run`) transports events to it and executes the returned effects.
//!
//! * Key-down arms: the microphone starts capturing at once, so words
//!   spoken during the hold threshold are kept. Hold >= threshold: the
//!   recording (with that audio) starts at the threshold; the release stops
//!   it. A tap drops the armed audio unheard.
//! * Release before the threshold: a tap, which presses Enter.
//! * Escape while armed or recording: cancel.
//! * Presses while the pipeline is busy are remembered and resolved when it
//!   drains, measured from the real key-down (ideas from Handy's
//!   `transcription_coordinator.rs`, commit 6b217ab6). A tap while busy
//!   becomes an Enter that runs only after the paste has finished, because
//!   the pipeline reports "finished" only after its paste completed.
//!
//! No release grace window: on Windows a held key repeats key-*downs* only, so
//! there is no synthetic release to absorb, and a grace window would add its
//! length to every release-to-paste latency.

use std::time::{Duration, Instant};

/// Presses closer together than this are switch bounce.
pub const DEBOUNCE: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Press,
    Release,
    Escape,
    /// The pipeline pasted (or gave up) and is ready for the next dictation.
    ProcessingFinished,
    /// The recording could not start (no microphone, no model).
    StartFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Key-down: start capturing (kept only if the hold becomes a recording).
    Arm { pressed_at: Instant },
    /// The armed key-down didn't become a recording: drop its audio.
    Disarm,
    /// Start recording. `pressed_at` is the real key-down.
    Start { pressed_at: Instant },
    /// Stop recording, transcribe and paste.
    Stop { released_at: Instant },
    /// Discard the recording.
    Cancel,
    /// Press Enter. `released_at` is the key-up of the tap.
    Submit { released_at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    /// Key down, threshold not reached yet.
    Armed { pressed_at: Instant },
    Recording,
    Processing,
}

/// A press that arrived while the pipeline was busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    pressed_at: Instant,
    /// Released before the threshold: an Enter once the pipeline drains.
    submit_at: Option<Instant>,
}

#[derive(Debug)]
pub struct Machine {
    stage: Stage,
    pending: Option<Pending>,
    last_press: Option<Instant>,
    pub threshold: Duration,
}

impl Machine {
    pub fn new(threshold: Duration) -> Self {
        Self { stage: Stage::Idle, pending: None, last_press: None, threshold }
    }

    /// Recording or armed: Escape belongs to us.
    pub fn wants_escape(&self) -> bool {
        matches!(self.stage, Stage::Armed { .. } | Stage::Recording)
    }

    #[cfg(test)]
    pub fn is_recording(&self) -> bool {
        self.stage == Stage::Recording
    }

    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.stage == Stage::Processing
    }

    /// When `on_deadline` must run next.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.stage {
            Stage::Armed { pressed_at } => Some(pressed_at + self.threshold),
            _ => None,
        }
    }

    pub fn on_deadline(&mut self, now: Instant) -> Option<Effect> {
        match self.stage {
            Stage::Armed { pressed_at } if now >= pressed_at + self.threshold => {
                self.stage = Stage::Recording;
                Some(Effect::Start { pressed_at })
            }
            _ => None,
        }
    }

    /// `at` is when the input happened (the hook's timestamp), not when the
    /// coordinator dequeued it.
    pub fn on_input(&mut self, input: Input, at: Instant) -> Option<Effect> {
        match input {
            Input::Press => self.on_press(at),
            Input::Release => self.on_release(at),
            Input::Escape => self.on_escape(),
            Input::ProcessingFinished => self.on_finished(at),
            Input::StartFailed => {
                if self.stage == Stage::Recording {
                    self.stage = Stage::Idle;
                }
                None
            }
        }
    }

    fn on_press(&mut self, at: Instant) -> Option<Effect> {
        if self.last_press.is_some_and(|t| at.saturating_duration_since(t) < DEBOUNCE) {
            return None;
        }
        self.last_press = Some(at);
        match self.stage {
            Stage::Idle => {
                self.stage = Stage::Armed { pressed_at: at };
                Some(Effect::Arm { pressed_at: at })
            }
            Stage::Processing => {
                // One remembered press at most; further presses are noise.
                if self.pending.is_none() {
                    self.pending = Some(Pending { pressed_at: at, submit_at: None });
                }
                None
            }
            // Key repeat or a second press while held.
            Stage::Armed { .. } | Stage::Recording => None,
        }
    }

    fn on_release(&mut self, at: Instant) -> Option<Effect> {
        match self.stage {
            Stage::Armed { pressed_at } => {
                self.stage = Stage::Idle;
                let held = at.saturating_duration_since(pressed_at);
                // A release at/after the threshold whose deadline has not been
                // processed yet was meant as a hold, but nothing was recorded.
                Some(if held < self.threshold { Effect::Submit { released_at: at } } else { Effect::Disarm })
            }
            Stage::Recording => {
                self.stage = Stage::Processing;
                Some(Effect::Stop { released_at: at })
            }
            Stage::Processing => {
                if let Some(p) = &mut self.pending {
                    if p.submit_at.is_none() {
                        if at.saturating_duration_since(p.pressed_at) < self.threshold {
                            p.submit_at = Some(at);
                        } else {
                            // Held through the busy window and released:
                            // there was never a recording to stop.
                            self.pending = None;
                        }
                    }
                }
                None
            }
            Stage::Idle => None,
        }
    }

    fn on_escape(&mut self) -> Option<Effect> {
        match self.stage {
            Stage::Armed { .. } => {
                self.stage = Stage::Idle;
                Some(Effect::Disarm)
            }
            Stage::Recording => {
                self.stage = Stage::Idle;
                Some(Effect::Cancel)
            }
            Stage::Processing => {
                self.pending = None;
                None
            }
            Stage::Idle => None,
        }
    }

    fn on_finished(&mut self, now: Instant) -> Option<Effect> {
        if self.stage != Stage::Processing {
            return None;
        }
        self.stage = Stage::Idle;
        let pending = self.pending.take()?;
        if let Some(released_at) = pending.submit_at {
            return Some(Effect::Submit { released_at });
        }
        // Still held: arm from the real key-down; a hold that already passed
        // the threshold starts recording right away.
        self.stage = Stage::Armed { pressed_at: pending.pressed_at };
        self.on_deadline(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_millis(300);

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn tap_presses_enter_and_never_records() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        assert_eq!(m.on_input(Input::Press, t0), Some(Effect::Arm { pressed_at: t0 }));
        assert_eq!(m.next_deadline(), Some(ms(t0, 300)));
        assert_eq!(m.on_input(Input::Release, ms(t0, 120)), Some(Effect::Submit { released_at: ms(t0, 120) }));
        assert_eq!(m.next_deadline(), None);
        assert!(!m.is_recording());
    }

    #[test]
    fn hold_records_at_threshold_and_release_stops() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        assert_eq!(m.on_deadline(ms(t0, 299)), None);
        assert_eq!(m.on_deadline(ms(t0, 300)), Some(Effect::Start { pressed_at: t0 }));
        assert!(m.is_recording());
        assert_eq!(m.on_input(Input::Release, ms(t0, 2000)), Some(Effect::Stop { released_at: ms(t0, 2000) }));
        assert!(m.is_busy());
        assert_eq!(m.on_input(Input::ProcessingFinished, ms(t0, 2100)), None);
        assert!(!m.is_busy());
    }

    #[test]
    fn key_repeats_are_ignored() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        for i in 1..20 {
            assert_eq!(m.on_input(Input::Press, ms(t0, 33 * i)), None);
        }
        // Repeats must not move the deadline.
        assert_eq!(m.next_deadline(), Some(ms(t0, 300)));
        assert_eq!(m.on_deadline(ms(t0, 300)), Some(Effect::Start { pressed_at: t0 }));
        assert_eq!(m.on_input(Input::Press, ms(t0, 700)), None);
        assert!(m.is_recording());
    }

    #[test]
    fn escape_cancels_recording_and_armed_press() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        assert!(m.wants_escape());
        assert_eq!(m.on_input(Input::Escape, ms(t0, 100)), Some(Effect::Disarm));
        // The release after an escaped arm is not a tap.
        assert_eq!(m.on_input(Input::Release, ms(t0, 150)), None);

        m.on_input(Input::Press, ms(t0, 1000));
        m.on_deadline(ms(t0, 1300));
        assert_eq!(m.on_input(Input::Escape, ms(t0, 1500)), Some(Effect::Cancel));
        assert_eq!(m.on_input(Input::Release, ms(t0, 1600)), None);
        assert!(!m.wants_escape());
    }

    #[test]
    fn tap_while_busy_submits_after_drain() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::Release, ms(t0, 1000));
        // Tap while transcribing/pasting.
        assert_eq!(m.on_input(Input::Press, ms(t0, 1050)), None);
        assert_eq!(m.on_input(Input::Release, ms(t0, 1100)), None);
        assert_eq!(
            m.on_input(Input::ProcessingFinished, ms(t0, 1200)),
            Some(Effect::Submit { released_at: ms(t0, 1100) })
        );
        assert!(!m.is_busy());
    }

    #[test]
    fn hold_started_while_busy_records_after_drain() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::Release, ms(t0, 1000));
        m.on_input(Input::Press, ms(t0, 1050));
        // Drains before the new press reached the threshold: armed.
        assert_eq!(m.on_input(Input::ProcessingFinished, ms(t0, 1100)), None);
        assert_eq!(m.next_deadline(), Some(ms(t0, 1350)));
        assert_eq!(m.on_deadline(ms(t0, 1350)), Some(Effect::Start { pressed_at: ms(t0, 1050) }));
    }

    #[test]
    fn hold_past_threshold_while_busy_starts_on_drain() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::Release, ms(t0, 1000));
        m.on_input(Input::Press, ms(t0, 1050));
        assert_eq!(
            m.on_input(Input::ProcessingFinished, ms(t0, 1500)),
            Some(Effect::Start { pressed_at: ms(t0, 1050) })
        );
    }

    #[test]
    fn long_hold_released_while_busy_is_dropped() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::Release, ms(t0, 1000));
        m.on_input(Input::Press, ms(t0, 1050));
        m.on_input(Input::Release, ms(t0, 1500));
        assert_eq!(m.on_input(Input::ProcessingFinished, ms(t0, 1600)), None);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn bounce_is_debounced() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_input(Input::Release, ms(t0, 5));
        // Bounce 10 ms later: ignored, so no second tap.
        assert_eq!(m.on_input(Input::Press, ms(t0, 15)), None);
        assert_eq!(m.on_input(Input::Release, ms(t0, 20)), None);
    }

    #[test]
    fn late_deadline_release_is_not_a_tap() {
        // The coordinator was busy and saw the release before the deadline.
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        assert_eq!(m.on_input(Input::Release, ms(t0, 450)), Some(Effect::Disarm));
    }

    #[test]
    fn start_failure_returns_to_idle() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::StartFailed, ms(t0, 310));
        assert!(!m.is_recording());
        assert_eq!(m.on_input(Input::Release, ms(t0, 900)), None);
    }

    #[test]
    fn escape_while_busy_forgets_remembered_tap() {
        let t0 = Instant::now();
        let mut m = Machine::new(T);
        m.on_input(Input::Press, t0);
        m.on_deadline(ms(t0, 300));
        m.on_input(Input::Release, ms(t0, 1000));
        m.on_input(Input::Press, ms(t0, 1050));
        m.on_input(Input::Release, ms(t0, 1100));
        m.on_input(Input::Escape, ms(t0, 1150));
        assert_eq!(m.on_input(Input::ProcessingFinished, ms(t0, 1200)), None);
    }
}
