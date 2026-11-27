//! Text pipeline: filler removal and custom-word correction.
//!
//! Custom words fix Handy's flaw (`audio_toolkit/text.rs`): there the score
//! is edit distance / length, multiplied by 0.3 on a Soundex match and
//! accepted under 0.18, so "next" (1/4 * 0.3 = 0.075) always became "Nuxt".
//! Here every word has its own minimum similarity. The default depends on the
//! word's length: words of four characters or fewer need an exact
//! (case-insensitive) match, and phonetic similarity is only a small bonus
//! that never applies to short words.

use crate::config::{CustomWord, FillerWords};

/// A custom-word replacement, logged with its score.
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    pub word: String,
    pub original: String,
    /// Similarity in [0, 1]; 1 is an exact case-insensitive match.
    pub score: f32,
    /// The minimum similarity the word required.
    pub required: f32,
}

/// Lowercase alphanumeric key used for matching.
fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Character-level Levenshtein distance.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// American Soundex of an ASCII-alphabetic key; `None` otherwise.
fn soundex(s: &str) -> Option<[u8; 4]> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    let code = |c: u8| match c.to_ascii_lowercase() {
        b'b' | b'f' | b'p' | b'v' => b'1',
        b'c' | b'g' | b'j' | b'k' | b'q' | b's' | b'x' | b'z' => b'2',
        b'd' | b't' => b'3',
        b'l' => b'4',
        b'm' | b'n' => b'5',
        b'r' => b'6',
        b'h' | b'w' => b'-',
        _ => b'0',
    };
    let mut out = [b'0'; 4];
    out[0] = bytes[0].to_ascii_uppercase();
    let mut n = 1;
    let mut last = code(bytes[0]);
    for &c in &bytes[1..] {
        let d = code(c);
        if d == b'-' {
            continue;
        }
        if d != b'0' && d != last {
            out[n] = d;
            n += 1;
            if n == 4 {
                break;
            }
        }
        last = d;
    }
    Some(out)
}

/// Default minimum similarity for a custom word, by key length.
pub fn default_required(len: usize) -> f32 {
    match len {
        0..=4 => 1.0,  // exact only: "next" never becomes "Nuxt"
        5..=6 => 0.8,  // one edit
        7..=9 => 0.85, // one edit
        _ => 0.8,      // two edits from ten characters
    }
}

/// Similarity of two keys in [0, 1].
pub fn similarity(candidate: &str, target: &str) -> f32 {
    let len = candidate.chars().count().max(target.chars().count());
    if len == 0 {
        return 0.0;
    }
    let mut sim = 1.0 - levenshtein(candidate, target) as f32 / len as f32;
    // Phonetic bonus only for longer words, and never enough to turn a
    // two-edit miss into a hit on its own.
    if sim < 1.0 && len >= 6 && soundex(candidate).is_some_and(|s| Some(s) == soundex(target)) {
        sim = (sim + 0.05).min(0.99);
    }
    sim
}

struct Target<'a> {
    word: &'a CustomWord,
    key: String,
    required: f32,
}

/// Splits a token into (leading punctuation, core, trailing punctuation).
fn split_punct(token: &str) -> (&str, &str, &str) {
    let start = token.char_indices().find(|(_, c)| c.is_alphanumeric()).map_or(token.len(), |(i, _)| i);
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map_or(start, |(i, c)| i + c.len_utf8());
    if start >= end {
        return (token, "", "");
    }
    (&token[..start], &token[start..end], &token[end..])
}

