//! Logitech mouse button as a trigger, without Logitech software.
//!
//! Some Logitech mice have buttons that can't hold a keystroke on the device
//! (the MX Vertical's top DPI button, CID 0x00FD, is only divertable). This
//! thread talks HID++ 2.0 to the Unifying / Bolt receiver, diverts that
//! button (feature 0x1B04, REPROG_CONTROLS_V4) and turns its press and
//! release notifications into the same events the keyboard hook sends.
//!
//! Diversion is volatile: the mouse drops it when it powers off or
//! reconnects. The receiver reports reconnects (HID++ 1.0 notification
//! 0x41), and a check every minute re-diverts if needed. On exit the button
//! is handed back.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::coordinator::Input;
use crate::hook::KeyEvent;

/// The MX Vertical's top button ("DPI switch").
const CID_TOP: u16 = 0x00FD;
const FEATURE_REPROG: u16 = 0x1B04;
const SW_ID: u8 = 0x0C;
const SHORT: u8 = 0x10;
const LONG: u8 = 0x11;
const VERIFY_EVERY: Duration = Duration::from_secs(60);
const RESCAN_EVERY: Duration = Duration::from_secs(10);

pub(crate) enum Msg {
    Report(Vec<u8>),
    Gone,
    Enable(bool),
    Quit(Sender<()>),
}

static CTL: OnceLock<Sender<Msg>> = OnceLock::new();

/// Starts the button thread. `enabled` is the initial config value.
pub fn start(events: Sender<KeyEvent>, enabled: bool) {
    let (tx, rx) = mpsc::channel();
    let _ = CTL.set(tx.clone());
    let _ = std::thread::Builder::new()
        .name("logi".into())
        .spawn(move || Manager { events, tx, rx, enabled, pressed: false }.run());
}

pub fn set_enabled(on: bool) {
    if let Some(tx) = CTL.get() {
        let _ = tx.send(Msg::Enable(on));
    }
}

/// Hands the button back to the mouse. Waits briefly.
pub fn shutdown() {
    if let Some(tx) = CTL.get() {
        let (done, wait) = mpsc::channel();
        if tx.send(Msg::Quit(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_millis(500));
        }
    }
}

#[cfg_attr(windows, path = "logi/win.rs")]
#[cfg_attr(target_os = "macos", path = "logi/mac.rs")]
mod transport;
use transport::{Receiver_, open_receiver};

struct Device {
    index: u8,
    reprog: u8,
}

enum Outcome {
    Quit,
    Gone,
}

struct Manager {
    events: Sender<KeyEvent>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    enabled: bool,
    pressed: bool,
}

