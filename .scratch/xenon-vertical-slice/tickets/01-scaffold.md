# Ticket 01: Project scaffold + Tauri config

## Goal
Initialize the Tauri v2 project with correct config, dependencies, and build setup.

## Deliverables
- `src-tauri/Cargo.toml` with all locked dependencies (tauri 2.12, whisper-rs 0.16 + metal, sherpa-onnx 2.0.1, cpal 0.16, xcap 0.9.10, enigo 0.6.1, reqwest json+stream, tokio full, async-channel 2)
- `src-tauri/tauri.conf.json` with: transparent window, always-on-top, frameless, targets dmg+app, minimumSystemVersion 11.0, unsigned signing identity, universal build target
- `src-tauri/Info.plist` with NSMicrophoneUsageDescription, NSScreenCaptureUsageDescription, NSAppleEventsUsageDescription
- `src-tauri/Entitlements.plist` with com.apple.security.device.audio-input
- `src-tauri/build.rs` (Tauri build script)
- `.cargo/config.toml` with whisper-rs metal rustflags for aarch64-apple-darwin
- Single `[[bin]]` in Cargo.toml — no helper binaries
- Directory structure: src-tauri/src/{main.rs, runtime.rs, audio.rs, provider.rs, agent.rs, tools.rs, grounding.rs, tts.rs, ui.rs, events.rs}, src/{index.html, main.js, style.css}

## Dependencies
None (root ticket).

## Notes
- No `macos-private-api` feature needed (no-op in 2.12.1+)
- Click-through is runtime `set_ignore_cursor_events`, not config
- whisper-rs needs cmake on PATH
- sherpa-onnx auto-downloads prebuilt osx-x64 libs
