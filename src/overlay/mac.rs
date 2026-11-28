//! macOS host of the overlay: a borderless, non-activating panel above
//! everything (all Spaces, full-screen apps too), transparent except for the
//! card, its content view showing the rendered bitmap. Click-through unless
//! a card with buttons is shown. Same layout, easing and timers as the
//! Windows host.

use std::cell::{Cell, RefCell};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::*;
use crate::mac::sys::CGImageRelease;
use crate::mac::{Id, after_main, on_main};

const FRAME_MS: u64 = 33;

static POSTED: AtomicBool = AtomicBool::new(false);

struct Anim {
    g: Option<Gfx>,
    visible: bool,
    shown_at: Instant,
    hiding_since: Option<Instant>,
    last: Option<Frame>,
    level: f32,
    timer: bool,
    geo: Option<Geo>,
    min_w: f32,
    min_h: f32,
    /// Bumped whenever the mode changes; stale hide timers check it.
    hide_gen: u64,
    /// DIP -> pixel scale of the last frame (for hit-testing).
    scale: f32,
}

thread_local! {
    static PANEL: RefCell<Option<(Id, Id)>> = const { RefCell::new(None) };
    static ANIM: RefCell<Anim> = RefCell::new(Anim {
        g: None,
        visible: false,
        shown_at: Instant::now(),
        hiding_since: None,
        last: None,
        level: 0.0,
        timer: false,
        geo: None,
        min_w: 0.0,
        min_h: 0.0,
        hide_gen: 0,
        scale: 2.0,
    });
    static LAST_MODE: RefCell<Option<Mode>> = const { RefCell::new(None) };
    static CLICKABLE: Cell<bool> = const { Cell::new(false) };
}

/// Any thread: the view changed.
pub(super) fn post() {
    if !POSTED.swap(true, Ordering::AcqRel) {
        on_main(|| {
            POSTED.store(false, Ordering::Release);
            arm_hide();
            render();
        });
    }
}

/// Hide timers for notices and cards, restarted when the mode changes.
fn arm_hide() {
    let mode = VIEW.lock().unwrap().mode.clone();
    let changed = LAST_MODE.with(|m| {
        let mut m = m.borrow_mut();
        let changed = m.as_ref() != Some(&mode);
        *m = Some(mode.clone());
        changed
    });
    if !changed {
        return;
    }
    let ms = match &mode {
        Mode::Notice(_) => Some(3500),
        Mode::Card { copied: true, .. } => Some(1200),
        Mode::Card { .. } => Some(120_000),
        _ => None,
    };
    let generation = ANIM.with(|a| {
        let mut a = a.borrow_mut();
        a.hide_gen += 1;
        a.hide_gen
    });
    if let Some(ms) = ms {
        after_main(ms, move || {
            if ANIM.with(|a| a.borrow().hide_gen) != generation {
                return;
            }
            let mut v = VIEW.lock().unwrap();
            if matches!(v.mode, Mode::Notice(_) | Mode::Card { .. }) {
                v.mode = Mode::Hidden;
                drop(v);
                LAST_MODE.with(|m| *m.borrow_mut() = Some(Mode::Hidden));
                render();
            }
        });
    }
}

fn set_timer(anim: &mut Anim, on: bool) {
    if on && !anim.timer {
        anim.timer = true;
        tick();
    } else if !on {
        anim.timer = false;
    }
}

fn tick() {
    after_main(FRAME_MS, || {
        let on = ANIM.with(|a| a.borrow().timer);
        if on {
            render();
            if ANIM.with(|a| a.borrow().timer) {
                tick();
            }
        }
    });
}

fn panel() -> Option<(Id, Id)> {
    PANEL.with(|p| p.borrow().clone())
}

/// The screen with the mouse (where the user is working), else the main one.
fn screen() -> Option<Id> {
    unsafe {
        let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        let screens: Id = msg_send![class!(NSScreen), screens];
        let n: usize = msg_send![&*screens, count];
        for i in 0..n {
            let s: Id = msg_send![&*screens, objectAtIndex: i];
            let f: NSRect = msg_send![&*s, frame];
            if mouse.x >= f.origin.x && mouse.x < f.origin.x + f.size.width && mouse.y >= f.origin.y && mouse.y < f.origin.y + f.size.height {
                return Some(s);
            }
        }
        msg_send![class!(NSScreen), mainScreen]
    }
}

