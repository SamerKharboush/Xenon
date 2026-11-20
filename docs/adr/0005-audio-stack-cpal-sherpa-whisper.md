# Audio stack: cpal 0.16 + sherpa-onnx + whisper-rs

- **Capture**: `cpal = "0.16"` — runs on macOS 10.15+, wider compatibility than 0.18 (which requires 14.2+). No loopback, so 0.16's older API is fine.
- **VAD**: `whisper-rs`'s built-in `WhisperVadContext` with the whisper.cpp-format Silero model (`ggml-silero-v6.2.0.bin`). Chosen over sherpa-onnx Silero VAD because it reuses the same ggml model format and the same whisper-rs dependency as STT ("one model format, one dep"); sherpa-onnx Silero VAD needs a separate ONNX model. Its `detect_speech`+`probabilities` API is batch-only, so we stream it by re-running it over a bounded contiguous 16 kHz buffer and feeding the probability delta into our own pure `Segmenter` (see `audio_dsp.rs`).
- **STT**: `whisper-rs = "0.16"` with `metal` feature — Metal GPU acceleration, compiles whisper.cpp from source via CMake.
- **TTS**: `sherpa-onnx = "2.0.1"` — neural voices (Kokoro/Matcha), prebuilt `osx-x64` libs auto-downloaded by build.rs.

Rejected: `webrtc-vad`/`webrtcvad-rs` (dead since 2019), `kalosm-sound` (no TTS, stale, pins cpal 0.15), `tts` crate (robotic system voices), `piper-rs` (bus-factor-1). sherpa-onnx covering TTS (and, optionally, a future streaming ONNX VAD) in one active dep was the deciding math; for VAD specifically the ggml/single-dep argument won.

## Wake-word engine (2026-11-20)

Chosen: VAD-gated ≤2.5 s segments transcribed by `ggml-tiny.en.bin` (falls back to the configured main whisper model), with fuzzy matching over the first N words of the transcript (Levenshtein per word; budget 0/2/4 by sensitivity). The main STT model stays unloaded until a wake fires, so idle RAM stays low.

Rejected: sherpa-onnx 2.0.1 KWS (`sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01`). Its `tokens.txt` has 500 tokens, no `▁X` and no byte-fallback tokens, so "xenon" cannot be tokenized at all. `KeywordSpotter::create_stream_with_keywords` accepts pre-tokenized BPE keywords only; open-vocabulary free text such as a configurable pet name is not representable. Revisit only if a byte-level/BPE-open-vocab KWS model with a Rust binding ships.
