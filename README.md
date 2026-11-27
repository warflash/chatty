# chatty

Push-to-talk dictation for one Windows 11 PC. One Rust executable in the
tray; everything runs locally.

* **Hold** Ctrl+Numpad / (the mouse button behind the wheel) for 300 ms or
  more: records from the threshold on, live text in an overlay; release
  pastes the transcript into the focused field.
* **Tap**: presses Enter (after any paste still running).
* **Escape** while recording: cancel.
* Tray menu: model, language, cleanup pass, statistics, config.

Configuration is `config.toml` (next to the exe, or `%APPDATA%\chatty`),
reloaded on save. Models, the statistics database, history audio and the
log live in `%LOCALAPPDATA%\chatty`.

## Build

Needs Rust (MSVC) and the Visual Studio Build Tools C++ workload (linker,
`lib.exe`, `dumpbin.exe`). Nothing C/C++ is compiled; native libraries are
prebuilt downloads.

```powershell
./scripts/fetch-native.ps1   # transcribe.cpp + llama.cpp prebuilt DLLs
cargo build --release        # target/release/chatty.exe + DLLs
./scripts/package.ps1        # dist/chatty-<version>-windows-x64.zip
```

Tag `v*` to publish a release from CI.

## Measure

```powershell
chatty.exe --bench sample.wav parakeet-unified-en-0.6b granite-speech-5.0-470m-turboctc
chatty.exe --bench-cleanup "so um i think we should ship it on friday"
```

Every dictation logs per-stage timings to `chatty.log` and the database.

See [docs/DESIGN.md](docs/DESIGN.md) for the design and
[docs/MEASUREMENTS.md](docs/MEASUREMENTS.md) for measured numbers.
