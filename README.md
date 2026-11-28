# chatty

Push-to-talk dictation for one Windows 11 PC or Apple Silicon Mac. One Rust
executable in the tray (menu bar on macOS); everything runs locally.

* **Hold** Ctrl+Numpad / (the mouse button behind the wheel) for 300 ms or
  more: records from the threshold on, live text in an overlay; release
  pastes the transcript into the focused field.
* **Tap**: presses Enter (after any paste still running).
* **Escape** while recording: cancel.
* Tray menu: model, language, cleanup pass, statistics, config.

Configuration is `config.toml` (next to the exe, or `%APPDATA%\chatty`),
reloaded on save. Models, the statistics database, history audio and the
log live in `%LOCALAPPDATA%\chatty`.

## macOS

Apple Silicon, macOS 13.3 or later. The same app, with Mac conventions:

* **Hold** the trigger: Control + keypad / by default (the same chord as on
  Windows, so a mouse button mapped to it works), or the right Option key,
  the right Command key or fn (Globe) — `trigger` in config.toml, or
  Settings › Trigger. Release pastes with ⌘V; a tap presses Return; Escape
  cancels. With a modifier trigger, pressing any other key while it is held
  makes it a normal modifier again (Option+L still types @).
* chatty lives in the menu bar (no Dock icon until its window is open).
* On first start macOS asks for **Accessibility** (to see the trigger, paste
  and check the focused field) and **Microphone** access. Grant both in
  System Settings › Privacy & Security; the menu bar menu offers a shortcut
  while Accessibility is missing.
* Speech and cleanup run on the GPU through Metal (`backend = "metal"` or
  `"auto"`).
* Config, models, the statistics database, history audio and the log live in
  `~/Library/Application Support/chatty`. "Open at login" installs a
  LaunchAgent (`~/Library/LaunchAgents/com.github.fantasyflip.chatty.plist`).

### Build on macOS

Needs Rust and the Xcode command line tools. Nothing C/C++ is compiled; the
native libraries are prebuilt arm64 downloads.

```sh
./scripts/fetch-native.sh      # transcribe.cpp + llama.cpp prebuilt dylibs (Metal)
cargo build --release          # target/release/chatty
./scripts/package-macos.sh     # dist/chatty.app + dist/chatty-<version>-macos-arm64.zip
```

Run the packaged `chatty.app` (it is ad-hoc signed, so the permission grants
stick; a bare `target/release/chatty` loses them on every rebuild). The
macOS-only checks: `chatty --overlay-test`, `chatty --clipboard-test`,
`chatty --render-overlay card out.png`.

## Build on Windows

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
