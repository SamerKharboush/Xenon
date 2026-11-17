# Ticket 10: runtime.rs — Tokio task orchestration

## Goal
Wire everything together: Tokio tasks, async channels, connect audio → agent → TTS/UI.

## Deliverables
- `src-tauri/src/runtime.rs` with:
  - `Runtime` struct: owns channels, spawns tasks
  - `start()` — spawns audio pipeline, agent loop, TTS/UI sync
  - Channels: `audio_tx/rx` (AudioEvent), `agent_tx/rx` (AgentAction)
  - Audio pipeline task: reads audio_rx, calls agent::run_utterance, sends to agent_tx
  - TTS/UI sync task: reads agent_tx, calls tts::speak for Speak actions, updates pet state for SetPetState
  - Graceful shutdown handling
- `src-tauri/src/main.rs` — entry point, creates Runtime, starts Tauri
- Unit tests: runtime spawns tasks correctly; channels connect modules

## Dependencies
- Ticket 02 (events types)
- Ticket 03 (Provider)
- Ticket 04 (Tools)
- Ticket 05 (Grounding)
- Ticket 06 (Agent)
- Ticket 07 (Audio)
- Ticket 08 (TTS)
- Ticket 09 (UI)

## Notes
- This is the integration ticket — all modules wired together
- Tokio + async-channel (ADR-0001)
- Task architecture from PRD Section 4
- No tests for actual Tauri IPC — validate manually
