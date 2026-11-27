# Third-party components

Shipped in the Windows release zip (macOS: the same projects' arm64 Metal
dylibs in `chatty.app/Contents/Frameworks`, licenses in
`Contents/Resources/licenses`; no OpenMP runtime):

| Component | Files | License |
|---|---|---|
| transcribe.cpp 0.5.1 (handy-computer) | `transcribe.dll` | MIT (`licenses/transcribe.cpp/`) |
| ggml (bundled with transcribe.cpp) | `ggml*.dll` | MIT (`licenses/transcribe.cpp/ggml/`) |
| llama.cpp b12774 (ggml-org) | `llama/llama.dll`, `llama/ggml*.dll` | MIT, https://github.com/ggml-org/llama.cpp/blob/master/LICENSE |
| LLVM OpenMP runtime | `llama/libomp.dll` | Apache-2.0 WITH LLVM-exception (`llama/LICENSE-LLVM-OpenMP`) |

Downloaded on first use (not redistributed):

| Model | License |
|---|---|
| parakeet-unified-en-0.6b | NVIDIA Open Model License |
| nemotron-3.5-asr-streaming-0.6b | OpenMDW-1.1 |
| Voxtral Mini 4B Realtime 2602 | Apache-2.0 |
| parakeet-ultra | CC-BY-4.0 |
| cohere-transcribe-03-2026 | Apache-2.0 |
| Qwen3-ASR-1.7B | Apache-2.0 |
| granite-speech-5.0-470m-turboctc | Apache-2.0 |
| Silero VAD v6.2 | MIT |
| S1-mini by Superwhisper | Apache-2.0 with an attribution term: refer to it as "S1-mini" by "Superwhisper" |
| Qwen2.5-0.5B-Instruct, Qwen3.5-0.8B | Apache-2.0 |
