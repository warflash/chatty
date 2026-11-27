//! Command-line measurements (no tray, no hook):
//!
//!   chatty --bench <file.wav> [model-id]     transcription latency per model
//!   chatty --bench-cleanup "<text>" [model]  cleanup latency and output
//!   chatty --download <model-id>             fetch a model ahead of time

use std::time::Instant;

use transcribe_cpp::{Model, ModelOptions, RunOptions};

use crate::models;

fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn ensure(asset: &'static models::Asset) -> Result<std::path::PathBuf, String> {
    let mut last = -1i32;
    models::ensure(asset, |f| {
        let pct = (f * 100.0) as i32;
        if pct / 10 != last / 10 {
            println!("  {} {pct}%", asset.file);
            last = pct;
        }
    })
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn bench_asr(wav: &str, ids: &[String]) -> Result<(), String> {
    transcribe_cpp::init_backends_default().map_err(|e| e.to_string())?;
    let (pcm, rate) = crate::wav::read(std::path::Path::new(wav)).map_err(|e| format!("{wav}: {e}"))?;
    let mut audio = Vec::new();
    let mut rs = crate::audio::Resampler::new(rate);
    rs.process(&pcm, &mut audio);
    rs.flush(&mut audio);
    let secs = audio.len() as f64 / 16000.0;
    println!("audio {wav}: {secs:.2} s");
    let ids: Vec<&str> = if ids.is_empty() { vec!["parakeet-unified-en-0.6b"] } else { ids.iter().map(String::as_str).collect() };
    for id in ids {
        let spec = models::asr(id).ok_or_else(|| format!("unknown model {id}"))?;
        let path = ensure(&spec.asset)?;
        let t = Instant::now();
        let model = Model::load_with(&path, &ModelOptions::default()).map_err(|e| e.to_string())?;
        let mut session = model.session().map_err(|e| e.to_string())?;
        let load = ms(t);
        let run = RunOptions { language: spec.language_arg("auto"), ..Default::default() };
        // The engine's warm-up, then the first real run.
        let t = Instant::now();
        crate::engine::warm_up(&mut session, spec, &run);
        let warm = ms(t);
        let t = Instant::now();
        let _ = session.run(&audio, &run);
        let first = ms(t);
        println!("  warm-up {warm:.0} ms, first real run after warm-up {first:.0} ms");
        let mut times = Vec::new();
        let mut text = String::new();
        for _ in 0..3 {
            let t = Instant::now();
            text = session.run(&audio, &run).map_err(|e| e.to_string())?.text;
            times.push(ms(t));
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{id} on {}: load {load:.0} ms, first run {first:.0} ms, offline run min {:.1} / median {:.1} ms (RTF {:.4})",
            model.backend(),
            times[0],
            times[1],
            times[1] / 1000.0 / secs
        );
        println!("  offline: {}", text.trim());
        if spec.is_streaming() {
            let opts = crate::engine::stream_options_for_bench(spec);
            let mut finals = Vec::new();
            let mut feeds = 0.0;
            for _ in 0..3 {
                let mut stream = session.stream(&run, &opts).map_err(|e| e.to_string())?;
                let t = Instant::now();
                // 20 ms device-sized chunks, as the engine feeds them.
                for chunk in audio.chunks(320) {
                    stream.feed(chunk).map_err(|e| e.to_string())?;
                }
                feeds = ms(t);
                let t = Instant::now();
                stream.finalize().map_err(|e| e.to_string())?;
                finals.push(ms(t));
                text = stream.text().full;
            }
            println!("  streaming finalize per run (first is right after warm-up): {finals:.1?} ms");
            finals.sort_by(f64::total_cmp);
            println!(
                "  streaming: feeds {feeds:.0} ms total ({:.3} x realtime), finalize after release min {:.1} / median {:.1} ms",
                feeds / 1000.0 / secs,
                finals[0],
                finals[1]
            );
            println!("  streaming: {}", text.trim());
        }
    }
    if models::VAD.is_ready() || ensure(&models::VAD).is_ok() {
        let vad = Model::load_with(models::VAD.path(), &ModelOptions { backend: transcribe_cpp::Backend::Cpu, ..Default::default() })
            .and_then(|m| m.vad_session().map(|s| (m, s)));
        if let Ok((_m, mut s)) = vad {
            let t = Instant::now();
            let mut probs = Vec::new();
            for chunk in audio.chunks(320) {
                probs.extend(s.stream_feed(chunk).map(|r| r.probs).unwrap_or_default());
            }
            let sp = crate::engine::analyze_speech(&probs, 0.5, audio.len());
            println!("silero vad: {:.1} ms for {secs:.1} s streamed in 20 ms chunks, speech {} ms, range {:?}", ms(t), sp.speech_ms, sp.range);
        }
    }
    Ok(())
}

fn bench_cleanup(text: &str, id: Option<&str>) -> Result<(), String> {
    let cfg = crate::config::Cleanup::default();
    let spec = models::cleanup(id.unwrap_or("s1-mini")).ok_or("unknown cleanup model")?;
    ensure(&spec.asset)?;
    let t = Instant::now();
    let mut c = crate::cleanup::Cleaner::load(spec, &cfg)?;
    println!("{} loaded in {:.0} ms (incl. warm-up; total {:.0} ms with DLLs)", spec.id, c.load_ms, ms(t));
    for i in 0..5 {
        let r = c.clean(text, Some("en"), &cfg);
        println!(
            "  run {i}: {:.0} ms, {} {:?}: {}",
            r.ms,
            if r.accepted { "accepted" } else { "rejected" },
            r.note,
            r.output
        );
    }
    Ok(())
}

pub fn main(args: &[String]) -> i32 {
    attach_console();
    let result = match args[0].as_str() {
        "--bench" if args.len() >= 2 => bench_asr(&args[1], &args[2..]),
        "--bench-cleanup" if args.len() >= 2 => bench_cleanup(&args[1], args.get(2).map(String::as_str)),
        "--download" if args.len() >= 2 => {
            let a = models::asr(&args[1]).map(|m| &m.asset).or_else(|| models::cleanup(&args[1]).map(|m| &m.asset));
            match a {
                Some(a) => ensure(a).map(|p| println!("{}", p.display())),
                None => Err(format!("unknown model {}", args[1])),
            }
        }
        _ => Err("usage: chatty [--bench <wav> [model...] | --bench-cleanup <text> [model] | --download <model>]".into()),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
