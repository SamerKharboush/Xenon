# Audio stack: cpal 0.16 + sherpa-onnx + whisper-rs

- **Capture**: `cpal = "0.16"` — runs on macOS 10.15+, wider compatibility than 0.18 (which requires 14.2+). No loopback, so 0.16's older API is fine.
- **VAD**: `sherpa-onnx` (Silero VAD) — bundled in the same crate as TTS, no separate VAD dependency.
- **STT**: `whisper-rs = "0.16"` with `metal` feature — Metal GPU acceleration, compiles whisper.cpp from source via CMake.
- **TTS**: `sherpa-onnx = "2.0.1"` — neural voices (Kokoro/Matcha), prebuilt `osx-x64` libs auto-downloaded by build.rs.

Rejected: `webrtc-vad`/`webrtcvad-rs` (dead since 2019), `kalosm-sound` (no TTS, stale, pins cpal 0.15), `tts` crate (robotic system voices), `piper-rs` (bus-factor-1). sherpa-onnx covering VAD + TTS in one active dep was the deciding math.
