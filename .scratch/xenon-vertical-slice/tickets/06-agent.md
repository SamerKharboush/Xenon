# Ticket 06: agent.rs — Hand-rolled ReAct loop

## Goal
Implement the ReAct agent loop: receive Utterance → call Provider with tools → parse tool calls → execute → feed back → emit AgentAction.

## Deliverables
- `src-tauri/src/agent.rs` with:
  - `run_utterance(provider: &dyn Provider, tools: &[Box<dyn Tool>], utterance: &Utterance) -> Vec<AgentAction>`
  - ReAct loop: call Provider with tools schema → parse tool call → execute Tool → feed result back → repeat until final response or max iterations
  - Max iterations: 5 (safety)
  - Emits `AgentAction::Speak(String)` for final response
  - Emits `AgentAction::ExecuteTool(ToolCall)` for each tool call
  - Emits `AgentAction::SetPetState(PetState::Thinking)` at start, `PetState::Speaking` at end
- Unit tests: given a mock Provider that returns a tool call, calls the right Tool with right args and feeds result back; given a final response, emits Speak; given a tool error, retries or reports

## Dependencies
- Ticket 02 (events types)
- Ticket 03 (Provider trait)
- Ticket 04 (Tool trait)

## Notes
- No kalosm, no rig — hand-rolled (ADR-0004)
- The loop is ~150 lines — deliberately small
- Provider is mocked in tests; we test the dispatch logic, not the LLM's intelligence
