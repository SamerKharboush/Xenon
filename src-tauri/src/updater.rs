//! In-app update check against the public GitHub Releases API.
//!
//! Flow: GET `/releases/latest` → compare semver → if newer, show a pet-bubble
//! notice and (on demand) download the universal DMG to `~/Downloads`, verify
//! its SHA-256 against `SHA256SUMS`, then `open` it so the user can drag the
//! app to Applications. Nothing is installed silently.
//!
//! Check schedule: 60 s after launch if the last check was > 24 h ago, then
//! every 24 h while running. `last_update_check` (unix seconds) and
//! `skipped_version` persist in the config.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::sync::RwLock;

use crate::ui::ArcRwLockConfig;

/// The running app version, from `CARGO_PKG_VERSION`.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const API_URL: &str =
    "https://api.github.com/repos/SamerKharboush/Xenon/releases/latest";
/// Test override for the release API (harness 6 uses a local mock server).
fn api_url() -> String {
    std::env::var("XENON_UPDATE_API").unwrap_or_else(|_| API_URL.to_string())
}
const USER_AGENT: &str = "Xenon-Updater";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// One asset of a GitHub Release (only the fields we read).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// A GitHub Release (only the fields we read). Unknown fields are ignored, so
/// future API additions don't break parsing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// What the Settings window shows about the last / current check.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub available: bool,
    /// Unix seconds of the last successful check, or `None` if never.
    pub last_checked: Option<i64>,
    pub message: String,
}

/// A newer release that is ready to download, kept in memory until the user
/// acts on it (download, skip, or restart).
#[derive(Debug, Clone)]
pub struct PendingUpdate {
    pub version: String,
    pub dmg_name: String,
    pub dmg_url: String,
    pub checksums_url: Option<String>,
}

/// Shared updater state managed by Tauri.
pub struct UpdaterState {
    pub pending: RwLock<Option<PendingUpdate>>,
    /// Tray menu + update item, set by `setup_tray` once the tray is built. The
    /// updater appends the item ("Update to vX.Y.Z…") when a newer release is
    /// found and removes it on skip. Both `None` until the tray is built.
    pub menu: RwLock<Option<tauri::menu::Menu<tauri::Wry>>>,
    pub update_item: RwLock<Option<tauri::menu::MenuItem<tauri::Wry>>>,
}

impl Default for UpdaterState {
    fn default() -> Self {
        Self {
            pending: RwLock::new(None),
            menu: RwLock::new(None),
            update_item: RwLock::new(None),
        }
    }
}

impl UpdaterState {
    /// Reveal + relabel the tray item ("Update to vX.Y.Z…"), appending it to
    /// the tray menu the first time. No-op if the tray isn't built yet.
    pub async fn show_update_item(&self, version: &str) {
        let menu = { self.menu.read().await.clone() };
        let item = { self.update_item.read().await.clone() };
        let (Some(menu), Some(item)) = (menu, item) else {
            return;
        };
        let _ = item.set_text(format!("Update to v{version}…"));
        let _ = item.set_enabled(true);
        // Append once; `get` tells us if it's already in the menu.
        if menu.get("update").is_none() {
            let _ = menu.append(&item);
        }
    }

    /// Remove the tray item (used when the user skips the version).
    pub async fn hide_update_item(&self) {
        let menu = { self.menu.read().await.clone() };
        let item = { self.update_item.read().await.clone() };
        if let (Some(menu), Some(item)) = (menu, item) {
            let _ = menu.remove(&item);
        }
    }
}

// --- pure helpers (unit-tested) ---------------------------------------------

/// Strip an optional leading `v`/`V` and parse a semver `Version`.
pub fn parse_version(s: &str) -> Option<Version> {
    let t = s.trim();
    let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
    Version::parse(t).ok()
}

/// True when `latest` is a strictly newer version than `current`. Any
/// unparseable input yields `false` (we never nag about a version we can't
/// reason about). Pre-release tags follow semver ordering, so `0.3.0-rc.1` is
/// newer than `0.2.0` but older than `0.3.0`.
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Decision for the scheduler: should a check run at `now` (unix seconds)
/// given the last check at `last`? A never-run check (`last == 0`) or one
/// older than 24 h means yes.
pub fn should_check(now: i64, last: i64) -> bool {
    if last <= 0 {
        return true;
    }
    let elapsed = now.saturating_sub(last);
    elapsed >= CHECK_INTERVAL.as_secs() as i64
}

