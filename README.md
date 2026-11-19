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
| `ui.rs` | Tauri window management (incl. lazy Settings/Onboarding windows) |

Two extra helper scripts live in `scripts/`: `measure-ram.sh` (memory, above) and
`resize-sprites.sh` (regenerates `src/sprites/light/` from `src/sprites/`).

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

## Memory

Idle footprint, measured with `scripts/measure-ram.sh` (physical footprint per
process — RSS counts shared clean framework pages several times over):

```bash
scripts/measure-ram.sh [path-to-Xenon.app] [settle-seconds]
```

It launches the app, waits, prints the `footprint` of the `xenon` process plus
every WebKit XPC process (WebContent / GPU / Networking) the app just caused
launchd to spawn, then a total, and kills the app. Numbers are the 0.2.0
universal release build on macOS 14, ~15 s after launch:

| Process | Idle (all toggles on) |
| --- | --- |
| `xenon` | ~32 MB |
| WebContent (pet) | ~13-17 MB |
| WebContent (onboarding, only while permissions are missing) | ~13 MB |
| WebKit GPU | ~18 MB |
| WebKit Networking | ~4 MB |
| **Total** | **~78-83 MB** (budget: < 100 MB) |

Grant Accessibility + Screen Recording and the onboarding process is never
created: total settles at ~67-70 MB.

### Performance toggles

Settings → **Performance** exposes the three reductions, all **on** by default
(the budget-meeting configuration). Turning one off restores the previous
behaviour exactly.

| Config key | Default | Toggles |
| --- | --- | --- |
| `light_sprites` | `true` | 320×320 sprites in `src/sprites/light/`, rare poses decoded on first use. Off: full-size 448×448 originals in `src/sprites/`, all preloaded (applies immediately) |
| `lazy_audio` | `true` | Mic, VAD and TTS stay unloaded until the hotkey; tokio runs on 2 workers. Off: capture + VAD preloaded at startup, TTS model loaded, default per-core tokio pool (applies after restart) |
| `idle_power_saver` | `true` | Idle draws at 15 FPS. Off: Idle draws at 60 FPS (CPU only, no RAM effect) |

Measured with `scripts/measure-ram.sh` on the release build, ~15 s after launch,
onboarding window open in both runs:

| | xenon | WebContent (pet) | GPU | Networking | Onboarding | Total |
| --- | --- | --- | --- | --- | --- | --- |
| Toggles on | 32 MB | 13-14 MB | 15-16 MB | 4 MB | 13 MB | **78 MB** |
| Toggles off | 38-39 MB | 16-19 MB | 18-20 MB | 4 MB | 13 MB | **90-94 MB** |

The toggles-off run shows what each contributes: `xenon` +6-7 MB (per-core tokio
pool instead of 2 workers), pet WebContent +3-6 MB (all 27 poses preloaded at
448²). The audio half of `lazy_audio = false` could not be measured on the
measurement host — it has no audio input device, so capture preload fails with
"no supported input config" and the pipeline stays cold. On a Mac with a mic the
same run additionally holds the cpal stream + Silero VAD (and the TTS model if
one is installed).

What is lazy by default, and why the number is what it is:

- **Settings and onboarding windows are created on demand** (`ui::show_settings`
  / `ui::show_onboarding`), not declared in `tauri.conf.json`. Each WKWebView
  costs its own WebContent process (~13-25 MB); keeping them out of the startup
  config saves ~40 MB. Closing a window destroys it, so the next open rebuilds it.
- **Sprite poses decode on first use** (`light_sprites = true`). Shipped light
  sprites are 320×320 (the pet draws at 240 CSS px) from 448×448 originals — 28
  decoded 448² bitmaps would be ~22 MB. Regenerate with
  `scripts/resize-sprites.sh`.
- **Audio is lazy in hotkey listen mode** (the default, `lazy_audio = true`): no
  cpal input stream, no VAD context, no whisper STT until `Cmd+Shift+Space` opens
  a session. Whisper and the sherpa-onnx TTS model are loaded on first use and
  unloaded when idle.
- **Tokio runs on 2 worker threads**, not one per core, via
  `tauri::async_runtime::set` in `main.rs`.
- **Idle canvas draws at 15 FPS** (`idle_power_saver = true`); full rate in every
  other state or while a speech bubble is up.

Active RAM with whisper/STT/TTS/LLM loaded is several hundred MB — see
`CONTEXT.md` ("Idle Shell" vs "Active with Models").

## Credits & license

Pet sprite artwork from [pet-mochi](https://github.com/cskwork/pet-mochi) (MIT, © 2026 cskwork). Inspired by [Open-APA](https://github.com/laiye-ai/Open-APA) (laiye-ai) and Personal-AI-Jarvis. See `THIRD_PARTY_NOTICES.md` for full third-party license text.

MIT.