//! Quartz event tap on its own thread, the macOS counterpart of the
//! `WH_KEYBOARD_LL` hook. The callback only classifies the key, flips an
//! atomic, enqueues an event and returns. The trigger and, while recording,
//! Escape are swallowed so apps never see them. Events chatty posts itself
//! carry a tag in `kCGEventSourceUserData` and pass untouched. (Keys other
//! processes synthesize count as real, so `CHATTY_ACCEPT_INJECTED` is not
//! needed here.)
//!
//! Triggers (`trigger` in config.toml):
//! * `ctrl+numpad_divide` (default): Control + keypad /, the same chord as on
//!   Windows (a mouse button mapped to it works too). Swallowed.
//! * `right_option`, `right_command`, `fn`: hold a modifier alone. Modifier
//!   changes are never swallowed; pressing any other key while it is held
//!   makes it a normal modifier again (Escape to the state machine, so a
//!   chord like Option+L never presses Enter or records).
//!
//! Needs Accessibility access; until it is granted the tap cannot be created
//! and the thread retries every two seconds.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::mac::sys::*;
use crate::coordinator::Input;

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub input: Input,
    pub at: Instant,
}

/// Tag in `kCGEventSourceUserData` on keys chatty posts.
pub const OUR_INPUT: i64 = 0x6368_6174; // "chat"

const KEY_KEYPAD_DIVIDE: i64 = 0x4B;
const KEY_ESCAPE: i64 = 0x35;
const KEY_RIGHT_COMMAND: i64 = 0x36;
const KEY_RIGHT_OPTION: i64 = 0x3D;
const KEY_FN: i64 = 0x3F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Trigger {
    CtrlKeypadDivide = 0,
    RightOption = 1,
    RightCommand = 2,
    Fn = 3,
}

impl Trigger {
    fn from_u8(v: u8) -> Trigger {
        match v {
            1 => Trigger::RightOption,
            2 => Trigger::RightCommand,
            3 => Trigger::Fn,
            _ => Trigger::CtrlKeypadDivide,
        }
    }

    /// The modifier key and its flag bit, for modifier-only triggers.
    fn modifier(self) -> Option<(i64, u64)> {
        match self {
            Trigger::RightOption => Some((KEY_RIGHT_OPTION, NX_DEVICERALTKEYMASK)),
            Trigger::RightCommand => Some((KEY_RIGHT_COMMAND, NX_DEVICERCMDKEYMASK)),
            Trigger::Fn => Some((KEY_FN, kCGEventFlagMaskSecondaryFn)),
            Trigger::CtrlKeypadDivide => None,
        }
    }
}

static TRIGGER_KIND: AtomicU8 = AtomicU8::new(0);
static TRIGGER: AtomicBool = AtomicBool::new(false);
/// A modifier trigger turned into a plain modifier by another key.
static ABORTED: AtomicBool = AtomicBool::new(false);
static ESC_SWALLOWED: AtomicBool = AtomicBool::new(false);
/// Set by the coordinator while armed or recording.
pub static ESCAPE_WANTED: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<Sender<KeyEvent>> = OnceLock::new();

struct Tap(CFMachPortRef);
// SAFETY: the mach port is only re-enabled, which CoreGraphics allows from the callback thread.
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}
static TAP: OnceLock<Tap> = OnceLock::new();

pub fn set_trigger(spec: &str) {
    let t = match spec {
        "right_option" => Trigger::RightOption,
        "right_command" => Trigger::RightCommand,
        "fn" => Trigger::Fn,
        "ctrl+numpad_divide" | "" => Trigger::CtrlKeypadDivide,
        other => {
            log::warn!("unknown trigger {other:?}; using ctrl+numpad_divide");
            Trigger::CtrlKeypadDivide
        }
    };
    let old = TRIGGER_KIND.swap(t as u8, Ordering::Relaxed);
    if old != t as u8 {
        log::info!("trigger: {t:?}");
        if TRIGGER.swap(false, Ordering::Relaxed) {
            send(Input::Release);
        }
    }
}

/// True while any trigger key is physically down.
pub fn trigger_keys_down() -> bool {
    let flags = unsafe { CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState) };
    match Trigger::from_u8(TRIGGER_KIND.load(Ordering::Relaxed)).modifier() {
        Some((_, bit)) => flags & bit != 0,
        // The physical key state, not just our flag: a missed key-up must
        // not block pasting forever.
        None => {
            flags & kCGEventFlagMaskControl != 0
                || unsafe { CGEventSourceKeyState(kCGEventSourceStateHIDSystemState, KEY_KEYPAD_DIVIDE as u16) }
        }
    }
}

