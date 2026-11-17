# 100% Rust, no Python

Xenon is implemented entirely in Rust. No Python backends, no Electron wrappers, no embedded interpreters. Native macOS APIs are reached via Rust FFI crates (cpal for CoreAudio, whisper-rs for whisper.cpp, sherpa-onnx for TTS/VAD, xcap for CoreGraphics, enigo for CGEvent). The Tauri shell is Rust; the frontend is minimal vanilla JS + Canvas with no framework. This is the founding constraint — every dependency decision is checked against it.
