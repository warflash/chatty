//! Minimal file logger. Every line carries local wall time with milliseconds,
//! which the latency measurements in the log rely on.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Mutex<Option<File>>,
    level: log::LevelFilter,
}

fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && !m.target().starts_with("egui") && !m.target().starts_with("eframe")
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
    }));
}