/// Applies custom words to `text`. Considers n-grams of up to three words so
/// "nuxt link" can become "NuxtLink", never across punctuation.
pub fn apply_custom_words(text: &str, words: &[CustomWord]) -> (String, Vec<Correction>) {
    let targets: Vec<Target> = words
        .iter()
        .filter_map(|w| {
            let k = key(&w.word);
            if k.is_empty() {
                return None;
            }
            let required = w.strictness.unwrap_or_else(|| default_required(k.chars().count())).clamp(0.5, 1.0);
            Some(Target { word: w, key: k, required })
        })
        .collect();
    if targets.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut corrections = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        // (n, target index, score)
        let mut best: Option<(usize, usize, f32)> = None;
        for n in 1..=3usize {
            if i + n > tokens.len() {
                break;
            }
            let gram = &tokens[i..i + n];
            // Inner tokens may not carry punctuation.
            if gram[..n - 1].iter().any(|t| !split_punct(t).2.is_empty())
                || gram[1..].iter().any(|t| !split_punct(t).0.is_empty())
            {
                break;
            }
            let cand: String = gram.iter().map(|t| key(t)).collect();
            if cand.is_empty() || cand.chars().count() > 40 {
                continue;
            }
            let clen = cand.chars().count();
            for (ti, t) in targets.iter().enumerate() {
                // A multi-word candidate must start where the word starts,
                // or "a nuxt link" would swallow the "a".
                if n > 1 && cand.chars().next() != t.key.chars().next() {
                    continue;
                }
                let tlen = t.key.chars().count();
                let max_diff = ((clen.max(tlen) as f32) * 0.25).max(1.0) as usize;
                if clen.abs_diff(tlen) > max_diff {
                    continue;
                }
                let score = similarity(&cand, &t.key);
                // n grows, so `>=` lets the longer n-gram win a tie
                // ("nuxt link" -> NuxtLink rather than Nuxt + link).
                if score >= t.required && best.is_none_or(|(_, _, s)| score >= s) {
                    best = Some((n, ti, score));
                }
            }
        }
        match best {
            Some((n, ti, score)) => {
                let gram = &tokens[i..i + n];
                let (pre, _, _) = split_punct(gram[0]);
                let (_, _, post) = split_punct(gram[n - 1]);
                let original = gram.join(" ");
                let replaced = format!("{pre}{}{post}", targets[ti].word.word);
                if replaced != original {
                    corrections.push(Correction {
                        word: targets[ti].word.word.clone(),
                        original: split_punct(&original).1.to_string(),
                        score,
                        required: targets[ti].required,
                    });
                }
                out.push(replaced);
                i += n;
            }
            None => {
                out.push(tokens[i].to_string());
                i += 1;
            }
        }
    }
    (out.join(" "), corrections)
}

fn base_lang(lang: &str) -> &str {
    lang.split(['-', '_']).next().unwrap_or(lang)
}

/// Removes filler words. `any` applies to every language; the per-language
/// lists only when the output language is known ("um" is a German word).
/// Returns the text and the removed fillers.
pub fn remove_fillers(text: &str, lang: Option<&str>, fillers: &FillerWords) -> (String, Vec<String>) {
    let mut set: Vec<String> = fillers.any.iter().map(|w| w.to_lowercase()).collect();
    if let Some(lang) = lang {
        if let Some(list) = fillers.by_lang.get(base_lang(lang)) {
            set.extend(list.iter().map(|w| w.to_lowercase()));
        }
    }
    if set.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut kept: Vec<String> = Vec::new();
    let mut removed = Vec::new();
    let mut capitalize_next = false;
    for token in text.split_whitespace() {
        let (pre, core, post) = split_punct(token);
        if !core.is_empty() && set.iter().any(|f| f == &core.to_lowercase()) && !post.contains(['\'', '-']) {
            removed.push(core.to_lowercase());
            let opens = kept.last().is_none_or(|k| k.ends_with(['.', '!', '?', '…']));
            if opens && core.starts_with(char::is_uppercase) {
                capitalize_next = true;
            }
            // Keep sentence-ending punctuation: "I think, um." -> "I think."
            let end: String = post.chars().filter(|c| matches!(c, '.' | '!' | '?' | '…')).collect();
            if !end.is_empty() {
                if let Some(last) = kept.last_mut() {
                    while last.ends_with([',', ';', ':']) {
                        last.pop();
                    }
                    if !last.ends_with(['.', '!', '?', '…']) {
                        last.push_str(&end);
                    }
                }
                capitalize_next = true;
            }
            let _ = pre;
            continue;
        }
        let mut t = token.to_string();
        if capitalize_next {
            if let Some((idx, c)) = t.char_indices().find(|(_, c)| c.is_alphanumeric()) {
                let upper: String = c.to_uppercase().collect();
                t.replace_range(idx..idx + c.len_utf8(), &upper);
            }
            capitalize_next = false;
        }
        kept.push(t);
    }
    (kept.join(" "), removed)
}

/// Collapses a word repeated three or more times ("I I I think").
pub fn collapse_stutters(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let w = words[i].to_lowercase();
        let mut n = 1;
        if w.chars().all(char::is_alphabetic) {
            while i + n < words.len() && words[i + n].to_lowercase() == w {
                n += 1;
            }
        }
        out.push(words[i]);
        i += if n >= 3 { n } else { 1 };
    }
    out.join(" ")
}

/// Words: whitespace-separated tokens containing a letter or digit.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().filter(|t| t.chars().any(char::is_alphanumeric)).count()
}

