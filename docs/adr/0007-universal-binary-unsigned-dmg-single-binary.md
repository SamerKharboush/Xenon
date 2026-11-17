# Universal binary, unsiged .dmg, single binary target

- **Universal**: `tauri build --target universal-apple-darwin` produces one `.dmg` with both x86_64 (Intel) and aarch64 (Apple Silicon) slices. Intel-only was rejected because a universal binary is the professional default and costs one config line.
- **Unsigned**: No Apple Developer ID / notarization for this build (requires paid account + credentials not available in-session). Tauri bundle config includes commented signing hooks so flipping to Developer ID + notarization later is a one-file change. The `.dmg` is fully functional; users bypass Gatekeeper with right-click→Open.
- **Single binary target**: `src-tauri/Cargo.toml` has one `[[bin]]`. A known Tauri universal-build bug silently mis-bundles apps with two binary targets (only the main binary gets lipo'd, CI passes green having uploaded nothing). One binary is a hard constraint, not a preference.
- **Floor**: `minimumSystemVersion: "11.0"` (Big Sur), set by cpal 0.16.
