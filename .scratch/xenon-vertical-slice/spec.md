# Spec: Xenon Vertical Slice

Status: ready-for-agent
Type: task

## Problem Statement

There is no lightweight, local-first desktop companion that combines a fluid transparent pet UI, natural voice interaction, and computer-use agent capabilities in a single fast application. Existing solutions stitch together heavy Python backends and Electron wrappers, resulting in high memory usage, slow UI, and startup delays. Users want a desktop pet that actually listens, thinks, speaks, and controls their computer — without the bloat.

## Solution

Build a vertical slice of Project Xenon: a 100% Rust + Tauri v2 desktop companion for macOS that delivers one complete end-to-end path through every architectural layer. The pet lives as a transparent always-on-top overlay, listens via local STT, reasons via a configurable LLM provider, speaks via local neural TTS, and controls the computer via a hand-rolled ReAct agent loop. Delivered as a universal `.dmg`.

## User Stories

1. As a macOS user, I want Xenon to appear as a small transparent pet on my desktop, so that I have a constant companion without a bulky window.
2. As a user, I want the pet to be click-through when idle, so that it doesn't block my work.
3. As a user, I want the pet to show a waveform animation when listening, so that I know it's hearing me.
4. As a user, I want the pet to show a pulsing animation when thinking, so that I know it's processing.
5. As a user, I want the pet to lip-sync when speaking, so that it feels alive.
6. As a user, I want the pet's eyes to track mouse/screenshot activity when acting, so that I can see what it's doing.
7. As a user, I want to speak naturally and have Xenon transcribe my words locally, so that my voice stays private.
8. As a user, I want Xenon to detect when I start and stop speaking, so that it doesn't transcribe silence.
9. As a user, I want Xenon to respond with a neural voice, so that it sounds human-like.
10. As a user, I want to configure any OpenAI-compatible LLM endpoint (Ollama, LM Studio, OpenAI, etc.), so that I can use the model I prefer.
11. As a user, I want to ask Xenon to "open Spotify and play my playlist", so that it can control my computer.
12. As a user, I want Xenon to take a screenshot, reason about it, and click on the right thing, so that it can perform GUI tasks.
13. As a user, I want Xenon to verify that its action worked, so that it doesn't blindly repeat failed clicks.
14. As a user, I want a single `.dmg` that works on both Intel and Apple Silicon Macs, so that I can share it with anyone.
15. As a user, I want the app to use minimal RAM when idle, so that it doesn't slow down my Mac.
16. As a user, I want the UI to run at 60fps with input-to-photon under 50ms, so that it feels fluid.
17. As a user, I want the app to request microphone, accessibility, and screen recording permissions on first use, so that it can function.
18. As a developer, I want the provider hub to be a single trait so that adding a new LLM backend is one implementation.
19. As a developer, I want each tool to be a single trait so that adding a new capability is one implementation.
20. As a developer, I want the agent loop to be a hand-rolled ReAct loop, so that I control the prompt, memory, and tool dispatch without framework lock-in.

## Implementation Decisions

### Modules

- **`src-tauri/`** — Tauri v2 Rust backend (single `[[bin]]`, no helper binaries)
  - `runtime.rs` — Tokio task orchestration: spawns audio pipeline, agent loop, TTS/UI sync; owns the `async_channel` channels
  - `audio.rs` — Audio Pipeline: cpal capture → sherpa-onnx VAD → whisper-rs STT → `AudioEvent::Speech(Utterance)`
  - `provider.rs` — Provider Hub: `Provider` trait (async chat completions with streaming + tool-call parsing), one `reqwest` client, OpenAI-compatible HTTP only
  - `agent.rs` — Hand-rolled ReAct loop: receives `Utterance`, calls `Provider` with tools schema, parses tool calls, executes tools, feeds results back, emits `AgentAction`
  - `tools.rs` — `Tool` trait + implementations: `take_screenshot` (xcap), `click_mouse` (enigo), `open_app` (macOS `open`)
  - `grounding.rs` — Screenshot → 512×512 downscale → VLM via Provider → parse JSON coords → enigo click → verify
  - `tts.rs` — sherpa-onnx TTS: streaming synthesis from agent tokens, audio playback via cpal/rodio
  - `ui.rs` — Tauri window management: transparent, frameless, always-on-top, click-through toggle via `set_ignore_cursor_events`, emits Pet State events to frontend
  - `events.rs` — Shared types: `AudioEvent`, `AgentAction`, `PetState`, `ToolCall`
