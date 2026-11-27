//! `WH_KEYBOARD_LL` hook on its own thread. The callback only classifies the
//! key, flips an atomic, enqueues an event and returns. The trigger
//! (Ctrl+Numpad /) and, while recording, Escape are swallowed so apps never
//! see them. Injected input (ours included) passes through untouched.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_DIVIDE, VK_ESCAPE, VK_LCONTROL, VK_RCONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    SetWindowsHookExW, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::coordinator::Input;

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub input: Input,
    pub at: Instant,
}

static LCTRL: AtomicBool = AtomicBool::new(false);
static RCTRL: AtomicBool = AtomicBool::new(false);
static TRIGGER: AtomicBool = AtomicBool::new(false);
static ESC_SWALLOWED: AtomicBool = AtomicBool::new(false);
/// Set by the coordinator while armed or recording.
pub static ESCAPE_WANTED: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<Sender<KeyEvent>> = OnceLock::new();
/// `CHATTY_ACCEPT_INJECTED=1`: treat synthesized trigger keys as real (for
/// end-to-end tests). Ctrl+V and Enter that chatty sends itself still pass.
static ACCEPT_INJECTED: AtomicBool = AtomicBool::new(false);

/// True while any trigger key is physically down.
pub fn trigger_keys_down() -> bool {
    let ctrl = (LCTRL.load(Ordering::Relaxed) || RCTRL.load(Ordering::Relaxed))
        // Cross-check: a missed key-up (e.g. across the secure desktop) must
        // not block pasting forever.
        && unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
    ctrl || TRIGGER.load(Ordering::Relaxed)
}

fn send(input: Input) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeyEvent { input, at: Instant::now() });
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        let ours = kb.dwExtraInfo == crate::input::OUR_INPUT;
        if !injected || (ACCEPT_INJECTED.load(Ordering::Relaxed) && !ours) {
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            let vk = kb.vkCode as u16;
            if vk == VK_LCONTROL.0 {
                LCTRL.store(down, Ordering::Relaxed);
            } else if vk == VK_RCONTROL.0 {
                RCTRL.store(down, Ordering::Relaxed);
            } else if vk == VK_DIVIDE.0 {
                let ctrl = LCTRL.load(Ordering::Relaxed) || RCTRL.load(Ordering::Relaxed);
                if down {
                    if TRIGGER.load(Ordering::Relaxed) {
                        return LRESULT(1); // key repeat
                    }
                    if ctrl {
                        TRIGGER.store(true, Ordering::Relaxed);
                        send(Input::Press);
                        return LRESULT(1);
                    }
                } else if up && TRIGGER.swap(false, Ordering::Relaxed) {
                    send(Input::Release);
                    return LRESULT(1);
                }
            } else if vk == VK_ESCAPE.0 {
                if down {
                    if ESC_SWALLOWED.load(Ordering::Relaxed) {
                        return LRESULT(1);
                    }
                    if ESCAPE_WANTED.load(Ordering::Relaxed) {
                        ESC_SWALLOWED.store(true, Ordering::Relaxed);
                        send(Input::Escape);
                        return LRESULT(1);
                    }
                } else if up && ESC_SWALLOWED.swap(false, Ordering::Relaxed) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Installs the hook on a dedicated thread that does nothing but pump
/// messages, so a busy UI thread can never delay key handling.
pub fn start(tx: Sender<KeyEvent>) {
    let _ = TX.set(tx);
    if std::env::var_os("CHATTY_ACCEPT_INJECTED").is_some() {
        log::warn!("accepting injected trigger keys (test mode)");
        ACCEPT_INJECTED.store(true, Ordering::Relaxed);
    }
    std::thread::Builder::new()
        .name("hook".into())
        .spawn(|| unsafe {
            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                Ok(h) => h,
                Err(e) => {
                    log::error!("keyboard hook failed: {e}");
                    return;
                }
            };
            log::info!("keyboard hook installed ({:?})", hook.0);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
        })
        .expect("spawn hook thread");
}