/// Lays out, draws and shows (or hides) the panel. Main thread only.
fn render() {
    let Some((win, view)) = panel() else { return };
    ANIM.with(|cell| {
        let mut anim = cell.borrow_mut();
        let anim = &mut *anim;
        let (mode, live, split, hover) = {
            let v = VIEW.lock().unwrap();
            (v.mode.clone(), v.live.clone(), v.split, v.hover)
        };
        let wanted = match &mode {
            Mode::Live(_) | Mode::Transcribing => ENABLED.load(std::sync::atomic::Ordering::Relaxed),
            Mode::Hidden => false,
            _ => true,
        };
        let now = Instant::now();
        let status = crate::status::get();

        let frame = if wanted {
            if !anim.visible || anim.hiding_since.is_some() {
                anim.shown_at = now;
                anim.hiding_since = None;
            }
            anim.visible = true;
            let target = if matches!(mode, Mode::Live(_)) { status.level } else { 0.0 };
            let k = if target > anim.level { 0.55 } else { 0.18 };
            anim.level += (target - anim.level) * k;
            let elapsed = if status.phase == crate::status::Phase::Recording { status.phase_since.elapsed().as_secs_f32() } else { 0.0 };
            let f = Frame { mode: mode.clone(), live, split, level: anim.level, elapsed, t: (now - anim.shown_at).as_secs_f32(), hover };
            anim.last = Some(f.clone());
            f
        } else {
            if !anim.visible {
                set_timer(anim, false);
                return;
            }
            let since = *anim.hiding_since.get_or_insert(now);
            if (now - since).as_secs_f32() >= FADE_OUT || anim.last.is_none() {
                anim.visible = false;
                anim.hiding_since = None;
                anim.last = None;
                anim.level = 0.0;
                anim.geo = None;
                anim.min_w = 0.0;
                anim.min_h = 0.0;
                set_timer(anim, false);
                unsafe {
                    let _: () = msg_send![&*win, orderOut: std::ptr::null::<AnyObject>()];
                }
                return;
            }
            let mut f = anim.last.clone().unwrap();
            f.t = (now - anim.shown_at).as_secs_f32();
            f
        };

        let Some(screen) = screen() else { return };
        let (work, scale): (NSRect, f64) = unsafe { (msg_send![&*screen, visibleFrame], msg_send![&*screen, backingScaleFactor]) };
        let scale = scale.max(1.0) as f32;
        let dpi = 96.0 * scale;
        if anim.g.is_none() {
            anim.g = Gfx::new(1, 1, dpi);
        }
        let Some(g) = anim.g.as_mut() else { return };
        g.set_dpi(dpi);
        let mut target = layout(g, &frame);
        let recording = matches!(frame.mode, Mode::Live(_) | Mode::Transcribing);
        if recording && wanted {
            if target.w < anim.min_w {
                // Keep the width; wrap the text to it.
                let mut f2 = frame.clone();
                f2.t = 0.0;
                target = layout_at(g, &f2, anim.min_w);
            }
            target.h = target.h.max(anim.min_h);
            anim.min_w = target.w;
            anim.min_h = target.h;
        } else if wanted {
            anim.min_w = 0.0;
            anim.min_h = 0.0;
        }
        // Ease the displayed geometry toward the target.
        let geo = match anim.geo {
            Some(cur) if anim.hiding_since.is_none() => {
                let k = 0.34;
                let step = |a: f32, b: f32| if (b - a).abs() < 0.4 { b } else { a + (b - a) * k };
                Geo { w: step(cur.w, target.w), h: step(cur.h, target.h), scroll: step(cur.scroll, target.scroll), ..target }
            }
            Some(cur) => cur,
            None => target,
        };
        let settling = (geo.w - target.w).abs() > 0.01 || (geo.h - target.h).abs() > 0.01 || (geo.scroll - target.scroll).abs() > 0.01;
        anim.geo = Some(geo);
        let (w, h) = (MAX_W + 2.0 * MARGIN, geo.h + 2.0 * MARGIN);
        let px = |v: f32| (v * scale).round() as i32;
        let g = anim.g.as_mut().unwrap();
        g.resize(px(w) as usize, px(h) as usize);
        g.begin();
        g.clear(Color(0.0, 0.0, 0.0, 0.0));
        let buttons = draw(g, &frame, &geo);
        g.end();
        let image = g.image();
        anim.scale = scale;

        // Fade and slide.
        let fade = if let Some(since) = anim.hiding_since {
            1.0 - ease((now - since).as_secs_f32() / FADE_OUT)
        } else {
            ease((now - anim.shown_at).as_secs_f32() / FADE_IN)
        };
        let slide = 8.0 * (1.0 - fade) as f64;
        // Bottom-left origin: the card's bottom sits LIFT above the work area.
        let x = work.origin.x + (work.size.width - w as f64) / 2.0;
        let y = work.origin.y + LIFT as f64 - MARGIN as f64 - slide;
        let card = matches!(frame.mode, Mode::Card { .. }) && anim.hiding_since.is_none();
        unsafe {
            let rect = NSRect::new(NSPoint::new(x.round(), y.round()), NSSize::new(w as f64, h as f64));
            let _: () = msg_send![&*win, setFrame: rect, display: false];
            let layer: Option<Id> = msg_send![&*view, layer];
            if let Some(layer) = layer
                && !image.is_null()
            {
                let _: () = msg_send![class!(CATransaction), begin];
                let _: () = msg_send![class!(CATransaction), setDisableActions: true];
                let _: () = msg_send![&*layer, setContentsScale: scale as f64];
                let _: () = msg_send![&*layer, setContents: image as *mut AnyObject];
                let _: () = msg_send![class!(CATransaction), commit];
            }
            if !image.is_null() {
                CGImageRelease(image);
            }
            let _: () = msg_send![&*win, setAlphaValue: fade as f64];
            // Clickable only as a card.
            if CLICKABLE.with(|c| c.replace(card)) != card {
                let _: () = msg_send![&*win, setIgnoresMouseEvents: !card];
            }
            let visible: bool = msg_send![&*win, isVisible];
            if !visible {
                let _: () = msg_send![&*win, orderFrontRegardless];
            }
        }
        {
            let to_px = |r: Rect| Px { left: px(r.x), top: px(r.y), right: px(r.right()), bottom: px(r.bottom()) };
            let mut v = VIEW.lock().unwrap();
            match buttons {
                Some((copy, close)) => {
                    v.copy_btn = to_px(copy);
                    v.close_btn = to_px(close);
                }
                None => {
                    v.copy_btn = ZERO;
                    v.close_btn = ZERO;
                }
            }
        }
        let moving = matches!(frame.mode, Mode::Live(_) | Mode::Transcribing | Mode::Progress(..))
            || anim.hiding_since.is_some()
            || fade < 1.0
            || settling;
        set_timer(anim, moving);
    });
}

