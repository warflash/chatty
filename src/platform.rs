//! Words and small facts that differ between Windows and macOS, so the
//! shared UI and config read naturally on each.

#[cfg(windows)]
mod imp {
    pub const OS_NAME: &str = "Windows";
    pub const GPU_BACKEND: &str = "vulkan";
    pub const GPU_NAME: &str = "Vulkan";
    pub const DEFAULT_FONT: &str = "Segoe UI Variable";
    pub const PASTE_KEYS: &str = "Ctrl+V";
    pub const AUTOSTART_LABEL: &str = "Start with Windows";
    pub const FONTS_HINT: &str = r"Installed fonts, or .ttf files in %LOCALAPPDATA%\chatty\fonts";
}

#[cfg(target_os = "macos")]
mod imp {
    pub const OS_NAME: &str = "macOS";
    pub const GPU_BACKEND: &str = "metal";
    pub const GPU_NAME: &str = "Metal";
    pub const DEFAULT_FONT: &str = "SF Pro";
    pub const PASTE_KEYS: &str = "⌘V";
    pub const AUTOSTART_LABEL: &str = "Open at login";
    pub const FONTS_HINT: &str = "Installed fonts, or .ttf files in ~/Library/Application Support/chatty/fonts";
}

pub use imp::*;

/// How to start a dictation, for the Overview page.
pub fn trigger_hint(trigger: &str) -> &'static str {
    if cfg!(windows) {
        return "Hold Ctrl + Numpad / to dictate into any window. A short tap presses Enter; Escape cancels.";
    }
    match trigger {
        "right_option" => "Hold the right Option key to dictate into any app. A short tap presses Enter; Escape cancels.",
        "right_command" => "Hold the right Command key to dictate into any app. A short tap presses Enter; Escape cancels.",
        "fn" => "Hold the fn (Globe) key to dictate into any app. A short tap presses Enter; Escape cancels.",
        _ => "Hold Control + keypad / to dictate into any app. A short tap presses Enter; Escape cancels.",
    }
}

/// Local calendar time of a unix-ms timestamp (DST-correct).
#[cfg(target_os = "macos")]
pub struct Tm {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday.
    pub weekday: u32,
}

#[cfg(target_os = "macos")]
pub fn local_tm(epoch_ms: i64) -> Tm {
    let secs: libc::time_t = epoch_ms.div_euclid(1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    Tm {
        year: tm.tm_year + 1900,
        month: tm.tm_mon as u32 + 1,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
        weekday: tm.tm_wday as u32,
    }
}
