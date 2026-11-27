#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod bench;
mod cleanup;
mod clipboard;
mod config;
mod coordinator;
mod engine;
mod hook;
mod input;
mod llama;
mod logger;
mod models;
mod overlay;
mod paths;
mod sqlite;
mod stats;
mod stats_ui;
mod text;
mod tray;
mod wav;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage};
use windows::core::w;

use coordinator::{Effect, Input, Machine};
use hook::KeyEvent;
use tray::TrayState;

fn set_autostart(enabled: bool) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    // Never register a dev build, or a release build run from cargo's target dir.
    let exe = std::env::current_exe().unwrap_or_default();
    if cfg!(debug_assertions) || exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")) {
        return;
    }
    unsafe {
        if enabled {
            let exe = std::env::current_exe().unwrap_or_default();
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                w!("chatty"),
                REG_SZ.0,
                Some(value.as_ptr() as *const _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, w!("chatty"));
        }
    }
}

/// The coordinator thread: feeds key events to the state machine and
/// executes its effects. Never blocks on the pipeline.
fn coordinate(rx: Receiver<KeyEvent>, engine: engine::Handle, cfg: config::Shared, stats: stats::Writer) {
    let mut m = Machine::new(Duration::from_millis(cfg.get().hold_threshold_ms));
    loop {
        let event = match m.next_deadline() {
            Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(e) => Some(e),
                Err(_) => return,
            },
        };
        m.threshold = Duration::from_millis(cfg.get().hold_threshold_ms);
        let effect = match event {
            Some(e) => {
                log::debug!("input {:?} (+{:?} queued)", e.input, e.at.elapsed());
                if matches!(e.input, Input::ProcessingFinished | Input::StartFailed) {
                    tray::set_state(TrayState::Idle);
                }
                m.on_input(e.input, e.at)
            }
            None => m.on_deadline(Instant::now()),
        };
        hook::ESCAPE_WANTED.store(m.wants_escape(), std::sync::atomic::Ordering::Relaxed);
        match effect {
            None => {}
            Some(Effect::Start { pressed_at }) => {
                let now = Instant::now();
                log::info!("recording (threshold reached {:?} after key-down)", now - pressed_at);
                tray::set_state(TrayState::Recording);
                engine.send(engine::Cmd::Begin { pressed_at, started_at: now });
            }
            Some(Effect::Stop { released_at }) => {
                // Abort any running preview before the final transcript.
                engine.preview_cancel.cancel();
                engine.send(engine::Cmd::Finish { released_at });
                tray::set_state(TrayState::Busy);
                overlay::transcribing();
            }
            Some(Effect::Cancel) => {
                engine.preview_cancel.cancel();
                engine.send(engine::Cmd::Cancel);
                tray::set_state(TrayState::Idle);
                overlay::hide();
                log::info!("cancelled");
            }
            Some(Effect::Submit { released_at }) => {
                let waited = input::wait_trigger_released(Duration::from_secs(1));
                let app = input::foreground_app();
                let result = input::enter();
                let latency = released_at.elapsed();
                match result {
                    Ok(()) => log::info!(
                        "tap -> Enter in {:.1} ms (waited {:.1} ms for keys) app {app:?}",
                        latency.as_secs_f64() * 1000.0,
                        waited.as_secs_f64() * 1000.0
                    ),
                    Err(e) => log::error!("Enter: {e}"),
                }
                stats.enter(released_at, app, latency.as_secs_f64() * 1000.0, waited.as_secs_f64() * 1000.0);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    logger::init();
    if args.len() > 1 {
        let code = bench::main(&args[1..]);
        log::logger().flush();
        std::process::exit(code);
    }

    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\chatty-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            log::info!("already running");
            return;
        }
        std::mem::forget(_mutex);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let t0 = Instant::now();
    log::info!("chatty {} starting ({})", env!("CARGO_PKG_VERSION"), paths::exe_dir().display());

    let cfg_path = config::path();
    let cfg = config::Shared::new(cfg_path.clone(), config::load_or_create(&cfg_path));
    log::info!("config {}", cfg_path.display());

    transcribe_cpp::init_logging();
    if let Err(e) = transcribe_cpp::init_backends_default() {
        log::error!("transcribe backends: {e}");
    }
    for d in transcribe_cpp::devices() {
        log::info!("device {} ({:?}): {}", d.name, d.device_type, d.description.trim());
    }

    overlay::ENABLED.store(cfg.get().overlay, std::sync::atomic::Ordering::Relaxed);
    let stats = stats::spawn_writer(cfg.clone());
    let (audio_ctl, capture) = audio::spawn();
    let (coord_tx, coord_rx) = mpsc::channel::<KeyEvent>();
    let engine = engine::spawn(cfg.clone(), coord_tx.clone(), stats.clone(), audio_ctl.clone(), capture);
    hook::start(coord_tx);
    {
        let (engine, cfg, stats) = (engine.clone(), cfg.clone(), stats.clone());
        std::thread::Builder::new()
            .name("coordinator".into())
            .spawn(move || coordinate(coord_rx, engine, cfg, stats))
            .expect("spawn coordinator");
    }
    {
        let engine = engine.clone();
        let audio_ctl = audio_ctl.clone();
        config::watch(cfg.clone(), move |old, new| {
            if old.microphone != new.microphone {
                audio_ctl.close();
            }
            if old.autostart != new.autostart {
                set_autostart(new.autostart);
            }
            overlay::ENABLED.store(new.overlay, std::sync::atomic::Ordering::Relaxed);
            engine.send(engine::Cmd::Config(std::sync::Arc::new(new.clone())));
            tray::set_tooltip("");
        });
    }
    set_autostart(cfg.get().autostart);

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.map(|h| HINSTANCE(h.0)).unwrap_or_default();
    overlay::create(instance);
    let stats_cfg = cfg.clone();
    let main_hwnd = tray::create(
        instance,
        tray::Actions { cfg: cfg.clone(), open_stats: Box::new(move || stats_ui::open(stats_cfg.clone())) },
    );
    clipboard::OWNER.store(main_hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
    engine.send(engine::Cmd::Warm);
    log::info!("ready in {:?}", t0.elapsed());

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tray::remove_icon();
}
