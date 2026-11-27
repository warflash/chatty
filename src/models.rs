//! Curated model list. Adding a model means adding an entry here. Files are
//! pinned to a Hugging Face revision and verified with SHA-256 after
//! download into `%LOCALAPPDATA%\chatty\models`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Instant;

use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct Asset {
    pub repo: &'static str,
    pub revision: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

impl Asset {
    pub fn path(&self) -> PathBuf {
        crate::paths::models_dir().join(self.file)
    }

    fn url(&self) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, self.file)
    }

    /// Present and verified (the marker is written after the hash check).
    pub fn is_ready(&self) -> bool {
        let p = self.path();
        std::fs::metadata(&p).is_ok_and(|m| m.len() == self.size)
            && std::fs::read_to_string(marker(&p)).is_ok_and(|s| s.trim() == self.sha256)
    }
}

fn marker(p: &std::path::Path) -> PathBuf {
    p.with_extension("gguf.sha256")
}

/// How the model takes a language argument (transcribe.cpp compares tags
/// verbatim against the model's list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Bare codes ("en", "de"); `None` auto-detects or is ignored.
    Bare,
    /// Locales ("en-US", "de-DE") and required; "auto" detects.
    Locale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streaming {
    /// Offline: live preview by re-transcribing the growing buffer.
    No,
    /// Parakeet buffered streaming (left, chunk, right) in ms.
    Buffered { left_ms: i32, chunk_ms: i32, right_ms: i32 },
    /// Cache-aware FastConformer streaming; right context in 80 ms frames.
    CacheAware { att_right: i32 },
    /// Voxtral Realtime: delay tokens (80 ms each).
    Voxtral { delay_tokens: i32, min_decode_interval_ms: i32 },
}

#[derive(Debug)]
pub struct AsrModel {
    pub id: &'static str,
    pub name: &'static str,
    pub asset: Asset,
    pub languages: &'static [&'static str],
    pub lang: Lang,
    pub streaming: Streaming,
    /// Why this model is on the list (documentation; not shown in the UI).
    #[allow(dead_code)]
    pub note: &'static str,
}

