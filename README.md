# Xenon

A lightweight, local-first desktop companion for macOS built in 100% Rust.

## Build Status

Vertical slice implementation complete. See [.scratch/xenon-vertical-slice/spec.md](.scratch/xenon-vertical-slice/spec.md) for the full specification.

## Architecture

- **Tauri v2** + vanilla JS + Canvas (transparent always-on-top pet UI)
- **whisper-rs** (Metal GPU) for local speech-to-text
- **sherpa-onnx** for neural TTS and VAD
- **OpenAI-compatible HTTP** provider hub (Ollama, LM Studio, etc.)
- Hand-rolled ReAct agent loop with computer control tools

## Building

```bash
cd src-tauri
cargo build --release
cargo tauri build --bundles dmg,app --target universal-apple-darwin
```

The `.dmg` will be in `src-tauri/target/universal-apple-darwin/release/bundle/dmg/`.

## Configuration

Set provider endpoint (defaults to localhost:11434 for Ollama):

```bash
export XENON_PROVIDER_URL="http://localhost:11434"
export XENON_PROVIDER_API_KEY="your-key"  # optional
```

## Permissions

On first run, macOS will request:
- **Microphone** (for voice input)
- **Accessibility** (for mouse/keyboard control)
- **Screen Recording** (for screenshots)

Grant these in System Settings → Privacy & Security.

## Testing

```bash
cd src-tauri
cargo test
```

## Project Structure

```
src-tauri/
  src/
    agent.rs      - ReAct agent loop
    audio.rs      - Audio pipeline (cpal + VAD + STT)
    events.rs     - Shared types (AudioEvent, AgentAction, etc.)
    grounding.rs  - Screenshot → VLM → click pipeline
    provider.rs   - LLM provider hub (OpenAI-compatible HTTP)
    runtime.rs    - Tokio task orchestration
    tools.rs      - Tool trait + implementations
    tts.rs        - TTS engine (sherpa-onnx)
    ui.rs         - Tauri window management
  Cargo.toml
  tauri.conf.json

src/
  index.html      - Canvas element
  main.js         - Sprite renderer (5 pet states)
```

## Credits

Pet sprite artwork (`src/sprites/`) is from [pet-mochi](https://github.com/cskwork/pet-mochi) (MIT License, Copyright (c) 2026 cskwork). See `THIRD_PARTY_NOTICES.md` for full license text.

## License

MIT
