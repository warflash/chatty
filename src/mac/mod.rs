//! macOS process plumbing: main-thread dispatch, Objective-C helpers,
//! single instance, login item, permissions and the AppKit run loop.

pub mod sys;

use std::ffi::c_void;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

use crate::{config, engine, logi, overlay, tray, ui};

// ---------------------------------------------------------------- dispatch --

unsafe extern "C" fn run_boxed(ctx: *mut c_void) {
    let f = unsafe { Box::from_raw(ctx as *mut Box<dyn FnOnce()>) };
    objc2::rc::autoreleasepool(|_| f());
}

/// Runs `f` on the main thread, soon (from any thread).
pub fn on_main(f: impl FnOnce() + Send + 'static) {
    let b: Box<Box<dyn FnOnce()>> = Box::new(Box::new(f));
    unsafe { sys::dispatch_async_f(sys::main_queue(), Box::into_raw(b) as *mut c_void, run_boxed) };
}

/// Runs `f` on the main thread after `ms` milliseconds.
pub fn after_main(ms: u64, f: impl FnOnce() + Send + 'static) {
    let b: Box<Box<dyn FnOnce()>> = Box::new(Box::new(f));
    unsafe {
        let when = sys::dispatch_time(0, (ms * 1_000_000) as i64);
        sys::dispatch_after_f(when, sys::main_queue(), Box::into_raw(b) as *mut c_void, run_boxed);
    }
}

/// Runs `f` on the main thread and waits for its result. Inline when called
/// on the main thread.
pub fn main_sync<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    if sys::is_main_thread() {
        return f();
    }
    let mut out: Option<R> = None;
    {
        let slot = &mut out;
        let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || *slot = Some(f()));
        // SAFETY: dispatch_sync returns only after the job ran, so the
        // borrowed closure outlives its use.
        let job: Box<dyn FnOnce()> = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Box<dyn FnOnce()>>(job) };
        let b: Box<Box<dyn FnOnce()>> = Box::new(job);
        unsafe { sys::dispatch_sync_f(sys::main_queue(), Box::into_raw(b) as *mut c_void, run_boxed) };
    }
    out.expect("main_sync job ran")
}

// ------------------------------------------------------------------- objc --

pub type Id = Retained<AnyObject>;

pub fn nsstring(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// `NSString` → Rust.
pub fn rust_string(s: &AnyObject) -> String {
    let p: *const std::ffi::c_char = unsafe { msg_send![s, UTF8String] };
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

pub fn app() -> Id {
    unsafe { msg_send![class!(NSApplication), sharedApplication] }
}

/// Shows or hides the Dock icon: chatty is a menu bar app, but its window
/// should behave like a normal app window while open.
pub fn set_regular(regular: bool) {
    let app = app();
    // NSApplicationActivationPolicyRegular = 0, Accessory = 1.
    let policy: isize = if regular { 0 } else { 1 };
    let _: bool = unsafe { msg_send![&app, setActivationPolicy: policy] };
    if regular {
        let _: () = unsafe { msg_send![&app, activateIgnoringOtherApps: true] };
    }
}

pub fn open_path(path: &std::path::Path) {
    // Text files open in the default text editor, folders in Finder.
    let text = path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e, "toml" | "log" | "txt"));
    let mut cmd = std::process::Command::new("/usr/bin/open");
    if text {
        cmd.arg("-t");
    }
    if let Err(e) = cmd.arg(path).spawn() {
        log::warn!("open {}: {e}", path.display());
    }
}

// --------------------------------------------------------------- process --

/// False when another chatty already runs (an flock on a file in the data
/// folder, released by the kernel when the process exits).
pub fn single_instance() -> bool {
    let dir = crate::paths::local_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = std::ffi::CString::new(dir.join("chatty.lock").as_os_str().as_encoded_bytes()).unwrap_or_default();
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
        if fd < 0 {
            return true;
        }
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            libc::close(fd);
            return false;
        }
    }
    true
}

const AGENT: &str = "com.github.fantasyflip.chatty";

fn agent_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/LaunchAgents").join(format!("{AGENT}.plist")))
}

/// A LaunchAgent that starts chatty at login (RunAtLoad, no KeepAlive).
pub fn set_autostart(enabled: bool) {
    // Never register a dev build, or a release build run from cargo's target dir.
    let exe = std::env::current_exe().unwrap_or_default();
    if cfg!(debug_assertions) || exe.components().any(|c| c.as_os_str() == "target") {
        return;
    }
    let Some(path) = agent_path() else { return };
    if !enabled {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return;
    }
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
        esc(&exe.to_string_lossy())
    );
    if std::fs::read_to_string(&path).ok().as_deref() != Some(plist.as_str()) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, plist) {
            log::warn!("login item {}: {e}", path.display());
        }
    }
}

/// Accessibility access: the event tap (to see and swallow the trigger),
/// synthesized ⌘V / Return, and the focused-field check all need it. Asks
/// once (the system shows its prompt) and returns the current state.
pub fn accessibility_trusted(prompt: bool) -> bool {
    unsafe {
        if !prompt {
            return sys::AXIsProcessTrusted() != 0;
        }
        let opts = sys::dict(&[(sys::kAXTrustedCheckOptionPrompt, sys::kCFBooleanTrue)]);
        sys::AXIsProcessTrustedWithOptions(opts.as_ptr()) != 0
    }
}

