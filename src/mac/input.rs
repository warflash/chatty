//! Synthesized keys and frontmost-app queries (macOS).

use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::mac::sys::*;

const KEY_V: u16 = 0x09;
const KEY_RETURN: u16 = 0x24;

/// Posts a key press and release, tagged so the tap lets it through.
fn tap_key(code: u16, flags: u64) -> Result<(), String> {
    unsafe {
        for down in [true, false] {
            let e = CGEventCreateKeyboardEvent(std::ptr::null_mut(), code, down);
            if e.is_null() {
                return Err("CGEventCreateKeyboardEvent failed".into());
            }
            CGEventSetFlags(e, flags);
            CGEventSetIntegerValueField(e, kCGEventSourceUserData, crate::hook::OUR_INPUT);
            CGEventPost(kCGHIDEventTap, e);
            CFRelease(e as CFTypeRef);
        }
    }
    Ok(())
}

/// ⌘V (named after the Windows chord the engine sends).
pub fn ctrl_v() -> Result<(), String> {
    tap_key(KEY_V, kCGEventFlagMaskCommand)
}

pub fn enter() -> Result<(), String> {
    tap_key(KEY_RETURN, 0)
}

/// Waits until no trigger key is physically down, so a synthesized ⌘V or
/// Return never combines with the trigger's modifiers. Returns how long it
/// waited; gives up after `limit`.
pub fn wait_trigger_released(limit: Duration) -> Duration {
    let start = Instant::now();
    while crate::hook::trigger_keys_down() && start.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(2));
    }
    if crate::hook::trigger_keys_down() {
        log::warn!("trigger keys still down after {limit:?}; sending anyway");
    }
    start.elapsed()
}

/// What has keyboard focus, as far as pasting is concerned.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Editable(String),
    /// Clearly not a text field (Finder's desktop, a list, a button, ...).
    NotEditable(String),
    /// Could not tell (no accessibility support, a browser or Electron app
    /// without its tree, timeout): paste anyway.
    Unknown(String),
}

/// Terminals take ⌘V; their text views report as editable anyway, but be
/// sure.
const PASTE_APPS: &[&str] = &["Terminal", "iTerm2", "Alacritty", "kitty", "WezTerm", "Ghostty", "Warp"];

/// Browser and Electron surfaces may expose no tree until a screen reader
/// asks; their focused element says nothing about the caret.
const OPAQUE_ROLES: &[&str] = &["AXWebArea", "AXGroup", "AXWindow", "AXScrollArea", "AXUnknown"];

const NOT_TEXT: &[&str] = &[
    "AXList",
    "AXRow",
    "AXCell",
    "AXOutline",
    "AXTable",
    "AXBrowser",
    "AXButton",
    "AXMenuButton",
    "AXPopUpButton",
    "AXMenuItem",
    "AXMenu",
    "AXMenuBar",
    "AXMenuBarItem",
    "AXImage",
    "AXLink",
    "AXStaticText",
    "AXTabGroup",
    "AXRadioButton",
    "AXCheckBox",
    "AXToolbar",
    "AXSlider",
    "AXDisclosureTriangle",
];

fn copy_attr(el: AXUIElementRef, name: &str) -> Option<Cf> {
    let attr = Cf::str(name);
    let mut v: CFTypeRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(el, attr.as_ptr(), &mut v) };
    if err != kAXErrorSuccess {
        return None;
    }
    Cf::new(v)
}

fn settable(el: AXUIElementRef, name: &str) -> Option<bool> {
    let attr = Cf::str(name);
    let mut b: Boolean = 0;
    (unsafe { AXUIElementIsAttributeSettable(el, attr.as_ptr(), &mut b) } == kAXErrorSuccess).then_some(b != 0)
}

/// Asks the accessibility API whether the focused element takes text.
pub fn focus_target() -> Target {
    let app = foreground_app();
    if PASTE_APPS.contains(&app.as_str()) {
        return Target::Editable(format!("terminal {app}"));
    }
    if !crate::mac::accessibility_trusted(false) {
        return Target::Unknown("no Accessibility access".into());
    }
    let system = unsafe { AXUIElementCreateSystemWide() };
    let Some(system) = Cf::new(system) else { return Target::Unknown("no system element".into()) };
    unsafe { AXUIElementSetMessagingTimeout(system.as_ptr(), 0.2) };
    let Some(el) = copy_attr(system.as_ptr(), "AXFocusedUIElement") else {
        return if app == "Finder" { Target::NotEditable("Finder (desktop)".into()) } else { Target::Unknown(format!("no focused element in {app}")) };
    };
    let role = copy_attr(el.as_ptr(), "AXRole").and_then(|r| string(r.as_ptr())).unwrap_or_default();
    let subrole = copy_attr(el.as_ptr(), "AXSubrole").and_then(|r| string(r.as_ptr())).unwrap_or_default();
    let what = format!("role {role} {subrole} in {app}");
    if matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField") {
        return Target::Editable(what);
    }
    if settable(el.as_ptr(), "AXValue") == Some(true) && role != "AXSlider" {
        return Target::Editable(what);
    }
    // Web content: an editable element (contenteditable, inputs inside a
    // web area) carries AXEditable or a selected text range.
    if copy_attr(el.as_ptr(), "AXSelectedTextRange").is_some() && settable(el.as_ptr(), "AXSelectedText") == Some(true) {
        return Target::Editable(what);
    }
    if OPAQUE_ROLES.contains(&role.as_str()) {
        return Target::Unknown(what);
    }
    if NOT_TEXT.contains(&role.as_str()) {
        return Target::NotEditable(what);
    }
    Target::Unknown(what)
}

fn workspace() -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSWorkspace), sharedWorkspace] }
}

fn app_name(app: &AnyObject) -> Option<String> {
    let name: Option<Retained<AnyObject>> = unsafe { msg_send![app, localizedName] };
    name.map(|n| crate::mac::rust_string(&n))
}

/// Name of the frontmost app ("Code", "Safari").
pub fn foreground_app() -> String {
    objc2::rc::autoreleasepool(|_| {
        let app: Option<Retained<AnyObject>> = unsafe { msg_send![&*workspace(), frontmostApplication] };
        app.and_then(|a| app_name(&a)).unwrap_or_default()
    })
}

/// App name of a process id.
pub fn process_name(pid: u32) -> Option<String> {
    objc2::rc::autoreleasepool(|_| {
        let app: Option<Retained<AnyObject>> =
            unsafe { msg_send![class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid as i32] };
        app.and_then(|a| app_name(&a))
    })
}

/// macOS has no integrity levels that drop synthesized keys; secure input
/// (password fields) blocks the event tap instead, not posting.
pub fn foreground_blocks_input() -> bool {
    false
}
