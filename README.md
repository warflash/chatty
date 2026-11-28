<div align="center">

<img src="assets/banner.avif" alt="chatty: hold a key, talk, let go, and the text is typed where you are" width="100%">

[![CI](https://github.com/warflash/chatty/actions/workflows/ci.yml/badge.svg)](https://github.com/warflash/chatty/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-8284ff)](LICENSE) ![Windows 11](https://img.shields.io/badge/Windows%2011-x64-a78bfa) ![macOS](https://img.shields.io/badge/macOS%2013.3%2B-Apple%20Silicon-a78bfa)

</div>

Push-to-talk dictation for Windows and Apple Silicon Macs. One Rust executable in the tray (the menu bar on macOS); speech recognition runs locally on your GPU (Vulkan) or CPU, and nothing leaves the PC. Text is pasted about 120 ms after you let go.

While you hold the key, the overlay shows the words as they arrive:

<img src="assets/overlay-live.avif" alt="Live overlay while recording: level bars, words arriving, a timer" width="100%">

If no text field has focus, the transcript waits on a card instead of being pasted:

<img src="assets/overlay-copy.avif" alt="Transcript card with a Copy button when no text field had focus" width="100%">

## Install

Download the zip from [Releases](https://github.com/warflash/chatty/releases), unzip, run `chatty.exe`. The default model (Parakeet Unified EN, 697 MB) downloads on first use.

## Use

| Input | Action |
|---|---|
| **Hold** Ctrl + Numpad / | Record, live text in the overlay |
| **Release** | Paste into the focused field |
| **Tap** | Press Enter |
| **Escape** while recording | Cancel |

Map a spare mouse button to Ctrl + Numpad / for one-handed dictation. Right-click the tray icon for a panel with the last transcript (click it to copy), model, language, overlay and LLM cleanup switches; left-click opens the window with history and statistics. Settings live in `config.toml` (next to the exe, or `%APPDATA%\chatty`).

Updates come from GitHub Releases: when a newer one is out, the window's sidebar offers to install it (Settings › Updates picks Stable, Beta or Off).

Seven speech models are available, three of them streaming, covering up to 32 languages. Timings are in [docs/MEASUREMENTS.md](docs/MEASUREMENTS.md), the design in [docs/DESIGN.md](docs/DESIGN.md).

## macOS

Apple Silicon, macOS 13.3 or later. Download `chatty-<version>-macos-arm64.zip` from Releases, unzip, move `chatty.app` to Applications and open it. The same app, with Mac conventions:

* **Hold** the trigger: Control + keypad / by default (the same chord as on Windows, so a mouse button mapped to it works), or the right Option key, the right Command key or fn (Globe): `trigger` in config.toml, or Settings › Trigger. Release pastes with ⌘V; a tap presses Return; Escape cancels. With a modifier trigger, pressing any other key while it is held makes it a normal modifier again (Option+L still types @).
* chatty lives in the menu bar (no Dock icon until its window is open). Its menu shows the last transcript (click it to copy), model, language, overlay and cleanup switches.
* On first start macOS asks for **Accessibility** (to see the trigger, paste and check the focused field) and **Microphone** access. Grant both in System Settings › Privacy & Security; the menu bar menu offers a shortcut while Accessibility is missing. The app is ad-hoc signed, so macOS may ask for Accessibility again after an update.
* Speech and cleanup run on the GPU through Metal (`backend = "metal"` or `"auto"`).
* Config, models, the statistics database, history audio and the log live in `~/Library/Application Support/chatty`. "Open at login" installs a LaunchAgent (`~/Library/LaunchAgents/com.github.fantasyflip.chatty.plist`).

## Build on Windows

Needs Rust (MSVC) and the Visual Studio Build Tools C++ workload. Nothing C/C++ is compiled; transcribe.cpp and llama.cpp come as prebuilt DLLs.

```powershell
./scripts/fetch-native.ps1
cargo build --release
./scripts/package.ps1        # dist/chatty-<version>-windows-x64.zip
```

## Build on macOS

Needs Rust and the Xcode command line tools. Nothing C/C++ is compiled; the native libraries are prebuilt arm64 downloads.

```sh
./scripts/fetch-native.sh      # transcribe.cpp + llama.cpp prebuilt dylibs (Metal)
cargo build --release          # target/release/chatty
./scripts/package-macos.sh     # dist/chatty.app + dist/chatty-<version>-macos-arm64.zip
```

Run the packaged `chatty.app` (it is ad-hoc signed, so the permission grants stick; a bare `target/release/chatty` loses them on every rebuild). The macOS-only checks: `chatty --overlay-test`, `chatty --clipboard-test`, `chatty --render-overlay card out.png`.

Pushing a `v*` tag publishes a release from CI, with both the Windows and the macOS zip.

## Credits

[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), [llama.cpp](https://github.com/ggml-org/llama.cpp), and [Handy](https://github.com/cjpais/Handy) for parts of the input handling. Model licenses: [THIRD-PARTY.md](THIRD-PARTY.md). chatty is MIT licensed.