- **`src/`** (or `ui/`) — Frontend: vanilla JS + Canvas 2D, no framework, no build step
  - Sprite sheet renderer with 5 states (Idle/Listening/Thinking/Speaking/Agentic)
  - Listens for Tauri events to switch states and trigger animations
  - Click-through toggle coordinated with Rust via Tauri commands

### Interfaces

- **`Provider` trait** (the LLM seam):
  ```rust
  #[async_trait]
  trait Provider {
      async fn chat_stream(&self, req: ChatRequest) -> Stream<Item = ChatChunk>;
      async fn chat_with_tools(&self, req: ChatRequestWithTools) -> ChatResponse;
  }
  ```
  One implementation: `OpenAICompatProvider` (reqwest, configurable base_url + api_key). All LLM traffic — completions and vision — goes through this trait.

- **`Tool` trait** (the agent capability seam):
  ```rust
  #[async_trait]
  trait Tool {
      fn name(&self) -> &str;
      fn description(&self) -> &str;
      fn schema(&self) -> serde_json::Value; // JSON schema for LLM tool-calling
      async fn execute(&self, args: serde_json::Value) -> ToolResult;
  }
  ```
  Implementations: `TakeScreenshotTool`, `ClickMouseTool`, `OpenAppTool`.

- **Channels** (the runtime seam):
  - `AudioEvent` (audio → agent): `Speech(Utterance)` | `Silence`
  - `AgentAction` (agent → TTS/UI): `Speak(String)` | `SetPetState(PetState)` | `ExecuteTool(ToolCall)`

- **Tauri commands** (frontend ↔ backend): `set_click_through(bool)`, `get_pet_state()`, `trigger_listen()`

### Crates (locked)

```
tauri = "2.12"
whisper-rs = { version = "0.16", features = ["metal"] }
sherpa-onnx = "2.0.1"          # VAD + TTS
cpal = "0.16"                  # mic capture, macOS 11+
xcap = "0.9.10"                # screen capture
enigo = "0.6.1"                # input simulation
reqwest = { features = ["json", "stream"] }
tokio = { features = ["full"] }
async-channel = "2"
```

### Architectural Constraints (ADR-locked)

- 100% Rust backend, no Python, no Electron (ADR-0001)
- Tauri v2 + vanilla JS + Canvas, no Svelte/Vue/React (ADR-0002)
- Provider Hub = OpenAI-compatible HTTP only, no in-process llama.cpp (ADR-0003)
- Hand-rolled ReAct loop, no kalosm/rig (ADR-0004)
- cpal 0.16 + sherpa-onnx + whisper-rs audio stack (ADR-0005)
- Honest perf budget: 60fps / <50ms input-to-photon, idle shell <40MB + active-with-models broken out (ADR-0006)
- Universal unsigned `.dmg`, single binary, min macOS 11 (ADR-0007)

### Build & Distribution

- `tauri.conf.json`: `targets: ["dmg", "app"]`, `minimumSystemVersion: "11.0"`, `signingIdentity: "-"` (ad-hoc, unsigned), commented hooks for Developer ID + notarization
- `Info.plist`: `NSMicrophoneUsageDescription`, `NSScreenCaptureUsageDescription`, `NSAppleEventsUsageDescription`
- `Entitlements.plist`: `com.apple.security.device.audio-input`
- Build: `tauri build --bundles dmg --target universal-apple-darwin`
- Single `[[bin]]` in `src-tauri/Cargo.toml` — no helper binaries (Tauri universal-build bug otherwise)

