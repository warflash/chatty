//! macOS host of the main window: an `NSWindow` whose content view forwards
//! mouse and keyboard input to the immediate-mode frame and shows the
//! rendered bitmap as its layer's contents. Main thread only, except the
//! notification entry points.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::gfx::Gfx;
use super::{APP, App, Page, frame, init, reload_report, run_actions, theme, with_app};
use crate::mac::sys::CGImageRelease;
use crate::mac::{Id, after_main, nsstring, on_main};
use crate::{config, stats};

static STANDALONE: AtomicBool = AtomicBool::new(false);
static NOTIFY_PENDING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// (window, content view)
    static WIN: RefCell<Option<(Id, Id)>> = const { RefCell::new(None) };
    static TIMER: Cell<bool> = const { Cell::new(false) };
    /// A draw is queued on the main queue; further events only update input.
    static QUEUED: Cell<bool> = const { Cell::new(false) };
}

const MIN_W: f64 = 860.0;
const MIN_H: f64 = 560.0;

fn window() -> Option<(Id, Id)> {
    WIN.with(|w| w.borrow().clone())
}

fn visible() -> bool {
    window().is_some_and(|(w, _)| unsafe { msg_send![&*w, isVisible] })
}

/// Live status changed (any thread).
pub fn notify() {
    if !NOTIFY_PENDING.swap(true, Ordering::AcqRel) {
        on_main(|| {
            NOTIFY_PENDING.store(false, Ordering::Release);
            if visible() {
                draw();
            }
        });
    }
}

/// The statistics database changed (any thread).
pub fn stats_changed() {
    on_main(|| {
        if window().is_some()
            && let Some((r, l)) = with_app(|a| (a.report.clone(), a.loading.clone()))
        {
            reload_report(r, l);
        }
    });
}

/// Repaint request from any thread (e.g. a finished report).
pub fn redraw() {
    on_main(|| {
        if visible() {
            draw();
        }
    });
}

pub fn open_path(p: &std::path::Path) {
    crate::mac::open_path(p);
}

thread_local! {
    static SOUND: RefCell<Option<Id>> = const { RefCell::new(None) };
}

pub fn play(p: &str) {
    unsafe {
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSSound), alloc];
        let sound: Option<Id> = msg_send![alloc, initWithContentsOfFile: &*nsstring(p), byReference: true];
        if let Some(s) = sound {
            let _: Bool = msg_send![&*s, play];
            // Keep it alive while it plays (replaced by the next one).
            SOUND.with(|c| *c.borrow_mut() = Some(s));
        } else {
            log::warn!("cannot play {p}");
        }
    }
}

/// Draws once after the events already queued are handled. A trackpad sends
/// scroll and move events faster than a frame draws; drawing for each one
/// let the page fall behind the fingers.
fn request_draw() {
    if !QUEUED.with(|q| q.replace(true)) {
        on_main(|| {
            QUEUED.with(|q| q.set(false));
            if visible() {
                draw();
            }
        });
    }
}

fn schedule_tick() {
    if !TIMER.with(|t| t.replace(true)) {
        after_main(16, || {
            TIMER.with(|t| t.set(false));
            if visible() {
                draw();
            }
        });
    }
}

/// Draws one frame into the view's layer.
fn draw() {
    let Some((win, view)) = window() else { return };
    let (bounds, scale): (NSRect, f64) = unsafe { (msg_send![&*view, bounds], msg_send![&*win, backingScaleFactor]) };
    let scale = scale.max(1.0);
    let (w, h) = (bounds.size.width, bounds.size.height);
    let (wpx, hpx) = ((w * scale).round() as usize, (h * scale).round() as usize);
    let dpi = (96.0 * scale) as f32;
    let result = with_app(|a| {
        match a.gfx.as_mut() {
            Some(g) => g.resize(wpx, hpx),
            None => a.gfx = Gfx::new(wpx, hpx, dpi),
        }
        a.gfx.as_ref()?;
        let actions = frame(a, w as f32, h as f32, dpi);
        let image = a.gfx.as_ref().map(|g| g.image()).unwrap_or(std::ptr::null_mut());
        Some((actions, a.animating, a.mem.hand, a.mem.ibeam, image))
    })
    .flatten();
    let Some((actions, animating, hand, ibeam, image)) = result else { return };
    unsafe {
        let layer: Option<Id> = msg_send![&*view, layer];
        if let Some(layer) = layer
            && !image.is_null()
        {
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: true];
            let _: () = msg_send![&*layer, setContentsScale: scale];
            let _: () = msg_send![&*layer, setContents: image as *mut AnyObject];
            let _: () = msg_send![class!(CATransaction), commit];
        }
        if !image.is_null() {
            CGImageRelease(image);
        }
        let cursor: Id = if ibeam {
            msg_send![class!(NSCursor), IBeamCursor]
        } else if hand {
            msg_send![class!(NSCursor), pointingHandCursor]
        } else {
            msg_send![class!(NSCursor), arrowCursor]
        };
        let _: () = msg_send![&*cursor, set];
    }
    let had_actions = !actions.is_empty();
    run_actions(actions);
    if animating || had_actions {
        schedule_tick();
    }
}

