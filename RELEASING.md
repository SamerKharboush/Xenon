# Releasing Xenon

Xenon ships as a universal (Intel + Apple Silicon) `.dmg`, built by GitHub
Actions and published to [GitHub Releases](https://github.com/SamerKharboush/Xenon/releases).
The build is ad-hoc signed / unsigned (ADR-0007); users bypass Gatekeeper with
right-click → Open on first launch.

## Versioning

`Xenon_<version>_universal.dmg` uses the version in **`src-tauri/tauri.conf.json`**.
`src-tauri/Cargo.toml` (and therefore the running app's `CARGO_PKG_VERSION`) must
match it. The release workflow **fails** if a tag's version differs from
`tauri.conf.json`, so a mismatch can never publish a mislabelled DMG.

## Cutting a release

1. Make sure `main` is green (CI: build + test + clippy + JS check).
2. Bump the version and tag locally — this **does not push**:

   ```bash
   scripts/release.sh 0.3.0
   ```

   It updates `tauri.conf.json`, `Cargo.toml` and `Cargo.lock`, commits
   `release: v0.3.0`, and creates the annotated tag `v0.3.0`.

3. Push both the commit and the tag:

   ```bash
   git push origin main
   git push origin v0.3.0
   ```

   The push of `v*` triggers `.github/workflows/release.yml`, which builds the
   universal DMG, zips the `.app` (`ditto -c -k --keepParent`), computes
   `SHA256SUMS`, and creates the GitHub Release with notes generated from the
   commits since the previous tag.

### Manual trigger

`release.yml` also has **workflow_dispatch**. Run it against `main`; it derives
the tag from `tauri.conf.json` and creates the release for that version. Use
this only to rebuild a release (e.g. after a CI flake) — do not use it to
publish a version that was never tagged.

## What gets published

Each release attaches:

- `Xenon_<ver>_universal.dmg` — the installer.
- `Xenon_<ver>_universal.app.zip` — zipped `Xenon.app` (via `ditto`, so macOS
  bundle metadata survives).
- `SHA256SUMS` — SHA-256 of both artifacts.

## In-app updates

The app checks `GET /releases/latest` on GitHub 60 s after launch (if the last
check was > 24 h ago) and every 24 h while running. When a newer version exists:

- the pet shows a bubble: **"Xenon vX.Y.Z is available"**;
- a tray item **"Update to vX.Y.Z…"** appears; clicking it downloads the DMG to
  `~/Downloads`, verifies its SHA-256 against `SHA256SUMS`, and `open`s it;
- Settings → **Updates** can force a check, show the last result, and
  **Skip this version**.

Nothing is installed silently — the user always confirms by dragging the app to
Applications. The last-check timestamp and skipped version persist in the config
file.