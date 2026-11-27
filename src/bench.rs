//! Command-line measurements (no tray, no hook):
//!
//!   chatty --bench <file.wav> [model-id]     transcription latency per model
//!   chatty --bench-cleanup "<text>" [model]  cleanup latency and output
//!   chatty --download <model-id>             fetch a model ahead of time

use std::time::Instant;

use transcribe_cpp::{Model, ModelOptions, RunOptions};

use crate::models;

#[cfg(target_os = "macos")]
fn attach_console() {}

#[cfg(windows)]
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
        let lang = std::env::var("CHATTY_BENCH_LANG").unwrap_or_else(|_| "auto".into());
        let run = RunOptions { language: spec.language_arg(&lang), ..Default::default() };
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
        // Segment decoder as the engine runs it: 20 ms chunks, streaming VAD,
        // a preview every 300 ms of audio, final pass after "release".
        if let Ok((_vm, mut vs)) = Model::load_with(models::VAD.path(), &ModelOptions { backend: transcribe_cpp::Backend::Cpu, ..Default::default() })
            .and_then(|m| m.vad_session().map(|s| (m, s)))
        {
            let mut seg = crate::engine::Segmenter::default();
            let mut pcm = Vec::new();
            let mut probs = Vec::new();
            let mut previews = Vec::new();
            for (i, chunk) in audio.chunks(320).enumerate() {
                pcm.extend_from_slice(chunk);
                probs.extend(vs.stream_feed(chunk).map(|r| r.probs).unwrap_or_default());
                let due = i % 15 == 14;
                let t = Instant::now();
                if seg.step(&mut session, &run, &pcm, &probs, 0.5, due) && due {
                    previews.push(ms(t));
                }
            }
            probs.extend(vs.stream_flush().map(|r| r.probs).unwrap_or_default());
            let fin = seg.finish(&mut session, &run, &pcm, &probs, 0.5)?;
            previews.sort_by(f64::total_cmp);
            println!(
                "  segments: {} closed while recording, final pass after release {fin:.1} ms, preview run median {:.1} ms, total compute {:.0} ms ({:.3} x realtime)",
                seg.closed,
                previews.get(previews.len() / 2).copied().unwrap_or(0.0),
                seg.compute_ms,
                seg.compute_ms / 1000.0 / secs
            );
            println!("  segments: {}", seg.committed.join(" "));
        }
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

/// Checks delayed-rendering paste confirmation without keystrokes: a reader
/// thread plays the target app. The user's clipboard is saved and restored.
#[cfg(target_os = "macos")]
fn clipboard_test() -> Result<(), String> {
    crate::mac::clipboard_test()
}