// ------------------------------------------------------------------ view --

/// Event position in window pixels (top-left origin), as the button rects.
fn event_px(view: &AnyObject, e: *mut AnyObject) -> Option<(i32, i32)> {
    let e = unsafe { e.as_ref() }?;
    let scale = ANIM.with(|a| a.borrow().scale);
    unsafe {
        let p: NSPoint = msg_send![e, locationInWindow];
        let v: NSPoint = msg_send![view, convertPoint: p, fromView: std::ptr::null::<AnyObject>()];
        Some(((v.x as f32 * scale) as i32, (v.y as f32 * scale) as i32))
    }
}

fn set_hover(h: Option<Button>) {
    let changed = {
        let mut v = VIEW.lock().unwrap();
        let c = v.hover != h;
        v.hover = h;
        c
    };
    if changed {
        render();
    }
    let cursor: Id = unsafe {
        if h.is_some() { msg_send![class!(NSCursor), pointingHandCursor] } else { msg_send![class!(NSCursor), arrowCursor] }
    };
    let _: () = unsafe { msg_send![&*cursor, set] };
}

extern "C-unwind" fn yes(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

extern "C-unwind" fn yes1(_this: *mut AnyObject, _sel: Sel, _e: *mut AnyObject) -> Bool {
    Bool::YES
}

extern "C-unwind" fn mouse_moved(this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    if let Some((x, y)) = event_px(this, e) {
        set_hover(hover_at(x, y));
    }
}

extern "C-unwind" fn mouse_exited(_this: *mut AnyObject, _sel: Sel, _e: *mut AnyObject) {
    set_hover(None);
}

extern "C-unwind" fn mouse_up(this: *mut AnyObject, _sel: Sel, e: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    if let Some((x, y)) = event_px(this, e) {
        click(x, y);
    }
}

extern "C-unwind" fn ignore(_this: *mut AnyObject, _sel: Sel, _e: *mut AnyObject) {}

fn view_class() -> &'static AnyClass {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut b = ClassBuilder::new(c"ChattyOverlayView", class!(NSView)).expect("ChattyOverlayView");
        type Ev = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
        unsafe {
            b.add_method(sel!(isFlipped), yes as extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool);
            b.add_method(sel!(acceptsFirstMouse:), yes1 as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool);
            b.add_method(sel!(mouseDown:), ignore as Ev);
            b.add_method(sel!(mouseUp:), mouse_up as Ev);
            b.add_method(sel!(mouseMoved:), mouse_moved as Ev);
            b.add_method(sel!(mouseExited:), mouse_exited as Ev);
        }
        b.register();
    });
    AnyClass::get(c"ChattyOverlayView").expect("registered")
}

