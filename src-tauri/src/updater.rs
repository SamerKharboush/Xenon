//! In-app update check against the public GitHub Releases API.
//!
//! Flow: GET `/releases/latest` → compare semver → if newer, show a pet-bubble
//! notice and (on demand) download the universal DMG to `~/Downloads`, verify
//! its SHA-256 against `SHA256SUMS`, mount it read-only, verify the bundled
//! app, then stage it next to the running bundle and relaunch after exit.
//!
//! Check schedule: 10 s after launch if the last check was > 6 h ago, then
//! every 6 h while running (plus once whenever Settings opens). `last_update_check`
//! (unix seconds) and `skipped_version` persist in the config.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
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
/// Public Releases page, shown when a download ultimately fails.
const RELEASES_PAGE: &str = "https://github.com/SamerKharboush/Xenon/releases";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(10);
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

// --- in-place install helpers (pure parts unit-tested) ---------------------

const BUNDLE_ID: &str = "com.xenon.app";
const APPLICATIONS: &str = "/Applications";

/// Where an in-place install may write. `Fallback` keeps the old
/// download-and-open behaviour (the user drags the app themselves).
#[derive(Debug, PartialEq, Eq)]
pub enum InstallTarget {
    Install { target: PathBuf, staged: PathBuf },
    Fallback(String),
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn plist_string_on(line: &str) -> Option<String> {
    let start = line.find("<string>")?;
    let rest = &line[start + "<string>".len()..];
    let end = rest.find("</string>")?;
    Some(xml_unescape(&rest[..end]))
}

/// All `<string>` values following `<key>{key}</key>` in an XML plist.
/// Handles the one-value-per-line format emitted by `hdiutil -plist` and
/// `plutil -convert xml1 -o -`.
fn plist_strings_for(xml: &str, key: &str) -> Vec<String> {
    let needle = format!("<key>{key}</key>");
    let lines: Vec<&str> = xml.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&needle) {
            continue;
        }
        // Same line (e.g. `<key>k</key><string>v</string>`).
        if let Some(after) = line.split_once(&needle).map(|(_, s)| s) {
            if let Some(v) = plist_string_on(after) {
                out.push(v);
                continue;
            }
        }
        for next in lines.iter().skip(i + 1).take(3) {
            if let Some(v) = plist_string_on(next) {
                out.push(v);
                break;
            }
            if next.contains("<key>") {
                break;
            }
        }
    }
    out
}

/// Mount points from an `hdiutil attach -plist` blob. A DMG can carry several
/// partitions; callers pick the one containing `Xenon.app`.
pub fn parse_mount_points(plist: &str) -> Vec<String> {
    plist_strings_for(plist, "mount-point")
}

/// Verify the app's `Info.plist` XML against the release version + bundle ID.
pub fn verify_bundle_plist(xml: &str, expected_version: &str) -> Result<(), String> {
    let id = plist_strings_for(xml, "CFBundleIdentifier")
        .into_iter()
        .next()
        .ok_or("Info.plist has no CFBundleIdentifier")?;
    let version = plist_strings_for(xml, "CFBundleShortVersionString")
        .into_iter()
        .next()
        .ok_or("Info.plist has no CFBundleShortVersionString")?;
    let expected = parse_version(expected_version)
        .map(|v| v.to_string())
        .unwrap_or_else(|| expected_version.trim_start_matches(['v', 'V']).to_string());
    if id != BUNDLE_ID {
        return Err(format!("wrong bundle identifier: {id}"));
    }
    if version != expected {
        return Err(format!("wrong bundle version: {version} (expected {expected})"));
    }
    Ok(())
}