/// Creates the menu bar item, overlay and window support, then runs AppKit
/// on the main thread until Quit.
pub fn run(cfg: config::Shared, engine: engine::Handle, t0: Instant) {
    objc2::rc::autoreleasepool(|_| {
        let app = app();
        set_regular(false);
        // Dark controls for any system UI chatty shows (menus stay system).
        let dark = nsstring("NSAppearanceNameDarkAqua");
        let appearance: Option<Id> = unsafe { msg_send![class!(NSAppearance), appearanceNamed: &*dark] };
        if let Some(a) = appearance {
            let _: () = unsafe { msg_send![&app, setAppearance: &*a] };
        }
        overlay::create();
        ui::init(cfg.clone(), Some(engine.clone()));
        tray::create(cfg.clone());
        engine.send(engine::Cmd::Warm);
        log::info!("ready in {:?}", t0.elapsed());
    });
    let app = app();
    let _: () = unsafe { msg_send![&app, run] };
    logi::shutdown();
}

/// Quits from the menu: hands the Logitech button back, flushes the log.
pub fn quit() -> ! {
    log::info!("quit");
    logi::shutdown();
    overlay::shutdown();
    log::logger().flush();
    std::process::exit(0);
}

/// `chatty --clipboard-test`: checks lazy-provider paste confirmation with
/// `pbpaste` as the foreign reader. The user's clipboard is saved and
/// restored, whether the checks pass or not. AppKit's run loop serves the
/// provider meanwhile.
pub fn clipboard_test() -> Result<(), String> {
    use std::sync::{Arc, Mutex};
    // As in the app: a menu bar (accessory) process.
    set_regular(false);
    let result: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));
    let out = result.clone();
    std::thread::spawn(move || {
        let r = (|| -> Result<(), String> {
            let saved = crate::clipboard::save()?;
            println!("saved {} clipboard items", saved.len());
            let r = clipboard_steps();
            let restored = crate::clipboard::restore(saved, crate::clipboard::sequence());
            println!("restored: {restored:?}");
            r.and(restored.map(|_| ()))
        })();
        *out.lock().unwrap() = Some(r);
        on_main(|| {
            let app = app();
            let _: () = unsafe { msg_send![&app, stop: std::ptr::null::<AnyObject>()] };
            // `stop:` takes effect after the next event; post one.
            post_dummy_event();
        });
    });
    let app = app();
    let _: () = unsafe { msg_send![&app, run] };
    result.lock().unwrap().take().unwrap_or_else(|| Err("test did not finish".into()))
}

/// The pasteboard text as another process sees it. pbpaste waits for the
/// provider; never let it hang the test.
fn read_foreign() -> Result<String, String> {
    use std::time::Duration;
    // Without a UTF-8 locale pbpaste writes MacRoman and drops what it
    // cannot encode (the emoji), so the text would never match.
    let mut child = std::process::Command::new("/usr/bin/pbpaste")
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let t = Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if t.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let calls = crate::clipboard::PROVIDE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            return Err(format!("pbpaste got no answer in 5 s (provider asked {calls} times)"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn clipboard_steps() -> Result<(), String> {
    use std::time::Duration;
    // 1. A reader 80 ms after "⌘V": confirmed, with the right text.
    let text = "chatty clipboard test äöü 👍";
    crate::clipboard::set_text_deferred(text)?.ok_or("no provider")?;
    std::thread::sleep(Duration::from_millis(60));
    println!("   refused before arm: {:?}", crate::clipboard::arm());
    let sent = Instant::now();
    let reader = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(80));
        read_foreign()
    });
    let r = crate::clipboard::wait_read(sent, Duration::from_millis(1500));
    let got = reader.join().unwrap();
    println!("   provider asked {} times", crate::clipboard::PROVIDE_CALLS.load(std::sync::atomic::Ordering::Relaxed));
    let got = got?;
    println!("1. read after {:?}, text matches: {}", r.map(|r| r.at - sent), got == text);
    if r.is_none() || got != text {
        return Err(format!("lazy provider read not observed (got {got:?})"));
    }

    // 2. Nobody reads: unconfirmed after the timeout.
    crate::clipboard::set_text_deferred(text)?.ok_or("no provider")?;
    std::thread::sleep(Duration::from_millis(60));
    crate::clipboard::arm();
    let t = Instant::now();
    let r = crate::clipboard::wait_read(t, Duration::from_millis(1500));
    println!("2. no reader: {:?} after {:?}", r.map(|r| r.at - t), t.elapsed());
    if r.is_some() {
        return Err("read reported without a reader".into());
    }
    // 3. A late reader still gets the text.
    let late = read_foreign()?;
    println!("3. late read text matches: {}", late == text);
    if late != text {
        return Err(format!("late read got {late:?}"));
    }
    Ok(())
}

/// Wakes `-[NSApplication run]` so a pending `stop:` returns.
fn post_dummy_event() {
    use objc2_foundation::NSPoint;
    unsafe {
        // NSEventTypeApplicationDefined = 15
        let ev: Option<Id> = msg_send![class!(NSEvent),
            otherEventWithType: 15usize,
            location: NSPoint::new(0.0, 0.0),
            modifierFlags: 0usize,
            timestamp: 0.0f64,
            windowNumber: 0isize,
            context: std::ptr::null::<AnyObject>(),
            subtype: 0i16,
            data1: 0isize,
            data2: 0isize];
        if let Some(ev) = ev {
            let app = app();
            let _: () = msg_send![&app, postEvent: &*ev, atStart: true];
        }
    }
}
