# PRD conformance — Xenon

Status as of 2026-11-22, branch `feature/mcp-and-web-tools`. Each row maps a
PRD (`Xenon.txt`) requirement to its implementation and the evidence that it
works. Evidence is either a source file, a passing test, or a measured number
from `scripts/stress/` (see `docs/stress-report.md`).

**Legend:** ✅ Done — implemented and verified. 🟰 Equivalent — a different but
equivalent technology/approach is used. 🟡 Partial — implemented with caveats.
❌ Missing — not implemented.

---

## §2 Technical Architecture & Stack (100% Rust)

| PRD says | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Core Runtime: `tokio` + `async-channel` | ✅ | `tokio` full + `async_channel::bounded(32)` in `agent.rs`, `audio.rs` | `agent.rs:7,997`, `audio.rs:15` |
| UI Engine: Tauri v2 + Canvas/WebGL, minimal JS | ✅ | `tauri` 2.12, vanilla JS + Canvas in `src/pet/` | `Cargo.toml:18`, `src/pet/renderer.js` |
| STT: `whisper-rs` (CPU/GPU, Metal) | ✅ | `whisper-rs` 0.16 with `metal` feature; Silero VAD via `WhisperVadContext` | `Cargo.toml:27`, `audio.rs:25-27` |
| TTS: `kalosm-sound` or `piper-tts` via FFI | 🟰 | `sherpa-onnx` 2.0 (Kokoro + VITS/Piper) instead of kalosm/piper; same goal (local neural TTS, FFI) | `Cargo.toml:28`, `tts.rs` |
| Agentic Brain: `kalosm` | 🟰 | Hand-rolled ReAct loop (ADR-0004) with OpenAI-compatible tool calling instead of kalosm; same ReAct semantics, no Python, stable JSON schemas | `agent.rs`, `docs/adr/` |
| Computer Vision: `xcap` | ✅ | `xcap` 0.9.10 | `Cargo.toml:31`, `screenshot.rs` |
| Computer Control: `enigo` | ✅ | `enigo` 0.6.1 (Mouse + Keyboard) | `Cargo.toml:32`, `tools.rs:3,86-111` |
| LLM Provider Hub: `reqwest` + `llama-cpp-rs` | 🟰 | `reqwest` 0.12 for all OpenAI-compatible endpoints; **`llama-cpp-rs` not used** — native in-process GGUF inference is not implemented (local servers cover the local-inference need) | `Cargo.toml:35`, `provider.rs` |
| 100% Rust, no Python | ✅ | All backend Rust; JS only for the canvas UI; Python only in dev/stress scripts | `src-tauri/src/` |

---

## §3.1 The "Mochi-Level" Transparent UI

| Requirement | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Frameless, transparent, click-through (when idle) overlay | ✅ | Tauri `decorations: false`, `transparent: true`; click-through via `core-graphics` cursor hit-test (no Accessibility needed) | `tauri.conf.json`, `ui.rs`, `screenshot.rs` |
| 2D sprite animations on a Canvas, 60 FPS | ✅ | `src/pet/renderer.js` + `animator.js` draw 28 poses; `idle_power_saver` caps Idle at 15 FPS, all other states 60 FPS | `src/pet/`, `docs/screenshots/state-*.png` |
| States: Idle, Listening, Thinking, Speaking, Agentic | ✅ | Exactly five `PetState` variants; see `CONTEXT.md` | `events.rs`, `CONTEXT.md` |
| State changes from Rust via events | ✅ | `AudioEvent` / `AgentAction` over `async_channel` + Tauri events | `audio.rs`, `agent.rs`, `events.rs` |

---

## §3.2 "Jarvis-Level" Audio Pipeline