/// Creates the panel. Call on the main thread.
pub fn create() {
    unsafe {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0));
        // NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel
        let style: usize = 1 << 7;
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSPanel), alloc];
        let win: Option<Id> = msg_send![alloc, initWithContentRect: rect, styleMask: style, backing: 2usize, defer: false];
        let Some(win) = win else {
            log::error!("overlay panel");
            return;
        };
        let clear: Id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![&*win, setBackgroundColor: &*clear];
        let _: () = msg_send![&*win, setOpaque: false];
        let _: () = msg_send![&*win, setHasShadow: false];
        let _: () = msg_send![&*win, setReleasedWhenClosed: false];
        let _: () = msg_send![&*win, setHidesOnDeactivate: false];
        let _: () = msg_send![&*win, setFloatingPanel: true];
        let _: () = msg_send![&*win, setBecomesKeyOnlyIfNeeded: true];
        let _: () = msg_send![&*win, setIgnoresMouseEvents: true];
        let _: () = msg_send![&*win, setAcceptsMouseMovedEvents: true];
        // NSStatusWindowLevel: above normal and floating windows.
        let _: () = msg_send![&*win, setLevel: 25isize];
        // canJoinAllSpaces | stationary | ignoresCycle | fullScreenAuxiliary
        let behavior: usize = (1 << 0) | (1 << 4) | (1 << 6) | (1 << 8);
        let _: () = msg_send![&*win, setCollectionBehavior: behavior];

        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![view_class(), alloc];
        let view: Id = msg_send![alloc, initWithFrame: rect];
        let _: () = msg_send![&*view, setWantsLayer: true];
        let opts: usize = 0x01 | 0x02 | 0x80 | 0x200;
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSTrackingArea), alloc];
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        let area: Id = msg_send![alloc, initWithRect: zero, options: opts, owner: &*view, userInfo: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![&*view, addTrackingArea: &*area];
        let _: () = msg_send![&*win, setContentView: &*view];
        PANEL.with(|p| *p.borrow_mut() = Some((win, view)));
    }
    // Anything posted before the panel existed.
    post();
}

pub fn shutdown() {
    ANIM.with(|a| a.borrow_mut().g = None);
}

// ---------------------------------------------------------------- preview --

