//! Clipboard save / set / restore around a paste.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner, GetClipboardSequenceNumber,
    GetOpenClipboardWindow, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::w;

/// Clipboard owner window (on the main thread, which pumps messages).
pub static OWNER: AtomicIsize = AtomicIsize::new(0);

const CF_UNICODETEXT: u32 = 13;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_PALETTE: u32 = 9;
const CF_ENHMETAFILE: u32 = 14;
const CF_OWNERDISPLAY: u32 = 0x80;
const CF_DSPTEXT: u32 = 0x81;
const CF_DSPBITMAP: u32 = 0x82;
const CF_DSPMETAFILEPICT: u32 = 0x83;
const CF_DSPENHMETAFILE: u32 = 0x8E;
const MAX_FORMAT: usize = 64 << 20;

pub struct Saved(Vec<(u32, Vec<u8>)>);

impl Saved {
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

struct Open;

impl Open {
    fn new() -> Result<Open, String> {
        let owner = HWND(OWNER.load(Ordering::Relaxed) as *mut _);
        // Another app may hold the clipboard for a moment.
        for _ in 0..100 {
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Ok(Open);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err("clipboard busy".into())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

unsafe fn alloc(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let p = GlobalLock(hg) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(hg);
        Some(hg)
    }
}

unsafe fn put(format: u32, bytes: &[u8]) {
    unsafe {
        if let Some(hg) = alloc(bytes) {
            if SetClipboardData(format, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
            }
        }
    }
}

/// Copies every global-memory format. GDI handle formats are skipped;
/// Windows re-synthesizes CF_BITMAP from a saved CF_DIB.
pub fn save() -> Result<Saved, String> {
    let _open = Open::new()?;
    let mut formats = Vec::new();
    let mut f = 0;
    loop {
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 {
            break;
        }
        if matches!(
            f,
            CF_BITMAP
                | CF_METAFILEPICT
                | CF_PALETTE
                | CF_ENHMETAFILE
                | CF_OWNERDISPLAY
                | CF_DSPTEXT
                | CF_DSPBITMAP
                | CF_DSPMETAFILEPICT
                | CF_DSPENHMETAFILE
        ) {
            continue;
        }
        let Ok(h) = (unsafe { GetClipboardData(f) }) else { continue };
        let hg = HGLOBAL(h.0);
        let size = unsafe { GlobalSize(hg) };
        if size == 0 || size > MAX_FORMAT {
            continue;
        }
        let p = unsafe { GlobalLock(hg) } as *const u8;
        if p.is_null() {
            continue;
        }
        formats.push((f, unsafe { std::slice::from_raw_parts(p, size) }.to_vec()));
        let _ = unsafe { GlobalUnlock(hg) };
    }
    Ok(Saved(formats))
}

/// Puts `text` on the clipboard, excluded from clipboard history and cloud
/// sync. Returns the clipboard sequence number after the change.
pub fn set_text(text: &str) -> Result<u32, String> {
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard().map_err(|e| e.to_string())?;
        put(CF_UNICODETEXT, &utf16z(text));
        put_exclusions();
    }
    drop(_open);
    Ok(unsafe { GetClipboardSequenceNumber() })
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect()
}

unsafe fn put_exclusions() {
    for (name, value) in [
        (w!("ExcludeClipboardContentFromMonitorProcessing"), 1u32),
        (w!("CanIncludeInClipboardHistory"), 0),
        (w!("CanUploadToCloudClipboard"), 0),
    ] {
        let f = unsafe { RegisterClipboardFormatW(name) };
        if f != 0 {
            unsafe { put(f, &value.to_le_bytes()) };
        }
    }
}

/// A read of delayed-rendered text: when and by which process.
#[derive(Clone, Copy, Debug)]
pub struct Read {
    pub at: Instant,
    pub pid: u32,
}

struct Deferred {
    text: Option<Vec<u8>>,
    /// Set right before Ctrl+V. Readers before that are clipboard monitors
    /// (WSLg's msrdc.exe reads every change within milliseconds); they get
    /// nothing, which keeps the format delayed for the paste's own read.
    armed: bool,
    read: Option<Read>,
    refused: Vec<u32>,
    /// Processes seen reading before Ctrl+V: monitors, refused after it too.
    monitors: Vec<u32>,
}

static DEFERRED: Mutex<Deferred> =
    Mutex::new(Deferred { text: None, armed: false, read: None, refused: Vec::new(), monitors: Vec::new() });
static READ: Condvar = Condvar::new();

/// Puts `text` on the clipboard with delayed rendering: the bytes are only
/// handed over when an app asks for them (WM_RENDERFORMAT on the owner
/// window), so the caller learns exactly when the paste read the clipboard.
/// Returns the sequence number, or None when there is no owner window.
pub fn set_text_deferred(text: &str) -> Result<Option<u32>, String> {
    if OWNER.load(Ordering::Relaxed) == 0 {
        return Ok(None);
    }
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard().map_err(|e| e.to_string())?;
        {
            let mut d = DEFERRED.lock().unwrap();
            d.text = Some(utf16z(text));
            d.armed = false;
            d.read = None;
            d.refused.clear();
        }
        // A NULL handle means "render on request"; the call reports that as
        // an error, so its result says nothing.
        let _ = SetClipboardData(CF_UNICODETEXT, None);
        put_exclusions();
    }
    drop(_open);
    Ok(Some(unsafe { GetClipboardSequenceNumber() }))
}

/// From now on the deferred text is handed to whoever asks. Returns the
/// process ids that asked before (and got nothing).
pub fn arm() -> Vec<u32> {
    let mut d = DEFERRED.lock().unwrap();
    d.armed = true;
    std::mem::take(&mut d.refused)
}

/// Process ids refused since the last call.
pub fn take_refused() -> Vec<u32> {
    std::mem::take(&mut DEFERRED.lock().unwrap().refused)
}

/// Waits until the deferred text is read after `since`, up to `timeout`.
pub fn wait_read(since: Instant, timeout: Duration) -> Option<Read> {
    let deadline = Instant::now() + timeout;
    let mut d = DEFERRED.lock().unwrap();
    loop {
        if let Some(r) = d.read.filter(|r| r.at >= since) {
            return Some(r);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || d.text.is_none() {
            return None;
        }
        d = READ.wait_timeout(d, left).unwrap().0;
    }
}

/// Main window messages for delayed rendering. Returns true when handled.
pub fn on_message(hwnd: HWND, msg: u32, format: u32) -> bool {
    match msg {
        WM_RENDERFORMAT => {
            if format == CF_UNICODETEXT {
                render(false);
            }
            true
        }
        WM_RENDERALLFORMATS => {
            // Exiting while still the owner: leave real data behind.
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                if unsafe { GetClipboardOwner() }.ok() == Some(hwnd) {
                    render(true);
                }
                let _ = unsafe { CloseClipboard() };
            }
            true
        }
        WM_DESTROYCLIPBOARD => {
            let mut d = DEFERRED.lock().unwrap();
            d.text = None;
            drop(d);
            READ.notify_all();
            true
        }
        _ => false,
    }
}

/// Hands the deferred bytes to the clipboard (which is open by the reader)
/// and records who asked. Before `arm` the request is refused.
fn render(always: bool) {
    let mut d = DEFERRED.lock().unwrap();
    if d.text.is_none() {
        return;
    }
    let mut pid = 0u32;
    if let Ok(w) = unsafe { GetOpenClipboardWindow() } {
        unsafe { GetWindowThreadProcessId(w, Some(&mut pid)) };
    }
    if !always && (!d.armed || (pid != 0 && d.monitors.contains(&pid))) {
        d.refused.push(pid);
        if !d.armed && pid != 0 && !d.monitors.contains(&pid) {
            d.monitors.push(pid);
        }
        return;
    }
    if let Some(bytes) = d.text.as_ref() {
        unsafe { put(CF_UNICODETEXT, bytes) };
    }
    d.read = Some(Read { at: Instant::now(), pid });
    drop(d);
    READ.notify_all();
}

/// Plain text on the clipboard, if any.
pub fn get_text() -> Option<String> {
    let _open = Open::new().ok()?;
    unsafe {
        let h = GetClipboardData(CF_UNICODETEXT).ok()?;
        let hg = HGLOBAL(h.0);
        let p = GlobalLock(hg) as *const u16;
        if p.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        let _ = GlobalUnlock(hg);
        Some(s)
    }
}

/// Restores `saved` unless someone changed the clipboard since `sequence`.
pub fn restore(saved: Saved, sequence: u32) -> Result<bool, String> {
    if unsafe { GetClipboardSequenceNumber() } != sequence {
        return Ok(false);
    }
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard().map_err(|e| e.to_string())?;
        for (f, data) in &saved.0 {
            put(*f, data);
        }
    }
    Ok(true)
}
