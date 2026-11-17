# Ticket 07: audio.rs — Audio Pipeline

## Goal
Implement the audio pipeline: cpal capture → sherpa-onnx VAD → whisper-rs STT → AudioEvent::Speech(Utterance).

## Deliverables
- `src-tauri/src/audio.rs` with:
  - `AudioPipeline` struct: owns cpal stream, sherpa-onnx VAD, whisper-rs STT
  - `start(audio_tx: Sender<AudioEvent>)` — spawns capture loop
  - Capture: cpal 0.16, 16kHz mono
  - VAD: sherpa-onnx Silero VAD — detects speech start/stop
  - STT: whisper-rs 0.16 (metal feature) — transcribes only when VAD triggers
  - Emits `AudioEvent::Speech(Utterance)` on speech end
  - Emits `AudioEvent::Silence` on silence
- Unit tests: VAD triggers correctly (mock audio buffer with speech/silence); STT produces text from mock audio

## Dependencies
- Ticket 01 (project structure, crates)
- Ticket 02 (events types)

## Notes
- whisper-rs builds from source via CMake — cmake must be on PATH
- whisper-rs metal feature for GPU acceleration
- sherpa-onnx auto-downloads prebuilt osx-x64 libs
- cpal 0.16 for macOS 11+ compatibility (not 0.18 which needs 14.2+)
- No tests for actual audio hardware — validate manually
