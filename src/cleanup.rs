//! Optional cleanup pass with a small local LLM. Small models sometimes
//! hallucinate, answer the text, or echo their prompt, so every output is
//! checked against the input (length ratio, normalized edit distance, prompt
//! leakage) and the raw text is used when it drifts.

use std::time::Instant;

use crate::config;
use crate::llama::Llm;
use crate::models::{CleanupModel, Prompt};

/// S1-mini's system prompt. Must be verbatim (see its model card).
const S1_SYSTEM: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

const INSTRUCT_SYSTEM: &str = "You clean up dictated text. Fix punctuation, capitalization, spelling and obvious speech-recognition errors. Keep the language, the meaning and the wording; do not translate, answer, summarize or add anything. Output only the cleaned text.";

pub struct Cleaner {
    spec: &'static CleanupModel,
    llm: Llm,
    pub load_ms: f64,
}

#[derive(Debug, Clone)]
pub struct Cleaned {
    pub output: String,
    pub accepted: bool,
    pub note: Option<String>,
    pub ms: f64,
    pub length_ratio: Option<f64>,
    pub edit_ratio: Option<f64>,
}

pub fn prompt(spec: &CleanupModel, text: &str, cfg: &config::Cleanup) -> String {
    match spec.prompt {
        Prompt::S1 => format!(
            "<|im_start|>system\n{S1_SYSTEM}<|im_end|>\n<|im_start|>user\n[Styling: {}] [Structure: {}] [Context: {}]\n{text}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
            cfg.styling, cfg.structure, cfg.context
        ),
        Prompt::Instruct { think } => format!(
            "<|im_start|>system\n{INSTRUCT_SYSTEM}<|im_end|>\n<|im_start|>user\n{text}<|im_end|>\n<|im_start|>assistant\n{}",
            if think { "<think>\n\n</think>\n\n" } else { "" }
        ),
    }
}

/// Removes think blocks and surrounding quotes the model may add.
pub fn strip_output(s: &str) -> String {
    let mut s = s.to_string();
    while let (Some(a), Some(b)) = (s.find("<think>"), s.find("</think>")) {
        if b < a {
            break;
        }
        s.replace_range(a..b + "</think>".len(), "");
    }
    let t = s.trim();
    let t = t.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(t);
    t.trim().to_string()
}

/// Lowercase, alphanumerics and single spaces: compares wording, not style.
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ok((length ratio, edit ratio)) when `cleaned` stays close to `raw`.
pub fn check(raw: &str, cleaned: &str, cfg: &config::Cleanup) -> Result<(f64, f64), (String, Option<f64>, Option<f64>)> {
    const LEAKS: &[&str] = &["<|im_", "</think>", "<think>", "[Styling:", "text normalizer", "Transcript:", "cleaned text"];
    if let Some(l) = LEAKS.iter().find(|l| cleaned.contains(*l)) {
        return Err((format!("prompt leaked ({l})"), None, None));
    }
    let raw_len = raw.chars().count().max(1) as f64;
    let length_ratio = cleaned.chars().count() as f64 / raw_len;
    let (a, b) = (normalize(raw), normalize(cleaned));
    let edit_ratio = crate::text::levenshtein(&a, &b) as f64 / a.chars().count().max(1) as f64;
    if cleaned.is_empty() {
        // S1-mini returns nothing for filler-only input; trust that only
        // for very short input.
        if crate::text::word_count(raw) <= 3 {
            return Ok((0.0, 1.0));
        }
        return Err(("empty output".into(), Some(0.0), Some(1.0)));
    }
    if length_ratio < cfg.min_length_ratio as f64 || length_ratio > cfg.max_length_ratio as f64 {
        return Err((format!("length ratio {length_ratio:.2}"), Some(length_ratio), Some(edit_ratio)));
    }
    if edit_ratio > cfg.max_edit_ratio as f64 {
        return Err((format!("edit ratio {edit_ratio:.2}"), Some(length_ratio), Some(edit_ratio)));
    }
    Ok((length_ratio, edit_ratio))
}

impl Cleaner {
    pub fn load(spec: &'static CleanupModel, cfg: &config::Cleanup) -> Result<Cleaner, String> {
        let t0 = Instant::now();
        let mut llm = Llm::load(&spec.asset.path(), cfg.gpu_layers, 2048)?;
        // Warm-up so the first real request does not pay for shader setup.
        let _ = llm.complete(&prompt(spec, "hello world", cfg), 4);
        Ok(Cleaner { spec, llm, load_ms: t0.elapsed().as_secs_f64() * 1000.0 })
    }

    pub fn id(&self) -> &'static str {
        self.spec.id
    }

    pub fn clean(&mut self, text: &str, language: Option<&str>, cfg: &config::Cleanup) -> Cleaned {
        let t0 = Instant::now();
        let done = |output: String, accepted, note: Option<String>, lr, er| Cleaned {
            output,
            accepted,
            note,
            ms: t0.elapsed().as_secs_f64() * 1000.0,
            length_ratio: lr,
            edit_ratio: er,
        };
        // S1-mini is English-only.
        if self.spec.prompt == Prompt::S1 && language.is_some_and(|l| !l.starts_with("en")) {
            return done(String::new(), false, Some(format!("skipped: language {}", language.unwrap())), None, None);
        }
        let p = prompt(self.spec, text, cfg);
        let max_tokens = (self.llm.tokenize(text).map_or(64, |t| t.len()) * 3 / 2 + 24).min(1024);
        match self.llm.complete(&p, max_tokens) {
            Ok((out, n_prompt, n_gen)) => {
                let out = strip_output(&out);
                log::debug!("cleanup: {n_prompt} prompt tokens, {n_gen} generated");
                match check(text, &out, cfg) {
                    Ok((lr, er)) => done(out, true, None, Some(lr), Some(er)),
                    Err((why, lr, er)) => done(out, false, Some(why), lr, er),
                }
            }
            Err(e) => done(String::new(), false, Some(e), None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> config::Cleanup {
        config::Cleanup::default()
    }

    #[test]
    fn accepts_light_edits() {
        let raw = "so i think we should um ship it on friday";
        let out = "So I think we should ship it on Friday.";
        assert!(check(raw, out, &cfg()).is_ok());
    }

    #[test]
    fn rejects_answers_and_echoes() {
        let raw = "what is the capital of france";
        assert!(check(raw, "The capital of France is Paris, a city known for the Eiffel Tower.", &cfg()).is_err());
        assert!(check(raw, "<|im_start|>assistant What is the capital of France?", &cfg()).is_err());
        assert!(check("hello there", "You are a text normalizer for speech-to-text transcripts.", &cfg()).is_err());
    }

    #[test]
    fn rejects_truncation() {
        let raw = "first we load the model then we warm it up and finally we measure the latency of every stage";
        assert!(check(raw, "First we load the model.", &cfg()).is_err());
    }

    #[test]
    fn empty_output_only_for_short_input() {
        assert!(check("uh um", "", &cfg()).is_ok());
        assert!(check("this is a full sentence with content", "", &cfg()).is_err());
    }

    #[test]
    fn strips_think_and_quotes() {
        assert_eq!(strip_output("<think>\nhmm\n</think>\n\n\"Hello.\""), "Hello.");
        assert_eq!(strip_output("  Plain.  "), "Plain.");
    }

    #[test]
    fn s1_prompt_shape() {
        let spec = crate::models::cleanup("s1-mini").unwrap();
        let p = prompt(spec, "hi", &cfg());
        assert!(p.contains("[Styling: semi-formal] [Structure: prose] [Context: general]\nhi<|im_end|>"));
        assert!(p.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }
}
