//! Pasteboard save / set / restore around a paste (macOS).
//!
//! The paste text is offered lazily: an `NSPasteboardItem` whose data
//! provider is asked for the bytes only when an app reads them, so chatty
//! learns exactly when the paste read the pasteboard (the counterpart of
//! Windows delayed rendering). All pasteboard writes run on the main thread,
//! where AppKit calls the provider.

use std::cell::RefCell;
use std::sync::{Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};

use crate::mac::{Id, main_sync, nsstring, rust_string};

const TEXT_TYPE: &str = "public.utf8-plain-text";
/// nspasteboard.org markers: clipboard managers skip transient content.
const TRANSIENT_TYPE: &str = "org.nspasteboard.TransientType";
const MAX_FORMAT: usize = 64 << 20;

/// Every item's (type, bytes) pairs.
pub struct Saved(Vec<Vec<(String, Vec<u8>)>>);

impl Saved {
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

fn pasteboard() -> Id {
    unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
}

/// The pasteboard's change count (as the Windows sequence number).
pub fn sequence() -> u32 {
    let n: isize = unsafe { msg_send![&*pasteboard(), changeCount] };
    n as u32
}

fn nsdata(bytes: &[u8]) -> Id {
    unsafe { msg_send![class!(NSData), dataWithBytes: bytes.as_ptr() as *const std::ffi::c_void, length: bytes.len()] }
}

fn data_bytes(d: &AnyObject) -> Vec<u8> {
    let len: usize = unsafe { msg_send![d, length] };
    let p: *const u8 = unsafe { msg_send![d, bytes] };
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
}

fn array(objs: &[&AnyObject]) -> Id {
    unsafe { msg_send![class!(NSArray), arrayWithObjects: objs.as_ptr(), count: objs.len()] }
}

/// Copies every item's data.
pub fn save() -> Result<Saved, String> {
    main_sync(|| {
        objc2::rc::autoreleasepool(|_| {
            let pb = pasteboard();
            let items: Option<Id> = unsafe { msg_send![&*pb, pasteboardItems] };
            let Some(items) = items else { return Ok(Saved(Vec::new())) };
            let n: usize = unsafe { msg_send![&*items, count] };
            let mut out = Vec::new();
            for i in 0..n {
                let item: Id = unsafe { msg_send![&*items, objectAtIndex: i] };
                let types: Id = unsafe { msg_send![&*item, types] };
                let tn: usize = unsafe { msg_send![&*types, count] };
                let mut formats = Vec::new();
                for j in 0..tn {
                    let ty: Id = unsafe { msg_send![&*types, objectAtIndex: j] };
                    let data: Option<Id> = unsafe { msg_send![&*item, dataForType: &*ty] };
                    if let Some(d) = data {
                        let bytes = data_bytes(&d);
                        if bytes.len() <= MAX_FORMAT {
                            formats.push((rust_string(&ty), bytes));
                        }
                    }
                }
                out.push(formats);
            }
            Ok(Saved(out))
        })
    })
}

/// Puts `text` on the pasteboard, marked transient for clipboard managers.
/// Returns the change count after the change.
pub fn set_text(text: &str) -> Result<u32, String> {
    let text = text.to_string();
    main_sync(move || {
        objc2::rc::autoreleasepool(|_| {
            let pb = pasteboard();
            let _: isize = unsafe { msg_send![&*pb, clearContents] };
            let s = nsstring(&text);
            let ok: bool = unsafe { msg_send![&*pb, setString: &*s, forType: &*nsstring(TEXT_TYPE)] };
            let empty = nsdata(&[]);
            let _: bool = unsafe { msg_send![&*pb, setData: &*empty, forType: &*nsstring(TRANSIENT_TYPE)] };
            if ok { Ok(sequence()) } else { Err("pasteboard refused the text".into()) }
        })
    })
}

/// A read of lazily provided text: when, and by which process (macOS does
/// not say; always 0).
#[derive(Clone, Copy, Debug)]
pub struct Read {
    pub at: Instant,
    pub pid: u32,
}

struct Deferred {
    text: Option<String>,
    /// The provider object serving `text`.
    owner: usize,
    /// Set right before ⌘V. Earlier readers are clipboard monitors; they get
    /// nothing, which keeps the data lazy for the paste's own read.
    armed: bool,
    read: Option<Read>,
    refused: Vec<u32>,
}

static DEFERRED: Mutex<Deferred> = Mutex::new(Deferred { text: None, owner: 0, armed: false, read: None, refused: Vec::new() });
static READ: Condvar = Condvar::new();

thread_local! {
    /// The provider object (main thread); kept alive while it owns the item.
    static PROVIDER: RefCell<Option<Id>> = const { RefCell::new(None) };
}

/// How often AppKit asked the provider (diagnostics).
pub static PROVIDE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C-unwind" fn provide(this: *mut AnyObject, _sel: Sel, _pb: *mut AnyObject, item: *mut AnyObject, ty: *mut AnyObject) {
    PROVIDE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let this: &AnyObject = unsafe { &*this };
    let mut d = DEFERRED.lock().unwrap();
    if d.owner != this as *const AnyObject as usize {
        return;
    }
    let Some(text) = d.text.clone() else { return };
    if !d.armed {
        d.refused.push(0);
        return;
    }
    if let (Some(item), Some(ty)) = (unsafe { item.as_ref() }, unsafe { ty.as_ref() }) {
        let s = nsstring(&text);
        let _: bool = unsafe { msg_send![item, setString: &*s, forType: ty] };
    }
    d.read = Some(Read { at: Instant::now(), pid: 0 });
    drop(d);
    READ.notify_all();
}

