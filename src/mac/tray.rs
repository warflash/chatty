//! Menu bar item and its menu (macOS counterpart of the tray icon).

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::NSSize;

use crate::mac::sys::*;
use crate::mac::{Id, nsstring, on_main};
use crate::{config, models};

const ID_MODEL: isize = 1000;
const ID_CLEANUP_MODEL: isize = 1100;
const ID_LANG: isize = 1200;
const ID_CLEANUP_TOGGLE: isize = 1300;
const ID_STATS: isize = 1301;
const ID_CONFIG: isize = 1302;
const ID_LOG: isize = 1303;
const ID_FOLDER: isize = 1304;
const ID_QUIT: isize = 1305;
const ID_OVERLAY: isize = 1306;
const ID_OPEN: isize = 1307;
const ID_SETTINGS: isize = 1308;const ID_COPY_LAST: isize = 1310;
const ID_UPDATE: isize = 1311;

const LANGUAGES: &[(&str, &str)] = &[("auto", "Auto-detect"), ("en", "English"), ("de", "German")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrayState {
    Idle = 0,
    Recording = 1,
    Busy = 2,
    Loading = 3,
    Error = 4,
}

static STATE: AtomicU8 = AtomicU8::new(TrayState::Loading as u8);
static TOOLTIP: Mutex<String> = Mutex::new(String::new());
static CFG: OnceLock<config::Shared> = OnceLock::new();

thread_local! {
    static ITEM: RefCell<Option<(Id, Id)>> = const { RefCell::new(None) }; // (status item, target)
    static ICONS: RefCell<Vec<Id>> = const { RefCell::new(Vec::new()) };
}

pub fn set_state(s: TrayState) {
    use crate::status::{Phase, set_phase};
    set_phase(match s {
        TrayState::Idle => Phase::Idle,
        TrayState::Recording => Phase::Recording,
        TrayState::Busy => Phase::Transcribing,
        TrayState::Loading => Phase::Loading,
        TrayState::Error => Phase::Error,
    });
    if STATE.swap(s as u8, Ordering::Relaxed) != s as u8 {
        TOOLTIP.lock().unwrap().clear();
        on_main(update);
    }
}

pub fn set_tooltip(s: &str) {
    *TOOLTIP.lock().unwrap() = s.to_string();
    on_main(update);
}

fn state() -> TrayState {
    match STATE.load(Ordering::Relaxed) {
        0 => TrayState::Idle,
        1 => TrayState::Recording,
        2 => TrayState::Busy,
        3 => TrayState::Loading,
        _ => TrayState::Error,
    }
}

/// 18 pt anti-aliased circle at 2x: filled while active, a ring with a dot
/// otherwise. The idle icon is a template, so it follows the menu bar.
fn make_icon(rgb: (u8, u8, u8), filled: bool, template: bool) -> Option<Id> {
    const N: usize = 36;
    unsafe {
        let cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        let ctx = CGBitmapContextCreate(std::ptr::null_mut(), N, N, 8, 0, cs, kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
        CGColorSpaceRelease(cs);
        if ctx.is_null() {
            return None;
        }
        let (r, g, b) = (rgb.0 as f64 / 255.0, rgb.1 as f64 / 255.0, rgb.2 as f64 / 255.0);
        CGContextSetRGBFillColor(ctx, r, g, b, 1.0);
        let c = N as f64 / 2.0;
        let circle = |rad: f64| CGRect::new(c - rad, c - rad, 2.0 * rad, 2.0 * rad);
        if filled {
            CGContextFillEllipseInRect(ctx, circle(12.0));
        } else {
            CGContextSetRGBStrokeColor(ctx, r, g, b, 1.0);
            CGContextSetLineWidth(ctx, 3.0);
            CGContextStrokeEllipseInRect(ctx, circle(10.5));
            CGContextFillEllipseInRect(ctx, circle(4.5));
        }
        let img = CGBitmapContextCreateImage(ctx);
        CGContextRelease(ctx);
        if img.is_null() {
            return None;
        }
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
        let ns: Option<Id> = msg_send![alloc, initWithCGImage: img as *mut AnyObject, size: NSSize::new(18.0, 18.0)];
        CGImageRelease(img);
        if let Some(ns) = &ns {
            let _: () = msg_send![&**ns, setTemplate: template];
        }
        ns
    }
}

fn icon_for(s: TrayState) -> Option<Id> {
    ICONS.with(|icons| {
        let mut icons = icons.borrow_mut();
        if icons.is_empty() {
            for (rgb, filled, template) in [
                ((0, 0, 0), false, true),
                ((232, 59, 59), true, false),
                ((245, 165, 30), true, false),
                ((59, 138, 208), false, false),
                ((150, 40, 40), false, false),
            ] {
                if let Some(i) = make_icon(rgb, filled, template) {
                    icons.push(i);
                }
            }
        }
        icons.get(s as usize).cloned()
    })
}

fn tooltip() -> String {
    let tip = TOOLTIP.lock().unwrap().clone();
    if !tip.is_empty() {
        return tip;
    }
    let model = CFG.get().map(|c| c.get().model.clone()).unwrap_or_default();
    match state() {
        TrayState::Idle => format!("chatty: {model}"),
        TrayState::Recording => "chatty: recording".into(),
        TrayState::Busy => "chatty: transcribing".into(),
        TrayState::Loading => format!("chatty: loading {model}"),
        TrayState::Error => "chatty: error (see log)".into(),
    }
}

/// Main thread: icon and tooltip follow the state.
fn update() {
    ITEM.with(|item| {
        let item = item.borrow();
        let Some((status, _)) = item.as_ref() else { return };
        unsafe {
            let button: Option<Id> = msg_send![&**status, button];
            let Some(button) = button else { return };
            if let Some(icon) = icon_for(state()) {
                let _: () = msg_send![&*button, setImage: &*icon];
            }
            let _: () = msg_send![&*button, setToolTip: &*nsstring(&tooltip())];
        }
    });
}

fn add(menu: &AnyObject, target: &AnyObject, title: &str, tag: isize, checked: bool, enabled: bool) -> Id {
    unsafe {
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
        let item: Id = msg_send![alloc, initWithTitle: &*nsstring(title), action: sel!(menuAction:), keyEquivalent: &*nsstring("")];
        let _: () = msg_send![&*item, setTarget: target];
        let _: () = msg_send![&*item, setTag: tag];
        let _: () = msg_send![&*item, setState: if checked { 1isize } else { 0isize }];
        let _: () = msg_send![&*item, setEnabled: enabled];
        let _: () = msg_send![menu, addItem: &*item];
        item
    }
}

fn separator(menu: &AnyObject) {
    unsafe {
        let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: &*sep];
    }
}

fn new_menu() -> Id {
    unsafe {
        let m: Id = msg_send![class!(NSMenu), new];
        let _: () = msg_send![&*m, setAutoenablesItems: false];
        m
    }
}

fn submenu(menu: &AnyObject, title: &str, sub: &AnyObject) {
    unsafe {
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
        let item: Id = msg_send![alloc, initWithTitle: &*nsstring(title), action: None::<Sel>, keyEquivalent: &*nsstring("")];
        let _: () = msg_send![&*item, setSubmenu: sub];
        let _: () = msg_send![menu, addItem: &*item];
    }
}

/// Rebuilds the menu each time it opens (models download, config changes).
fn rebuild(menu: &AnyObject, target: &AnyObject) {
    let Some(cfg) = CFG.get().map(|c| c.get()) else { return };
    unsafe {
        let _: () = msg_send![menu, removeAllItems];
    }
    add(menu, target, &format!("chatty {}", env!("CARGO_PKG_VERSION")), 0, false, false);
    separator(menu);

    // The last transcript; clicking copies it (the Windows flyout's card).
    match crate::status::get().last {
        Some((text, at_ms)) => {
            add(menu, target, &format!("Last transcript · {}", ago(at_ms, now_ms())), 0, false, false);
            let item = add(menu, target, &preview(&text, 48), ID_COPY_LAST, false, true);
            unsafe {
                let _: () = msg_send![&*item, setToolTip: &*nsstring("Copy to the clipboard")];
            }
        }
        None => {
            add(menu, target, "No transcript yet", 0, false, false);
        }
    }
    separator(menu);

    let models_menu = new_menu();
    for (i, m) in models::ASR.iter().enumerate() {
        let label = if m.asset.is_ready() { m.name.to_string() } else { format!("{}  (download {} MB)", m.name, m.asset.size >> 20) };
        add(&models_menu, target, &label, ID_MODEL + i as isize, m.id == cfg.model, true);
    }
    submenu(menu, "Model", &models_menu);
    let lang_menu = new_menu();
    for (i, (code, name)) in LANGUAGES.iter().enumerate() {
        add(&lang_menu, target, name, ID_LANG + i as isize, cfg.language == *code, true);
    }
    submenu(menu, "Language", &lang_menu);
    let cleanup_menu = new_menu();
    add(&cleanup_menu, target, "Enabled", ID_CLEANUP_TOGGLE, cfg.cleanup.enabled, true);
    separator(&cleanup_menu);
    for (i, m) in models::CLEANUP.iter().enumerate() {
        let label = if m.asset.is_ready() { m.name.to_string() } else { format!("{}  (download {} MB)", m.name, m.asset.size >> 20) };
        add(&cleanup_menu, target, &label, ID_CLEANUP_MODEL + i as isize, m.id == cfg.cleanup.model, true);
    }
    submenu(menu, "Cleanup (LLM)", &cleanup_menu);
    add(menu, target, "Live overlay", ID_OVERLAY, cfg.overlay, true);
    separator(menu);
    add(menu, target, "Open chatty", ID_OPEN, false, true);
    add(menu, target, "Statistics", ID_STATS, false, true);
    add(menu, target, "Settings…", ID_SETTINGS, false, true);
    add(menu, target, "Open config.toml", ID_CONFIG, false, true);
    add(menu, target, "Open log", ID_LOG, false, true);
    add(menu, target, "Open data folder", ID_FOLDER, false, true);
    match crate::update::state() {
        crate::update::State::Available(r) | crate::update::State::Failed(r, _) => {
            separator(menu);
            add(menu, target, &format!("Update to {}", r.version), ID_UPDATE, false, true);
        }
        crate::update::State::Installing(r, f) => {
            separator(menu);
            add(menu, target, &format!("Installing {}  {:.0}%", r.version, f * 100.0), 0, false, false);
        }
        _ => {}
    }
    if !crate::mac::accessibility_trusted(false) {
        separator(menu);
        add(menu, target, "Grant Accessibility access…", ID_ACCESS, false, true);
    }
    separator(menu);
    add(menu, target, "Quit chatty", ID_QUIT, false, true);
}

const ID_ACCESS: isize = 1309;

fn handle(id: isize) {
    let Some(cfg) = CFG.get() else { return };
    match id {
        ID_STATS => crate::ui::open(Some(crate::ui::Page::Stats)),
        ID_OPEN => crate::ui::open(None),
        ID_SETTINGS => crate::ui::open(Some(crate::ui::Page::Settings)),
        ID_CONFIG => crate::mac::open_path(&cfg.path),
        ID_LOG => crate::mac::open_path(&crate::paths::log_path()),
        ID_FOLDER => crate::mac::open_path(&crate::paths::local_dir()),
        ID_ACCESS => {
            crate::mac::accessibility_trusted(true);
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                .spawn();
        }
        ID_QUIT => crate::mac::quit(),
        ID_COPY_LAST => {
            if let Some((text, _)) = crate::status::get().last {
                match crate::clipboard::set_text(&text) {
                    Ok(_) => crate::overlay::notice("Copied"),
                    Err(e) => log::warn!("copy: {e}"),
                }
            }
        }
        ID_UPDATE => crate::update::install(),
        ID_CLEANUP_TOGGLE => cfg.set(&[("cleanup.enabled", (!cfg.get().cleanup.enabled).into())]),
        ID_OVERLAY => cfg.set(&[("overlay", (!cfg.get().overlay).into())]),
        i if (ID_MODEL..ID_MODEL + models::ASR.len() as isize).contains(&i) => {
            cfg.set(&[("model", models::ASR[(i - ID_MODEL) as usize].id.into())]);
        }
        i if (ID_CLEANUP_MODEL..ID_CLEANUP_MODEL + models::CLEANUP.len() as isize).contains(&i) => {
            cfg.set(&[("cleanup.model", models::CLEANUP[(i - ID_CLEANUP_MODEL) as usize].id.into())]);
        }
        i if (ID_LANG..ID_LANG + LANGUAGES.len() as isize).contains(&i) => {
            cfg.set(&[("language", LANGUAGES[(i - ID_LANG) as usize].0.into())]);
        }
        _ => {}
    }
}

extern "C-unwind" fn menu_action(_this: *mut AnyObject, _sel: Sel, sender: *mut AnyObject) {
    let Some(sender) = (unsafe { sender.as_ref() }) else { return };
    let tag: isize = unsafe { msg_send![sender, tag] };
    handle(tag);
}

extern "C-unwind" fn menu_needs_update(this: *mut AnyObject, _sel: Sel, menu: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    if let Some(menu) = unsafe { menu.as_ref() } {
        rebuild(menu, this);
    }
}

fn target_class() -> &'static AnyClass {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut b = ClassBuilder::new(c"ChattyMenuTarget", class!(NSObject)).expect("ChattyMenuTarget");
        unsafe {
            b.add_method(sel!(menuAction:), menu_action as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject));
            b.add_method(sel!(menuNeedsUpdate:), menu_needs_update as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject));
        }
        if let Some(p) = objc2::runtime::AnyProtocol::get(c"NSMenuDelegate") {
            b.add_protocol(p);
        }
        b.register();
    });
    AnyClass::get(c"ChattyMenuTarget").expect("registered")
}

