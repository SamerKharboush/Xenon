# Ticket 02: events.rs — Shared types

## Goal
Define the shared types that flow between runtime modules.

## Deliverables
- `src-tauri/src/events.rs` with:
  - `AudioEvent` enum: `Speech(Utterance)`, `Silence`
  - `AgentAction` enum: `Speak(String)`, `SetPetState(PetState)`, `ExecuteTool(ToolCall)`
  - `PetState` enum: `Idle`, `Listening`, `Thinking`, `Speaking`, `Agentic`
  - `ToolCall` struct: `name: String`, `args: serde_json::Value`
  - `Utterance` struct: `text: String`
  - All types derive `Debug`, `Clone`, `Serialize`, `Deserialize`
- Unit tests: round-trip serialization for each type

## Dependencies
- Ticket 01 (project structure)

## Notes
- These types are the runtime seam — every channel carries one of these
- serde derives are the only "logic" here
