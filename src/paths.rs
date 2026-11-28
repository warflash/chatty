use std::path::PathBuf;

pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(windows)]
fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(exe_dir).join("chatty")
}

/// `%APPDATA%\chatty`: the config.
#[cfg(windows)]
pub fn roaming_dir() -> PathBuf {
    env_dir("APPDATA")
}

/// `%LOCALAPPDATA%\chatty`: models, database, history audio, log.
#[cfg(windows)]
pub fn local_dir() -> PathBuf {
    env_dir("LOCALAPPDATA")
}

/// `~/Library/Application Support/chatty`: config, models, database,
/// history audio and log.
#[cfg(target_os = "macos")]
pub fn roaming_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
        .unwrap_or_else(exe_dir)
        .join("chatty")
}

#[cfg(target_os = "macos")]
pub fn local_dir() -> PathBuf {
    roaming_dir()
}

pub fn models_dir() -> PathBuf {
    local_dir().join("models")
}

pub fn history_dir() -> PathBuf {
    local_dir().join("history")
}

pub fn db_path() -> PathBuf {
    local_dir().join("stats.db")
}

pub fn log_path() -> PathBuf {
    local_dir().join("chatty.log")
}

/// The llama.cpp library file.
pub const LLAMA_LIB: &str = if cfg!(windows) { "llama.dll" } else { "libllama.1.dylib" };

/// llama.cpp libraries: `llama/` next to the exe (macOS app bundle:
/// `Contents/Frameworks/llama`); in a dev build the fetched copy.
pub fn llama_dir() -> PathBuf {
    let candidates = [exe_dir().join("llama"), exe_dir().join("../Frameworks/llama")];
    for dir in candidates {
        if dir.join(LLAMA_LIB).exists() {
            return dir;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("native").join("llama")
}