fn send(input: Input) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeyEvent { input, at: Instant::now() });
    }
}

unsafe extern "C" fn callback(_proxy: *mut c_void, ty: u32, event: CGEventRef, _user: *mut c_void) -> CGEventRef {
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        // The system disables a tap that once took too long; turn it back on.
        if let Some(t) = TAP.get() {
            unsafe { CGEventTapEnable(t.0, true) };
        }
        log::warn!("event tap re-enabled after {}", if ty == kCGEventTapDisabledByTimeout { "a timeout" } else { "user input" });
        return event;
    }
    if unsafe { CGEventGetIntegerValueField(event, kCGEventSourceUserData) } == OUR_INPUT {
        return event;
    }
    let key = unsafe { CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode) };
    let flags = unsafe { CGEventGetFlags(event) };
    let trigger = Trigger::from_u8(TRIGGER_KIND.load(Ordering::Relaxed));
    let swallow = std::ptr::null_mut();

    if ty == kCGEventFlagsChanged {
        if let Some((code, bit)) = trigger.modifier()
            && key == code
        {
            let down = flags & bit != 0;
            if down && !TRIGGER.swap(true, Ordering::Relaxed) {
                ABORTED.store(false, Ordering::Relaxed);
                send(Input::Press);
            } else if !down && TRIGGER.swap(false, Ordering::Relaxed) && !ABORTED.swap(false, Ordering::Relaxed) {
                send(Input::Release);
            }
        }
        return event;
    }

    let down = ty == kCGEventKeyDown;
    let up = ty == kCGEventKeyUp;
    let repeat = unsafe { CGEventGetIntegerValueField(event, kCGKeyboardEventAutorepeat) } != 0;

    if key == KEY_ESCAPE {
        if down {
            if ESC_SWALLOWED.load(Ordering::Relaxed) {
                return swallow;
            }
            if ESCAPE_WANTED.load(Ordering::Relaxed) {
                ESC_SWALLOWED.store(true, Ordering::Relaxed);
                send(Input::Escape);
                return swallow;
            }
        } else if up && ESC_SWALLOWED.swap(false, Ordering::Relaxed) {
            return swallow;
        }
        return event;
    }

    match trigger.modifier() {
        None if key == KEY_KEYPAD_DIVIDE => {
            if down {
                if TRIGGER.load(Ordering::Relaxed) || repeat {
                    return swallow; // key repeat
                }
                if flags & kCGEventFlagMaskControl != 0 {
                    TRIGGER.store(true, Ordering::Relaxed);
                    send(Input::Press);
                    return swallow;
                }
            } else if up && TRIGGER.swap(false, Ordering::Relaxed) {
                send(Input::Release);
                return swallow;
            }
        }
        Some(_) if down && TRIGGER.load(Ordering::Relaxed) && !ABORTED.load(Ordering::Relaxed) => {
            // A chord (Option+L, ⌘C): the modifier is just a modifier.
            ABORTED.store(true, Ordering::Relaxed);
            send(Input::Escape);
        }
        _ => {}
    }
    event
}

/// Installs the tap on a dedicated thread that only runs its run loop, so a
/// busy main thread can never delay key handling.
pub fn start(tx: Sender<KeyEvent>) {
    let _ = TX.set(tx);
    std::thread::Builder::new()
        .name("hook".into())
        .spawn(|| unsafe {
            let mask = (1u64 << kCGEventKeyDown) | (1u64 << kCGEventKeyUp) | (1u64 << kCGEventFlagsChanged);
            let mut asked = false;
            let mut noticed = false;
            let port = loop {
                if crate::mac::accessibility_trusted(!asked) {
                    let port = CGEventTapCreate(
                        kCGSessionEventTap,
                        kCGHeadInsertEventTap,
                        kCGEventTapOptionDefault,
                        mask,
                        callback,
                        std::ptr::null_mut(),
                    );
                    if !port.is_null() {
                        break port;
                    }
                }
                asked = true;
                if !noticed {
                    noticed = true;
                    log::warn!("keyboard tap unavailable: Accessibility access not granted yet");
                    crate::overlay::notice("Allow chatty in System Settings › Privacy & Security › Accessibility");
                }
                sleep_ms(2000);
            };
            let _ = TAP.set(Tap(port));
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
            log::info!("keyboard tap installed");
            if noticed {
                crate::overlay::notice("chatty is ready: hold the trigger to dictate");
            }
            CFRunLoopRun();
        })
        .expect("spawn hook thread");
}
