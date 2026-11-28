//! Minimal file logger. Every line carries local wall time with milliseconds,
//! which the latency measurements in the log rely on.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Mutex<Option<File>>,
    level: log::LevelFilter,
}

#[cfg(target_os = "macos")]
fn timestamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let t = crate::platform::local_tm(now.as_millis() as i64);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, now.subsec_millis()
    )
}

#[cfg(windows)]
fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        let t = m.target();
        // transcribe.cpp logs every chunk at debug; keep it to trace.
        let level = if t.starts_with("transcribe_cpp") && self.level < log::LevelFilter::Trace {
            log::LevelFilter::Info
        } else {
            self.level
        };
        m.level() <= level && !t.starts_with("egui") && !t.starts_with("eframe") && !t.starts_with("winit")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let line = format!("{} {:5} [{}] {}\n", timestamp(), r.level(), thread.name().unwrap_or("?"), r.args());
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = f.flush();
        }
    }
}

pub fn init() {
    let path = crate::paths::log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep one previous log once this one passes 5 MB.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let level = match std::env::var("CHATTY_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Info,
    };
    let _ = log::set_boxed_logger(Box::new(Logger { file: Mutex::new(file), level }));
    log::set_max_level(level);
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
        // Also visible when run from a terminal (the command-line checks).
        eprintln!("panic: {info}");
    }));
}
