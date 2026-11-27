//! `config.toml`: next to the exe if present there, else `%APPDATA%\chatty`.
//! Reloaded on change. There is no settings UI; the tray menu edits the few
//! keys it switches (model, language, cleanup) in place with `toml_edit`, so
//! comments survive.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomWord {
    pub word: String,
    /// Minimum similarity in [0.5, 1]; `None` uses the length-based default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strictness: Option<f32>,
}

impl<'de> Deserialize<'de> for CustomWord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Plain(String),
            Full { word: String, strictness: Option<f32> },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Plain(word) => CustomWord { word, strictness: None },
            Repr::Full { word, strictness } => CustomWord { word, strictness },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillerWords {
    /// Removed in every language.
    #[serde(default)]
    pub any: Vec<String>,
    /// Removed only when the output language is known: `en = [...]`.
    #[serde(flatten)]
    pub by_lang: HashMap<String, Vec<String>>,
}

impl Default for FillerWords {
    fn default() -> Self {
        let mut by_lang = HashMap::new();
        by_lang.insert("en".into(), vec!["um".into(), "umm".into(), "ah".into(), "er".into(), "erm".into()]);
        by_lang.insert("de".into(), vec!["äh".into(), "ähm".into(), "öhm".into(), "hm".into()]);
        FillerWords {
            any: ["uh", "uhm", "uhh", "hmm", "mmm", "ehm"].map(String::from).to_vec(),
            by_lang,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cleanup {
    pub enabled: bool,
    /// Cleanup model id from `models::CLEANUP`.
    pub model: String,
    /// S1-mini control line values.
    pub styling: String,
    pub structure: String,
    pub context: String,
    /// Fall back to the raw text when cleaned/raw length leaves this range.
    pub min_length_ratio: f32,
    pub max_length_ratio: f32,
    /// Fall back when edit distance / raw length exceeds this.
    pub max_edit_ratio: f32,
    /// Layers offloaded to the GPU (99 = all).
    pub gpu_layers: i32,
}

impl Default for Cleanup {
    fn default() -> Self {
        Cleanup {
            enabled: false,
            model: "s1-mini".into(),
            styling: "semi-formal".into(),
            structure: "prose".into(),
            context: "general".into(),
            min_length_ratio: 0.5,
            max_length_ratio: 1.4,
            max_edit_ratio: 0.45,
            gpu_layers: 99,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Input device name as Windows shows it; empty = system default.
    pub microphone: String,
    pub hold_threshold_ms: u64,
    /// Speech model id from `models::ASR`.
    pub model: String,
    /// "auto", "en", "de", ...
    pub language: String,
    /// "auto", "vulkan" or "cpu".
    pub backend: String,
    /// Unload the speech model after this many idle minutes (0 = never).
    pub idle_unload_minutes: u64,
    /// Keep the microphone stream open this long after a dictation (0 = close
    /// immediately). Reopening costs tens of milliseconds.
    pub mic_keep_open_secs: u64,
    pub paste_delay_before_ms: u64,
    pub paste_delay_after_ms: u64,
    /// Live preview cadence for offline (non-streaming) models.
    pub preview_interval_ms: u64,
    /// "segments": full-context passes (live-corrected preview, final pass
    /// on release). "stream": the model's streaming decoder (lower load,
    /// for CPU-only use).
    pub decode: String,
    /// Silero VAD speech probability threshold.
    pub vad_threshold: f32,
    pub remove_fillers: bool,
    pub filler_words: FillerWords,
    pub custom_words: Vec<CustomWord>,
    pub cleanup: Cleanup,
    /// Dictations whose audio is kept for the history view.
    pub history_size: usize,
    /// Your typing speed, for "time saved".
    pub typing_wpm: f32,
    pub autostart: bool,
    /// Before pasting, ask UI Automation whether a text field has focus; if
    /// clearly not, show the transcript with a Copy button instead.
    pub check_focus: bool,
    pub overlay: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            microphone: String::new(),
            hold_threshold_ms: 300,
            model: "parakeet-unified-en-0.6b".into(),
            language: "auto".into(),
            backend: "auto".into(),
            idle_unload_minutes: 60,
            mic_keep_open_secs: 120,
            paste_delay_before_ms: 60,
            paste_delay_after_ms: 60,
            preview_interval_ms: 300,
            decode: "segments".into(),
            vad_threshold: 0.5,
            remove_fillers: true,
            filler_words: FillerWords::default(),
            custom_words: Vec::new(),
            cleanup: Cleanup::default(),
            history_size: 20,
            typing_wpm: 45.0,
            autostart: true,
            check_focus: true,
            overlay: true,
        }
    }
}

/// Values taken from Handy's settings on first run.
#[derive(Debug, Default)]
struct HandySeed {
    microphone: Option<String>,
    custom_words: Vec<String>,
    remove_fillers: Option<bool>,
    paste_before: Option<u64>,
    paste_after: Option<u64>,
    hold_threshold: Option<u64>,
}

fn read_handy_seed() -> HandySeed {
    let Some(appdata) = std::env::var_os("APPDATA") else { return HandySeed::default() };
    let path = Path::new(&appdata).join("com.pais.handy").join("settings_store.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return HandySeed::default() };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return HandySeed::default() };
    let s = &json["settings"];
    log::info!("seeding config from {}", path.display());
    HandySeed {
        microphone: s["selected_microphone"].as_str().map(String::from),
        custom_words: s["custom_words"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        remove_fillers: s["filler_word_removal_enabled"].as_bool(),
        paste_before: s["paste_delay_ms"].as_u64(),
        paste_after: s["paste_delay_after_ms"].as_u64(),
        hold_threshold: s["hold_threshold_ms"].as_u64(),
    }
}

fn toml_str(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_string()
}

/// The commented file written on first run.
fn template(seed: &HandySeed) -> String {
    let d = Config::default();
    let mic = seed.microphone.clone().unwrap_or_default();
    let words = if seed.custom_words.is_empty() {
        "# { word = \"Nuxt\", strictness = 1.0 },".to_string()
    } else {
        seed.custom_words.iter().map(|w| format!("  {},", toml_str(w))).collect::<Vec<_>>().join("\n")
    };
    format!(
        r#"# chatty configuration. Saved changes apply immediately.

# Input device name as Windows shows it ("" = system default).
microphone = {mic}

# Hold the trigger this long to record; a shorter tap presses Enter.
hold_threshold_ms = {hold}

# Speech model (switch from the tray): parakeet-unified-en-0.6b,
# nemotron-3.5-asr-streaming-0.6b, voxtral-mini-4b-realtime-2602,
# parakeet-ultra, cohere-transcribe-03-2026, qwen3-asr-1.7b,
# granite-speech-5.0-470m-turboctc
model = "{model}"
# "auto", or a language code such as "en" or "de".
language = "auto"
# "auto" (GPU when available), "vulkan" or "cpu".
backend = "auto"
# Unload the speech model after this many idle minutes (0 = never).
idle_unload_minutes = {idle}
# Keep the microphone open this long after a dictation (0 = close at once).
mic_keep_open_secs = {mic_keep}

# Clipboard paste timing.
paste_delay_before_ms = {before}
paste_delay_after_ms = {after}

# How often the live preview is re-transcribed (it corrects itself as you speak).
preview_interval_ms = {preview}
# "segments": every pass sees the whole sentence; the pasted text comes from
# a final pass over your speech. "stream": the model's streaming decoder
# (cheaper, for CPU-only machines).
decode = "segments"
# Silero VAD speech threshold (0..1).
vad_threshold = {vad}

remove_fillers = {fillers}

# Corrections applied after transcription. A plain string uses a default
# strictness by length (words of 4 letters or fewer must match exactly);
# a table sets the minimum similarity (0.5..1.0) for that word.
custom_words = [
{words}
]

# Keep audio of this many recent dictations for the history view.
history_size = {history}
# Your typing speed, used for "time saved".
typing_wpm = {wpm}
autostart = true
overlay = true
# Before pasting, check that a text field has focus; if clearly not, the
# transcript waits at the bottom of the screen with a Copy button.
check_focus = true

[filler_words]
any = ["uh", "uhm", "uhh", "hmm", "mmm", "ehm"]
en = ["um", "umm", "ah", "er", "erm"]
de = ["äh", "ähm", "öhm", "hm"]

# Optional cleanup with a small local LLM (toggle from the tray).
[cleanup]
enabled = false
# s1-mini, qwen2.5-0.5b, qwen3.5-0.8b
model = "s1-mini"
# S1-mini control line: casual | semi-casual | semi-formal | formal,
# prose | lists, general | email.
styling = "semi-formal"
structure = "prose"
context = "general"
# Fall back to the raw text when the output drifts too far.
min_length_ratio = {minr}
max_length_ratio = {maxr}
max_edit_ratio = {maxe}
gpu_layers = 99
"#,
        mic = toml_str(&mic),
        hold = seed.hold_threshold.unwrap_or(d.hold_threshold_ms),
        model = d.model,
        idle = d.idle_unload_minutes,
        mic_keep = d.mic_keep_open_secs,
        before = seed.paste_before.unwrap_or(d.paste_delay_before_ms),
        after = seed.paste_after.unwrap_or(d.paste_delay_after_ms),
        preview = d.preview_interval_ms,
        vad = d.vad_threshold,
        fillers = seed.remove_fillers.unwrap_or(true),
        history = d.history_size,
        wpm = d.typing_wpm,
        minr = d.cleanup.min_length_ratio,
        maxr = d.cleanup.max_length_ratio,
        maxe = d.cleanup.max_edit_ratio,
    )
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Where the config lives: next to the exe if a file is there, else APPDATA.
pub fn path() -> PathBuf {
    let beside = crate::paths::exe_dir().join("config.toml");
    if beside.exists() { beside } else { crate::paths::roaming_dir().join("config.toml") }
}

/// Loads the config, creating it (seeded from Handy) on first run.
pub fn load_or_create(path: &Path) -> Config {
    if !path.exists() {
        let seed = read_handy_seed();
        let text = template(&seed);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, &text) {
            log::error!("cannot write {}: {e}", path.display());
        }
        return parse(&text).unwrap_or_default();
    }
    match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
        Ok(c) => c,
        Err(e) => {
            log::error!("config {}: {e}; using defaults", path.display());
            Config::default()
        }
    }
}

/// Sets top-level or dotted (`cleanup.enabled`) keys, keeping comments.
pub fn set_values(path: &Path, values: &[(&str, toml_edit::Value)]) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    for (key, value) in values {
        let mut parts: Vec<&str> = key.split('.').collect();
        let last = parts.pop().unwrap();
        let mut table = doc.as_table_mut();
        for p in parts {
            if !table.contains_key(p) {
                table.insert(p, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            table = table[p].as_table_mut().ok_or_else(|| format!("{p} is not a table"))?;
        }
        match table.get_mut(last).and_then(|i| i.as_value_mut()) {
            Some(existing) => {
                let decor = existing.decor().clone();
                *existing = value.clone();
                *existing.decor_mut() = decor;
            }
            None => {
                table.insert(last, toml_edit::value(value.clone()));
            }
        }
    }
    std::fs::write(path, doc.to_string()).map_err(|e| e.to_string())
}

/// Shared, hot-reloaded configuration.
#[derive(Clone)]
pub struct Shared {
    inner: Arc<RwLock<Arc<Config>>>,
    pub path: Arc<PathBuf>,
}

impl Shared {
    pub fn new(path: PathBuf, config: Config) -> Self {
        Shared { inner: Arc::new(RwLock::new(Arc::new(config))), path: Arc::new(path) }
    }

    pub fn get(&self) -> Arc<Config> {
        self.inner.read().unwrap().clone()
    }

    fn replace(&self, config: Config) -> Arc<Config> {
        let new = Arc::new(config);
        *self.inner.write().unwrap() = new.clone();
        new
    }

    pub fn set(&self, values: &[(&str, toml_edit::Value)]) {
        if let Err(e) = set_values(&self.path, values) {
            log::error!("cannot update config: {e}");
        }
        // The watcher picks the change up and notifies everyone.
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Watches the config's directory and calls `on_change(old, new)` after a
/// successful reload. Blocks in the kernel between changes (no polling).
pub fn watch(shared: Shared, on_change: impl Fn(&Config, &Config) + Send + 'static) {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FindFirstChangeNotificationW,
        FindNextChangeNotification,
    };
    use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
    use windows::core::HSTRING;

    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let path = shared.path.as_ref().clone();
            let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            let handle = match unsafe {
                FindFirstChangeNotificationW(
                    &HSTRING::from(dir.as_os_str()),
                    false,
                    FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME,
                )
            } {
                Ok(h) => h,
                Err(e) => {
                    log::error!("config watch failed: {e}");
                    return;
                }
            };
            let mut last = mtime(&path);
            loop {
                if unsafe { WaitForSingleObject(handle, INFINITE) } != WAIT_OBJECT_0 {
                    return;
                }
                // Editors write in several steps; let them finish.
                std::thread::sleep(std::time::Duration::from_millis(150));
                let _ = unsafe { FindNextChangeNotification(handle) };
                let now = mtime(&path);
                if now == last {
                    continue;
                }
                last = now;
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                match parse(&text) {
                    Ok(config) => {
                        let old = shared.get();
                        if *old == config {
                            continue;
                        }
                        let new = shared.replace(config);
                        log::info!("config reloaded");
                        on_change(&old, &new);
                    }
                    Err(e) => {
                        log::error!("config not reloaded: {e}");
                        crate::overlay::notice(&format!("config.toml: {}", e.lines().next().unwrap_or("")));
                    }
                }
            }
        })
        .expect("spawn config watcher");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_with_seed() {
        let seed = HandySeed {
            microphone: Some("Mikrofon (Aukey-PC-LM1E Audio)".into()),
            custom_words: vec!["Nuxt".into(), "Tailwind".into(), "Vite".into(), "NuxtLink".into(), "NuxtLinkLocale".into()],
            remove_fillers: Some(true),
            paste_before: Some(60),
            paste_after: Some(60),
            hold_threshold: Some(300),
        };
        let c = parse(&template(&seed)).unwrap();
        assert_eq!(c.microphone, "Mikrofon (Aukey-PC-LM1E Audio)");
        assert_eq!(c.custom_words.len(), 5);
        assert_eq!(c.custom_words[3], CustomWord { word: "NuxtLink".into(), strictness: None });
        assert_eq!(c.paste_delay_before_ms, 60);
        assert_eq!(c.filler_words.by_lang["de"], vec!["äh", "ähm", "öhm", "hm"]);
        assert!(!c.cleanup.enabled);
    }

    #[test]
    fn custom_word_table_form() {
        let c = parse(r#"custom_words = ["Vite", { word = "Nuxt", strictness = 0.9 }]"#).unwrap();
        assert_eq!(c.custom_words[1].strictness, Some(0.9));
    }

    #[test]
    fn set_values_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("chatty-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "# hello\nmodel = \"a\" # trailing\n[cleanup]\nenabled = false\n").unwrap();
        set_values(&p, &[("model", "b".into()), ("cleanup.enabled", true.into())]).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("# hello"));
        assert!(t.contains("model = \"b\" # trailing"));
        assert!(parse(&t).unwrap().cleanup.enabled);
        std::fs::remove_dir_all(dir).ok();
    }
}