/// Lowercase word keys for vocabulary statistics.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split_whitespace().map(key).filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cw(words: &[(&str, Option<f32>)]) -> Vec<CustomWord> {
        words.iter().map(|(w, s)| CustomWord { word: w.to_string(), strictness: *s }).collect()
    }

    fn handy_words() -> Vec<CustomWord> {
        cw(&[("Nuxt", None), ("Tailwind", None), ("Vite", None), ("NuxtLink", None), ("NuxtLinkLocale", None)])
    }

    fn fillers() -> FillerWords {
        let mut by_lang = HashMap::new();
        by_lang.insert("en".into(), vec!["um".into(), "ah".into(), "er".into()]);
        by_lang.insert("de".into(), vec!["äh".into(), "ähm".into()]);
        FillerWords { any: vec!["uh".into(), "uhm".into(), "hmm".into()], by_lang }
    }

    #[test]
    fn next_never_becomes_nuxt() {
        let (out, c) = apply_custom_words("go to the next page", &handy_words());
        assert_eq!(out, "go to the next page");
        assert!(c.is_empty());
    }

    #[test]
    fn exact_case_fix_is_a_correction() {
        let (out, c) = apply_custom_words("I like nuxt and vite.", &handy_words());
        assert_eq!(out, "I like Nuxt and Vite.");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].score, 1.0);
    }

    #[test]
    fn already_correct_is_not_logged() {
        let (out, c) = apply_custom_words("Nuxt uses Vite", &handy_words());
        assert_eq!(out, "Nuxt uses Vite");
        assert!(c.is_empty());
    }

    #[test]
    fn fuzzy_long_word() {
        let (out, c) = apply_custom_words("styled with tailwnd today", &handy_words());
        assert_eq!(out, "styled with Tailwind today");
        assert_eq!(c[0].original, "tailwnd");
        assert!(c[0].score >= 0.85);
    }

    #[test]
    fn ngram_joins_words() {
        let (out, _) = apply_custom_words("use a nuxt link here", &handy_words());
        assert_eq!(out, "use a NuxtLink here");
        let (out, _) = apply_custom_words("the nuxt link locale component", &handy_words());
        assert_eq!(out, "the NuxtLinkLocale component");
    }

    #[test]
    fn ngram_stops_at_punctuation() {
        let (out, _) = apply_custom_words("nuxt, link", &handy_words());
        assert_eq!(out, "Nuxt, link");
    }

    #[test]
    fn punctuation_is_kept() {
        let (out, _) = apply_custom_words("(tailwind)!", &handy_words());
        assert_eq!(out, "(Tailwind)!");
    }

    #[test]
    fn per_word_strictness() {
        // Loosened: one edit on a four-letter word is accepted.
        let loose = cw(&[("Nuxt", Some(0.75))]);
        assert_eq!(apply_custom_words("next", &loose).0, "Nuxt");
        // Tightened: a one-edit miss on a long word is rejected.
        let strict = cw(&[("Tailwind", Some(1.0))]);
        assert_eq!(apply_custom_words("tailwnd", &strict).0, "tailwnd");
    }

    #[test]
    fn short_words_need_exact_match() {
        assert_eq!(default_required(4), 1.0);
        let (out, _) = apply_custom_words("white bite kite", &handy_words());
        assert_eq!(out, "white bite kite");
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("äh", "ah"), 1);
    }

    #[test]
    fn soundex_codes() {
        assert_eq!(soundex("robert"), Some(*b"R163"));
        assert_eq!(soundex("rupert"), Some(*b"R163"));
        assert_eq!(soundex("ashcraft"), Some(*b"A261"));
        assert_eq!(soundex("x1"), None);
    }

    #[test]
    fn english_fillers() {
        let (out, removed) = remove_fillers("Um, so I think, uh, we should go.", Some("en"), &fillers());
        assert_eq!(out, "So I think, we should go.");
        assert_eq!(removed, vec!["um", "uh"]);
    }

    #[test]
    fn german_um_is_kept() {
        let (out, removed) = remove_fillers("Das ist, äh, um zehn Uhr.", Some("de"), &fillers());
        assert_eq!(out, "Das ist, um zehn Uhr.");
        assert_eq!(removed, vec!["äh"]);
    }

    #[test]
    fn unknown_language_uses_universal_only() {
        let (out, _) = remove_fillers("um uh yes", None, &fillers());
        assert_eq!(out, "um yes");
    }

    #[test]
    fn filler_before_period_moves_punctuation() {
        let (out, _) = remove_fillers("I think, um. Next one.", Some("en"), &fillers());
        assert_eq!(out, "I think. Next one.");
        let (out, _) = remove_fillers("Done uh. then", Some("en"), &fillers());
        assert_eq!(out, "Done. Then");
    }

    #[test]
    fn filler_only_input_is_empty() {
        let (out, removed) = remove_fillers("Uh, um.", Some("en"), &fillers());
        assert_eq!(out, "");
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn stutters_collapse() {
        assert_eq!(collapse_stutters("I I I think so so"), "I think so so");
    }

    #[test]
    fn counts_words() {
        assert_eq!(word_count("Hello, world - it's 5 o'clock."), 5);
        assert_eq!(word_count(""), 0);
    }
}
