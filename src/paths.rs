use std::path::PathBuf;

pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(exe_dir).join("chatty")
}

/// `%APPDATA%\chatty`: the config.
pub fn roaming_dir() -> PathBuf {
    env_dir("APPDATA")
}

/// `%LOCALAPPDATA%\chatty`: models, database, history audio, log.
pub fn local_dir() -> PathBuf {
    env_dir("LOCALAPPDATA")
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

/// llama.cpp DLLs: `llama\` next to the exe; in a dev build the fetched copy.
pub fn llama_dir() -> PathBuf {
    let beside = exe_dir().join("llama");
    if beside.join("llama.dll").exists() {
        return beside;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("native").join("llama")
}