fn version_component(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// True when a directory can hold the staged copy (owner/group/other write
/// bit). A metadata failure is treated as "not writable" so we never try to
/// swap a bundle we cannot replace.
#[cfg(unix)]
pub fn app_dir_writable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| !m.permissions().readonly() && m.permissions().mode() & 0o222 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn app_dir_writable(_: &Path) -> bool {
    false
}

/// Compute the install target and the hidden staged copy next to the running
/// bundle. Only `/Applications/Xenon.app` is eligible; a relocated (e.g.
/// translocated) bundle must fall back to opening the DMG.
pub fn install_paths(exe: &Path, version: &str) -> InstallTarget {
    let Some(bundle) = exe.parent().and_then(Path::parent).and_then(Path::parent) else {
        return InstallTarget::Fallback("cannot locate the running app bundle".to_string());
    };
    if bundle.file_name().and_then(|s| s.to_str()) != Some("Xenon.app") {
        return InstallTarget::Fallback("not running from Xenon.app".to_string());
    }
    let path = bundle.to_string_lossy().to_string();
    if path.contains("AppTranslocation")
        || path.starts_with("/private/var/folders/")
        || path.starts_with("/var/folders/")
    {
        return InstallTarget::Fallback("running from a translocated path".to_string());
    }
    let Some(parent) = bundle.parent() else {
        return InstallTarget::Fallback("cannot locate the Applications folder".to_string());
    };
    if parent != Path::new(APPLICATIONS) {
        return InstallTarget::Fallback(format!("app is not in {APPLICATIONS}"));
    }
    InstallTarget::Install {
        target: bundle.to_path_buf(),
        staged: parent.join(format!(".Xenon-update-{}.app", version_component(version))),
    }
}

/// Shell-quote a path for the helper script.
#[cfg_attr(not(test), allow(dead_code))]
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Detached swap helper. `$1` = staged app, `$2` = target app, `$3` = trash
/// destination (e.g. `~/.Trash/Xenon 0.3.0 123.app`). Commands are injected so
/// the pure text can be checked and the ignored integration test can run a swap
/// without a real Trash, /Applications, or `open`.
pub fn helper_script(wait: &str, trash: &str, launch: &str, restore: &str) -> String {
    format!(
        "#!/bin/sh\n\
trap '' HUP\n\
staged=$1\ntarget=$2\ndest=$3\n\
log() {{ f=${{XENON_UPDATE_LOG:-$HOME/Library/Logs/Xenon/update.log}}; mkdir -p \"$(dirname \"$f\")\" && echo \"$1\" >> \"$f\"; }}\n\
{wait}\n\
if {trash}; then log \"trash old ok\"; else log \"trash old failed\"; {restore}; rm -rf \"$staged\" 2>/dev/null || true; {launch}; log \"relaunch old\"; exit 1; fi\n\
if mv \"$staged\" \"$target\"; then log \"move staged ok\"; else log \"move staged failed\"; {restore}; rm -rf \"$staged\" 2>/dev/null || true; {launch}; log \"relaunch old\"; exit 1; fi\n\
xattr -dr com.apple.quarantine \"$target\" 2>/dev/null || true\n\
{launch}\n\
log \"launch ok\"\n"
    )
}

fn helper_script_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("xenon-finish-update-{}-{nanos}.sh", std::process::id()))
}

/// Restore the old bundle after a failed swap, if it was moved to the trash
/// destination and the staged copy is not in place.
fn restore_old_cmd() -> &'static str {
    r#"if [ ! -e "$target" ] && [ -e "$dest" ]; then mv "$dest" "$target" || ditto "$dest" "$target"; fi"#
}

/// Commands used by the production swap helper. `$dest` is the trash path
/// (e.g. `~/.Trash/Xenon 0.3.0 123.app`) passed by Rust as `$3`.
fn production_helper_script(pid: u32, _trash_dest: &Path) -> String {
    let wait = format!("while kill -0 {pid} 2>/dev/null; do sleep 0.2; done");
    let trash = r#"if mv "$target" "$dest"; then :; else ditto "$target" "$dest" && rm -rf "$target"; fi"#;
    let launch = r#"open -a "$target" >/dev/null 2>&1"#;
    helper_script(&wait, trash, launch, restore_old_cmd())
}

#[cfg_attr(not(test), allow(dead_code))]
fn run_swap_helper(
    staged: &Path,
    target: &Path,
    trash: &str,
    wait: &str,
    launch: &str,
) -> Result<String, String> {
    let trash_dest = target.parent().unwrap_or(Path::new("/tmp")).join("old.app");
    run_swap_helper_with_dest(staged, target, &trash_dest, trash, wait, launch)
}

fn run_swap_helper_with_dest(
    staged: &Path,
    target: &Path,
    trash_dest: &Path,
    trash: &str,
    wait: &str,
    launch: &str,
) -> Result<String, String> {
    let script = write_helper_script_text(&helper_script(wait, trash, launch, restore_old_cmd()))?;
    let out = Command::new("/bin/sh")
        .arg(&script)
        .arg(staged)
        .arg(target)
        .arg(trash_dest)
        .output()
        .map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&script);
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Mounted DMG; always detached (even on an early error) by `Drop`.
struct MountedDmg {
    mount: PathBuf,
}

impl Drop for MountedDmg {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount)
            .arg("-quiet")
            .output();
    }
}

