# Ticket 08: tts.rs — sherpa-onnx TTS

## Goal
Implement TTS: receive tokens from agent, synthesize audio chunks via sherpa-onnx, play via cpal/rodio.

## Deliverables
- `src-tauri/src/tts.rs` with:
  - `TtsEngine` struct: owns sherpa-onnx OfflineTts, cpal output stream
  - `synthesize(text: &str) -> Vec<f32>` — returns audio samples
  - `speak(text: &str)` — synthesizes and plays
  - `speak_stream(tokens: Receiver<String>)` — synthesizes chunks as tokens arrive
- Unit tests: synthesize produces non-empty audio; speak_stream handles multiple tokens

## Dependencies
- Ticket 01 (project structure, crates)
- Ticket 02 (events types — AgentAction::Speak)

## Notes
- sherpa-onnx 2.0.1 — Kokoro/Matcha neural voices
- Prebuilt osx-x64 libs auto-downloaded
- cpal output stream for playback
- Streaming TTS: start speaking before LLM finishes generating
- No neural voice quality tests — validate manually
