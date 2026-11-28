//! macOS HID transport for the HID++ receiver (IOKit). The receiver's
//! vendor interface (usage page 0xFF00) is one `IOHIDDevice` carrying both
//! the short (0x10) and long (0x11) reports; input reports arrive on a
//! reader thread's run loop with the report ID as their first byte, the
//! same layout ReadFile gives on Windows.

use std::ffi::c_void;
use std::sync::mpsc::{self, Sender};

use super::Msg;
use crate::mac::sys::*;

const LOGITECH: i32 = 0x046D;
const VENDOR_PAGE: i32 = 0xFF00;

#[derive(Clone, Copy)]
struct Dev(IOHIDDeviceRef);
// SAFETY: IOHIDDevice may be used from several threads; reports are read on
// the run loop it is scheduled on, writes are synchronous calls.
unsafe impl Send for Dev {}

#[derive(Clone, Copy)]
struct Loop(CFRunLoopRef);
unsafe impl Send for Loop {}

pub struct Receiver_ {
    dev: Dev,
    run_loop: Loop,
    /// Keeps the device's manager alive.
    _manager: Cf,
}

impl Receiver_ {
    /// Writes one long (20-byte) HID++ report.
    pub fn write(&self, report: &[u8; 20]) -> Result<(), String> {
        let r = unsafe { IOHIDDeviceSetReport(self.dev.0, kIOHIDReportTypeOutput, report[0] as CFIndex, report.as_ptr(), 20) };
        if r == kIOReturnSuccess { Ok(()) } else { Err(format!("IOHIDDeviceSetReport {r:#x}")) }
    }
}

impl Drop for Receiver_ {
    fn drop(&mut self) {
        // Stopping the run loop ends the reader thread, which closes the device.
        unsafe { CFRunLoopStop(self.run_loop.0) };
    }
}

struct ReaderCtx {
    tx: Sender<Msg>,
    buf: Vec<u8>,
}

unsafe extern "C" fn on_report(ctx: *mut c_void, _result: IOReturn, _sender: *mut c_void, _ty: u32, _id: u32, report: *mut u8, len: CFIndex) {
    let ctx = unsafe { &*(ctx as *const ReaderCtx) };
    if report.is_null() || len <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(report, len as usize) }.to_vec();
    let _ = ctx.tx.send(Msg::Report(bytes));
}

unsafe extern "C" fn on_removed(ctx: *mut c_void, _result: IOReturn, _sender: *mut c_void) {
    let ctx = unsafe { &*(ctx as *const ReaderCtx) };
    let _ = ctx.tx.send(Msg::Gone);
    unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
}

fn int_prop(dev: IOHIDDeviceRef, key: &str) -> Option<i64> {
    let k = Cf::str(key);
    number(unsafe { IOHIDDeviceGetProperty(dev, k.as_ptr()) })
}

/// True when the device's usage pairs include the long-report collection.
fn has_usage(dev: IOHIDDeviceRef, usage: i64) -> bool {
    if int_prop(dev, "PrimaryUsage") == Some(usage) {
        return true;
    }
    let k = Cf::str("DeviceUsagePairs");
    let pairs = unsafe { IOHIDDeviceGetProperty(dev, k.as_ptr()) };
    if pairs.is_null() || unsafe { CFGetTypeID(pairs) != CFArrayGetTypeID() } {
        return false;
    }
    let page_key = Cf::str("DeviceUsagePage");
    let usage_key = Cf::str("DeviceUsage");
    let n = unsafe { CFArrayGetCount(pairs) };
    (0..n).any(|i| unsafe {
        let d = CFArrayGetValueAtIndex(pairs, i);
        number(CFDictionaryGetValue(d, page_key.as_ptr())) == Some(VENDOR_PAGE as i64)
            && number(CFDictionaryGetValue(d, usage_key.as_ptr())) == Some(usage)
    })
}

/// Finds the receiver's HID++ interface and starts a reader on it.
pub fn open_receiver(tx: &Sender<Msg>) -> Option<Receiver_> {
    unsafe {
        let manager = Cf::new(IOHIDManagerCreate(std::ptr::null(), 0) as CFTypeRef)?;
        let (vid_k, page_k) = (Cf::str("VendorID"), Cf::str("PrimaryUsagePage"));
        let (vid, page) = (cfnum_i32(LOGITECH), cfnum_i32(VENDOR_PAGE));
        let matching = dict(&[(vid_k.as_ptr(), vid.as_ptr()), (page_k.as_ptr(), page.as_ptr())]);
        IOHIDManagerSetDeviceMatching(manager.as_ptr() as IOHIDManagerRef, matching.as_ptr());
        let set = Cf::new(IOHIDManagerCopyDevices(manager.as_ptr() as IOHIDManagerRef))?;
        let n = CFSetGetCount(set.as_ptr());
        let mut devs = vec![std::ptr::null(); n as usize];
        CFSetGetValues(set.as_ptr(), devs.as_mut_ptr());
        let dev = devs
            .into_iter()
            .map(|d| d as IOHIDDeviceRef)
            .find(|&d| has_usage(d, 2) && int_prop(d, "MaxInputReportSize").unwrap_or(0) >= 20)?;
        if IOHIDDeviceOpen(dev, 0) != kIOReturnSuccess {
            log::warn!("logitech: cannot open the receiver's HID++ interface");
            return None;
        }
        CFRetain(dev as CFTypeRef);
        let dev = Dev(dev);
        let (loop_tx, loop_rx) = mpsc::channel();
        let tx = tx.clone();
        let spawned = std::thread::Builder::new().name("logi-read".into()).spawn(move || {
            let dev = dev;
            let ctx = Box::into_raw(Box::new(ReaderCtx { tx, buf: vec![0u8; 64] }));
            let rl = CFRunLoopGetCurrent();
            IOHIDDeviceScheduleWithRunLoop(dev.0, rl, kCFRunLoopDefaultMode);
            IOHIDDeviceRegisterInputReportCallback(dev.0, (*ctx).buf.as_mut_ptr(), 64, Some(on_report), ctx as *mut c_void);
            IOHIDDeviceRegisterRemovalCallback(dev.0, Some(on_removed), ctx as *mut c_void);
            let _ = loop_tx.send(Loop(rl));
            CFRunLoopRun();
            IOHIDDeviceRegisterInputReportCallback(dev.0, (*ctx).buf.as_mut_ptr(), 64, None, std::ptr::null_mut());
            IOHIDDeviceRegisterRemovalCallback(dev.0, None, std::ptr::null_mut());
            IOHIDDeviceUnscheduleFromRunLoop(dev.0, rl, kCFRunLoopDefaultMode);
            IOHIDDeviceClose(dev.0, 0);
            CFRelease(dev.0 as CFTypeRef);
            drop(Box::from_raw(ctx));
        });
        if spawned.is_err() {
            return None;
        }
        let run_loop = loop_rx.recv().ok()?;
        Some(Receiver_ { dev, run_loop, _manager: manager })
    }
}