fn verify_bundle(app: &Path, version: &str) -> Result<(), String> {
    let info = app.join("Contents/Info.plist");
    let plist = Command::new("plutil")
        .args(["-convert", "xml1", "-o", "-"])
        .arg(&info)
        .output()
        .map_err(|e| format!("plutil: {e}"))?;
    if !plist.status.success() {
        return Err(format!("cannot read {}", info.display()));
    }
    verify_bundle_plist(&String::from_utf8_lossy(&plist.stdout), version)?;

    let sig = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .map_err(|e| format!("codesign: {e}"))?;
    if !sig.status.success() {
        return Err(format!(
            "codesign verification failed: {}",
            String::from_utf8_lossy(&sig.stderr).trim()
        ));
    }

    let req = Command::new("codesign")
        .args(["-dr", "-"])
        .arg(app)
        .output()
        .map_err(|e| format!("codesign -dr: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&req.stdout),
        String::from_utf8_lossy(&req.stderr)
    );
    if !req.status.success() || !text.contains("identifier \"com.xenon.app\"") {
        return Err(format!("designated requirement is not {BUNDLE_ID}: {}", text.trim()));
    }
    Ok(())
}

fn open_dmg(dmg: &Path) -> Result<(), String> {
    if std::env::var_os("XENON_UPDATE_NO_OPEN").is_none() {
        Command::new("open")
            .arg(dmg)
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        log::info!("update harness: skipping `open` (XENON_UPDATE_NO_OPEN)");
    }
    Ok(())
}

/// Downloaded + SHA-verified DMG → in-place install, or the old open-and-drag
/// fallback when the running bundle is not in writable `/Applications`.
fn install_dmg<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dmg: &Path,
    version: &str,
) -> Result<(), String> {
    notify(Some(app), "Installing…");
    let exe = std::env::current_exe().map_err(|e| format!("running bundle path: {e}"))?;
    let (target, staged) = match install_paths(&exe, version) {
        InstallTarget::Install { target, staged } => (target, staged),
        InstallTarget::Fallback(reason) => return install_fallback(app, dmg, &reason),
    };
    if !app_dir_writable(target.parent().expect("bundle has a parent")) {
        return install_fallback(app, dmg, &format!("{} is not writable", target.parent().unwrap().display()));
    }

    let result = swap_in_place(app, dmg, version, &target, &staged);
    if result.is_err() {
        // Never touch the running bundle on failure; drop only our staged copy.
        let _ = fs::remove_dir_all(&staged);
    }
    result
}

fn install_fallback<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dmg: &Path,
    reason: &str,
) -> Result<(), String> {
    log::info!("in-place update unavailable: {reason}");
    open_dmg(dmg)?;
    notify(
        Some(app),
        format!("Automatic install needs /Applications ({reason}). Drag Xenon to Applications."),
    );
    Ok(())
}