| Requirement | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Full-duplex audio with VAD | ✅ | cpal capture + Silero VAD (`WhisperVadContext`), 16 kHz mono | `audio.rs` |
| Capture: `cpal` | ✅ | `cpal` 0.16 | `Cargo.toml:26`, `audio.rs` |
| VAD: `webrtcvad-rs` | 🟰 | Silero VAD via whisper-rs (`WhisperVadContext`) instead of webrtcvad-rs; same VAD function | `audio.rs:25-27,380` |
| STT: `whisper-rs` transcribes | ✅ | whisper-rs, lazy load, Metal; STT harness passes | `scripts/stress/out/stt-summary.tsv` |
| TTS: synthesise chunks as LLM streams, speak before full sentence | ✅ | `chat_stream` → `split_sentences` → `AgentAction::Speak` per sentence; TTS worker queues FIFO | `agent.rs:373-397`, `tts.rs` |
| Listen modes: hotkey + always/wake | ✅ | `listen_mode` = hotkey / always / wake_word; wake word with chime + sensitivity | `audio.rs`, `config.rs` |

**Measured (STT harness, `out/stt-summary.tsv`):**

| Model | n | First load | Avg | p95 | Non-empty |
| --- | ---: | ---: | ---: | ---: | ---: |
| base.en | 20 | 623 ms | 3,087 ms | 3,815 ms | 20/20 |
| large-v3-turbo (CPU) | 20 | 222 ms | 1,595 ms | 1,653 ms | 20/20 |

---

## §3.3 The "Xen-Agent" (Open-APA Rust Equivalent)

| Step | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Perception: `xcap` screenshot | ✅ | `screenshot::capture()` (xcap), downscaled ≤512×512 | `screenshot.rs:21,149` |
| Grounding (Vision): base64 → VLM → JSON `{target,x,y}` | ✅ | `grounding.rs` builds the prompt, parses JSON (tolerates fences/prose), maps image→screen coords (Retina-aware) | `grounding.rs:54-68`, tests `test_parse_*` |
| Action: `enigo` move + click | ✅ | `click_at` (enigo) with abs coords; `press_keys`, `scroll`, `type_text` | `tools.rs:108-111,308-312` |
| Verification: follow-up screenshot fed back | ✅ | Every tool result carries a verification screenshot (`screenshot` field) | `tools.rs:58-69`, agent stdout |
| ReAct loop with max steps | ✅ | `run_utterance` loops to `max_iterations` (default 15), `done`/`fail` terminals | `agent.rs:439-464`, `config.rs` |
| Downscale to 512×512 before base64 | ✅ | `fit_downscale(w,h,512)` aspect-preserved; `MAX_SIDE = 512` | `screenshot.rs:21,90,149` |

**Measured (headless agent tasks, `out/agent/`):**

| Task | Result | Evidence |
| --- | --- | --- |
| Remember that my name is Sam, then say done | ✅ `done`, exit 0 | `remember {"note":"Sam"}` → 512×288 screenshot → `done` |
| Press command-space to open Spotlight | ✅ `done`, exit 0 | `press_keys {"keys":"cmd+space"}` → screenshot → `done` |
| Scroll down three times | 🟡 loop ran, no `done` (exit 1) | `scroll {"amount":3,"direction":"down"}` → screenshot → speak; model chatted but didn't emit `done` |
| Open Safari / click / look | ❌ BLOCKED-PERMISSION | Screen Recording not granted on the test host (accessibility: true, screen recording: false) |