// ------------------------------------------------------------------ view --

fn mouse_pos(view: &AnyObject, event: *mut AnyObject) -> (f32, f32) {
    let Some(event) = (unsafe { event.as_ref() }) else { return (-1.0, -1.0) };
    unsafe {
        let p: NSPoint = msg_send![event, locationInWindow];
        let v: NSPoint = msg_send![view, convertPoint: p, fromView: std::ptr::null::<AnyObject>()];
        (v.x as f32, v.y as f32)
    }
}

extern "C-unwind" fn yes(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

extern "C-unwind" fn yes1(_this: *mut AnyObject, _sel: Sel, _e: *mut AnyObject) -> Bool {
    Bool::YES
}

extern "C-unwind" fn mouse_down(this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    let p = mouse_pos(this, e);
    with_app(|a| {
        a.input.mouse = p;
        a.input.down = true;
        a.input.pressed = true;
    });
    draw();
}

extern "C-unwind" fn mouse_up(this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    let p = mouse_pos(this, e);
    with_app(|a| {
        a.input.mouse = p;
        a.input.down = false;
        a.input.released = true;
    });
    draw();
}

extern "C-unwind" fn mouse_moved(this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    let p = mouse_pos(this, e);
    with_app(|a| a.input.mouse = p);
    request_draw();
}

extern "C-unwind" fn mouse_exited(_this: *mut AnyObject, _sel: Sel, _e: *mut AnyObject) {
    with_app(|a| a.input.mouse = (-1.0, -1.0));
    draw();
}

extern "C-unwind" fn scroll_wheel(_this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let Some(e) = (unsafe { e.as_ref() }) else { return };
    let (dy, precise): (f64, Bool) = unsafe { (msg_send![e, scrollingDeltaY], msg_send![e, hasPreciseScrollingDeltas]) };
    // The frame scrolls `wheel * 1.2` DIPs (Windows: 120 per notch).
    let wheel = if precise.as_bool() { dy / 1.2 } else { dy * 120.0 };
    with_app(|a| {
        a.input.wheel += wheel as f32;
        a.input.wheel_direct = precise.as_bool();
    });
    request_draw();
}

/// macOS key codes mapped to the Windows virtual keys the widgets read.
fn vk(code: u16) -> Option<u16> {
    Some(match code {
        51 => 0x08,       // delete (backspace)
        117 => 0x2E,      // forward delete
        123 => 0x25,      // left
        124 => 0x27,      // right
        115 => 0x24,      // home
        119 => 0x23,      // end
        36 | 76 => 0x0D,  // return, keypad enter
        53 => 0x1B,       // escape
        116 => 0x21,      // page up
        121 => 0x22,      // page down
        96 => 0x74,       // F5
        _ => return None,
    })
}

extern "C-unwind" fn key_down(_this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let Some(e) = (unsafe { e.as_ref() }) else { return };
    let (code, flags): (u16, usize) = unsafe { (msg_send![e, keyCode], msg_send![e, modifierFlags]) };
    let chars: Option<Id> = unsafe { msg_send![e, charactersIgnoringModifiers] };
    let chars = chars.map(|c| crate::mac::rust_string(&c)).unwrap_or_default();
    let typed: Option<Id> = unsafe { msg_send![e, characters] };
    let typed = typed.map(|c| crate::mac::rust_string(&c)).unwrap_or_default();
    let cmd = flags & (1 << 20) != 0;
    if cmd {
        match chars.as_str() {
            "v" => {
                with_app(|a| a.input.chars.push('\u{16}'));
            }
            "w" => {
                if let Some((w, _)) = window() {
                    let _: () = unsafe { msg_send![&*w, performClose: std::ptr::null::<AnyObject>()] };
                }
                return;
            }
            "r" => key_vk(0x74),
            _ => return,
        }
        draw();
        return;
    }
    if let Some(k) = vk(code) {
        key_vk(k);
    } else {
        let cs: Vec<char> = typed.chars().filter(|c| !('\u{F700}'..='\u{F8FF}').contains(c)).collect();
        with_app(|a| a.input.chars.extend(cs));
    }
    draw();
}

/// The Windows host's WM_KEYDOWN handling: widgets get the key; without a
/// focused field, F5 reloads and paging keys scroll.
fn key_vk(vk: u16) {
    with_app(|a| {
        a.input.keys.push(vk);
        if a.mem.focus == 0 {
            if vk == 0x74 {
                a.input.keys.clear();
                reload_report(a.report.clone(), a.loading.clone());
            }
            match vk {
                0x21 => a.scroll_target -= a.viewport_h * 0.9,
                0x22 => a.scroll_target += a.viewport_h * 0.9,
                0x24 => a.scroll_target = 0.0,
                0x23 => a.scroll_target = a.content_h,
                _ => {}
            }
        }
    });
}

fn view_class() -> &'static AnyClass {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut b = ClassBuilder::new(c"ChattyView", class!(NSView)).expect("ChattyView");
        type Ev = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
        unsafe {
            b.add_method(sel!(isFlipped), yes as extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool);
            b.add_method(sel!(acceptsFirstResponder), yes as extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool);
            b.add_method(sel!(acceptsFirstMouse:), yes1 as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool);
            b.add_method(sel!(mouseDown:), mouse_down as Ev);
            b.add_method(sel!(mouseUp:), mouse_up as Ev);
            b.add_method(sel!(mouseDragged:), mouse_moved as Ev);
            b.add_method(sel!(mouseMoved:), mouse_moved as Ev);
            b.add_method(sel!(mouseExited:), mouse_exited as Ev);
            b.add_method(sel!(scrollWheel:), scroll_wheel as Ev);
            b.add_method(sel!(keyDown:), key_down as Ev);
        }
        b.register();
    });
    AnyClass::get(c"ChattyView").expect("registered")
}

