//! Clipboard save / set / restore around a paste.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
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

struct Open;

impl Open {
    fn new() -> Result<Open, String> {
        let owner = HWND(OWNER.load(Ordering::Relaxed) as *mut _);
        // Another app may hold the clipboard for a moment.
        for _ in 0..20 {
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
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        put(CF_UNICODETEXT, bytes);
        for (name, value) in [
            (w!("ExcludeClipboardContentFromMonitorProcessing"), 1u32),
            (w!("CanIncludeInClipboardHistory"), 0),
            (w!("CanUploadToCloudClipboard"), 0),
        ] {
            let f = RegisterClipboardFormatW(name);
            if f != 0 {
                put(f, &value.to_le_bytes());
            }
        }
    }
    drop(_open);
    Ok(unsafe { GetClipboardSequenceNumber() })
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