/// Parse a `SHA256SUMS` blob into a map of lowercase hex hash → filename.
pub fn parse_checksums(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(hash) = it.next() else { continue };
        let Some(name) = it.next() else { continue };
        map.insert(hash.to_ascii_lowercase(), name.to_string());
    }
    map
}

/// SHA-256 hex of arbitrary bytes. Used by the download path's unit test
/// (the real download hashes incrementally while streaming to disk).
#[cfg_attr(not(test), allow(dead_code))]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Pick the universal DMG asset from a release. Prefers a `.dmg` whose name
/// contains `universal`; falls back to any `.dmg`.
pub fn find_dmg(release: &Release) -> Option<&Asset> {
    release
        .assets
        .iter()
        .find(|a| {
            a.name.to_ascii_lowercase().ends_with(".dmg")
                && a.name.to_ascii_lowercase().contains("universal")
        })
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|a| a.name.to_ascii_lowercase().ends_with(".dmg"))
        })
}

/// The `SHA256SUMS` asset from a release, if present.
pub fn find_checksums(release: &Release) -> Option<&Asset> {
    release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS"))
}

/// Look up the expected SHA-256 for a DMG filename in a parsed sums map.
/// The DMG basename is matched; the leading path, if any, is ignored.
pub fn expected_checksum(
    sums: &std::collections::HashMap<String, String>,
    dmg_name: &str,
) -> Option<String> {
    let base = dmg_name.rsplit('/').next().unwrap_or(dmg_name);
    sums.iter()
        .find(|(_, name)| name.as_str() == base)
        .map(|(hash, _)| hash.clone())
}

/// Build the update-status payload from the pending update + config.
pub fn status_from(pending: Option<&PendingUpdate>, current: &str) -> UpdateStatus {
    match pending {
        Some(p) => UpdateStatus {
            current_version: current.to_string(),
            latest_version: Some(p.version.clone()),
            available: true,
            last_checked: None,
            message: format!("Xenon v{} is available", p.version),
        },
        None => UpdateStatus {
            current_version: current.to_string(),
            latest_version: None,
            available: false,
            last_checked: None,
            message: "No newer version available".to_string(),
        },
    }
}

// --- network -----------------------------------------------------------------

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// GET the latest release. Returns `Ok(None)` for 404 (no releases yet) and
/// rate-limit responses, so callers can stay quiet instead of surfacing noise.
async fn fetch_latest() -> Result<Option<Release>, String> {
    let client = http_client()?;
    let resp = client
        .get(api_url())
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    match resp.status() {
        reqwest::StatusCode::NOT_FOUND => {
            log::info!("update check: no releases published (404)");
            Ok(None)
        }
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            log::warn!("update check: rate limited (HTTP {})", resp.status().as_u16());
            Ok(None)
        }
        s if !s.is_success() => {
            log::warn!("update check: unexpected HTTP {}", s.as_u16());
            Ok(None)
        }
        _ => resp
            .json::<Release>()
            .await
            .map(Some)
            .map_err(|e| format!("parse failed: {e}")),
    }
}

// --- orchestration -----------------------------------------------------------

