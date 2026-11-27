# chatty design note

One Rust process, tray only. Hold Ctrl+Numpad / (sent by the mouse) to
dictate; release to paste; tap to press Enter.

## Crates

| Need | Choice | Why |
|---|---|---|
| Speech | `transcribe-cpp` 0.5.1 linked to the **prebuilt** `transcribe-native-*-windows-x86_64-cpu-vulkan` bundle | no C++ build; Vulkan + CPU backends load at runtime |
| Win32 (hook, tray, overlay, clipboard, SendInput) | `windows` | direct API, nothing else |
| Microphone | `cpal` (WASAPI) | small, pure Rust |
| Ring buffer | `rtrb` | lock-free SPSC between the audio callback and the engine |
| Stats window | `eframe`/`egui` (glow) | native, no webview; started lazily |
| SQLite | own 200-line FFI to Windows' `winsqlite3.dll` via `raw-dylib` | nothing compiled, nothing shipped |
| Config | `toml` + `toml_edit` | tray edits keep comments |
| Downloads | `ureq` with native-tls (SChannel) | no `ring`/C build |
| Cleanup LLM | llama.cpp **prebuilt** Vulkan DLLs, loaded at runtime | no C++ build |

Nothing in the dependency graph compiles C or C++.

### Prebuilt native libraries

`scripts/fetch-native.ps1` reads the transcribe.cpp version from the
`transcribe-cpp` line in `Cargo.toml` (the only pin), downloads the release
tarball (SHA-256 pinned) and assembles the install prefix that
`transcribe-cpp-sys`'s `TRANSCRIBE_DIR` path expects. The Windows bundle
ships only DLLs, so the script generates `lib/transcribe.lib` from
`transcribe.dll`'s exports (`dumpbin` + `lib`) and writes
`lib/transcribe-link.json` (shared, backend_dl). `.cargo/config.toml` sets
`TRANSCRIBE_DIR`. The sys crate copies `bin/*.dll` next to the exe.

llama.cpp's release ships its own `ggml.dll`/`ggml-base.dll`, a different
build with the same file names as transcribe.cpp's, already loaded in the
process. `llama.dll` is therefore loaded inside a side-by-side activation
context (`llama\llama.manifest`), which redirects its imports to its own
copies; the code checks the resolved path of `ggml.dll`.

## Threads and channels

```
hook thread ─ WH_KEYBOARD_LL callback: classify, flip atomics, send, return
     │ KeyEvent{Press|Release|Escape, at}
     ▼
coordinator ─ pure state machine (coordinator.rs) + effect executor
     │ Begin / Finish / Cancel                ▲ ProcessingFinished / StartFailed
     ▼                                        │
engine ──── model, VAD, cleanup LLM; drains ring every 20 ms, streams or
     │       previews, text pipeline, paste, then reports finished
     ├─ audio control thread: owns the WASAPI stream (open lazily, pause
     │    between dictations, close after mic_keep_open_secs)
     │    WASAPI callback ──rtrb──▶ engine
     ├─ download threads (on demand)
     └─ stats writer thread ── SQLite
main thread ─ message loop: tray icon + menu, overlay window, clipboard owner
config watcher ─ FindFirstChangeNotification, reloads config.toml
stats-ui thread ─ egui, started on first open, hidden on close
```

The hook callback never blocks: it reads/writes atomics and does one
`mpsc::Sender::send`. Our own injected keys carry `LLKHF_INJECTED` and pass
through.

## State machine

`Idle → Armed(pressed_at)` on key-down. The coordinator waits with
`recv_timeout` until `pressed_at + threshold`:

* deadline reached while held → `Recording` (`Start`): the microphone opens
  only now, so a tap never opens it;
* key-up before the deadline → `Submit` (Enter);
* key-up while recording → `Processing` (`Stop`);
* Escape while armed/recording → cancel (Escape is swallowed only then).

While `Processing`, one press is remembered with its real key-down time
(from Handy 6b217ab6): a tap becomes an Enter after the pipeline drains; a
hold becomes `Armed` with the original key-down and starts recording at once
if the threshold already passed. The engine reports `ProcessingFinished`
only after Ctrl+V was sent **and** the clipboard restored, unlike Handy's
`FinishGuard`, so Enter always lands after the text.

No release grace window: Windows repeats key-downs only, so a grace window
would only add latency. Key repeats are dropped in the hook; a 30 ms press
debounce covers switch bounce. Before Ctrl+V or Enter the executor waits
until Ctrl and the trigger are physically up.

## Pipeline

1. Transcribe: streaming models (`parakeet-unified`, `nemotron`, `voxtral`)
   stream for real: 20 ms feeds, `finalize` on release. Offline models
   re-transcribe the growing buffer every `preview_interval_ms`; the
   coordinator cancels that run's `CancelToken` the moment the key comes up,
   then the final run covers only the VAD-trimmed speech.
2. Silero VAD v6.2 is streamed alongside (512-sample frames): speech time,
   silence ratio, trim range, and "empty" detection.
3. Fillers: `any` list always; per-language lists only when the output
   language is known (German "um" is a word).
4. Custom words: per-word minimum similarity; default by length (≤4 chars
   exact only). Phonetic bonus is +0.05 and only for ≥6 chars. Multi-word
   candidates must start with the word's first letter. Every correction is
   logged with its score.
5. Optional LLM cleanup (S1-mini by Superwhisper by default). Output is
   rejected on prompt leakage, length ratio outside [0.5, 1.4], or
   normalized edit distance > 0.45; raw and cleaned text are both stored.
6. Paste: save clipboard → set text (excluded from clipboard history) →
   60 ms → Ctrl+V → 60 ms → restore if unchanged. Elevated foreground
   windows (UIPI) get the text left on the clipboard and a notice.

## Files

```
src/main.rs         startup, coordinator thread, autostart
src/hook.rs         WH_KEYBOARD_LL
src/coordinator.rs  state machine + tests
src/engine.rs       recording, streaming/preview, VAD, pipeline, paste
src/audio.rs        WASAPI capture, ring buffer, resampler
src/text.rs         fillers, custom words + tests
src/cleanup.rs      LLM prompt + drift guard + tests
src/llama.rs        llama.dll binding (activation context)
src/models.rs       curated model list, verified downloads
src/stats.rs        schema, writer, report queries
src/stats_ui.rs     egui statistics window
src/sqlite.rs       winsqlite3 FFI
src/overlay.rs      click-through overlay
src/tray.rs         tray icon and menu
src/clipboard.rs, input.rs, config.rs, paths.rs, logger.rs, wav.rs, bench.rs
```

## Simplifications (noted, deliberate)

* Paste is plain save/set/Ctrl+V/restore with fixed delays, not Handy's
  delayed-render read receipts.
* Local day/hour/weekday are computed in Rust at insert time (DST-correct)
  because `winsqlite3` returns NULL for SQLite's `'localtime'` modifier.
* Clipboard formats backed by GDI handles (bitmaps, metafiles) are not
  saved; Windows re-synthesizes CF_BITMAP from a saved CF_DIB.
* The stats window is one egui event loop per process (winit limit), hidden
  rather than destroyed on close.
