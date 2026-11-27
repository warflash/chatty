# Measurements

Machine: Ryzen 9 9900X, RTX 3070 (Vulkan), Windows 11, 2026-11-27.
Release build, transcribe.cpp 0.5.1, llama.cpp b12774.

## End to end (the running app)

Driven by `scripts/e2e.ps1` (synthesized Ctrl+Numpad / with key repeats),
microphone replaced by `CHATTY_TEST_WAV` (jfk.wav, 11 s), pasting into
Notepad. Timestamps from `chatty.log`; release = the hook's key-up time.

| | release → Ctrl+V sent | stages |
|---|---|---|
| 12 s dictation, parakeet-unified (streaming) | 121 ms | drain 0.5, finalize 58, post 0.1, paste delay 60 |
| 6 s dictation | 115 ms | finalize 52, paste delay 60 |
| 4 dictations, app statistics | avg 151 ms, p50 134 ms | includes two with cleanup on |
| 12 s, cleanup on (S1-mini) | 212 ms | finalize 43, cleanup 101, paste delay 60 |

Tap → Enter: **3.2 ms** (key-up to `SendInput` return). A tap during a
paste: Enter sent 4 ms after the paste finished.

The 60 ms `paste_delay_before_ms` (seeded from Handy) is part of every
release → paste number; without it the pipeline is ~55–80 ms.

Microphone (Aukey, 48 kHz stereo F32, WASAPI): first open **36 ms**;
resuming the paused stream **0.1 ms**. Keeping the stream open (paused, so
no callbacks and no CPU) for `mic_keep_open_secs` (120 s) is worth it: a
cold open loses ~36 ms at the start of the recording.

Silent recordings: VAD finds no speech → status `empty`, nothing pasted.

## Per model (`chatty --bench`, 11 s clips, after the engine's warm-up)

| model | offline run | streaming finalize after release | feeds during recording | load |
|---|---|---|---|---|
| parakeet-unified-en-0.6b (Q8_0) | 55 ms | 36–47 ms | 0.19× realtime | 0.6 s |
| nemotron-3.5-asr-streaming-0.6b (Q8_0) | 94 ms (en), 108 ms (de) | 22–31 ms | 0.07× | 0.6 s |
| voxtral-mini-4b-realtime-2602 (Q4_K_M) | 918 ms | 181 ms | 0.15× | 2.0 s |
| parakeet-ultra (Q8_0) | 61 ms | – | – | 0.5 s |
| cohere-transcribe-03-2026 (Q5_K_M) | 131 ms (en), 184 ms (de) | – | – | 1.2 s |
| qwen3-asr-1.7b (Q8_0) | 260 ms (en), 312 ms (de) | – | – | 1.5 s |
| granite-speech-5.0-470m-turboctc (Q8_0) | 66 ms | – | – | 0.4 s |

All seven produced the correct jfk.wav transcript; the five multilingual
ones produced the correct FLEURS German sentence (nemotron with automatic
language detection). Granite outputs lowercase without punctuation.

First compute on a new model compiles Vulkan pipelines (up to 11 s once;
the driver caches them). The engine warms up both paths on 3 s of low noise
at load, so the first dictation runs at the numbers above.

Silero VAD v6.2 (CPU, streamed in 20 ms chunks): 30 ms per 11 s of audio,
spent during recording.

## Cleanup models (`chatty --bench-cleanup`, GPU, 5 runs)

Input: "so um i think we should uh ship the new nuxt version on friday and
then check the tailwind config"

| model | per cleanup | load (warm) | output |
|---|---|---|---|
| S1-mini by Superwhisper (Q4_K_M) | 80–88 ms | 0.8 s | So I think we should ship the new Nuxt version on Friday and then check the Tailwind config. |
| Qwen2.5-0.5B-Instruct (Q8_0) | 97–106 ms | 1.3 s | So, I think we should ship the new Nuxt version on Friday, and then check the Tailwind config. |
| Qwen3.5-0.8B (Q8_0) | 111–185 ms | 5.6 s | so, um, we should ship the new Nuxt version on Friday and then check the Tailwind config. |

S1-mini is the default. Qwen3.5-0.8B dropped "I think" and kept "um".

## Resources

* `chatty.exe`: 2.4 MB (thin LTO, GDI stats window; 8.2 MB with egui). Release zip
  adds transcribe.cpp (≈60 MB, mostly `ggml-vulkan.dll`) and llama.cpp
  (≈75 MB) DLLs.
* Idle CPU: **0.05 %** of one core over 60 s (model loaded, microphone
  stream paused).
* Memory, parakeet-unified on Vulkan: working set 150 MB after load,
  200–320 MB after dictations; private commit 1.0 GB after load, settling
  at 1.7–2.2 GB (WDDM system-memory backing for the GPU allocations; it
  plateaus, no growth per dictation). CPU backend: 1.07 GB working set.
  With S1-mini loaded: +450 MB working set.
* During recording the streaming feeds cost ~0.3 s of CPU (GPU submission)
  per second of audio on parakeet-unified.

## Build times

* Clean release build, 4 jobs: **37 s** (thin LTO, no egui). With the egui
  stats window it was 105 s (thin LTO) / 121 s (fat LTO, 1 codegen unit),
  and 6 m 26 s on the windows-latest runner.
* `cargo test --release` after a release build: 7 s.
* Incremental (one file touched): debug 8 s, release (thin LTO) 20 s.
