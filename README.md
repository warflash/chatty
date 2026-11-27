<div align="center">

<img src="assets/banner.avif" alt="chatty: hold a key, talk, let go, and the text is typed where you are" width="100%">

[![CI](https://github.com/warflash/chatty/actions/workflows/ci.yml/badge.svg)](https://github.com/warflash/chatty/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-8284ff)](LICENSE) ![Windows 11](https://img.shields.io/badge/Windows%2011-x64-a78bfa)

</div>

Push-to-talk dictation for Windows. One Rust executable in the tray; speech recognition runs locally on your GPU (Vulkan) or CPU, and nothing leaves the PC. Text is pasted about 120 ms after you let go.

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

Map a spare mouse button to Ctrl + Numpad / for one-handed dictation. The tray menu and window switch model, language and the optional LLM cleanup pass, and show history and statistics. Settings live in `config.toml` (next to the exe, or `%APPDATA%\chatty`).

Seven speech models are available, three of them streaming, covering up to 32 languages. Timings are in [docs/MEASUREMENTS.md](docs/MEASUREMENTS.md), the design in [docs/DESIGN.md](docs/DESIGN.md).

## Build

Needs Rust (MSVC) and the Visual Studio Build Tools C++ workload. Nothing C/C++ is compiled; transcribe.cpp and llama.cpp come as prebuilt DLLs.

```powershell
./scripts/fetch-native.ps1
cargo build --release
./scripts/package.ps1        # dist/chatty-<version>-windows-x64.zip
```

Pushing a `v*` tag publishes a release from CI.

## Credits

[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), [llama.cpp](https://github.com/ggml-org/llama.cpp), and [Handy](https://github.com/cjpais/Handy) for parts of the input handling. Model licenses: [THIRD-PARTY.md](THIRD-PARTY.md). chatty is MIT licensed.