impl AsrModel {
    /// The language argument for `RunOptions::language`.
    pub fn language_arg(&self, configured: &str) -> Option<String> {
        let configured = configured.trim();
        let auto = configured.is_empty() || configured == "auto";
        match self.lang {
            Lang::Locale => {
                if auto {
                    return Some("auto".into());
                }
                let base = configured.split(['-', '_']).next().unwrap_or(configured);
                self.languages
                    .iter()
                    .find(|l| l.eq_ignore_ascii_case(configured))
                    .or_else(|| self.languages.iter().find(|l| l.split('-').next() == Some(base)))
                    .map(|l| l.to_string())
                    .or(Some("auto".into()))
            }
            Lang::Bare => {
                if auto || self.languages.len() == 1 {
                    return None;
                }
                let base = configured.split(['-', '_']).next().unwrap_or(configured).to_lowercase();
                self.languages.contains(&base.as_str()).then_some(base)
            }
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming != Streaming::No
    }
}

pub static ASR: &[AsrModel] = &[
    AsrModel {
        id: "parakeet-unified-en-0.6b",
        name: "Parakeet Unified EN 0.6B (streaming)",
        asset: Asset {
            repo: "handy-computer/parakeet-unified-en-0.6b-gguf",
            revision: "843b1b175ace04f5a89d379c79e2518c0d065b25",
            file: "parakeet-unified-en-0.6b-Q8_0.gguf",
            sha256: "4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795",
            size: 731_357_568,
        },
        languages: &["en"],
        lang: Lang::Bare,
        // (70, 2, 2): 160 ms lookahead, 1.64% WER on test-clean-512 vs
        // 1.59% offline. (70, 1, 1) is 1.90%.
        streaming: Streaming::Buffered { left_ms: 5600, chunk_ms: 160, right_ms: 160 },
        note: "English, 1.60% LibriSpeech WER",
    },
    AsrModel {
        id: "nemotron-3.5-asr-streaming-0.6b",
        name: "Nemotron 3.5 ASR 0.6B (32 languages, streaming)",
        asset: Asset {
            repo: "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf",
            revision: "53ce288f69326b415aeef030cfcb02a08a8096dd",
            file: "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
            sha256: "b94545b313b3223fda7b2857a52681da813935c2127643d1e9ff0c23d988089c",
            size: 751_094_240,
        },
        languages: &[
            "en-US", "en-GB", "es-US", "es-ES", "fr-FR", "fr-CA", "it-IT", "pt-BR", "pt-PT", "nl-NL", "de-DE", "tr-TR",
            "ru-RU", "ar-AR", "hi-IN", "ja-JP", "ko-KR", "vi-VN", "uk-UA", "pl-PL", "sv-SE", "cs-CZ", "nb-NO", "da-DK",
            "bg-BG", "fi-FI", "hr-HR", "sk-SK", "zh-CN", "hu-HU", "ro-RO", "et-EE",
        ],
        lang: Lang::Locale,
        // att_context_right 6 = 480 ms lookahead; 13 is byte-equal to offline.
        streaming: Streaming::CacheAware { att_right: 6 },
        note: "needs a locale or auto; auto strips <lang> tags",
    },
    AsrModel {
        id: "voxtral-mini-4b-realtime-2602",
        name: "Voxtral Mini 4B Realtime (multilingual, streaming, GPU)",
        asset: Asset {
            repo: "handy-computer/Voxtral-Mini-4B-Realtime-2602-gguf",
            revision: "7fd3d424fc558edca2c53808578b0541ef52a81a",
            file: "Voxtral-Mini-4B-Realtime-2602-Q4_K_M.gguf",
            sha256: "39dc1f65539373a406edea7490505822d77c12edff521744678717eef4da4723",
            size: 2_830_493_984,
        },
        languages: &["en", "fr", "es", "de", "ru", "zh", "ja", "it", "pt", "nl", "ar", "hi", "ko"],
        lang: Lang::Bare,
        // 6 delay tokens = 480 ms (family default for streams).
        streaming: Streaming::Voxtral { delay_tokens: 6, min_decode_interval_ms: 250 },
        note: "auto language only; slower than realtime on CPU",
    },
    AsrModel {
        id: "parakeet-ultra",
        name: "Parakeet Ultra (25 European languages)",
        asset: Asset {
            repo: "handy-computer/parakeet-ultra-gguf",
            revision: "a1539be1b849ea7463b88343e5277b373d0c4ea9",
            file: "parakeet-ultra-Q8_0.gguf",
            sha256: "007a59761e9258779f189df396b50d12b83bb5b79e66df4b955c230d2f2a0a59",
            size: 740_363_168,
        },
        languages: &[
            "en", "de", "fr", "es", "it", "pt", "ru", "uk", "hr", "sl", "lv", "lt", "et", "fi", "sv", "da", "nl", "pl",
            "cs", "sk", "hu", "ro", "bg", "el", "mt",
        ],
        lang: Lang::Bare,
        streaming: Streaming::No,
        note: "best European accuracy; writes ss for ß",
    },
    AsrModel {
        id: "cohere-transcribe-03-2026",
        name: "Cohere Transcribe 2B (14 languages)",
        asset: Asset {
            repo: "handy-computer/cohere-transcribe-03-2026-gguf",
            revision: "7353b6040bdc1be9e5a3823d99d5fe12afe88264",
            file: "cohere-transcribe-03-2026-Q5_K_M.gguf",
            sha256: "14d02f1ad6dd77b3a60f82639879012c3adb4fe25c50a5a47a2c4c661daf1558",
            size: 1_770_270_208,
        },
        languages: &["en", "fr", "de", "es", "it", "pt", "nl", "pl", "el", "ar", "ja", "zh", "vi", "ko"],
        lang: Lang::Bare,
        streaming: Streaming::No,
        note: "no auto-detect: falls back to en without a language",
    },
    AsrModel {
        id: "qwen3-asr-1.7b",
        name: "Qwen3-ASR 1.7B (30 languages, GPU)",
        asset: Asset {
            repo: "handy-computer/Qwen3-ASR-1.7B-gguf",
            revision: "0e0a596e3959c3f3f2ab7d443a5b6f0f4a4bba6f",
            file: "Qwen3-ASR-1.7B-Q8_0.gguf",
            sha256: "9a0d81792dfea2d5f278b8a63deb3ea6e02139ce42c2301f32ea19c4f77526b7",
            size: 2_185_030_624,
        },
        languages: &[
            "zh", "en", "yue", "ar", "de", "fr", "es", "pt", "id", "it", "ko", "ru", "th", "vi", "ja", "tr", "hi", "ms",
            "nl", "sv", "da", "fi", "pl", "cs", "fil", "fa", "el", "ro", "hu", "mk",
        ],
        lang: Lang::Bare,
        streaming: Streaming::No,
        note: "top accuracy; supports vocabulary prompting",
    },
    AsrModel {
        id: "granite-speech-5.0-470m-turboctc",
        name: "Granite Speech 5.0 470M TurboCTC (fastest English)",
        asset: Asset {
            repo: "handy-computer/granite-speech-5.0-470m-turboctc-gguf",
            revision: "0cf5be9f4bcd479989e6a24d142094d34d552daa",
            file: "granite-speech-5.0-470m-turboctc-Q8_0.gguf",
            sha256: "0408fe0b33be19af5be423d8ba6b43768608b6f065e1ac97b78c84b55c140738",
            size: 505_606_496,
        },
        languages: &["en"],
        lang: Lang::Bare,
        streaming: Streaming::No,
        note: "lowercase output without punctuation",
    },
];

pub static VAD: Asset = Asset {
    repo: "handy-computer/silero-vad-v6.2-gguf",
    revision: "a9b2d61c20ae4ddf3644edb34969e8185a485448",
    file: "silero-vad-v6.2-F32.gguf",
    sha256: "0d871a800c420fdd5d1b17a3aea4296d1c39608f0ff4a9295eb76457eb1c88e6",
    size: 1_241_056,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// Superwhisper S1-mini: fixed system prompt + control line, thinking off.
    S1,
    /// Instruction model with ChatML; `think` adds an empty think block.
    Instruct { think: bool },
}

#[derive(Debug)]
pub struct CleanupModel {
    pub id: &'static str,
    pub name: &'static str,
    pub asset: Asset,
    pub prompt: Prompt,
}

pub static CLEANUP: &[CleanupModel] = &[
    // Apache-2.0 + attribution term: refer to it as "S1-mini" by "Superwhisper".
    CleanupModel {
        id: "s1-mini",
        name: "S1-mini by Superwhisper (0.6B)",
        asset: Asset {
            repo: "superwhisper/s1-mini-GGUF",
            revision: "34add00a48a2e5d24e5a4ee5405a99620a3a240c",
            file: "s1-mini-q4_k_m.gguf",
            sha256: "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634",
            size: 484_219_808,
        },
        prompt: Prompt::S1,
    },
    CleanupModel {
        id: "qwen2.5-0.5b",
        name: "Qwen2.5 0.5B Instruct",
        asset: Asset {
            repo: "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
            revision: "9217f5db79a29953eb74d5343926648285ec7e67",
            file: "qwen2.5-0.5b-instruct-q8_0.gguf",
            sha256: "ca59ca7f13d0e15a8cfa77bd17e65d24f6844b554a7b6c12e07a5f89ff76844e",
            size: 675_710_816,
        },
        prompt: Prompt::Instruct { think: false },
    },
    CleanupModel {
        id: "qwen3.5-0.8b",
        name: "Qwen3.5 0.8B",
        asset: Asset {
            repo: "unsloth/Qwen3.5-0.8B-GGUF",
            revision: "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0",
            file: "Qwen3.5-0.8B-Q8_0.gguf",
            sha256: "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c",
            size: 811_843_840,
        },
        prompt: Prompt::Instruct { think: true },
    },
];

pub fn asr(id: &str) -> Option<&'static AsrModel> {
    ASR.iter().find(|m| m.id == id)
}

