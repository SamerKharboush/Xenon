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

    The push of `v*` triggers `.github/workflows/release.yml`, which:

    1. builds the universal app bundle (`cargo tauri build --bundles app`);
    2. re-signs it with `scripts/sign-app.sh` — ad-hoc, but with a stable
       `designated => identifier "com.xenon.app"` requirement so macOS TCC keeps
       the Microphone / Screen Recording / Accessibility grants across rebuilds
       (a cdhash signature loses them on every build);
    3. packages the signed app into `Xenon_<ver>_universal.dmg` with
       `scripts/make-dmg.sh` (background + drag-to-Applications layout);
    4. zips the `.app` (`ditto -c -k --keepParent`, signature preserved),
       computes `SHA256SUMS`, and creates the GitHub Release with notes from the
       commits since the previous tag.

    ### Local build

    ```bash
    cd src-tauri && cargo tauri build --bundles app
    scripts/sign-app.sh src-tauri/target/release/bundle/macos/Xenon.app
    scripts/make-dmg.sh src-tauri/target/release/bundle/macos/Xenon.app dist/Xenon_local.dmg
    ```

    Only the `xenon` binary is bundled (`autobins = false` in `Cargo.toml`);
    the mock MCP server used by tests runs inside the test executable, so
    `Xenon.app/Contents/MacOS/` stays clean and `codesign` succeeds. CI runs
    `cargo test` (mock tests included) before any bundle step.

    > Users upgrading from v0.2.0 must re-grant Microphone and Screen Recording
    > once: the earlier build was cdhash-signed, so macOS treats it as a
    > different app. This is a one-time step.

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

The app checks `GET /releases/latest` 10 s after launch (if the last check was >
6 h ago), every 6 h while running, and whenever Settings opens. When a newer
version exists:

- the pet shows a bubble: **"Update to vX.Y.Z available"**;
- a tray item **"Update to vX.Y.Z…"** appears; clicking it downloads the DMG to
  `~/Downloads`, verifies its SHA-256 against `SHA256SUMS`, mounts it read-only,
  verifies the bundle identifier/version/signature, stages a copy beside the
  running app, moves the old app to the Trash, swaps in the new bundle, strips
  quarantine, and relaunches;
- the tray always has **"Check for Updates…"**, which reports the result in the
  bubble;
- Settings → **Updates** can force a check, show the last result, **Install &
  Restart**, and **Skip this version**.

If the running app is not in a writable `/Applications` (e.g. it was run from
another folder or is AppTranslocated), the updater opens the DMG and asks the
user to drag Xenon to Applications instead. The last-check timestamp and skipped
version persist in the config file.