fn sample(kind: &str) -> Frame {
    let long = "So I was thinking we could move the review to Thursday afternoon, because the numbers from the \
                second test run won't be ready before Wednesday evening, and I'd rather walk everyone through the \
                final version than a draft that changes again the next morning.";
    let short = "Let's ship the overlay redesign today and";
    let mut split = usize::MAX;
    let (mode, live, level, elapsed, hover) = match kind {
        "live" => {
            split = short[..20].rfind(' ').unwrap_or(0);
            (Mode::Live(short.into()), short.to_string(), 0.7, 7.0, None)
        }
        "listening" => (Mode::Live(String::new()), String::new(), 0.1, 1.0, None),
        "long" => {
            split = long[..long.len() - 40].rfind(' ').unwrap_or(0);
            (Mode::Live(long.into()), long.to_string(), 0.55, 23.0, None)
        }
        "transcribing" => (Mode::Transcribing, long.to_string(), 0.0, 0.0, None),
        "notice" => (Mode::Notice("Downloaded Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "warning" => (Mode::Notice("Download failed: Parakeet Unified EN 0.6B".into()), String::new(), 0.0, 0.0, None),
        "progress" => (Mode::Progress("Downloading Parakeet Unified EN 0.6B".into(), 0.42), String::new(), 0.0, 0.0, None),
        "copied" => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: true },
            String::new(),
            0.0,
            0.0,
            None,
        ),
        _ => (
            Mode::Card { text: long.into(), reason: "Not pasted · no text field had focus".into(), copied: false },
            String::new(),
            0.0,
            0.0,
            Some(Button::Copy),
        ),
    };
    Frame { mode, live, split, level, elapsed, t: 1.3, hover }
}

/// Draws a card through the live path and checks the pixels. No window.
pub fn selftest() -> Result<(), String> {
    objc2::rc::autoreleasepool(|_| {
        let frame = Frame {
            mode: Mode::Card { text: "Self-test".into(), reason: "Not pasted".into(), copied: false },
            live: String::new(),
            split: 0,
            level: 0.0,
            elapsed: 0.0,
            t: 0.0,
            hover: None,
        };
        let scale = 1.5f32;
        let mut g = Gfx::new(1, 1, 96.0 * scale).ok_or("bitmap")?;
        let geo = layout(&mut g, &frame);
        let (w, h) = (((MAX_W + 2.0 * MARGIN) * scale) as usize, ((geo.h + 2.0 * MARGIN) * scale) as usize);
        g.resize(w, h);
        g.begin();
        g.clear(Color(0.0, 0.0, 0.0, 0.0));
        draw(&mut g, &frame, &geo);
        g.end();
        let corner = g.pixel(1, 1);
        let inside = g.pixel(w / 2, (MARGIN * scale) as usize + 4);
        println!("corner bgra {corner:?}, card bgra {inside:?}, size {w}x{h}");
        if corner[3] != 0 || inside[3] < 240 {
            return Err("unexpected alpha".into());
        }
        Ok(())
    })
}

/// Draws one overlay state to a PNG over a sample backdrop, for design
/// review: `chatty --render-overlay <live|long|transcribing|notice|warning|progress|card|copied> out.png [scale]`.
pub fn render_png(kind: &str, path: &std::path::Path, scale: f32) -> Result<(), String> {
    objc2::rc::autoreleasepool(|_| {
        let _app = crate::mac::app();
        if let Ok(f) = std::env::var("CHATTY_FONT") {
            crate::ui::gfx::set_font(&f);
        }
        let frame = sample(kind);
        let dpi = 96.0 * scale;
        let mut g = Gfx::new(4, 4, dpi).ok_or("bitmap")?;
        let geo = layout(&mut g, &frame);
        let (bw, bh) = (MAX_W + 2.0 * MARGIN + 80.0, geo.h + 2.0 * MARGIN + 60.0);
        g.resize((bw * scale) as usize, (bh * scale) as usize);
        g.begin();
        // A busy, light-and-dark backdrop to judge contrast and the shadow.
        g.gradient(Rect::new(0.0, 0.0, bw, bh), 0.0, Color::rgb(58, 74, 96), Color::rgb(214, 220, 228));
        g.fill(Rect::new(0.0, bh * 0.55, bw * 0.45, bh), 0.0, Color::rgb(245, 245, 247));
        g.translate(40.0, 30.0);
        draw(&mut g, &frame, &geo);
        g.end();
        g.write_png(path)
    })
}