/// Creates the menu bar item. Main thread only.
pub fn create(cfg: config::Shared) {
    let _ = CFG.set(cfg);
    unsafe {
        let bar: Id = msg_send![class!(NSStatusBar), systemStatusBar];
        // NSVariableStatusItemLength
        let status: Id = msg_send![&*bar, statusItemWithLength: -1.0f64];
        let target: Id = msg_send![target_class(), new];
        let menu = new_menu();
        let _: () = msg_send![&*menu, setDelegate: &*target];
        rebuild(&menu, &target);
        let _: () = msg_send![&*status, setMenu: &*menu];
        ITEM.with(|i| *i.borrow_mut() = Some((status, target)));
    }
    update();
}

/// One line of `text`: whitespace collapsed, cut at a word after `max` chars.
fn preview(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end_matches([',', '.', ';', ':']))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn ago(at_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - at_ms).max(0) / 1000;
    match s {
        0..45 => "just now".into(),
        45..90 => "1 min ago".into(),
        90..3600 => format!("{} min ago", (s + 30) / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_is_one_short_line() {
        assert_eq!(preview("  hello\n\n world ", 48), "hello world");
        assert_eq!(preview("one two three four", 10), "one two…");
        assert_eq!(preview("", 10), "");
    }

    #[test]
    fn ago_buckets() {
        let m = 60_000;
        assert_eq!(ago(0, 10_000), "just now");
        assert_eq!(ago(0, 7 * m), "7 min ago");
        assert_eq!(ago(0, 3 * 60 * m), "3 h ago");
        assert_eq!(ago(10, 0), "just now");
    }
}
