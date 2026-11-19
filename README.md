# Xenon

A local-first macOS menu-bar pet that listens, thinks, speaks, and drives your computer.

## Features

- Menu-bar pet with five states: Idle, Listening, Thinking, Speaking, Agentic. Click to tap, double-click for settings, drag to move.
- Local speech pipeline: `cpal` capture → whisper-rs Silero VAD → whisper-rs STT (Metal). Whisper loads lazily and unloads when idle. Listen modes: hotkey (default) or always-on.
- Neural TTS via sherpa-onnx Kokoro (or VITS/Piper), streamed per sentence with lip-sync. Falls back to macOS `say` when no model is present.
- Hand-rolled ReAct agent loop (ADR-0004) with computer-control tools: `look`, `click`, `type_text`, `press_keys`, `scroll`, `open_app`, `wait`. Clicks use VLM grounding on a ≤512×512 screenshot mapped back to screen coordinates (Retina-aware). A verification screenshot is taken after each action; max steps configurable.
- Provider Hub: OpenAI-compatible profiles with configurable fallback order. Presets for Ollama (`http://localhost:11434/v1`), LM Studio (`http://localhost:1234/v1`), OpenAI, Gemini, and Anthropic (OpenAI-compatible endpoints). Separate vision model per profile.

## Install

Download `Xenon_0.2.0_universal.dmg` from a release, open it, and drag Xenon to Applications.

The build is unsigned/ad-hoc (ADR-0007), so macOS Gatekeeper blocks it on first launch. Right-click the app → **Open**, then confirm. Do this once.

## First run

An onboarding window checks permissions and opens the relevant System Settings panes:

- **Accessibility** — mouse/keyboard control
- **Screen Recording** — screenshots for grounding
- **Microphone** — voice input (prompted)

Then fetch the models and pick a provider (see below).

## Models

Run the fetch script to download default models:

```bash
scripts/fetch-models.sh
```

This installs `ggml-base.en` (whisper), `ggml-silero-v6.2.0` (VAD), and `kokoro-en-v0_19` (TTS) into `~/Library/Application Support/com.xenon.app/models/`. Paths are configurable in Settings; by default whisper also picks up `~/models/whisper/ggml-large-v3-turbo.bin` if present.

## Usage

### Hotkeys

- `Cmd+Shift+Space` — talk to / interrupt the pet
- `Cmd+Shift+Escape` — abort the agent turn and stop speech

### Tray menu

Show Pet · Talk · Settings · Quit

### Pet interactions

- Click — tap
- Double-click — open settings
- Drag — move the pet

## Configuration

Settings live in the app, backed by a config file at:

```
~/Library/Application Support/xenon/config.json
```

A corrupt config is backed up to `config.json.bak` before defaults are restored. Configure provider endpoint, vision model, model paths, listen mode, and agent max steps from Settings.

## Architecture

- **Tauri v2**, 100% Rust backend, vanilla JS + Canvas UI (`src/pet/`)
- **whisper-rs** (Metal) for VAD + STT
- **sherpa-onnx** for TTS
- **OpenAI-compatible HTTP** provider hub
- **ReAct** agent loop (ADR-0004) with computer-control tools

Core modules (`src-tauri/src/`):

| Module | Role |
| --- | --- |
| `agent.rs` | ReAct agent loop |
| `audio.rs` | Audio pipeline (cpal + VAD + STT) |
| `audio_dsp.rs` | Audio DSP helpers |
| `config.rs` | Config load/save, corrupt-config backup |
| `events.rs` | Shared types (AudioEvent, AgentAction, …) |
| `grounding.rs` | Screenshot → VLM → screen-point pipeline |
| `lib.rs` | App entry/wiring |
| `main.rs` | Binary entry point |
| `permissions.rs` | Permission checks + Settings deep links |
| `provider.rs` | Provider hub (OpenAI-compatible HTTP) |
| `runtime.rs` | Tokio task orchestration |
| `screenshot.rs` | Screen capture |
| `tools.rs` | Tool trait + implementations |
| `tts.rs` | TTS engine (sherpa-onnx) |
| `ui.rs` | Tauri window management |

See `CONTEXT.md` for the glossary and `docs/adr/` for architecture decisions.

## Building

Requires the Tauri v2 toolchain. From `src-tauri/`:

```bash
cargo tauri build --bundles app,dmg --target universal-apple-darwin
```

Output: `src-tauri/target/universal-apple-darwin/release/bundle/dmg/Xenon_0.2.0_universal.dmg`

Icons and DMG background are generated via `scripts/gen-icon.swift` and `scripts/gen-dmg.swift`; regenerate icons with `cargo tauri icon src-tauri/icons/source.png`.

### Signing and notarization (optional)

The default build is unsigned. To sign/notarize, set these env vars before `cargo tauri build` and uncomment the signing config in `tauri.conf.json`:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: ..."   # e.g. "Developer ID Application: Your Name (TEAMID)"
export APPLE_ID="you@example.com"
export APPLE_PASSWORD="app-specific-password"
export APPLE_TEAM_ID="TEAMID"
```

## Testing

```bash
cd src-tauri
cargo test
```

Real-model tests are marked `#[ignore]`:

```bash
cargo test -- --ignored
```

## Credits & license

Pet sprite artwork from [pet-mochi](https://github.com/cskwork/pet-mochi) (MIT, © 2026 cskwork). Inspired by [Open-APA](https://github.com/laiye-ai/Open-APA) (laiye-ai) and Personal-AI-Jarvis. See `THIRD_PARTY_NOTICES.md` for full third-party license text.

MIT.