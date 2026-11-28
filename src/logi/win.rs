//! Windows HID transport for the HID++ receiver: the vendor collections are
//! separate device interfaces, read with blocking ReadFile on two threads.

use std::sync::mpsc::Sender;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_CAPS, HidD_FreePreparsedData, HidD_GetHidGuid, HidD_GetPreparsedData, HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::core::PCWSTR;


use super::Msg;

const LOGITECH: &str = "vid_046d";

#[derive(Clone, Copy)]
struct Raw(isize);
// SAFETY: a file handle; used by one reader thread and the manager.
unsafe impl Send for Raw {}

impl Raw {
    fn h(self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
}

pub struct Receiver_ {
    write_long: Raw,
    handles: Vec<Raw>,
}

impl Drop for Receiver_ {
    fn drop(&mut self) {
        // Closing the read handles ends the blocked ReadFile calls.
        for h in self.handles.drain(..) {
            let _ = unsafe { CloseHandle(h.h()) };
        }
    }
}

impl Receiver_ {
    /// Writes one long (20-byte) HID++ report.
    pub fn write(&self, report: &[u8; 20]) -> Result<(), String> {
        unsafe { WriteFile(self.write_long.h(), Some(report), None, None) }.map_err(|e| e.to_string())
    }
}

/// Finds the receiver's HID++ collections (vendor page 0xFF00, usage 1 =
/// short reports, usage 2 = long reports) and starts a reader on each.
pub fn open_receiver(tx: &Sender<Msg>) -> Option<Receiver_> {
    let mut short = None;
    let mut long = None;
    for path in hid_paths() {
        if !path.to_lowercase().contains(LOGITECH) {
            continue;
        }
        let Some(caps) = caps(&path) else { continue };
        if caps.UsagePage != 0xFF00 {
            continue;
        }
        match caps.Usage {
            1 if short.is_none() => short = Some(path),
            2 if long.is_none() => long = Some(path),
            _ => {}
        }
    }
    let (short, long) = (short?, long?);
    let read_short = open(&short, GENERIC_READ.0 | GENERIC_WRITE.0)?;
    let read_long = open(&long, GENERIC_READ.0 | GENERIC_WRITE.0)?;
    let write_long = open(&long, GENERIC_READ.0 | GENERIC_WRITE.0)?;
    for (h, len) in [(read_short, 7usize), (read_long, 20)] {
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("logi-read".into()).spawn(move || {
            let mut buf = vec![0u8; len];
            loop {
                let mut n = 0u32;
                if unsafe { ReadFile(h.h(), Some(&mut buf), Some(&mut n), None) }.is_err() {
                    let _ = tx.send(Msg::Gone);
                    return;
                }
                if tx.send(Msg::Report(buf[..n as usize].to_vec())).is_err() {
                    return;
                }
            }
        });
    }
    Some(Receiver_ { write_long, handles: vec![read_short, read_long, write_long] })
}

fn open(path: &str, access: u32) -> Option<Raw> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    Some(Raw(h.0 as isize))
}

fn caps(path: &str) -> Option<HIDP_CAPS> {
    let h = open(path, 0)?;
    let mut pp = PHIDP_PREPARSED_DATA::default();
    let mut caps = HIDP_CAPS::default();
    let ok = unsafe { HidD_GetPreparsedData(h.h(), &mut pp) };
    if ok {
        unsafe {
            let _ = HidP_GetCaps(pp, &mut caps);
            let _ = HidD_FreePreparsedData(pp);
        }
    }
    let _ = unsafe { CloseHandle(h.h()) };
    ok.then_some(caps)
}

fn hid_paths() -> Vec<String> {
    let guid = unsafe { HidD_GetHidGuid() };
    let mut len = 0u32;
    if unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &guid, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) }
        != CR_SUCCESS
    {
        return Vec::new();
    }
    let mut buf = vec![0u16; len as usize];
    if unsafe { CM_Get_Device_Interface_ListW(&guid, PCWSTR::null(), &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) }
        != CR_SUCCESS
    {
        return Vec::new();
    }
    buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}