extern "C-unwind" fn finished(this: *mut AnyObject, _sel: Sel, _pb: *mut AnyObject) {
    let this: &AnyObject = unsafe { &*this };
    let mut d = DEFERRED.lock().unwrap();
    if d.owner == this as *const AnyObject as usize {
        d.text = None;
        d.owner = 0;
    }
    drop(d);
    READ.notify_all();
}

fn provider_class() -> &'static AnyClass {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut b = ClassBuilder::new(c"ChattyPasteProvider", class!(NSObject)).expect("ChattyPasteProvider");
        unsafe {
            b.add_method(
                sel!(pasteboard:item:provideDataForType:),
                provide as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject, *mut AnyObject),
            );
            b.add_method(sel!(pasteboardFinishedWithDataProvider:), finished as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject));
        }
        if let Some(p) = objc2::runtime::AnyProtocol::get(c"NSPasteboardItemDataProvider") {
            b.add_protocol(p);
        }
        b.register();
    });
    AnyClass::get(c"ChattyPasteProvider").expect("registered")
}

/// Puts `text` on the pasteboard lazily: the bytes are only handed over
/// when an app asks for them. Returns the change count.
pub fn set_text_deferred(text: &str) -> Result<Option<u32>, String> {
    let text = text.to_string();
    main_sync(move || {
        objc2::rc::autoreleasepool(|_| {
            let provider: Id = unsafe { msg_send![provider_class(), new] };
            let item: Id = unsafe { msg_send![class!(NSPasteboardItem), new] };
            let types = array(&[&nsstring(TEXT_TYPE)]);
            let ok: bool = unsafe { msg_send![&*item, setDataProvider: &*provider, forTypes: &*types] };
            if !ok {
                return Err("pasteboard item refused the provider".into());
            }
            let empty = nsdata(&[]);
            let _: bool = unsafe { msg_send![&*item, setData: &*empty, forType: &*nsstring(TRANSIENT_TYPE)] };
            let pb = pasteboard();
            // May tell the previous provider it is finished (ignored: it is
            // no longer the current one).
            let _: isize = unsafe { msg_send![&*pb, clearContents] };
            {
                let mut d = DEFERRED.lock().unwrap();
                d.text = Some(text);
                d.owner = &*provider as *const AnyObject as usize;
                d.armed = false;
                d.read = None;
                d.refused.clear();
            }
            let items = array(&[&item]);
            let ok: bool = unsafe { msg_send![&*pb, writeObjects: &*items] };
            if !ok {
                DEFERRED.lock().unwrap().text = None;
                return Err("pasteboard refused the item".into());
            }
            PROVIDER.with(|p| *p.borrow_mut() = Some(provider));
            Ok(Some(sequence()))
        })
    })
}

/// From now on the lazy text is handed to whoever asks. Returns the process
/// ids that asked before (and got nothing).
pub fn arm() -> Vec<u32> {
    let mut d = DEFERRED.lock().unwrap();
    d.armed = true;
    std::mem::take(&mut d.refused)
}

/// Process ids refused since the last call.
pub fn take_refused() -> Vec<u32> {
    std::mem::take(&mut DEFERRED.lock().unwrap().refused)
}

/// Waits until the lazy text is read after `since`, up to `timeout`.
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

/// Plain text on the pasteboard, if any.
pub fn get_text() -> Option<String> {
    main_sync(|| {
        objc2::rc::autoreleasepool(|_| {
            let s: Option<Retained<AnyObject>> = unsafe { msg_send![&*pasteboard(), stringForType: &*nsstring(TEXT_TYPE)] };
            s.map(|s| rust_string(&s))
        })
    })
}

/// Restores `saved` unless someone changed the pasteboard since `sequence`.
pub fn restore(saved: Saved, seq: u32) -> Result<bool, String> {
    main_sync(move || {
        objc2::rc::autoreleasepool(|_| {
            if sequence() != seq {
                return Ok(false);
            }
            let pb = pasteboard();
            let _: isize = unsafe { msg_send![&*pb, clearContents] };
            {
                let mut d = DEFERRED.lock().unwrap();
                d.text = None;
                d.owner = 0;
            }
            PROVIDER.with(|p| *p.borrow_mut() = None);
            if saved.0.is_empty() {
                return Ok(true);
            }
            let mut items = Vec::new();
            for formats in &saved.0 {
                let item: Id = unsafe { msg_send![class!(NSPasteboardItem), new] };
                for (ty, bytes) in formats {
                    let d = nsdata(bytes);
                    let _: bool = unsafe { msg_send![&*item, setData: &*d, forType: &*nsstring(ty)] };
                }
                items.push(item);
            }
            let refs: Vec<&AnyObject> = items.iter().map(|i| &**i).collect();
            let ok: bool = unsafe { msg_send![&*pb, writeObjects: &*array(&refs)] };
            if ok { Ok(true) } else { Err("pasteboard refused the saved items".into()) }
        })
    })
}