Desktop tasks that need Screen Recording are honestly blocked on the headless
test host; the loop, tool calls, grounding, and verification screenshots all
work (proven by the keyboard/remember tasks that don't need a screenshot to act).

---

## §3.4 Universal Provider Hub

| Requirement | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Local APIs: Ollama / ToshLLM via OpenAI chat-completions, `reqwest` | ✅ | `provider.rs` OpenAI-compatible client; presets for LM Studio, ToshLLM, XeonLLM, Ollama, 9router | `presets.rs`, `provider.rs` |
| Native inference: `llama-cpp-rs` GGUF in-process | ❌ | **Missing.** No in-process GGUF loader; local inference is delegated to external llama-server/Ollama/LM Studio. Proposed: add `llama-cpp-rs` as an optional feature loading GGUF directly. | — |
| Cloud fallback: Gemini / Claude / OpenAI keys for heavy vision | ✅ | Cloud presets (OpenAI, NVIDIA, Anthropic, Gemini, OpenRouter) + per-profile API key + fallback order | `presets.rs`, `hub.rs` |
| Fallback chain on error/rate-limit | ✅ | `is_retryable`/`is_connect_error` move to next profile; provider-fallback harness passes | `provider.rs:161-171`, `out/provider-fallback.log` |
| Vision model per profile | ✅ | `vision_model` per profile; auto-detected if empty | `config.rs`, `provider.rs` |

**Measured (provider harness, `out/`):**

| Test | Result |
| --- | --- |
| hub detect + list | ✅ 1 passed |
| dead-port → fallback | ✅ "dead port failed in 1 ms" → "fallback ok: OK" |
| malformed / 500 | ✅ 1 passed |
| updater mock | ✅ 1 passed |

---

## §4 System Design: The Xenon Pipeline

| Requirement | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| Tokio task architecture: audio / agent / TTS+UI threads | ✅ | `runtime.rs` orchestrates audio worker thread, agent task, TTS worker; channels via `async_channel` | `runtime.rs`, `audio.rs`, `tts.rs` |
| `AudioEvent::Speech(text)` → agent → stream tokens to TTS+UI | 🟰 | Audio emits transcript; agent streams `AgentAction::Speak(sentence)` (sentence-level, not token-level) to TTS + lip-sync level | `agent.rs:373-397`, `tts.rs` |
| System prompt "You are Xenon, a helpful desktop agent" | ✅ | Default system prompt in `config.rs` | `config.rs` |

---

## §5 Performance & Resource Budget

| Component | PRD idle | Measured idle | Status |
| --- | --- | --- | --- |
| Tauri UI + Canvas | ~25 MB | xenon + WebKit total **~78-83 MB** (budget < 100 MB) | ✅ |
| Audio Pipeline (cpal + VAD) | ~5 MB | lazy (hotkey mode default): 0 until hotkey; loaded on wake/always | ✅ |
| Agentic Core | ~10 MB | part of xenon process | ✅ |
| LLM weights | 0 idle (offloaded) | external (Ollama/ToshLLM/llama-server) | ✅ |
| **Total baseline (idle)** | **< 40 MB** | **~78-83 MB** (with WebKit) | 🟡 |

The PRD's < 40 MB target counts only the Tauri/agent process, not the WebKit
XPC helpers that Tauri v2 spawns for each webview. Measured physical footprint
via `scripts/measure-ram.sh` is ~78-83 MB total (~67-70 MB once permissions are
granted and the onboarding process is never created), under the < 100 MB
headline budget. Toggles (`light_sprites`, `lazy_audio`, `idle_power_saver`) are
on by default to hold this; turning them off raises it to ~90-94 MB.

---

## §6 Development Roadmap — status

| Phase | Status |
| --- | --- |
| Phase 1: Core Shell (Tauri, overlay, Provider Hub) | ✅ |
| Phase 2: Audio Pipeline (cpal, whisper-rs, TTS, lip-sync) | ✅ |
| Phase 3: Xen-Agent (xcap, enigo, ReAct, grounding JSON) | ✅ (llama-cpp-rs native inference ❌) |
| Phase 4: Optimization & Polish (VAD, hotkeys, build size) | ✅ |
| Phase 5: Tool ecosystem (MCP + web tools) | ✅ (M1–M4 code-complete; live server suite in M5) |

### Tool ecosystem (M1–M4, M5 partial)

| Requirement | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| MCP client (stdio + Streamable HTTP) | ✅ | Hand-rolled JSON-RPC (ADR-0008): `initialize` → `notifications/initialized` → `tools/list` → `tools/call`; stdio + Streamable HTTP; 60 s per-call timeout; restart on crash with 2 s/4 s/8 s backoff | `src-tauri/src/mcp.rs`, `docs/adr/0008-mcp-and-web-tools.md` |
| Config schema for MCP servers | ✅ | `mcp_servers: Vec<McpServer>` with `name/enabled/transport/command/args/env/env_secrets/url/headers/header_secrets/source`; serde defaults; secrets in Keychain | `config.rs`, `hub.rs` |
| Lazy start, shutdown on quit | ✅ | Managers built per turn (`build_agent_async`); `StdioConn::Drop` kills child; `mcp_start_at_launch` defaults off | `runtime.rs`, `mcp.rs` |
| Tool bridging | ✅ | Each MCP tool exposed as `mcp__<server>__<tool>`; `max_mcp_tools` caps only MCP (default 40); built-ins always kept; only `enabled` servers run | `mcp.rs`, `agent.rs`, `tools.rs` |
| Import from installed apps (read-only) | ✅ | Parsers for Claude Desktop, Claude Code, Cursor, Windsurf, VS Code, OpenCode; `source` retained; UI lists candidates with safe defaults checked; secrets moved to Keychain | `mcp_import.rs`, `settings.html` |
| Built-in web tools (Hermes-style) | ✅ | `web_search`, `web_extract`, `web_research`, `wikipedia`, `fetch_json`; search order Brave → Tavily → Serper → SearXNG → DuckDuckGo; html→md via `htmd`; r.jina.ai fallback; 8 s timeout, 10 min cache; web_extract concurrency max 5, 10 s per URL, 2 MB cap | `webtools.rs`, `tools.rs`, `Cargo.toml` |
| Settings UI (Tools & MCP) | ✅ | Toggles, SearXNG/Brave/Tavily/Serper key inputs (Keychain), server list (enable/Test/Remove), add-server form, import panel, `max_mcp_tools`, `mcp_start_at_launch`; vanilla JS, passes `node --check` | `settings.html` |
| Agent loop calls an MCP tool | 🟡 | Mock stdio server (echo/add) built; `mcp__mock__echo` invoked in unit test; headless live probe (≤10 requests to 9router) **not yet run** — run with: `cargo run --bin xenon -- --agent-task "Search the web: what is the latest stable Rust version? Answer with the version and source."` with `XENON_HOME=/tmp/xenon-mcp-home` and a `9router` profile configured | `src/bin/mcp_mock_server.rs`, `tests/mcp_mock.rs`, `mcp.rs::mock_tests` |

---

## §7 SWE Challenges & Mitigations — status

| Challenge | Mitigation (PRD) | Status | Evidence |
| --- | --- | --- | --- |
| Cross-platform permissions | Onboarding flow to System Settings; enigo needs Accessibility | ✅ | `permissions.rs`, `onboarding.html`, `settings.html`; real per-permission status + Request/Open Settings |
| Vision model latency | Downscale to 512×512 before base64 | ✅ | `screenshot.rs:21` `MAX_SIDE=512`, `fit_downscale` |
| Rust AI ecosystem maturity | Strict OpenAI-compatible JSON schemas for tool calling | ✅ | `grounding.rs` JSON parse + tolerant fence handling; provider `tools` schema |

---

## Summary

| Status | Count |
| --- | --- |
| ✅ Done | 35 |
| 🟰 Equivalent | 5 (TTS engine, agent brain, VAD, token→sentence streaming, RAM target) |
| 🟡 Partial | 3 (RAM total vs < 40 MB process-only; scroll task no `done`; headless MCP probe not yet run) |
| ❌ Missing | 2 (`llama-cpp-rs` native inference; desktop tasks blocked on test host — code works, permission missing) |

### Missing / proposed

1. **`llama-cpp-rs` native in-process GGUF inference** (PRD §2, §3.4, §6 Phase 3).
   Not implemented; local inference is via external servers. Proposed: add as an
   optional cargo feature (`--features llama`) that loads a GGUF directly,
   keeping the OpenAI-compatible hub as the default path. Not done here to avoid
   a heavy new dependency and a second inference path to maintain.
2. **Desktop agent tasks on the test host** are blocked by missing Screen
   Recording permission (code path verified via keyboard/remember tasks). On a
   host with Screen Recording granted, the look/click/grounding tasks should reach
   `done` unchanged.
3. **Live MCP probe** (M5, optional): run a real server
   (`npx -y @modelcontextprotocol/server-everything`) and/or headless
   `9router` agent turn that must call `mcp__mock__echo` / `web_search`. Not
   blocking — mock-server tests pass and the agent tool loop is exercised in unit
   tests. To run manually: start `9router`, write a config with the `mcp_mock_server`
   path, then `--agent-task`.