pub fn cleanup(id: &str) -> Option<&'static CleanupModel> {
    CLEANUP.iter().find(|m| m.id == id)
}

fn agent() -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .user_agent("chatty")
        .build()
        .into()
}

/// Downloads and verifies `asset` unless it is already present.
/// `progress` receives the completed fraction.
pub fn ensure(asset: &Asset, mut progress: impl FnMut(f32)) -> Result<PathBuf, String> {
    let path = asset.path();
    if asset.is_ready() {
        return Ok(path);
    }
    std::fs::create_dir_all(crate::paths::models_dir()).map_err(|e| e.to_string())?;
    let part = path.with_extension("gguf.part");
    let t0 = Instant::now();
    log::info!("downloading {} ({} MB)", asset.file, asset.size >> 20);
    let response = agent().get(&asset.url()).call().map_err(|e| format!("download {}: {e}", asset.file))?;
    let mut reader = response.into_body().into_with_config().limit(asset.size + 1).reader();
    let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    let mut last = 0.0f32;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download {}: {e}", asset.file))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        let f = done as f32 / asset.size as f32;
        if f - last >= 0.01 {
            last = f;
            progress(f.min(1.0));
        }
    }
    drop(file);
    let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if done != asset.size || digest != asset.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{}: size {done} sha256 {digest} does not match the pinned file", asset.file));
    }
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    std::fs::write(marker(&path), asset.sha256).map_err(|e| e.to_string())?;
    log::info!("downloaded {} in {:.1?}", asset.file, t0.elapsed());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_unique_and_hashes_well_formed() {
        let mut ids: Vec<&str> = ASR.iter().map(|m| m.id).chain(CLEANUP.iter().map(|m| m.id)).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for a in ASR.iter().map(|m| &m.asset).chain(CLEANUP.iter().map(|m| &m.asset)).chain([&VAD]) {
            assert_eq!(a.sha256.len(), 64, "{}", a.file);
            assert_eq!(a.revision.len(), 40, "{}", a.file);
        }
    }

    #[test]
    fn language_arguments() {
        let parakeet = asr("parakeet-unified-en-0.6b").unwrap();
        assert_eq!(parakeet.language_arg("de"), None);
        let nemo = asr("nemotron-3.5-asr-streaming-0.6b").unwrap();
        assert_eq!(nemo.language_arg("auto").as_deref(), Some("auto"));
        assert_eq!(nemo.language_arg("de").as_deref(), Some("de-DE"));
        assert_eq!(nemo.language_arg("en-GB").as_deref(), Some("en-GB"));
        let ultra = asr("parakeet-ultra").unwrap();
        assert_eq!(ultra.language_arg("de-DE").as_deref(), Some("de"));
        assert_eq!(ultra.language_arg("auto"), None);
        assert_eq!(ultra.language_arg("xx"), None);
    }
}