#[cfg(windows)]
fn clipboard_test() -> Result<(), String> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if crate::clipboard::on_message(h, m, wp.0 as u32) {
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }
    std::thread::spawn(|| unsafe {
        let class = WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: w!("chatty_cliptest"), ..Default::default() };
        RegisterClassW(&class);
        let h = CreateWindowExW(Default::default(), w!("chatty_cliptest"), w!(""), WS_POPUP, 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None)
            .expect("window");
        crate::clipboard::OWNER.store(h.0 as isize, Ordering::Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    });
    while crate::clipboard::OWNER.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let saved = crate::clipboard::save()?;
    println!("saved {} clipboard formats", saved_len(&saved));

    let read_foreign = || unsafe {
        OpenClipboard(None).map_err(|e| e.to_string())?;
        let text = GetClipboardData(13).ok().map(|h| {
            let p = GlobalLock(windows::Win32::Foundation::HGLOBAL(h.0)) as *const u16;
            let n = (0..).take_while(|&i| *p.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(h.0));
            s
        });
        let _ = CloseClipboard();
        Ok::<_, String>(text)
    };

    // 1. A reader 80 ms after "Ctrl+V": confirmed, with the right text.
    let text = "chatty clipboard test äöü 👍";
    crate::clipboard::set_text_deferred(text)?.ok_or("no owner")?;
    std::thread::sleep(Duration::from_millis(60));
    println!("   refused before arm: {:?}", crate::clipboard::arm());
    let sent = Instant::now();
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        read_foreign()
    });
    let r = crate::clipboard::wait_read(sent, Duration::from_millis(1000));
    let got = reader.join().unwrap()?;
    println!("1. read: {:?} after {:?}, text matches: {}", r.map(|r| r.pid), r.map(|r| r.at - sent), got.as_deref() == Some(text));
    if r.is_none() || got.as_deref() != Some(text) {
        return Err("delayed rendering read not observed".into());
    }

    // 2. Nobody reads: unconfirmed after the timeout.
    let seq = crate::clipboard::set_text_deferred(text)?.ok_or("no owner")?;
    std::thread::sleep(Duration::from_millis(60));
    println!("   refused before arm: {:?}", crate::clipboard::arm());
    let t = Instant::now();
    let r = crate::clipboard::wait_read(t, Duration::from_millis(1500));
    println!("2. no reader: {:?} after {:?}", r.map(|r| r.pid), t.elapsed());
    if r.is_some() {
        return Err("read reported without a reader".into());
    }
    // 3. A late reader still gets the text.
    let late = read_foreign()?;
    println!("3. late read text matches: {}", late.as_deref() == Some(text));

    let ok = crate::clipboard::restore(saved, seq)?;
    println!("restored: {ok}");
    Ok(())
}

#[cfg(windows)]
fn saved_len(s: &crate::clipboard::Saved) -> usize {
    s.len()
}

pub fn main(args: &[String]) -> i32 {
    attach_console();
    let result = match args[0].as_str() {
        "--render" if args.len() >= 3 => {
            let path = crate::config::path();
            let cfg = crate::config::Shared::new(path.clone(), crate::config::load_or_create(&path));
            use crate::ui::Page;
            let page = match args[1].as_str() {
                "overview" => Page::Overview,
                "history" => Page::History,
                "models" => Page::Models,
                "words" => Page::Words,
                "settings" => Page::Settings,
                _ => Page::Stats,
            };
            let num = |i: usize, d: f32| args.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(d);
            crate::ui::render_png(cfg, page, std::path::Path::new(&args[2]), num(3, 1200.0) as u32, num(4, 820.0) as u32, num(5, 1.0))
        }
        "--stats" | "--window" => {
            let path = crate::config::path();
            let cfg = crate::config::Shared::new(path.clone(), crate::config::load_or_create(&path));
            use crate::ui::Page;
            let page = match args.get(1).map(String::as_str) {
                Some("overview") => Page::Overview,
                Some("history") => Page::History,
                Some("models") => Page::Models,
                Some("words") => Page::Words,
                Some("settings") => Page::Settings,
                _ => Page::Stats,
            };
            crate::ui::run_standalone(cfg, page)
        }
        "--clipboard-test" => clipboard_test(),
        "--overlay-test" => crate::overlay::selftest(),
        "--render-overlay" if args.len() >= 3 => crate::overlay::render_png(
            &args[1],
            std::path::Path::new(&args[2]),
            args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.5),
        ),
        "--bench" if args.len() >= 2 => bench_asr(&args[1], &args[2..]),
        "--bench-cleanup" if args.len() >= 2 => bench_cleanup(&args[1], args.get(2).map(String::as_str)),
        "--download" if args.len() >= 2 => {
            let a = models::asr(&args[1]).map(|m| &m.asset).or_else(|| models::cleanup(&args[1]).map(|m| &m.asset));
            match a {
                Some(a) => ensure(a).map(|p| println!("{}", p.display())),
                None => Err(format!("unknown model {}", args[1])),
            }
        }
        _ => Err(
            "usage: chatty [--stats | --bench <wav> [model...] | --bench-cleanup <text> [model] | --download <model>]".into(),
        ),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