/// Run one update check: fetch, compare, update shared state, notify the user.
/// `force` ignores the "already checked recently" gate and always contacts the
/// API (used by the Settings "Check now" button).
pub async fn check_once(
    app: &tauri::AppHandle,
    config: &ArcRwLockConfig,
    state: &UpdaterState,
    force: bool,
) -> Result<UpdateStatus, String> {
    let now = chrono::Utc::now().timestamp();

    if !force {
        let last = { config.read().await.last_update_check };
        if !should_check(now, last) {
            return Ok(status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION));
        }
    }

    // Respect the auto-check toggle for scheduled checks.
    if !force && !config.read().await.auto_check_updates {
        return Ok(status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION));
    }

    let release = match fetch_latest().await? {
        Some(r) => r,
        None => {
            // Still record the attempt so the scheduler backs off.
            persist_last_check(config, now).await;
            let mut status = status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION);
            status.message = "No releases found".to_string();
            status.last_checked = Some(now);
            return Ok(status);
        }
    };

    persist_last_check(config, now).await;

    let latest_tag = release.tag_name.clone();
    let skipped = { config.read().await.skipped_version.clone() };
    let skipped = skipped.unwrap_or_default();

    if !is_newer_version(&latest_tag, CURRENT_VERSION) {
        let mut status =
            status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION);
        status.latest_version = Some(latest_tag.clone());
        status.message = format!("Up to date (v{CURRENT_VERSION})");
        status.last_checked = Some(now);
        return Ok(status);
    }

    if !skipped.is_empty() && skipped == latest_tag {
        // User skipped this version; don't nag.
        let mut status =
            status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION);
        status.latest_version = Some(latest_tag.clone());
        status.message = format!("v{latest_tag} skipped");
        status.last_checked = Some(now);
        return Ok(status);
    }

    let dmg = match find_dmg(&release) {
        Some(a) => a,
        None => {
            let mut status =
                status_from(state.pending.read().await.clone().as_ref(), CURRENT_VERSION);
            status.message = "New release has no DMG asset".to_string();
            status.last_checked = Some(now);
            return Ok(status);
        }
    };

    let pending = PendingUpdate {
        version: latest_tag.clone(),
        dmg_name: dmg.name.clone(),
        dmg_url: dmg.browser_download_url.clone(),
        checksums_url: find_checksums(&release).map(|a| a.browser_download_url.clone()),
    };

    {
        let mut guard = state.pending.write().await;
        *guard = Some(pending);
    }

    // Reveal + relabel the tray item ("Update to vX.Y.Z…").
    state.show_update_item(&latest_tag).await;

    let notice = format!("Xenon v{} is available", latest_tag);
    let _ = app.emit("xenon-notice", notice.clone());
    log::info!("update available: {latest_tag}");

    let mut status = status_from(Some(&state.pending.read().await.clone().unwrap()), CURRENT_VERSION);
    status.last_checked = Some(now);
    Ok(status)
}

async fn persist_last_check(config: &ArcRwLockConfig, now: i64) {
    let mut guard = config.write().await;
    guard.last_update_check = now;
    let _ = guard.save();
}

/// Skip the pending version: record it in config so future checks stay quiet.
pub async fn skip_pending(config: &ArcRwLockConfig, state: &UpdaterState) {
    let version = {
        let mut guard = state.pending.write().await;
        guard.take().map(|p| p.version)
    };
    if let Some(v) = version {
        let mut guard = config.write().await;
        guard.skipped_version = Some(v);
        let _ = guard.save();
    }
    // Remove the tray item again.
    state.hide_update_item().await;
}

/// Spawn the background scheduler. 60 s after launch if a check is due, then
/// every 24 h while the app runs.
pub fn setup(app: &tauri::AppHandle, config: ArcRwLockConfig, state: Arc<UpdaterState>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            let _ = check_once(&app, &config, &state, false).await;
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

// --- Tauri commands ----------------------------------------------------------

/// Current status for the Settings window (no network).
#[tauri::command]
pub fn get_update_status(state: tauri::State<'_, Arc<UpdaterState>>) -> UpdateStatus {
    let pending = state.pending.try_read().ok().and_then(|g| g.clone());
    let mut status = status_from(pending.as_ref(), CURRENT_VERSION);
    status.last_checked = None; // last check time is read from config by the UI
    status
}

/// "Check now" button: force a check and return a human-readable result.
#[tauri::command]
pub async fn check_now(
    app: tauri::AppHandle,
    config: tauri::State<'_, ArcRwLockConfig>,
    state: tauri::State<'_, Arc<UpdaterState>>,
) -> Result<UpdateStatus, String> {
    check_once(&app, &config, &state, true).await
}

/// "Skip this version" in Settings: record the pending version as skipped.
#[tauri::command]
pub async fn skip_update(
    config: tauri::State<'_, ArcRwLockConfig>,
    state: tauri::State<'_, Arc<UpdaterState>>,
) -> Result<(), String> {
    skip_pending(&config, &state).await;
    Ok(())
}

/// Tray menu / Settings button: download the pending DMG to ~/Downloads,
/// verify its SHA-256, and `open` it. Emits progress via `xenon-notice`.
pub async fn install_update_impl(
    app: &tauri::AppHandle,
    state: &UpdaterState,
) -> Result<(), String> {
    let pending = {
        let guard = state.pending.read().await;
        guard.clone()
    };
    let Some(p) = pending else {
        return Err("No update available".to_string());
    };

    download_and_open(Some(app), &p).await
}

#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<UpdaterState>>,
) -> Result<(), String> {
    install_update_impl(&app, &state).await
}

// --- download + verify -------------------------------------------------------

fn notify(app: Option<&tauri::AppHandle>, msg: impl Into<String>) {
    if let Some(app) = app {
        let _ = app.emit("xenon-notice", msg.into());
    }
}