fn swap_in_place<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dmg: &Path,
    version: &str,
    target: &Path,
    staged: &Path,
) -> Result<(), String> {
    let mount = std::env::temp_dir().join(format!(
        "xenon-dmg-mount-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    let out = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-plist", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .output()
        .map_err(|e| format!("hdiutil attach: {e}"))?;
    if !out.status.success() {
        let _ = fs::remove_dir(&mount);
        return Err(format!(
            "mount failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mounted = MountedDmg { mount };

    let points = parse_mount_points(&String::from_utf8_lossy(&out.stdout));
    let app_in_dmg = points
        .iter()
        .map(|p| PathBuf::from(p).join("Xenon.app"))
        .find(|p| p.is_dir())
        .ok_or("DMG has no mounted Xenon.app")?;
    verify_bundle(&app_in_dmg, version)?;

    if staged.exists() {
        fs::remove_dir_all(staged).map_err(|e| e.to_string())?;
    }
    let ditto = Command::new("ditto")
        .arg(&app_in_dmg)
        .arg(staged)
        .output()
        .map_err(|e| format!("ditto: {e}"))?;
    if !ditto.status.success() {
        return Err(format!(
            "staging copy failed: {}",
            String::from_utf8_lossy(&ditto.stderr).trim()
        ));
    }
    drop(mounted);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let trash_dest = dirs::home_dir()
        .unwrap_or_default()
        .join(".Trash")
        .join(format!("Xenon {} {nanos}.app", version_component(CURRENT_VERSION)));

    let script = write_helper_script_text(&production_helper_script(
        std::process::id(),
        &trash_dest,
    ))?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(staged)
        .arg(target)
        .arg(&trash_dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("swap helper: {e}"))?;

    notify(Some(app), "Restarting…");
    // Give the bubble a moment to paint before the process exits.
    std::thread::sleep(Duration::from_millis(500));
    app.exit(0);
    Ok(())
}

fn write_helper_script_text(text: &str) -> Result<PathBuf, String> {
    let path = helper_script_path();
    fs::write(&path, text).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(path)
}

// --- network -----------------------------------------------------------------

fn http_timeout() -> Duration {
    std::env::var("XENON_UPDATE_HTTP_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(HTTP_TIMEOUT)
}

/// Client for the small API / SHA256SUMS requests: a 10 s *total* timeout is
/// fine there. The DMG download uses `download_client` instead — a total
/// timeout would cut a large body mid-stream (the v0.3.0 → v0.3.1 bug).
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(http_timeout())
        .build()
        .map_err(|e| e.to_string())
}

/// Knobs for the DMG download. Injectable so tests can shrink timeouts.
#[derive(Debug, Clone, Copy)]
pub struct DownloadConfig {
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Idle timeout *between chunks* (not a total deadline).
    pub read_timeout: Duration,
    /// Retries after a mid-body error (each resumes via HTTP Range).
    pub max_retries: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            max_retries: 3,
        }
    }
}

/// Client for the DMG download: no total timeout, an idle read timeout, and
/// redirects followed (GitHub assets redirect to objects.githubusercontent.com).
fn download_client(cfg: &DownloadConfig) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(cfg.connect_timeout)
        .read_timeout(cfg.read_timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
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
pub async fn check_once<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
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

    let notice = format!("Update to v{latest_tag} available");
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

/// Spawn the background scheduler: 10 s after launch if a check is due, then
/// every 6 h while the app runs.
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

/// Force a check and show the result in the pet bubble. Used by the tray
/// "Check for Updates…" item and Settings → Check now.
pub async fn check_now_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<UpdateStatus, String> {
    let config = app.state::<ArcRwLockConfig>().inner().clone();
    let state = app.state::<Arc<UpdaterState>>().inner().clone();
    let status = check_once(app, &config, &state, true).await?;
    if !status.available {
        let msg = match (status.latest_version.as_deref(), status.message.as_str()) {
            (Some(v), m) if m.contains("skipped") => format!("Xenon v{v} skipped"),
            (_, m) if m.starts_with("Up to date") => {
                format!("Xenon is up to date (v{})", status.current_version)
            }
            _ => status.message.clone(),
        };
        notify(Some(app), msg);
    }
    Ok(status)
}

/// Settings opening is a good moment to check without waiting for the timer.
pub fn check_on_settings_open<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let config = app.state::<ArcRwLockConfig>().inner().clone();
    let state = app.state::<Arc<UpdaterState>>().inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = check_once(&app, &config, &state, false).await;
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

/// "Check now" button / tray item: force a check and return a human-readable
/// result (also emitted in the bubble).
#[tauri::command]
pub async fn check_now(app: tauri::AppHandle) -> Result<UpdateStatus, String> {
    check_now_impl(&app).await
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
/// verify its SHA-256, install it in place (or open the DMG if the running
/// app is not in writable /Applications). Emits progress via `xenon-notice`.
pub async fn install_update_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
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

fn notify<R: tauri::Runtime>(app: Option<&tauri::AppHandle<R>>, msg: impl Into<String>) {
    if let Some(app) = app {
        let _ = app.emit("xenon-notice", msg.into());
    }
}

async fn download_and_open<R: tauri::Runtime>(
    app: Option<&tauri::AppHandle<R>>,
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

    // Fetch and verify the checksum first (so we can bail before downloading
    // a multi-hundred-MB DMG that we can't validate). The small API client's
    // 10 s total timeout is fine here.
    let expected = if let Some(url) = &p.checksums_url {
        match fetch_checksums(&http_client()?, url).await {
            Ok(sums) => expected_checksum(&sums, &p.dmg_name),
            Err(e) => {
                log::warn!("checksum fetch failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let cfg = DownloadConfig::default();
    download_verified(&p.dmg_url, &dest, expected.as_deref(), &cfg, app, &p.version).await?;

    log::info!(
        "update DMG downloaded to {} ({} MB)",
        dest.display(),
        file_len(&dest) / 1_048_576
    );

    // Install in place when possible. Any failure here leaves the old app
    // untouched and the DMG in ~/Downloads.
    let result = match app {
        Some(app) => install_dmg(app, &dest, &p.version),
        None => install_dmg_paths_only(&dest, &p.version),
    };
    if let Err(e) = result {
        notify(app, format!("Update failed: {e} — the DMG is in Downloads"));
        return Err(format!("{e} — the DMG is in Downloads"));
    }
    Ok(())
}

/// Used by tests (no Tauri app handle): decide install vs. open fallback only.
fn install_dmg_paths_only(dmg: &Path, version: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("running bundle path: {e}"))?;
    match install_paths(&exe, version) {
        InstallTarget::Install { .. } => Err("in-place install needs a Tauri app handle".into()),
        InstallTarget::Fallback(reason) => {
            log::info!("in-place update unavailable: {reason}");
            open_dmg(dmg)
        }
    }
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Parse the total size out of a `Content-Range: bytes 0-9/123` header.
fn parse_content_range_total(value: &str) -> Option<u64> {
    value
        .rsplit('/')
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

/// Stream `url` into `dest` (via a `.part` file), retrying up to
/// `cfg.max_retries` times. A retry resumes with `Range: bytes=<written>-`
/// when the server answers 206; if it answers 200 the partial file is
/// discarded and the download restarts. The SHA-256 is hashed from the
/// finished file and the `.part` is renamed only after it verifies.
async fn download_verified<R: tauri::Runtime>(
    url: &str,
    dest: &std::path::Path,
    expected: Option<&str>,
    cfg: &DownloadConfig,
    app: Option<&tauri::AppHandle<R>>,
    version: &str,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom, Write};

    let mut part = dest.as_os_str().to_os_string();
    part.push(".part");
    let part = std::path::PathBuf::from(part);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&part)
        .map_err(|e| e.to_string())?;

    let mut written: u64 = 0;
    let mut total: u64 = 0;
    let mut last_step: i64 = -1;
    let mut attempt: u32 = 0;

    loop {
        // A fresh client per attempt: a connection that timed out or broke
        // mid-body must not be reused from the pool.
        let client = download_client(cfg)?;
        let mut req = client.get(url);
        if written > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={written}-"));
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if attempt < cfg.max_retries {
                    attempt += 1;
                    log::warn!("download attempt {attempt} failed: {e}; resuming at {written}");
                    continue;
                }
                let _ = fs::remove_file(&part);
                return Err(format!(
                    "download failed: {e} — download manually from {RELEASES_PAGE}"
                ));
            }
        };

        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            // Resume: keep the bytes already on disk (drop any stale tail).
            file.set_len(written).map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(written)).map_err(|e| e.to_string())?;
            if let Some(v) = resp.headers().get(reqwest::header::CONTENT_RANGE) {
                if let Some(t) = v.to_str().ok().and_then(parse_content_range_total) {
                    total = t;
                }
            }
        } else if status.is_success() {
            // Server ignored the Range header: restart from scratch.
            if written > 0 {
                log::warn!("server ignored Range (HTTP {status}); restarting download");
            }
            file.set_len(0).map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            written = 0;
            last_step = -1;
            total = resp.content_length().unwrap_or(0);
        } else {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "download HTTP {status} — download manually from {RELEASES_PAGE}"
            ));
        }

        let mut failed: Option<String> = None;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    failed = Some(format!("download body failed: {e}"));
                    break;
                }
            };
            if let Err(e) = file.write_all(&chunk) {
                failed = Some(e.to_string());
                break;
            }
            written += chunk.len() as u64;

            // Progress every 10%, with MB done/total (or just MB when the
            // server didn't advertise a size).
            let pct: i64 = written
                .saturating_mul(100)
                .checked_div(total)
                .map(|p| p as i64)
                .unwrap_or(-1);
            let step: i64 = if total > 0 {
                pct / 10
            } else {
                // Unknown size: emit once per MB.
                (written / 1_048_576) as i64
            };
            if step != last_step {
                last_step = step;
                let mb = |n: u64| (n as f64 / 1_048_576.0).round() as u64;
                if total > 0 {
                    notify(
                        app,
                        format!(
                            "Downloading Xenon v{version}… {pct}% ({} MB / {} MB)",
                            mb(written),
                            mb(total)
                        ),
                    );
                } else {
                    notify(
                        app,
                        format!("Downloading Xenon v{version}… {} MB", mb(written)),
                    );
                }
            }
        }

        if let Some(e) = failed {
            if attempt < cfg.max_retries {
                attempt += 1;
                file.flush().ok();
                log::warn!("download attempt {attempt} failed: {e}; resuming at {written}");
                continue;
            }
            let _ = fs::remove_file(&part);
            return Err(format!(
                "{e} — download manually from {RELEASES_PAGE}"
            ));
        }

        file.flush().map_err(|e| e.to_string())?;
        break;
    }
    drop(file);

    notify(app, "Verifying…");

    // Hash the finished file (works for both fresh and resumed downloads).
    let digest = {
        use std::io::Read;
        let mut f = std::fs::File::open(&part).map_err(|e| e.to_string())?;
        let mut h = Sha256::new();
        let mut buf = [0u8; 128 * 1024];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        format!("{:x}", h.finalize())
    };

    if let Some(want) = expected {
        if !want.eq_ignore_ascii_case(&digest) {
            let _ = fs::remove_file(&part);
            return Err(format!("checksum mismatch: expected {want}, got {digest}"));
        }
    } else {
        log::warn!("no SHA-256 available for {}; skipping verification", dest.display());
    }

    fs::rename(&part, dest).map_err(|e| e.to_string())?;
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
        let six_hours = 6 * 60 * 60;
        // Never checked → check.
        assert!(should_check(1000, 0));
        // Just checked → don't.
        assert!(!should_check(1000 + 10, 1000));
        // 5 h later → don't.
        assert!(!should_check(1000 + six_hours - 60, 1000));
        // 6 h later → yes.
        assert!(should_check(1000 + six_hours, 1000));
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
        download_and_open::<tauri::Wry>(None, &p).await.expect("good hash downloads");
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
        let err = download_and_open::<tauri::Wry>(None, &p).await.unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dl.join(&p.dmg_name).exists(), "bad DMG must be removed");
        eprintln!("updater mock: wrong hash refused");
    }

    fn test_body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    fn response_head(status: &str, len: usize, extra: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\n{extra}Connection: close\r\n\r\n")
    }

    fn dl_config(read_ms: u64, retries: u32) -> DownloadConfig {
        DownloadConfig {
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_millis(read_ms),
            max_retries: retries,
        }
    }

    /// (a) A slow stream (total time > the 10 s API timeout in production)
    /// completes: the download client has no total deadline.
    #[tokio::test]
    async fn download_slow_stream_completes() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let body = test_body(8192);
        let hash = sha256_hex(&body);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body2 = body.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut sock).await;
            let head = response_head("200 OK", body2.len(), "Content-Type: application/octet-stream\r\n");
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = (body2.len() / 8).max(1);
            for piece in body2.chunks(chunk) {
                sock.write_all(piece).await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });

        let dir = std::env::temp_dir().join(format!("xenon-dl-slow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("slow.dmg");
        let cfg = dl_config(5000, 0);
        download_verified::<tauri::Wry>(
            &format!("http://{addr}/dmg"),
            &dest,
            Some(&hash),
            &cfg,
            None,
            "9.9.9",
        )
        .await
        .expect("slow stream should complete");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (b) A connection dropped mid-body resumes with `Range` and the final
    /// file's SHA-256 still verifies.
    #[tokio::test]
    async fn download_resumes_after_drop() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let body = test_body(8192);
        let hash = sha256_hex(&body);
        let split = body.len() / 2;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body2 = body.clone();
        tokio::spawn(async move {
            // First connection: send half, then drop.
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut sock).await;
            let head = response_head("200 OK", body2.len(), "Content-Type: application/octet-stream\r\n");
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body2[..split]).await.unwrap();
            sock.flush().await.unwrap();
            drop(sock);

            // Second connection: must carry a Range header; answer 206.
            let (mut sock, _) = listener.accept().await.unwrap();
            let req = read_request(&mut sock).await;
            let start = req
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("range: bytes=")
                        .map(str::to_string)
                })
                .and_then(|v| v.split('-').next().map(str::to_string))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(|| panic!("no Range header on resume: {req}"));
            let head = response_head(
                "206 Partial Content",
                body2.len() - start,
                &format!("Content-Range: bytes {start}-{}/{}\r\n", body2.len() - 1, body2.len()),
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body2[start..]).await.unwrap();
            sock.flush().await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!("xenon-dl-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("resume.dmg");
        let cfg = dl_config(2000, 1);
        download_verified::<tauri::Wry>(
            &format!("http://{addr}/dmg"),
            &dest,
            Some(&hash),
            &cfg,
            None,
            "9.9.9",
        )
        .await
        .expect("resume should verify");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (c) A server that ignores `Range` (answers 200) causes a restart and
    /// the final file still verifies.
    #[tokio::test]
    async fn download_restarts_when_range_ignored() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let body = test_body(8192);
        let hash = sha256_hex(&body);
        let split = body.len() / 2;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body2 = body.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut sock).await;
            let head = response_head("200 OK", body2.len(), "Content-Type: application/octet-stream\r\n");
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body2[..split]).await.unwrap();
            sock.flush().await.unwrap();
            drop(sock);

            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut sock).await;
            // Ignore Range: full 200 response.
            let head = response_head("200 OK", body2.len(), "Content-Type: application/octet-stream\r\n");
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body2).await.unwrap();
            sock.flush().await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!("xenon-dl-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("restart.dmg");
        let cfg = dl_config(2000, 1);
        download_verified::<tauri::Wry>(
            &format!("http://{addr}/dmg"),
            &dest,
            Some(&hash),
            &cfg,
            None,
            "9.9.9",
        )
        .await
        .expect("restart should verify");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (d) An idle stall longer than the read timeout retries, then fails with
    /// the manual-download hint.
    #[tokio::test]
    async fn download_stall_retries_then_fails_with_hint() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let body = test_body(8192);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let _ = read_request(&mut sock).await;
                let head = response_head("200 OK", body.len(), "Content-Type: application/octet-stream\r\n");
                sock.write_all(head.as_bytes()).await.unwrap();
                sock.write_all(&body[..1000]).await.unwrap();
                sock.flush().await.unwrap();
                // Stall past the client's read timeout.
                tokio::time::sleep(Duration::from_millis(600)).await;
                drop(sock);
            }
        });

        let dir = std::env::temp_dir().join(format!("xenon-dl-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("stall.dmg");
        let cfg = dl_config(150, 1);
        let err = download_verified::<tauri::Wry>(
            &format!("http://{addr}/dmg"),
            &dest,
            None,
            &cfg,
            None,
            "9.9.9",
        )
        .await
        .expect_err("stalled download must fail");
        assert!(err.contains("download manually"), "{err}");
        assert!(err.contains("releases"), "{err}");
        assert!(!dest.exists(), "failed download must not leave a final file");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Live download of the latest real release DMG into a temp dir. Set
    /// `XENON_UPDATE_NO_OPEN=1`; run with
    /// `cargo test -- --ignored download_live -- --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn download_live() {
        let release = fetch_latest().await.expect("fetch").expect("release");
        let dmg = find_dmg(&release).expect("dmg asset");
        let p = PendingUpdate {
            version: release.tag_name.clone(),
            dmg_name: dmg.name.clone(),
            dmg_url: dmg.browser_download_url.clone(),
            checksums_url: find_checksums(&release).map(|a| a.browser_download_url.clone()),
        };
        let dir = std::env::temp_dir().join(format!("xenon-dl-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XENON_UPDATE_DOWNLOAD_DIR", &dir);
        std::env::set_var("XENON_UPDATE_NO_OPEN", "1");

        let started = std::time::Instant::now();
        download_and_open::<tauri::Wry>(None, &p).await.expect("live download");
        eprintln!(
            "live: downloaded {} in {:?} ({} MB), SHA-256 verified",
            p.dmg_name,
            started.elapsed(),
            file_len(&dir.join(&p.dmg_name)) / 1_048_576
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_mount_points() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>system-entities</key>
<array></array>
<key>mount-point</key>
<string>/private/tmp/xenon-dmg-123</string>
<key>partition-name</key>
<string>Xenon</string>
<key>mount-point</key>
<string>/private/tmp/xenon-dmg-456</string>
</dict></plist>"#;
        assert_eq!(
            parse_mount_points(plist),
            vec![
                "/private/tmp/xenon-dmg-123".to_string(),
                "/private/tmp/xenon-dmg-456".to_string()
            ]
        );
    }

    #[test]
    fn test_verify_bundle_plist() {
        let good = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key>
<string>com.xenon.app</string>
<key>CFBundleShortVersionString</key>
<string>0.3.4</string>
</dict></plist>"#;
        verify_bundle_plist(good, "v0.3.4").unwrap();

        let wrong_id = good.replace("com.xenon.app", "com.example.app");
        assert!(verify_bundle_plist(&wrong_id, "0.3.4").unwrap_err().contains("bundle identifier"));

        let wrong_version = good.replace("0.3.4", "0.3.3");
        assert!(verify_bundle_plist(&wrong_version, "0.3.4").unwrap_err().contains("bundle version"));

        let missing = r#"<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.xenon.app</string></dict></plist>"#;
        assert!(verify_bundle_plist(missing, "0.3.4").is_err());
    }

    #[test]
    fn test_install_paths() {
        let exe = Path::new("/Applications/Xenon.app/Contents/MacOS/xenon");
        match install_paths(exe, "0.3.4") {
            InstallTarget::Install { target, staged } => {
                assert_eq!(target, PathBuf::from("/Applications/Xenon.app"));
                assert_eq!(staged, PathBuf::from("/Applications/.Xenon-update-0.3.4.app"));
            }
            other => panic!("expected in-place install, got {other:?}"),
        }
        // A leading v is kept in the staging name; odd characters are replaced.
        match install_paths(exe, "v0.3.4+build 2") {
            InstallTarget::Install { staged, .. } => {
                assert_eq!(staged, PathBuf::from("/Applications/.Xenon-update-v0.3.4_build_2.app"));
            }
            other => panic!("expected in-place install, got {other:?}"),
        }
    }

    #[test]
    fn test_install_paths_fallbacks() {
        let translocated =
            Path::new("/private/var/folders/ab/AppTranslocation/xy/d/Xenon.app/Contents/MacOS/xenon");
        match install_paths(translocated, "0.3.4") {
            InstallTarget::Fallback(reason) => assert!(reason.contains("translocated"), "{reason}"),
            other => panic!("expected translocated fallback, got {other:?}"),
        }

        let elsewhere = Path::new("/Users/me/Applications/Xenon.app/Contents/MacOS/xenon");
        match install_paths(elsewhere, "0.3.4") {
            InstallTarget::Fallback(reason) => assert!(reason.contains("not in /Applications"), "{reason}"),
            other => panic!("expected /Applications fallback, got {other:?}"),
        }
    }

    #[test]
    fn test_helper_script() {
        let wait = "while kill -0 42 2>/dev/null; do sleep 0.2; done";
        let trash = r#"mv "$target" "$dest""#;
        let launch = r#"open -a "$target""#;
        let restore = r#"if [ ! -e "$target" ] && [ -e "$dest" ]; then mv "$dest" "$target" || ditto "$dest" "$target"; fi"#;
        let script = helper_script(wait, trash, launch, restore);
        assert!(script.contains("staged=$1"));
        assert!(script.contains("target=$2"));
        assert!(script.contains("dest=$3"));
        assert!(script.contains("log()"));
        assert!(script.contains("Library/Logs/Xenon/update.log"));
        assert!(script.contains(wait));
        assert!(script.contains(trash));
        assert!(script.contains(r#"mv "$staged" "$target""#));
        assert!(script.contains("xattr -dr com.apple.quarantine"));
        assert!(script.contains(launch));
        assert!(script.contains(restore));
        // Order: in the success path trash old app, move staged into place,
        // strip quarantine, then relaunch. The restore branch (before relaunch
        // on failure) also contains `launch`, so find the success-path launch.
        let trash_at = script.find("log \"trash old ok\"").unwrap();
        let move_at = script.find("log \"move staged ok\"").unwrap();
        let xattr_at = script.find("xattr -dr").unwrap();
        let launch_ok_at = script.find("log \"launch ok\"").unwrap();
        let launch_at = script[..launch_ok_at].rfind(launch).unwrap();
        assert!(trash_at < move_at && move_at < xattr_at && xattr_at < launch_at);
    }

    #[test]
    fn test_sh_quote() {
        assert_eq!(sh_quote("/Applications/Xenon.app"), "'/Applications/Xenon.app'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    /// Swap helper end-to-end against fake app dirs (no /Applications, no
    /// real Trash, no real relaunch). Injects a `launch` command that appends
    /// to a marker file so the test can verify restore/relaunch calls.
    /// Run with:
    /// `cargo test -- --ignored swap_helper_installs_staged_copy`
    #[test]
    #[ignore = "runs the swap helper against a temp target; no /Applications or Trash"]
    fn swap_helper_installs_staged_copy() {
        let root = std::env::temp_dir().join(format!("xenon-swap-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // Success: trash moves old away, staged moves into place.
        let staged = root.join("staged.app");
        let target = root.join("target.app");
        let trash = root.join("old.app");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(staged.join("Info.plist"), b"new").unwrap();
        fs::write(target.join("Info.plist"), b"old").unwrap();
        let trash_cmd = format!("mv \"$target\" {}", sh_quote(trash.to_str().unwrap()));
        run_swap_helper(&staged, &target, &trash_cmd, ":", ":").unwrap();
        assert_eq!(fs::read(target.join("Info.plist")).unwrap(), b"new");
        assert_eq!(fs::read(trash.join("Info.plist")).unwrap(), b"old");
        assert!(!staged.exists(), "staged copy should be renamed away");

        // Trash failure: helper restores old app and calls launch.
        let staged = root.join("staged2.app");
        let target = root.join("target2.app");
        let trash = root.join("old2.app");
        let launch_marker = root.join("launch2.txt");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(staged.join("Info.plist"), b"new2").unwrap();
        fs::write(target.join("Info.plist"), b"old2").unwrap();
        let trash_cmd = format!("mv \"$target\" {}", sh_quote(trash.to_str().unwrap()));
        // Make the trash destination path invalid (file in the way).
        fs::write(&trash, b"blocker").unwrap();
        let launch = format!(": > {} && echo called >> {}", sh_quote(launch_marker.to_str().unwrap()), sh_quote(launch_marker.to_str().unwrap()));
        let result = run_swap_helper_with_dest(&staged, &target, &trash, &trash_cmd, ":", &launch);
        assert!(result.is_err(), "trash failure should make helper exit non-zero");
        assert_eq!(fs::read(target.join("Info.plist")).unwrap(), b"old2");
        assert!(launch_marker.exists(), "launch should have been called");

        // Move-staged failure: helper restores old app and calls launch.
        let staged = root.join("staged3.app");
        let target = root.join("target3.app");
        let trash = root.join("old3.app");
        let launch_marker = root.join("launch3.txt");
        // staged does not exist, so `mv "$staged" "$target"` fails after the
        // old app was trashed; restore + relaunch must run.
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("Info.plist"), b"old3").unwrap();
        let trash_cmd = format!("mv \"$target\" {}", sh_quote(trash.to_str().unwrap()));
        let launch = format!(": > {} && echo called >> {}", sh_quote(launch_marker.to_str().unwrap()), sh_quote(launch_marker.to_str().unwrap()));
        // Use `run_swap_helper_with_dest` so `$3` (trash dest) is the path the
        // test's injected trash command expects; `run_swap_helper` derives it
        // as `<target>.parent/old.app`.
        let result = run_swap_helper_with_dest(&staged, &target, &trash, &trash_cmd, ":", &launch);
        assert!(result.is_err(), "missing staged copy should make helper exit non-zero");
        assert_eq!(fs::read(target.join("Info.plist")).unwrap(), b"old3");
        assert!(launch_marker.exists(), "launch should have been called");

        let _ = fs::remove_dir_all(&root);
    }
}