## Testing Decisions

### What makes a good test

- Test external behavior only: given input X, the module produces output Y. Do not assert internal state, private fields, or call counts.
- Each module is tested through its public API or its trait implementation.
- No tests for Tauri internals, OS permission flows, or actual audio/video hardware — those are integration concerns validated manually.
- No tests for the LLM's actual intelligence — the Provider is mocked; we test that the agent loop correctly parses tool calls, dispatches to the right Tool, and feeds results back.

### Modules to test

| Module | Seam | What to test |
|---|---|---|
| `provider.rs` | `Provider` trait | Given a `ChatRequest`, `OpenAICompatProvider` produces the correct HTTP request (URL, headers, body). Given a mock HTTP response, it parses chunks and tool calls correctly. |
| `tools.rs` | `Tool` trait | Each Tool implements `name()`, `description()`, `schema()` correctly. `execute()` with valid args produces a `ToolResult`. `execute()` with invalid args produces an error (not a panic). |
| `agent.rs` | Public API (`run_utterance`) | Given a mocked Provider that returns a tool call, the agent loop calls the right Tool with the right args and feeds the result back. Given a final response, it emits `AgentAction::Speak`. Given a tool error, it retries or reports. |
| `grounding.rs` | Public API (`ground_and_click`) | Given a screenshot, it downscales to 512×512, calls the VLM via Provider, parses JSON coords, and calls enigo. Given a malformed VLM response, it returns an error. |
| `events.rs` | Types | `AudioEvent` and `AgentAction` serialize/deserialize correctly (serde). |

### Prior art

No existing tests in this repo (greenfield). Test structure will follow Rust conventions: unit tests in-module ( `#[cfg(test)]`), integration tests in `src-tauri/tests/`. No test framework beyond `cargo test`. Mocking via a `MockProvider` struct that implements the `Provider` trait.

## Out of Scope

- Global hotkeys (`Cmd+Shift+Space`)
- Piper/neural TTS upgrade (sherpa-onnx is the TTS for this slice)
- Native llama.cpp in-process inference
- VAD tuning / silence-threshold polish
- Cloud fallback UI (key management, provider switching in a settings panel)
- Onboarding flow for Accessibility/Screen Recording permissions (Info.plist strings only)
- Settings UI / config persistence beyond a config file
- Optimization pass / RAM budget measurement
- Code signing + notarization (unsigned `.dmg`, Gatekeeper bypass required)
- Windows/Linux support
- Multi-modal input (image input from user, not just screenshots)
- Conversation memory / history across sessions
- Multiple concurrent conversations

## Further Notes

- The PRD's `kalosm::Agent::builder().tool(...)` API does not exist — kalosm has no Agent or tool-calling API. The agent loop is hand-rolled (ADR-0004).
- `webrtc-vad` / `webrtcvad-rs` is dead since 2019 — replaced by sherpa-onnx's Silero VAD (ADR-0005).
- `kalosm-sound` does no TTS — sherpa-onnx covers TTS (ADR-0005).
- `cpal` 0.18 requires macOS 14.2+; 0.16 runs on 10.15+. Using 0.16 for wider compatibility (ADR-0005).
- `macos-private-api` / `macOSPrivateApi` are no-ops in Tauri 2.12.1+ — not set.
- Click-through is a runtime call (`set_ignore_cursor_events`), not a config key.
- whisper-rs builds from source via CMake — `cmake` must be on PATH. `WHISPER_DONT_GENERATE_BINDINGS=1` skips bindgen.
- sherpa-onnx auto-downloads prebuilt `osx-x64` libs; `SHERPA_ONNX_LIB_DIR` / `SHERPA_ONNX_ARCHIVE_DIR` override.
- Tauri universal builds silently mis-bundle apps with two `[[bin]]` targets — keep one binary.
- The "sub-millisecond UI" and "<40MB idle RAM" PRD claims are restated honestly in ADR-0006.
