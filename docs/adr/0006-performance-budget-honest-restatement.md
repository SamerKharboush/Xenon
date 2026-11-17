# Performance budget: honest restatement

The PRD promises "sub-millisecond UI responsiveness" and "<40MB idle RAM". Both are restated honestly:

- **UI**: "sub-millisecond" is not physically achievable for a Tauri app — Tauri IPC + webview render + 60fps canvas is ~16.7ms/frame minimum. The honest target is **60fps, input-to-photon under 50ms**.
- **RAM**: The "<40MB" figure is the Idle Shell only (Tauri window + empty audio buffers, no models). Active with Models (whisper + LLM) is several hundred MB to several GB, and is the number a user experiences. The budget table will show both numbers broken out, not a single misleading figure.