impl Manager {
    fn run(mut self) {
        loop {
            if !self.enabled {
                match self.rx.recv() {
                    Ok(Msg::Enable(on)) => self.enabled = on,
                    Ok(Msg::Quit(done)) => {
                        let _ = done.send(());
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
                continue;
            }
            match open_receiver(&self.tx) {
                Some(rcv) => {
                    if let Outcome::Quit = self.serve(&rcv) {
                        return;
                    }
                    self.release();
                }
                None => {}
            }
            // No receiver (or it went away): look again later.
            match self.rx.recv_timeout(RESCAN_EVERY) {
                Ok(Msg::Quit(done)) => {
                    let _ = done.send(());
                    return;
                }
                Ok(Msg::Enable(on)) => self.enabled = on,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn serve(&mut self, rcv: &Receiver_) -> Outcome {
        let mut backlog = Vec::new();
        let mut dev = self.find_device(rcv, &mut backlog);
        match &dev {
            Some(d) => log::info!("logitech: top button (CID {CID_TOP:#06x}) on device {} diverted", d.index),
            None => log::info!("logitech: receiver found, no device with a divertable top button connected"),
        }
        let mut last_check = Instant::now();
        loop {
            for r in std::mem::take(&mut backlog) {
                self.handle(rcv, &mut dev, &r, &mut backlog);
            }
            let left = VERIFY_EVERY.saturating_sub(last_check.elapsed());
            match self.rx.recv_timeout(left) {
                Ok(Msg::Report(r)) => self.handle(rcv, &mut dev, &r, &mut backlog),
                Ok(Msg::Gone) => {
                    log::info!("logitech: receiver removed");
                    return Outcome::Gone;
                }
                Ok(Msg::Enable(on)) => {
                    self.enabled = on;
                    if !on {
                        if let Some(d) = &dev {
                            let _ = self.divert(rcv, d, false, &mut backlog);
                        }
                        self.release();
                        log::info!("logitech: top button handed back");
                        return Outcome::Gone;
                    }
                }
                Ok(Msg::Quit(done)) => {
                    if let Some(d) = &dev {
                        let _ = self.divert(rcv, d, false, &mut backlog);
                    }
                    let _ = done.send(());
                    return Outcome::Quit;
                }
                Err(RecvTimeoutError::Timeout) => {
                    last_check = Instant::now();
                    match &dev {
                        Some(d) => match self.diverted(rcv, d, &mut backlog) {
                            Ok(true) => {}
                            Ok(false) => {
                                log::info!("logitech: diversion lost, diverting again");
                                let _ = self.divert(rcv, d, true, &mut backlog);
                            }
                            // Asleep or off; the reconnect notification re-diverts.
                            Err(_) => {}
                        },
                        None => {
                            dev = self.find_device(rcv, &mut backlog);
                            if let Some(d) = &dev {
                                log::info!("logitech: top button on device {} diverted", d.index);
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Outcome::Quit,
            }
        }
    }

    fn handle(&mut self, rcv: &Receiver_, dev: &mut Option<Device>, r: &[u8], backlog: &mut Vec<Vec<u8>>) {
        if r.len() < 7 {
            return;
        }
        // HID++ 1.0 device connection notification from the receiver.
        if r[0] == SHORT && r[2] == 0x41 {
            let linked = r[4] & 0x40 == 0;
            log::info!("logitech: device {} {}", r[1], if linked { "connected" } else { "disconnected" });
            if !linked {
                if dev.as_ref().is_some_and(|d| d.index == r[1]) {
                    self.release();
                }
                return;
            }
            // Settings reset on reconnect. The device needs a moment.
            std::thread::sleep(Duration::from_millis(300));
            match dev {
                Some(d) if d.index == r[1] => {
                    for _ in 0..3 {
                        if self.divert(rcv, d, true, backlog).is_ok() {
                            log::info!("logitech: re-diverted after reconnect");
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
                Some(_) => {}
                None => {
                    *dev = self.find_device(rcv, backlog);
                }
            }
            return;
        }
        // Diverted button notification: feature reprog, function 0, sw id 0.
        if let Some(d) = dev.as_ref() {
            if r[0] == LONG && r.len() >= 12 && r[1] == d.index && r[2] == d.reprog && r[3] == 0x00 {
                let down = r[4..12].chunks(2).any(|c| u16::from_be_bytes([c[0], c[1]]) == CID_TOP);
                if down != self.pressed {
                    self.pressed = down;
                    log::info!("logitech: top button {}", if down { "down" } else { "up" });
                    self.send(if down { Input::Press } else { Input::Release });
                }
            }
        }
    }

    fn send(&self, input: Input) {
        let _ = self.events.send(KeyEvent { input, at: Instant::now() });
    }

    fn release(&mut self) {
        if self.pressed {
            self.pressed = false;
            self.send(Input::Release);
        }
    }

    fn find_device(&self, rcv: &Receiver_, backlog: &mut Vec<Vec<u8>>) -> Option<Device> {
        for index in 1..=6u8 {
            let Ok(r) = call(rcv, &self.rx, backlog, index, 0, 0, &FEATURE_REPROG.to_be_bytes()) else { continue };
            let reprog = r[0];
            if reprog == 0 {
                continue;
            }
            let dev = Device { index, reprog };
            if self.divert(rcv, &dev, true, backlog).is_ok() {
                return Some(dev);
            }
        }
        None
    }

    fn divert(&self, rcv: &Receiver_, d: &Device, on: bool, backlog: &mut Vec<Vec<u8>>) -> Result<(), String> {
        // setCidReporting: CID, flags (bit0 divert, bit1 divert valid), remap 0.
        let [hi, lo] = CID_TOP.to_be_bytes();
        let flags = if on { 0x03 } else { 0x02 };
        call(rcv, &self.rx, backlog, d.index, d.reprog, 3, &[hi, lo, flags, 0, 0]).map(|_| ())
    }

    fn diverted(&self, rcv: &Receiver_, d: &Device, backlog: &mut Vec<Vec<u8>>) -> Result<bool, String> {
        let r = call(rcv, &self.rx, backlog, d.index, d.reprog, 2, &CID_TOP.to_be_bytes())?;
        Ok(r[2] & 0x01 != 0)
    }
}

/// One HID++ 2.0 request on the long report. Reports that aren't the answer
/// go to `backlog`.
fn call(
    rcv: &Receiver_,
    rx: &Receiver<Msg>,
    backlog: &mut Vec<Vec<u8>>,
    index: u8,
    feature: u8,
    function: u8,
    params: &[u8],
) -> Result<[u8; 16], String> {
    let fs = (function << 4) | SW_ID;
    let mut req = [0u8; 20];
    req[..4].copy_from_slice(&[LONG, index, feature, fs]);
    req[4..4 + params.len()].copy_from_slice(params);
    rcv.write(&req)?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Msg::Report(r)) => {
                if r.len() >= 6 && r[1] == index {
                    if r[0] == LONG && r[2] == feature && r[3] == fs {
                        let mut out = [0u8; 16];
                        out.copy_from_slice(&r[4..20]);
                        return Ok(out);
                    }
                    // HID++ 2.0 error, or the receiver's HID++ 1.0 error.
                    if (r[2] == 0xFF || r[2] == 0x8F) && r[3] == feature && r[4] == fs {
                        return Err(format!("HID++ error {:#04x}", r[5]));
                    }
                }
                backlog.push(r);
            }
            // Control messages during a request are rare; drop them back in.
            Ok(m @ (Msg::Gone | Msg::Enable(_) | Msg::Quit(_))) => {
                requeue(m);
                return Err("interrupted".into());
            }
            Err(_) => return Err("timeout".into()),
        }
    }
}

fn requeue(m: Msg) {
    if let Some(tx) = CTL.get() {
        let _ = tx.send(m);
    }
}