// ------------------------------------------------------- window delegate --

extern "C-unwind" fn should_close(_this: *mut AnyObject, _sel: Sel, win: *mut AnyObject) -> Bool {
    if STANDALONE.load(Ordering::Relaxed) {
        let app = crate::mac::app();
        let _: () = unsafe { msg_send![&app, terminate: std::ptr::null::<AnyObject>()] };
        return Bool::YES;
    }
    if let Some(win) = unsafe { win.as_ref() } {
        let _: () = unsafe { msg_send![win, orderOut: std::ptr::null::<AnyObject>()] };
    }
    // Free the bitmap while hidden; leave the Dock.
    with_app(|a| a.gfx = None);
    crate::mac::set_regular(false);
    Bool::NO
}

extern "C-unwind" fn did_change(_this: *mut AnyObject, _sel: Sel, _n: *mut AnyObject) {
    draw();
}

fn delegate_class() -> &'static AnyClass {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut b = ClassBuilder::new(c"ChattyWindowDelegate", class!(NSObject)).expect("ChattyWindowDelegate");
        type N = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
        unsafe {
            b.add_method(sel!(windowShouldClose:), should_close as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool);
            b.add_method(sel!(windowDidResize:), did_change as N);
            b.add_method(sel!(windowDidChangeBackingProperties:), did_change as N);
            b.add_method(sel!(windowDidBecomeKey:), did_change as N);
        }
        if let Some(p) = objc2::runtime::AnyProtocol::get(c"NSWindowDelegate") {
            b.add_protocol(p);
        }
        b.register();
    });
    AnyClass::get(c"ChattyWindowDelegate").expect("registered")
}

thread_local! {
    static DELEGATE: RefCell<Option<Id>> = const { RefCell::new(None) };
}