async fn download_and_open(
    app: Option<&tauri::AppHandle>,
    p: &PendingUpdate,
) -> Result<(), String> {
    let downloads = std::env::var_os("XENON_UPDATE_DOWNLOAD_DIR")
        .map(std::path::PathBuf::from)
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
    std::fs::create_dir_all(&downloads).map_err(|e| e.to_string())?;
    let dest = downloads.join(&p.dmg_name);

    notify(
        app,
        format!("Downloading Xenon v{}…", p.version),
    );

    let client = http_client()?;

    // Fetch and verify the checksum first (so we can bail before downloading
    // a multi-hundred-MB DMG that we can't validate).
    let expected = if let Some(url) = &p.checksums_url {
        match fetch_checksums(&client, url).await {
            Ok(sums) => expected_checksum(&sums, &p.dmg_name),
            Err(e) => {
                log::warn!("checksum fetch failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let resp = client
        .get(&p.dmg_url)
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download HTTP {}", resp.status().as_u16()));
    }

    // Stream to disk so a multi-hundred-MB DMG never sits in memory, hashing
    // incrementally and emitting progress via `xenon-notice`.
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let total = resp.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut last_pct: i64 = -1;
    let mut last_notice = std::time::Instant::now();

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download body failed: {e}"))?;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        hasher.update(&chunk);
        written += chunk.len() as u64;

        // Emit progress at most every ~5 s and only on a new 10% step.
        // `total == 0` (unknown size) → -1, so no percentage is shown.
        let pct: i64 = written
            .saturating_mul(100)
            .checked_div(total)
            .map(|p| p as i64)
            .unwrap_or(-1);
        if pct >= 0 && pct != last_pct && last_notice.elapsed().as_secs() >= 5 {
            last_pct = pct;
            last_notice = std::time::Instant::now();
            notify(
                app,
                format!(
                    "Downloading Xenon v{}… {}%",
                    p.version, pct
                ),
            );
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);

    let digest = format!("{:x}", hasher.finalize());

    if let Some(want) = &expected {
        if !want.eq_ignore_ascii_case(&digest) {
            let _ = fs::remove_file(&dest);
            return Err(format!(
                "checksum mismatch: expected {want}, got {digest}"
            ));
        }
    } else {
        log::warn!("no SHA-256 available for {}; skipping verification", p.dmg_name);
    }

    notify(
        app,
        format!(
            "Verified ({} MB). Opening — drag Xenon to Applications.",
            (written as f64 / 1_048_576.0).round() as u64
        ),
    );

    if std::env::var_os("XENON_UPDATE_NO_OPEN").is_none() {
        std::process::Command::new("open")
            .arg(&dest)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        log::info!("update harness: skipping `open` (XENON_UPDATE_NO_OPEN)");
    }

    log::info!("update DMG downloaded to {}", dest.display());
    Ok(())
}

async fn fetch_checksums(
    client: &reqwest::Client,
    url: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("checksum request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("checksum HTTP {}", resp.status().as_u16()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    Ok(parse_checksums(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex_known_vector() {
        // SHA-256("") and SHA-256("abc") — NIST / FIPS-180 vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("v1.2.3").unwrap(), Version::new(1, 2, 3));
        assert_eq!(parse_version("1.2.3").unwrap(), Version::new(1, 2, 3));
        assert_eq!(parse_version("V0.2.0").unwrap(), Version::new(0, 2, 0));
        assert!(parse_version("not-a-version").is_none());
        assert!(parse_version("").is_none());
    }

    #[test]
    fn test_is_newer_version_basic() {
        assert!(is_newer_version("0.3.0", "0.2.0"));
        assert!(is_newer_version("v0.3.0", "0.2.0"));
        assert!(is_newer_version("0.2.1", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.1.0", "0.2.0"));
        assert!(!is_newer_version("garbage", "0.2.0"));
    }

    #[test]
    fn test_is_newer_version_prerelease() {
        // A pre-release of a newer version is still newer than the current one.
        assert!(is_newer_version("0.3.0-rc.1", "0.2.0"));
        // A pre-release of the current version is *older* than the release.
        assert!(!is_newer_version("0.2.0-rc.1", "0.2.0"));
        // Final release beats its own pre-release.
        assert!(is_newer_version("0.3.0", "0.3.0-rc.1"));
        // Pre-release ordering among themselves.
        assert!(is_newer_version("0.3.0-rc.2", "0.3.0-rc.1"));
    }

    #[test]
    fn test_should_check_schedule() {
        let day = 24 * 60 * 60;
        // Never checked → check.
        assert!(should_check(1000, 0));
        // Just checked → don't.
        assert!(!should_check(1000 + 10, 1000));
        // 23 h later → don't.
        assert!(!should_check(1000 + day - 60, 1000));
        // 24 h later → yes.
        assert!(should_check(1000 + day, 1000));
        // Clock skew (last in the future) → don't.
        assert!(!should_check(1000, 2000));
    }

    #[test]
    fn test_parse_checksums() {
        let text = "\
abc123  Xenon_0.3.0_universal.dmg
def456  Xenon_0.3.0_universal.app.zip
# comment
";
        let sums = parse_checksums(text);
        assert_eq!(sums.len(), 2);
        assert_eq!(sums.get("abc123").unwrap(), "Xenon_0.3.0_universal.dmg");
    }

    #[test]
    fn test_expected_checksum() {
        let sums = parse_checksums("abc123  Xenon_0.3.0_universal.dmg\ndef456  other");
        assert_eq!(
            expected_checksum(&sums, "Xenon_0.3.0_universal.dmg"),
            Some("abc123".to_string())
        );
        // Path in the name is ignored.
        assert_eq!(
            expected_checksum(&sums, "some/dir/Xenon_0.3.0_universal.dmg"),
            Some("abc123".to_string())
        );
        assert_eq!(expected_checksum(&sums, "missing.dmg"), None);
    }

    fn release_json(tag: &str) -> Release {
        let raw = format!(
            r#"{{
              "tag_name": "{tag}",
              "name": "Xenon {tag}",
              "assets": [
                {{"name": "Xenon_0.3.0_universal.dmg", "browser_download_url": "https://example.com/Xenon_0.3.0_universal.dmg", "size": 12345}},
                {{"name": "Xenon_0.3.0_universal.app.zip", "browser_download_url": "https://example.com/Xenon_0.3.0_universal.app.zip", "size": 23456}},
                {{"name": "SHA256SUMS", "browser_download_url": "https://example.com/SHA256SUMS", "size": 128}}
              ]
            }}"#
        );
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn test_parse_release_json() {
        let r = release_json("v0.3.0");
        assert_eq!(r.tag_name, "v0.3.0");
        assert_eq!(r.assets.len(), 3);
        assert_eq!(r.name.as_deref(), Some("Xenon v0.3.0"));
    }

    #[test]
    fn test_find_dmg_prefers_universal() {
        let r = release_json("v0.3.0");
        let dmg = find_dmg(&r).unwrap();
        assert!(dmg.name.ends_with(".dmg"));
        assert!(dmg.name.contains("universal"));
    }

    #[test]
    fn test_find_dmg_fallback() {
        let raw = r#"{
            "tag_name": "v0.3.0",
            "assets": [
                {"name": "Xenon_0.3.0_aarch64.dmg", "browser_download_url": "https://example.com/a.dmg"}
            ]
        }"#;
        let r: Release = serde_json::from_str(raw).unwrap();
        let dmg = find_dmg(&r).unwrap();
        assert_eq!(dmg.name, "Xenon_0.3.0_aarch64.dmg");
    }

    #[test]
    fn test_find_checksums() {
        let r = release_json("v0.3.0");
        let sums = find_checksums(&r).unwrap();
        assert_eq!(sums.name, "SHA256SUMS");
    }

    #[test]
    fn test_status_from() {
        let p = PendingUpdate {
            version: "0.3.0".to_string(),
            dmg_name: "Xenon_0.3.0_universal.dmg".to_string(),
            dmg_url: "https://example.com/x.dmg".to_string(),
            checksums_url: Some("https://example.com/SHA256SUMS".to_string()),
        };
        let s = status_from(Some(&p), "0.2.0");
        assert!(s.available);
        assert_eq!(s.latest_version.as_deref(), Some("0.3.0"));
        assert_eq!(s.message, "Xenon v0.3.0 is available");

        let s2 = status_from(None, "0.2.0");
        assert!(!s2.available);
        assert_eq!(s2.latest_version, None);
    }

    #[test]
    fn test_release_json_ignores_unknown_fields() {
        // The API adds fields over time; our parser must not break.
        let raw = r#"{
            "tag_name": "v0.3.0",
            "draft": false,
            "prerelease": false,
            "html_url": "https://example.com",
            "assets": []
        }"#;
        let r: Release = serde_json::from_str(raw).unwrap();
        assert_eq!(r.tag_name, "v0.3.0");
        assert!(r.assets.is_empty());
    }

    /// Mock-server end-to-end: release JSON -> download -> SHA-256 verify.
    /// Uses `XENON_UPDATE_API`, `XENON_UPDATE_DOWNLOAD_DIR` and
    /// `XENON_UPDATE_NO_OPEN`; never touches the network or opens Finder.
    #[tokio::test]
    #[ignore = "spawns a local mock server; run with -- --ignored stress_updater_mock"]
    async fn stress_updater_mock() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let dmg = b"Xenon test DMG";
        let good_hash = sha256_hex(dmg);

        async fn serve(listener: TcpListener, dmg: &'static [u8], hash: String) {
            for _ in 0..6 {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let addr = listener.local_addr().unwrap();
                let (status, ctype, body): (&str, &str, Vec<u8>) = match path {
                    "/releases/latest" => {
                        let json = format!(
                            r#"{{"tag_name":"v9.9.9","assets":[
                                {{"name":"Xenon_9.9.9_universal.dmg","browser_download_url":"http://{addr}/dmg"}},
                                {{"name":"SHA256SUMS","browser_download_url":"http://{addr}/sums"}}
                            ]}}"#
                        );
                        ("200 OK", "application/json", json.into_bytes())
                    }
                    "/dmg" => ("200 OK", "application/octet-stream", dmg.to_vec()),
                    "/sums" => (
                        "200 OK",
                        "text/plain",
                        format!("{hash}  Xenon_9.9.9_universal.dmg\n").into_bytes(),
                    ),
                    _ => ("404 Not Found", "text/plain", b"no".to_vec()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dl = std::env::temp_dir().join(format!("xenon-update-good-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dl);
        std::env::set_var("XENON_UPDATE_API", format!("http://{addr}/releases/latest"));
        std::env::set_var("XENON_UPDATE_DOWNLOAD_DIR", &dl);
        std::env::set_var("XENON_UPDATE_NO_OPEN", "1");
        tokio::spawn(serve(listener, dmg, good_hash.clone()));

        let result = fetch_latest().await;
        let release = result.expect("fetch_latest").expect("release json");
        assert_eq!(release.tag_name, "v9.9.9");
        let dmg_asset = find_dmg(&release).unwrap();
        let p = PendingUpdate {
            version: release.tag_name.clone(),
            dmg_name: dmg_asset.name.clone(),
            dmg_url: dmg_asset.browser_download_url.clone(),
            checksums_url: find_checksums(&release).map(|a| a.browser_download_url.clone()),
        };
        download_and_open(None, &p).await.expect("good hash downloads");
        assert_eq!(std::fs::read(dl.join(&p.dmg_name)).unwrap(), dmg);
        eprintln!("updater mock: correct hash accepted");

        // Wrong checksum must refuse the DMG and delete it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dl = std::env::temp_dir().join(format!("xenon-update-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dl);
        std::env::set_var("XENON_UPDATE_API", format!("http://{addr}/releases/latest"));
        std::env::set_var("XENON_UPDATE_DOWNLOAD_DIR", &dl);
        tokio::spawn(serve(listener, dmg, "0".repeat(64)));

        let release = fetch_latest().await.unwrap().expect("release json");
        let dmg_asset = find_dmg(&release).unwrap();
        let p = PendingUpdate {
            version: release.tag_name.clone(),
            dmg_name: dmg_asset.name.clone(),
            dmg_url: dmg_asset.browser_download_url.clone(),
            checksums_url: find_checksums(&release).map(|a| a.browser_download_url.clone()),
        };
        let err = download_and_open(None, &p).await.unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dl.join(&p.dmg_name).exists(), "bad DMG must be removed");
        eprintln!("updater mock: wrong hash refused");
    }

    /// Live check against the real GitHub API. Ignored by default (needs
    /// network); run with `cargo test -- --ignored fetch_latest_live`.
    /// The repo has no releases today, so this must return `Ok(None)` (404) and
    /// never an error.
    #[tokio::test]
    #[ignore]
    async fn fetch_latest_live() {
        match fetch_latest().await {
            Ok(Some(r)) => {
                eprintln!("live: latest release tag = {}", r.tag_name);
            }
            Ok(None) => {
                eprintln!("live: no releases yet (404 handled gracefully)");
            }
            Err(e) => panic!("fetch_latest returned an error: {e}"),
        }
    }
}