# Ticket 03: provider.rs — Provider Hub + OpenAICompatProvider

## Goal
Implement the Provider trait and the single OpenAICompatProvider implementation. This is the LLM seam.

## Deliverables
- `src-tauri/src/provider.rs` with:
  - `Provider` trait: `chat_stream(ChatRequest) -> Stream<ChatChunk>`, `chat_with_tools(ChatRequestWithTools) -> ChatResponse`
  - `OpenAICompatProvider` struct: `base_url: String`, `api_key: Option<String>`, `reqwest::Client`
  - `ChatRequest`, `ChatChunk`, `ChatResponse`, `ChatRequestWithTools`, `ToolSchema` types
  - Streaming SSE parsing from OpenAI-compatible endpoints
  - Tool-call parsing from response
  - Configurable base_url + api_key (from env or config file)
- Unit tests: given a mock HTTP response, parses chunks and tool calls correctly; produces correct HTTP request shape

## Dependencies
- Ticket 01 (project structure)
- Ticket 02 (events types)

## Notes
- All LLM traffic goes through this trait — completions and vision
- One reqwest client, reused
- No in-process llama.cpp (ADR-0003)