fn create_window() -> Option<(Id, Id)> {
    unsafe {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1200.0, 820.0));
        // titled | closable | miniaturizable | resizable
        let style: usize = 1 | 2 | 4 | 8;
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSWindow), alloc];
        let win: Option<Id> = msg_send![alloc, initWithContentRect: rect, styleMask: style, backing: 2usize, defer: false];
        let win = win?;
        let _: () = msg_send![&*win, setReleasedWhenClosed: false];
        let _: () = msg_send![&*win, setTitle: &*nsstring("chatty")];
        let _: () = msg_send![&*win, setContentMinSize: NSSize::new(MIN_W, MIN_H)];
        let _: () = msg_send![&*win, setTitlebarAppearsTransparent: true];
        let c = theme::SIDEBAR;
        let bg: Id = msg_send![class!(NSColor), colorWithSRGBRed: c.0 as f64, green: c.1 as f64, blue: c.2 as f64, alpha: 1.0f64];
        let _: () = msg_send![&*win, setBackgroundColor: &*bg];
        let dark: Option<Id> = msg_send![class!(NSAppearance), appearanceNamed: &*nsstring("NSAppearanceNameDarkAqua")];
        if let Some(dark) = dark {
            let _: () = msg_send![&*win, setAppearance: &*dark];
        }
        let _: () = msg_send![&*win, setFrameAutosaveName: &*nsstring("chatty.main")];

        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![view_class(), alloc];
        let view: Id = msg_send![alloc, initWithFrame: rect];
        let _: () = msg_send![&*view, setWantsLayer: true];
        // NSTrackingMouseEnteredAndExited | MouseMoved | ActiveAlways | InVisibleRect
        let opts: usize = 0x01 | 0x02 | 0x80 | 0x200;
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSTrackingArea), alloc];
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        let area: Id = msg_send![alloc, initWithRect: zero, options: opts, owner: &*view, userInfo: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![&*view, addTrackingArea: &*area];
        let _: () = msg_send![&*win, setContentView: &*view];
        let _: Bool = msg_send![&*win, makeFirstResponder: &*view];
        let _: () = msg_send![&*win, setAcceptsMouseMovedEvents: true];

        let delegate: Id = msg_send![delegate_class(), new];
        let _: () = msg_send![&*win, setDelegate: &*delegate];
        DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));
        let frame: NSRect = msg_send![&*win, frame];
        if frame.origin.x == 0.0 && frame.origin.y == 0.0 {
            let _: () = msg_send![&*win, center];
        }
        Some((win, view))
    }
}

/// Shows the window (creating it on first use) on `page`. Main thread only.
pub fn open(page: Option<Page>) {
    if window().is_none() {
        APP.with(|a| *a.borrow_mut() = Some(App::new()));
        let Some(w) = create_window() else {
            log::error!("cannot create the window");
            return;
        };
        WIN.with(|c| *c.borrow_mut() = Some(w));
    }
    with_app(|a| {
        if let Some(p) = page {
            a.page = p;
            a.scroll = 0.0;
            a.scroll_target = 0.0;
        }
        reload_report(a.report.clone(), a.loading.clone());
    });
    crate::mac::set_regular(true);
    if let Some((win, _)) = window() {
        unsafe {
            let _: () = msg_send![&*win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
    }
    draw();
}

/// `chatty --render <page> <out.png> [w h scale]`: draws a page offscreen
/// with the real statistics, no window.
pub fn render_png(cfg: config::Shared, page: Page, path: &std::path::Path, w: u32, h: u32, scale: f32) -> Result<(), String> {
    objc2::rc::autoreleasepool(|_| {
        let _app = crate::mac::app();
        init(cfg.clone(), None);
        crate::status::set_phase(crate::status::Phase::Idle);
        let dpi = 96.0 * scale;
        let mut app = App::new();
        app.gfx = Some(Gfx::new((w as f32 * scale) as usize, (h as f32 * scale) as usize, dpi).ok_or("bitmap")?);
        app.page = page;
        let c = cfg.get();
        app.report = Arc::new(Mutex::new(Some(stats::open().and_then(|db| stats::report(&db, c.typing_wpm as f64, 60)))));
        app.devices = crate::audio::device_names();
        let _ = frame(&mut app, w as f32, h as f32, dpi);
        app.gfx.as_ref().unwrap().write_png(path)
    })
}

/// `chatty --stats`: the window is the whole process; closing exits.
pub fn run_standalone(cfg: config::Shared, page: Page) -> Result<(), String> {
    STANDALONE.store(true, Ordering::Relaxed);
    let app = crate::mac::app();
    init(cfg, None);
    crate::status::set_phase(crate::status::Phase::Idle);
    open(Some(page));
    if window().is_none() {
        return Err("cannot create the window".into());
    }
    let _: () = unsafe { msg_send![&app, run] };
    Ok(())
}
