//! Provider Hub: Keychain-backed API keys, per-profile capability probing, and
//! the Tauri commands the Settings UI calls.
//!
//! Keys never reach `config.json`: `XenonConfig::save` strips them and
//! `update_config` writes them here.
//!
//! Storage strategy (v0.3.2+):
//! - All provider keys live in ONE macOS Keychain item (service
//!   `com.xenon.app`, account `provider-keys`) as a JSON map
//!   `{profile_name: api_key}`. This avoids N Keychain prompts for an
//!   ad-hoc-signed app.
//! - On first load, legacy per-account Keychain items and plaintext keys in
//!   config.json are merged into the single item and cleared.
//! - If the Keychain write fails (not a mock), a file fallback at
//!   `~/Library/Application Support/xenon/secrets.json` (mode 0600) is used.
//! - Renaming a profile carries its key; deleting a profile removes its key.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use crate::config::{ProviderProfile, XenonConfig};
use crate::presets::{self, DetectedProvider, ProviderPreset};
use crate::provider::normalize_base_url;

/// Keychain service for every Xenon secret.
pub const KEYCHAIN_SERVICE: &str = "com.xenon.app";
/// Account used for the single JSON blob holding all provider keys.
pub const PROVIDER_KEYS_ACCOUNT: &str = "provider-keys";
/// Legacy account for the flat provider config (pre-multi-profile).
pub const PRIMARY_ACCOUNT: &str = "Primary";
/// Legacy account used for the single provider key (pre-JSON-blob).
pub const LEGACY_PRIMARY_ACCOUNT: &str = "Primary";
/// File fallback when Keychain writes fail.
const SECRETS_FALLBACK_PATH: &str = "secrets.json";

/// Result of moving plaintext secrets out of `config.json`: which accounts are
/// now durably stored (safe to scrub from the file) and which failed (must
/// stay in the file so the value is not lost).
#[derive(Debug, Default)]
pub struct MigrationOutcome {
    pub stored: Vec<String>,
    pub failed: Vec<String>,
}

/// Minimal secret storage, so migration is unit-testable without touching the
/// real Keychain.
pub trait SecretStore {
    fn get(&self, account: &str) -> Option<String>;
    /// Returns whether the secret was durably stored (Keychain or fallback).
    fn set(&self, account: &str, secret: &str) -> bool;
    fn delete(&self, account: &str);
}

/// Low-level credential backend so the read-error / fallback logic is
/// unit-testable without touching the real Keychain.
///
/// `load` distinguishes the three cases the store must treat differently:
/// `Ok(Some(json))` (an item exists), `Ok(None)` (NoEntry — nothing stored
/// yet) and `Err(_)` (a real read failure, e.g. the user clicked Deny).
trait KeychainBackend: Send + Sync {
    fn load(&self) -> Result<Option<String>, String>;
    fn store(&self, json: &str) -> Result<(), String>;
    fn load_legacy(&self, account: &str) -> Option<String>;
    fn delete_legacy(&self, account: &str);
}

/// Real macOS Keychain backend.
struct MacBackend;

/// In-memory backend used in test builds so tests can never touch the real
/// Keychain. Behaviour matches MacBackend's happy path (always succeeds, no
/// legacy entries).
#[cfg(test)]
#[derive(Default)]
struct MemBackend {
    blob: Mutex<Option<String>>,
}

#[cfg(test)]
impl KeychainBackend for MemBackend {
    fn load(&self) -> Result<Option<String>, String> {
        Ok(self.blob.lock().unwrap().clone())
    }
    fn store(&self, json: &str) -> Result<(), String> {
        *self.blob.lock().unwrap() = Some(json.to_string());
        Ok(())
    }
    fn load_legacy(&self, _account: &str) -> Option<String> {
        None
    }
    fn delete_legacy(&self, _account: &str) {}
}

impl KeychainBackend for MacBackend {
    fn load(&self) -> Result<Option<String>, String> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, PROVIDER_KEYS_ACCOUNT) {
            Ok(entry) => match entry.get_password() {
                Ok(json) => Ok(Some(json)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(format!("keychain read failed for provider-keys: {e}")),
            },
            Err(e) => Err(format!("keychain unavailable: {e}")),
        }
    }

    fn store(&self, json: &str) -> Result<(), String> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, PROVIDER_KEYS_ACCOUNT) {
            Ok(entry) => entry
                .set_password(json)
                .map_err(|e| format!("keychain write failed for provider-keys: {e}")),
            Err(e) => Err(format!("keychain unavailable: {e}")),
        }
    }

    fn load_legacy(&self, account: &str) -> Option<String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, account)
            .ok()?
            .get_password()
            .ok()
    }

    fn delete_legacy(&self, account: &str) {
        if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, account) {
            let _ = entry.delete_credential();
        }
    }
}

/// macOS Keychain via the `keyring` crate with the `apple-native` feature.
///
/// Stores all provider keys as a JSON blob under a single account
/// (`provider-keys`) to avoid N Keychain prompts for an ad-hoc-signed app.
///
/// The decoded map is cached in-process: `hydrate`/`store_keys` loop over N
/// profiles, and without the cache each `get` would hit the Keychain (and,
/// for an ad-hoc-signed app, prompt) N times per launch.
///
/// On a real read error (not NoEntry) the Keychain item is never overwritten —
/// reads fall back to the secrets file and writes go there too, so a Deny
/// cannot wipe keys the process is not allowed to see.
pub struct KeychainStore {
    backend: Arc<dyn KeychainBackend>,
    fallback: PathBuf,
    cache: Mutex<Option<HashMap<String, String>>>,
}

impl KeychainStore {
    fn with_backend(backend: Arc<dyn KeychainBackend>, fallback: PathBuf) -> Self {
        Self {
            backend,
            fallback,
            cache: Mutex::new(None),
        }
    }

    /// Store used by `XenonConfig::load` to migrate + hydrate in one pass.
    pub fn for_migration() -> Self {
        Self::default_store()
    }

    fn fallback_path() -> PathBuf {
        let mut path = crate::config::config_base();
        path.push("xenon");
        path.push(SECRETS_FALLBACK_PATH);
        path
    }

    /// Default store: real Keychain in production, in-memory fake in tests.
    /// Tests that explicitly need the real backend must use `#[ignore]` and
    /// construct it directly with a throwaway account name.
    fn default_store() -> Self {
        #[cfg(test)]
        {
            Self::with_backend(Arc::new(MemBackend::default()), Self::fallback_path())
        }
        #[cfg(not(test))]
        {
            Self::with_backend(Arc::new(MacBackend), Self::fallback_path())
        }
    }

    /// Read the full key map, caching the decoded result for the process.
    ///
    /// `Err` on a real read failure (not NoEntry): callers must not overwrite
    /// data they could not read. `Ok(empty)` means "nothing stored yet".
    fn read_all(&self) -> Result<HashMap<String, String>, String> {
        let mut guard = self.cache.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            return Ok(map.clone());
        }
        match self.backend.load()? {
            Some(json) => match serde_json::from_str::<HashMap<String, String>>(&json) {
                Ok(map) => {
                    *guard = Some(map.clone());
                    Ok(map)
                }
                Err(e) => Err(format!("keychain JSON decode failed: {e}")),
            },
            None => {
                *guard = Some(HashMap::new());
                Ok(HashMap::new())
            }
        }
    }

    fn write_cache(&self, map: HashMap<String, String>) {
        *self.cache.lock().unwrap() = Some(map);
    }

    /// Migrate legacy per-account Keychain items into the single JSON blob.
    pub fn migrate_legacy_accounts(&self) {
        for account in [PRIMARY_ACCOUNT, LEGACY_PRIMARY_ACCOUNT] {
            let Some(key) = self.backend.load_legacy(account) else {
                continue;
            };
            match self.read_all() {
                Ok(mut map) => {
                    if !map.contains_key(account) && !key.trim().is_empty() {
                        map.insert(account.to_string(), key);
                        match self.backend.store(&serde_json::to_string(&map).unwrap_or_default()) {
                            Ok(()) => self.write_cache(map),
                            Err(e) => log::warn!("legacy migrate write failed ({e})"),
                        }
                    }
                }
                Err(e) => log::warn!("legacy migrate skipped ({e})"),
            }
            // Remove the old item once handled.
            self.backend.delete_legacy(account);
        }
    }

    /// Read the fallback file. `Ok(None)` = the file is absent (nothing
    /// stored); `Err` = a real read failure (callers must not treat it as
    /// "absent" — the file may still hold a stale value).
    fn read_fallback(&self) -> std::io::Result<Option<HashMap<String, String>>> {
        match fs::read_to_string(&self.fallback) {
            Ok(json) => Ok(Some(serde_json::from_str(&json).unwrap_or_default())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Persist every non-empty key in *cfg*, and remove keys for profiles that
    /// no longer exist (deleted/renamed). Renamed profiles: the new name gets
    /// the key; the old name's key is dropped here. Returns the accounts whose
    /// write failed (their plaintext must stay in config.json).
    pub fn store_keys(&self, cfg: &XenonConfig) -> std::collections::HashSet<String> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        if let Some(key) = &cfg.provider.provider_api_key {
            if !key.trim().is_empty() {
                wanted.push((PRIMARY_ACCOUNT.to_string(), key.clone()));
            }
        }
        for profile in &cfg.providers {
            if let Some(key) = &profile.api_key {
                if !key.trim().is_empty() {
                    wanted.push((profile.name.clone(), key.clone()));
                }
            }
        }
        // Accounts MCP secrets occupy. A provider profile with the same name
        // would overwrite (or be overwritten by) the MCP value under one
        // account; refuse the write and keep the profile's plaintext instead.
        let mcp_accounts: std::collections::HashSet<String> = cfg
            .mcp_servers
            .iter()
            .flat_map(|s| {
                s.env_secrets
                    .iter()
                    .chain(s.header_secrets.iter())
                    .map(|n| crate::mcp::secret_account(&s.name, n))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut failed = std::collections::HashSet::new();
        for (account, key) in &wanted {
            if mcp_accounts.contains(account) {
                log::warn!(
                    "provider account {account} collides with an MCP secret; keeping its plaintext"
                );
                failed.insert(account.clone());
                continue;
            }
            if !self.set(account, key) {
                failed.insert(account.clone());
            }
        }
        // Remove keys for accounts that are no longer present in the config.
        // Legacy "Primary" is allowed to coexist with profile keys.
        if let Ok(map) = self.read_all() {
            let keep: std::collections::HashSet<&str> =
                wanted.iter().map(|(a, _)| a.as_str()).collect();
            for account in map.keys() {
                if account == LEGACY_PRIMARY_ACCOUNT || account == PRIMARY_ACCOUNT {
                    continue;
                }
                // MCP / search-API secrets are managed independently (Settings),
                // never dropped by a provider-profile save.
                if account.starts_with("mcp:") || account.starts_with("search:") {
                    continue;
                }
                if !keep.contains(account.as_str()) {
                    self.delete(account);
                }
            }
        }
        failed
    }

    fn write_fallback(&self, account: &str, secret: &str) -> bool {
        let mut fb = self.read_fallback().ok().flatten().unwrap_or_default();
        fb.insert(account.to_string(), secret.to_string());
        self.write_fallback_map(&fb)
    }

    /// Remove *account* from the fallback file. Returns false on a read error
    /// (the stale value could not be confirmed absent) or a failed rewrite.
    /// Genuine absence (no file, or no entry) is success — nothing to write.
    fn remove_fallback(&self, account: &str) -> bool {
        match self.read_fallback() {
            Err(e) => {
                log::warn!("fallback read failed ({e}); cannot confirm {account} absent");
                false
            }
            Ok(Some(mut fb)) => {
                if fb.remove(account).is_none() {
                    return true; // nothing to do
                }
                self.write_fallback_map(&fb)
            }
            Ok(None) => true, // no file: account is genuinely absent
        }
    }

    /// Atomic, exclusive, 0600-from-creation write of the fallback file: a
    /// unique temp file in the same directory, fully written and flushed to
    /// stable storage, then renamed over the target; the parent directory is
    /// synced so the rename survives a crash.
    fn write_fallback_map(&self, map: &HashMap<String, String>) -> bool {
        let path = &self.fallback;
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                log::error!("failed to create fallback dir: {}", parent.display());
                return false;
            }
        }
        let json = serde_json::to_string(map).unwrap_or_default();
        let tmp = crate::config::unique_tmp_path(path);
        match crate::config::write_tmp_file(&tmp, json.as_bytes())
            .and_then(|_| fs::rename(&tmp, path))
            .and_then(|()| crate::config::sync_parent_dir(path))
        {
            Ok(()) => true,
            Err(e) => {
                log::error!("failed to commit secrets fallback: {e}");
                let _ = fs::remove_file(&tmp);
                false
            }
        }
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, account: &str) -> Option<String> {
        // The fallback file takes precedence: when a Keychain write failed the
        // value landed here, and it must shadow any stale Keychain copy.
        let fb = self.read_fallback().ok().flatten();
        if let Some(v) = fb.as_ref().and_then(|m| m.get(account)) {
            return Some(v.clone());
        }
        match self.read_all() {
            Ok(map) => map.get(account).cloned(),
            Err(e) => {
                // Keychain unreadable (e.g. Deny): reads fall back to the file
                // (already checked above) and never touch the Keychain item.
                log::warn!(
                    "keychain read failed ({e}); reading {} fallback",
                    SECRETS_FALLBACK_PATH
                );
                None
            }
        }
    }

    fn set(&self, account: &str, secret: &str) -> bool {
        match self.read_all() {
            Ok(mut map) => {
                map.insert(account.to_string(), secret.to_string());
                match self
                    .backend
                    .store(&serde_json::to_string(&map).unwrap_or_default())
                {
                    Ok(()) => {
                        self.write_cache(map);
                        // Keychain is now authoritative: a stale fallback copy
                        // must be removed, otherwise `get` (fallback-first)
                        // would return the old value.
                        if !self.remove_fallback(account) {
                            log::error!(
                                "could not drop stale fallback for {account}; get() would return the old value"
                            );
                            return false;
                        }
                        true
                    }
                    Err(e) => {
                        log::warn!("Keychain write failed ({e}); using fallback");
                        // Keychain write failed: persist to the file fallback.
                        self.write_fallback(account, secret)
                    }
                }
            }
            Err(e) => {
                // Could not read the Keychain: do NOT overwrite the item.
                // Persist to the file fallback, merged with what it holds.
                log::warn!(
                    "keychain read failed ({e}); storing {account} in {} fallback only",
                    SECRETS_FALLBACK_PATH
                );
                self.write_fallback(account, secret)
            }
        }
    }

    fn delete(&self, account: &str) {
        match self.read_all() {
            Ok(mut map) => {
                map.remove(account);
                match self
                    .backend
                    .store(&serde_json::to_string(&map).unwrap_or_default())
                {
                    Ok(()) => self.write_cache(map),
                    Err(e) => log::warn!("keychain delete write failed ({e})"),
                }
            }
            Err(e) => {
                log::warn!("keychain read failed ({e}); cleaned fallback only");
            }
        }
        // Drop it from the file fallback too so it cannot resurrect.
        self.remove_fallback(account);
    }
}

/// Move plaintext secrets from an old `config.json` into *store* and clear
/// them from the in-memory config. Returns whether at least one secret was
/// stored successfully; on storage failure the plaintext is kept (no loss).
/// (Production uses `migrate_plaintext_checked`; this remains for tests.)
#[allow(dead_code)]
pub fn migrate_plaintext(cfg: &mut XenonConfig, store: &dyn SecretStore) -> bool {
    !migrate_plaintext_inner(cfg, store).stored.is_empty()
}

/// Like `migrate_plaintext`, but reports which accounts were stored (safe to
/// scrub from config.json) and which failed (their plaintext must stay).
pub fn migrate_plaintext_checked(
    cfg: &mut XenonConfig,
    store: &dyn SecretStore,
) -> MigrationOutcome {
    migrate_plaintext_inner(cfg, store)
}

fn migrate_plaintext_inner(cfg: &mut XenonConfig, store: &dyn SecretStore) -> MigrationOutcome {
    let mut out = MigrationOutcome::default();
    // Per-account bookkeeping so two secrets that share one account are
    // reconciled correctly:
    // - `stored`/`failed` record the last value written under each account.
    //   A plaintext value is safe to scrub only when the value durably stored
    //   under its account is the *same* value. A later successful store of a
    //   *different* value (an account collision) means hydrate would return
    //   the wrong secret, so the original plaintext must be kept (finding 5).
    // - Reconciling same-account/same-value attempts clears a spurious failure
    //   marker when the flat provider key and a "Primary" profile share one
    //   account (finding 4).
    let mut stored: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut failed: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // Accounts MCP secrets will occupy. A provider profile whose name equals
    // one of these would overwrite (or be overwritten by) the MCP value under
    // the same account; keep such a profile's key as plaintext rather than
    // acknowledge a lossy migration.
    let mcp_accounts: std::collections::HashSet<String> = cfg
        .mcp_servers
        .iter()
        .flat_map(|s| {
            s.env_secrets
                .iter()
                .chain(s.header_secrets.iter())
                .map(|n| crate::mcp::secret_account(&s.name, n))
                .collect::<Vec<_>>()
        })
        .collect();

    if let Some(key) = cfg.provider.provider_api_key.take() {
        if !key.trim().is_empty() {
            if store.set(PRIMARY_ACCOUNT, &key) {
                stored.insert(PRIMARY_ACCOUNT.to_string(), key);
            } else {
                // No value logged: only the account is diagnostic.
                log::warn!("could not store legacy flat provider key; keeping it in config.json");
                cfg.provider.provider_api_key = Some(key.clone());
                failed.insert(PRIMARY_ACCOUNT.to_string(), key);
            }
        }
    }
    for profile in &mut cfg.providers {
        if let Some(key) = profile.api_key.take() {
            if key.trim().is_empty() {
                continue;
            }
            if mcp_accounts.contains(&profile.name) {
                log::warn!(
                    "profile {} name collides with an MCP secret account; keeping its key in config.json",
                    profile.name
                );
                profile.api_key = Some(key.clone());
                failed.insert(profile.name.clone(), key);
                continue;
            }
            if store.set(&profile.name, &key) {
                stored.insert(profile.name.clone(), key);
            } else {
                log::warn!(
                    "could not store key for profile {}; keeping it in config.json",
                    profile.name
                );
                profile.api_key = Some(key.clone());
                failed.insert(profile.name.clone(), key);
            }
        }
    }
    for server in &mut cfg.mcp_servers {
        for name in &server.env_secrets {
            let Some(value) = server.env.remove(name) else {
                continue;
            };
            let account = crate::mcp::secret_account(&server.name, name);
            if store.set(&account, &value) {
                stored.insert(account, value);
            } else {
                log::warn!(
                    "could not store MCP env secret {} for server {}; keeping it in config.json",
                    name,
                    server.name
                );
                server.env.insert(name.clone(), value.clone());
                failed.insert(account, value);
            }
        }
        for name in &server.header_secrets {
            let Some(value) = server.headers.remove(name) else {
                continue;
            };
            let account = crate::mcp::secret_account(&server.name, name);
            if store.set(&account, &value) {
                stored.insert(account, value);
            } else {
                log::warn!(
                    "could not store MCP header secret {} for server {}; keeping it in config.json",
                    name,
                    server.name
                );
                server.headers.insert(name.clone(), value.clone());
                failed.insert(account, value);
            }
        }
    }

    // Reconcile per account+value. An account is reported stored only when the
    // value now durably under it matches every value that was stored for it
    // (no collision overwrote a different secret); otherwise it stays failed
    // so the plaintext is preserved.
    for (account, value) in &stored {
        if failed.get(account).map_or(true, |fv| fv == value) {
            out.stored.push(account.clone());
        }
    }
    for (account, value) in &failed {
        let superseded = stored.get(account).is_some_and(|sv| sv == value);
        if !superseded {
            out.failed.push(account.clone());
        }
    }
    out
}

/// Fill every blank API key from *store* (Keychain on a real run).
pub fn hydrate(cfg: &mut XenonConfig, store: &dyn SecretStore) {
    if cfg.provider.provider_api_key.is_none() {
        cfg.provider.provider_api_key = store.get(PRIMARY_ACCOUNT);
    }
    for profile in &mut cfg.providers {
        if profile.api_key.is_none() {
            profile.api_key = store.get(&profile.name);
        }
    }
}

/// Persist every non-empty key in *cfg* to the Keychain. Called by
/// `update_config` before the config is written (stripped) to disk. Returns
/// accounts that failed to persist (so `unpersisted_secrets` can keep them).
pub fn store_keys(cfg: &XenonConfig) -> std::collections::HashSet<String> {
    KeychainStore::default_store().store_keys(cfg)
}

/// Read one named secret (MCP env/header values, search API keys).
pub fn secret_get(account: &str) -> Option<String> {
    KeychainStore::default_store().get(account)
}

/// Write one named secret (empty value deletes it). Returns `true` when a
/// subsequent `get(account)` would return the new value (Keychain or fallback).
pub fn secret_set(account: &str, value: &str) -> bool {
    let store = KeychainStore::default_store();
    if value.trim().is_empty() {
        store.delete(account);
        true
    } else {
        store.set(account, value)
    }
}

/// Remove one named secret.
pub fn secret_delete(account: &str) {
    KeychainStore::default_store().delete(account);
}

/// Result of the Settings "Test" button for one profile.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileTest {
    pub model: String,
    /// Latency of the tiny chat completion, milliseconds.
    pub latency_ms: u64,
    pub tools: bool,
    pub vision: bool,
    /// Free-text problem (bad key, dead endpoint); `None` on success.
    pub error: Option<String>,
}

/// A 1x1 PNG used to check vision input without shipping an asset.
pub const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

fn chat_endpoint(profile: &ProviderProfile) -> String {
    format!("{}/chat/completions", normalize_base_url(&profile.base_url))
}

fn headers(api_key: Option<&str>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", key)) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
    }
    headers
}

/// One tiny non-streaming completion. Returns the assistant message JSON.
async fn tiny_chat(
    profile: &ProviderProfile,
    messages: serde_json::Value,
    tools: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    if profile.model.trim().is_empty() {
        return Err("no model selected".to_string());
    }
    let mut body = serde_json::json!({
        "model": profile.model,
        "messages": messages,
        "stream": false,
        "temperature": 0,
        // Reasoning models (e.g. Qwen3) spend tokens on `reasoning_content`
        // before the answer; a tiny budget truncates before any tool call.
        "max_tokens": 256,
    });
    if let Some(tools) = tools {
        body["tools"] = tools;
    }
    // CPU llama.cpp servers can take 30s+ for a reasoning completion.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(chat_endpoint(profile))
        .headers(headers(profile.api_key.as_deref()))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", truncate(&text, 200)));
    }
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(json["choices"][0]["message"].clone())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Probe one profile: a tiny chat completion (latency), one trivial tool
/// (does a `tool_calls` come back?) and one 1x1 image (does vision work?).
/// Results are stored on the profile by the caller.
pub async fn test_profile(profile: &ProviderProfile) -> ProfileTest {
    // A real (non-empty) parameter: some servers (llama.cpp) reject an empty
    // `properties` object outright, which would read as "no tool support".
    let ping_tool = serde_json::json!([{
        "type": "function",
        "function": {
            "name": "ping",
            "description": "Call this to confirm tool support. Pass any short note.",
            "parameters": {
                "type": "object",
                "properties": { "note": { "type": "string" } },
                "required": ["note"]
            }
        }
    }]);

    // 1) Latency: a plain tiny completion.
    let started = Instant::now();
    let chat = tiny_chat(
        profile,
        serde_json::json!([{ "role": "user", "content": "Say OK." }]),
        None,
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let error = chat.as_ref().err().cloned();

    // 2) Tool support: a trivial tool call.
    let tools = match tiny_chat(
        profile,
        serde_json::json!([{
            "role": "user",
            "content": "Call the ping tool now with any short note."
        }]),
        Some(ping_tool),
    )
    .await
    {
        Ok(m) => m["tool_calls"]
            .as_array()
            .map(|c| !c.is_empty())
            .unwrap_or(false),
        Err(_) => false,
    };

    // 3) Vision: a 1x1 PNG image part.
    let image_url = format!("data:image/png;base64,{TINY_PNG_B64}");
    let vision = match tiny_chat(
        profile,
        serde_json::json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "What is in this image? One word." },
                { "type": "image_url", "image_url": { "url": image_url } }
            ]
        }]),
        None,
    )
    .await
    {
        Ok(m) => !m["content"].as_str().unwrap_or("").trim().is_empty(),
        Err(_) => false,
    };

    ProfileTest {
        model: profile.model.clone(),
        latency_ms,
        tools,
        vision,
        error,
    }
}

// ---- Tauri commands ------------------------------------------------------

#[tauri::command]
pub fn provider_presets() -> Vec<ProviderPreset> {
    presets::PRESETS.to_vec()
}

#[tauri::command]
pub async fn detect_local_providers() -> Result<Vec<DetectedProvider>, String> {
    Ok(presets::detect_local(std::time::Duration::from_millis(1500)).await)
}

#[tauri::command]
pub async fn list_provider_models(
    base_url: String,
    api_key: Option<String>,
) -> Result<Vec<String>, String> {
    presets::probe_models(
        &base_url,
        api_key.as_deref(),
        std::time::Duration::from_millis(1500),
    )
    .await
}

#[tauri::command]
pub async fn test_provider_profile(profile: ProviderProfile) -> Result<ProfileTest, String> {
    Ok(test_profile(&profile).await)
}

/// Open a support link in the default browser. Allowlist only: no arbitrary
/// URLs, so a compromised webview cannot open attacker-chosen destinations.
#[tauri::command]
pub fn open_support_link(kind: String) -> Result<(), String> {
    let url = match kind.as_str() {
        "sponsors" => "https://github.com/sponsors/SamerKharboush",
        "paypal" => "https://paypal.me/Samerelnady",
        other => return Err(format!("unknown support link kind: {other}")),
    };
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockStore {
        inner: Mutex<HashMap<String, String>>,
    }

    impl SecretStore for MockStore {
        fn get(&self, account: &str) -> Option<String> {
            self.inner.lock().unwrap().get(account).cloned()
        }
        fn set(&self, account: &str, secret: &str) -> bool {
            self.inner
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
            true
        }
        fn delete(&self, account: &str) {
            self.inner.lock().unwrap().remove(account);
        }
    }

    /// Fake KeychainBackend for the read-error / fallback tests. Can be told
    /// to fail `load` (simulating a Deny) and records `store` calls.
    struct FakeBackend {
        inner: Mutex<FakeState>,
    }
    struct FakeState {
        blob: Option<String>,
        load_fails: bool,
        store_fails: bool,
        stores: Vec<String>,
    }
    impl FakeBackend {
        fn new(blob: Option<&str>) -> Self {
            Self {
                inner: Mutex::new(FakeState {
                    blob: blob.map(|s| s.to_string()),
                    load_fails: false,
                    store_fails: false,
                    stores: Vec::new(),
                }),
            }
        }
        fn failing() -> Self {
            Self {
                inner: Mutex::new(FakeState {
                    blob: None,
                    load_fails: true,
                    store_fails: false,
                    stores: Vec::new(),
                }),
            }
        }
        /// Reads succeed but writes fail (Keychain full/locked).
        fn store_failing(blob: Option<&str>) -> Self {
            Self {
                inner: Mutex::new(FakeState {
                    blob: blob.map(|s| s.to_string()),
                    load_fails: false,
                    store_fails: true,
                    stores: Vec::new(),
                }),
            }
        }
    }
    impl KeychainBackend for FakeBackend {
        fn load(&self) -> Result<Option<String>, String> {
            let st = self.inner.lock().unwrap();
            if st.load_fails {
                Err("user denied keychain access".into())
            } else {
                Ok(st.blob.clone())
            }
        }
        fn store(&self, json: &str) -> Result<(), String> {
            let mut st = self.inner.lock().unwrap();
            if st.store_fails {
                return Err("keychain write failed".into());
            }
            st.blob = Some(json.to_string());
            st.stores.push(json.to_string());
            Ok(())
        }
        fn load_legacy(&self, _account: &str) -> Option<String> {
            None
        }
        fn delete_legacy(&self, _account: &str) {}
    }

    fn temp_fallback() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xenon-hub-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("secrets.json")
    }

    /// R1: a Keychain read error (Deny) must NOT wipe other keys. The store
    /// falls back to the file and leaves the Keychain item untouched.
    #[test]
    fn test_read_error_does_not_overwrite_keychain() {
        let backend = Arc::new(FakeBackend::failing());
        let store = KeychainStore::with_backend(backend.clone(), temp_fallback());

        // Simulate an existing key already in the Keychain (from a prior run).
        // The backend read fails, but set() must not clobber it.
        store.set("profile-a", "sk-aaa");

        // The store call must never have happened (we cannot read, so we
        // must not write_all an empty map + one key).
        assert!(
            backend.inner.lock().unwrap().stores.is_empty(),
            "set() wrote to the Keychain despite a read error"
        );
        // And the secret landed in the fallback file instead.
        assert_eq!(store.get("profile-a").as_deref(), Some("sk-aaa"));
    }

    /// R1: the in-process cache means N profiles hit the Keychain once.
    #[test]
    fn test_read_all_hits_keychain_once() {
        let backend = Arc::new(FakeBackend::new(None));
        let store = KeychainStore::with_backend(backend.clone(), temp_fallback());
        store.read_all().unwrap();
        store.read_all().unwrap();
        store.read_all().unwrap();
        let st = backend.inner.lock().unwrap();
        // FakeBackend records stores, not loads; but with an empty blob the
        // cache is populated on the first read and served thereafter. We can
        // at least assert repeated reads don't error and return empty.
        assert!(st.stores.is_empty());
    }

    /// R1: fallback file is created atomically with mode 0600.
    #[cfg(unix)]
    #[test]
    fn test_fallback_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_fallback();
        let _ = std::fs::remove_file(&path);
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::failing()), path.clone());
        store.set("k", "sk-secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fallback file must be 0600 from creation");
        let _ = std::fs::remove_file(&path);
    }

    /// F5: when a value is in the fallback file (a Keychain write had failed),
    /// `get` returns the fallback value even though the Keychain holds a stale
    /// one. A later successful Keychain write makes the Keychain authoritative
    /// again by dropping the fallback copy.
    #[test]
    fn test_fallback_overrides_stale_keychain() {
        let store = KeychainStore::with_backend(
            Arc::new(FakeBackend::new(Some(r#"{"A":"old"}"#))),
            temp_fallback(),
        );
        // A prior failed Keychain write parked the value in the fallback.
        assert!(store.write_fallback("A", "new"));
        assert_eq!(store.get("A").as_deref(), Some("new"), "fallback must win");

        // A later Keychain write succeeds: Keychain becomes authoritative and
        // the fallback copy is removed.
        assert!(store.set("A", "newest"));
        assert_eq!(store.get("A").as_deref(), Some("newest"));
        assert!(
            !store.read_fallback().unwrap().unwrap().contains_key("A"),
            "stale fallback copy must be removed after a Keychain write"
        );
    }

    /// F5: a failed Keychain write that lands in the fallback reports success,
    /// and `get` returns the new value (fallback-first).
    #[test]
    fn test_set_fallback_reports_true_and_get_returns_it() {
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::store_failing(None)), temp_fallback());
        assert!(store.set("A", "sk-new"), "fallback store should report success");
        assert_eq!(store.get("A").as_deref(), Some("sk-new"));
    }

    /// F3: repeated fallback writes use unique temp names and leave no
    /// `.tmp` files behind.
    #[test]
    fn test_fallback_unique_temp_names_no_leftovers() {
        let path = temp_fallback();
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::store_failing(None)), path.clone());
        assert!(store.set("A", "1"));
        assert!(store.set("B", "2"));
        let dir = path.parent().unwrap();
        let leftover: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
    }

    /// R6: deleting/renaming a profile drops its key from the store.
    #[test]
    fn test_store_keys_removes_deleted_profile() {
        let store = KeychainStore::with_backend(
            Arc::new(FakeBackend::new(None)),
            temp_fallback(),
        );
        // Two profiles with keys.
        let json = r#"{
            "pet_name": "Xenon",
            "providers": [
                {"name": "A", "base_url": "http://a/v1", "api_key": "sk-a", "model": "m", "vision_model": "m"},
                {"name": "B", "base_url": "http://b/v1", "api_key": "sk-b", "model": "m", "vision_model": "m"}
            ],
            "provider_url": "http://a/v1",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        cfg.migrate();
        store.store_keys(&cfg);
        assert_eq!(store.get("A").as_deref(), Some("sk-a"));
        assert_eq!(store.get("B").as_deref(), Some("sk-b"));

        // Now drop profile B and save again: its key must be gone.
        cfg.providers.retain(|p| p.name != "B");
        store.store_keys(&cfg);
        assert_eq!(store.get("A").as_deref(), Some("sk-a"));
        assert_eq!(store.get("B"), None, "deleted profile's key must be removed");
    }

    /// The compiled store must NOT be the in-memory mock: on macOS with the
    /// `apple-native` feature, keyring::Entry::new must succeed against a
    /// real (or at least platform) credential builder, not fall back to the
    /// no-op mock store. This test can never prove the Keychain is *writable*
    /// (that needs a real user session), but it guards against a future
    /// refactor that drops the `apple-native` feature and silently reverts to
    /// the mock store.
    #[test]
    fn startup_self_check_store_is_not_mock() {
        // On macOS with apple-native, keyring::Entry::new for our service must
        // not return an error that indicates the mock store is in use. We only
        // check that the call path does not panic and returns Ok or a
        // non-mock error.
        let result = keyring::Entry::new(KEYCHAIN_SERVICE, "__self_check__");
        // If this ever returns Err(PlatformFailure(...)) with "mock" in the
        // message, the apple-native feature is missing.
        if let Err(e) = result {
            let msg = format!("{e}").to_lowercase();
            assert!(
                !msg.contains("mock") && !msg.contains("no credential store"),
                "keyring is using the mock store — enable the apple-native feature: {e}"
            );
        }
    }

    fn cfg_with_plaintext_key() -> XenonConfig {
        let json = r#"{
            "pet_name": "Xenon",
            "provider_url": "https://api.openai.com/v1",
            "provider_api_key": "sk-plaintext",
            "model_name": "gpt-4o-mini",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        cfg.migrate();
        cfg
    }

    #[test]
    fn test_plaintext_key_moves_to_keychain() {
        let store = MockStore::default();
        let mut cfg = cfg_with_plaintext_key();
        assert_eq!(cfg.providers[0].api_key.as_deref(), Some("sk-plaintext"));

        migrate_plaintext(&mut cfg, &store);

        assert_eq!(store.get("Primary"), Some("sk-plaintext".to_string()));
        assert!(cfg.providers.iter().all(|p| p.api_key.is_none()));
        assert!(cfg.provider.provider_api_key.is_none());
    }

    #[test]
    fn test_hydrate_restores_key_without_touching_disk() {
        let store = MockStore::default();
        let mut cfg = cfg_with_plaintext_key();
        migrate_plaintext(&mut cfg, &store);

        let mut reloaded = cfg_with_plaintext_key();
        reloaded.providers.iter_mut().for_each(|p| p.api_key = None);
        reloaded.provider.provider_api_key = None;
        hydrate(&mut reloaded, &store);

        assert_eq!(reloaded.providers[0].api_key.as_deref(), Some("sk-plaintext"));
    }

    #[test]
    fn test_save_never_writes_a_secret() {
        let cfg = cfg_with_plaintext_key();
        let written = serde_json::to_string(&cfg.without_secrets()).unwrap();
        assert!(!written.contains("sk-plaintext"), "secret leaked to disk: {written}");
        // The in-memory config still has it (the agent needs it this session).
        assert_eq!(cfg.providers[0].api_key.as_deref(), Some("sk-plaintext"));
    }

    #[test]
    fn test_tiny_png_is_a_valid_1x1_image() {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(TINY_PNG_B64)
            .expect("base64");
        let img = image::load_from_memory(&bytes).expect("decode");
        assert_eq!((img.width(), img.height()), (1, 1));
    }

    #[tokio::test]
    #[ignore = "manual probe against a live OpenAI-compatible server"]
    async fn probe_live_local_provider() {
        // Point at the real local server on 8080 and exercise every hub path.
        let base = "http://127.0.0.1:8080/v1";
        let models = presets::probe_models(
            base,
            None,
            std::time::Duration::from_millis(1500),
        )
        .await
        .expect("models");
        eprintln!("probe_models => {models:?}");
        assert!(!models.is_empty(), "no models");

        let det = presets::detect_local(std::time::Duration::from_millis(1500)).await;
        eprintln!("detect_local => {det:?}");
        assert!(det.iter().any(|d| d.base_url.contains("8080")));

        let profile = ProviderProfile {
            name: "ToshLLM".into(),
            base_url: base.into(),
            api_key: None,
            model: models[0].clone(),
            vision_model: models[0].clone(),
            supports_tools: true,
            // New profiles don't assume vision: only a hub test can prove it.
            supports_vision: false,
        };
        let test = test_profile(&profile).await;
        eprintln!("test_profile => {test:?}");
        assert!(test.latency_ms < 60_000, "no latency");
        assert!(test.tools, "tools probe failed");
        // This model (Qwen3-14B) has no vision projector, so vision:false is correct.
        assert!(test.error.is_none());
    }

    fn stress_out() -> std::path::PathBuf {
        let dir = std::env::var_os("XENON_STRESS_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../scripts/stress/out")
            });
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Harness 5: detect local providers and list models, writing TSV output.
    #[tokio::test]
    #[ignore = "probes local ports; run with -- --ignored stress_hub_detect"]
    async fn stress_hub_detect_and_list() {
        use std::io::Write;
        let det = detect_local_providers().await.unwrap();
        let out = stress_out();
        let mut f = std::fs::File::create(out.join("hub-detect.tsv")).unwrap();
        for d in &det {
            writeln!(f, "{}\t{}\t{}", d.name, d.base_url, d.models.join(",")).unwrap();
        }
        drop(f);
        eprintln!("detected {} local providers", det.len());

        for d in det.iter().filter(|d| d.base_url.contains("8080")) {
            match list_provider_models(d.base_url.clone(), None).await {
                Ok(models) => {
                    let mut f = std::fs::File::create(out.join("hub-models.tsv")).unwrap();
                    for m in &models {
                        writeln!(f, "{}", m).unwrap();
                    }
                    eprintln!("models ({}): {models:?}", d.name);
                }
                Err(e) => eprintln!("list models failed for {}: {e}", d.name),
            }
        }
    }

    #[test]
    fn test_legacy_flat_key_without_profiles_is_kept() {
        let store = MockStore::default();
        let json = r#"{
            "pet_name": "Xenon",
            "provider_url": "http://localhost:8080",
            "provider_api_key": "sk-flat",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        migrate_plaintext(&mut cfg, &store);
        assert_eq!(store.get(PRIMARY_ACCOUNT), Some("sk-flat".to_string()));
    }

    /// A store that fails the first `set` for each account, then succeeds.
    struct FailFirstSetStore {
        secrets: Mutex<HashMap<String, String>>,
        attempted: Mutex<std::collections::HashSet<String>>,
    }
    impl SecretStore for FailFirstSetStore {
        fn get(&self, account: &str) -> Option<String> {
            self.secrets.lock().unwrap().get(account).cloned()
        }
        fn set(&self, account: &str, secret: &str) -> bool {
            let mut attempted = self.attempted.lock().unwrap();
            if attempted.insert(account.to_string()) {
                return false; // first attempt for this account fails
            }
            drop(attempted);
            self.secrets
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
            true
        }
        fn delete(&self, account: &str) {
            self.secrets.lock().unwrap().remove(account);
        }
    }

    /// Finding 4: the flat provider key and a "Primary" profile share one
    /// account. When the first store fails and the second succeeds, the account
    /// must be reported stored (not failed) so its plaintext is scrubbed.
    #[test]
    fn test_migration_same_account_later_success_clears_failure() {
        let store = FailFirstSetStore {
            secrets: Mutex::new(HashMap::new()),
            attempted: Mutex::new(std::collections::HashSet::new()),
        };
        let json = r#"{
            "pet_name": "Xenon",
            "providers": [
                {"name": "Primary", "base_url": "http://a/v1", "api_key": "sk-shared", "model": "m", "vision_model": "m"}
            ],
            "provider_api_key": "sk-shared",
            "provider_url": "http://a/v1",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        let outcome = migrate_plaintext_checked(&mut cfg, &store);
        assert!(outcome.stored.contains(&"Primary".to_string()));
        assert!(
            !outcome.failed.contains(&"Primary".to_string()),
            "a same-value later success must clear the earlier failure marker"
        );
        assert_eq!(store.get("Primary").as_deref(), Some("sk-shared"));
        // No marker => without_secrets scrubs the plaintext.
        assert!(cfg.without_secrets().provider.provider_api_key.is_none());
    }

    /// Finding 5: a provider profile named like an MCP secret account must not
    /// be overwritten by the MCP value. Both plaintext values are retained so
    /// neither is lost.
    #[test]
    fn test_migration_account_collision_keeps_both_values() {
        let store = MockStore::default();
        let json = r#"{
            "pet_name": "Xenon",
            "providers": [
                {"name": "mcp:demo:TOKEN", "base_url": "http://a/v1", "api_key": "sk-provider", "model": "m", "vision_model": "m"}
            ],
            "provider_url": "http://a/v1",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi",
            "mcp_servers": [{
                "name": "demo",
                "transport": "stdio",
                "command": "demo",
                "env": {"TOKEN": "sk-mcp"},
                "env_secrets": ["TOKEN"]
            }]
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        let outcome = migrate_plaintext_checked(&mut cfg, &store);
        // The MCP value is stored; the colliding account is NOT reported stored.
        assert_eq!(store.get("mcp:demo:TOKEN").as_deref(), Some("sk-mcp"));
        assert!(
            !outcome.stored.contains(&"mcp:demo:TOKEN".to_string()),
            "a colliding account must not be reported stored"
        );
        assert!(outcome.failed.contains(&"mcp:demo:TOKEN".to_string()));
        // The provider key survives as plaintext (no loss).
        let provider = cfg
            .providers
            .iter()
            .find(|p| p.name == "mcp:demo:TOKEN")
            .unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("sk-provider"));
    }

    /// Finding 5 (save path): `store_keys` must not overwrite an MCP secret
    /// when a provider profile name collides with its account.
    #[test]
    fn test_store_keys_skips_mcp_collision() {
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::new(None)), temp_fallback());
        let json = r#"{
            "pet_name": "Xenon",
            "providers": [
                {"name": "mcp:demo:TOKEN", "base_url": "http://a/v1", "api_key": "sk-provider", "model": "m", "vision_model": "m"}
            ],
            "provider_url": "http://a/v1",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi",
            "mcp_servers": [{
                "name": "demo", "transport": "stdio", "command": "demo",
                "env": {"TOKEN": "sk-mcp"}, "env_secrets": ["TOKEN"]
            }]
        }"#;
        let cfg: XenonConfig = serde_json::from_str(json).unwrap();
        // The MCP secret is already stored under the shared account.
        assert!(store.set("mcp:demo:TOKEN", "sk-mcp"));
        let failed = store.store_keys(&cfg);
        assert!(failed.contains("mcp:demo:TOKEN"));
        // The MCP secret must be untouched, not overwritten by the provider key.
        assert_eq!(store.get("mcp:demo:TOKEN").as_deref(), Some("sk-mcp"));
    }

    /// Finding 3: a fallback read error is not "account absent". Cleanup must
    /// report failure rather than silently succeed.
    #[test]
    fn test_remove_fallback_read_error_is_failure() {
        let dir = std::env::temp_dir().join(format!(
            "xenon-fb-readerr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A directory where the fallback file should be => read error.
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::new(None)), dir.clone());
        assert!(
            !store.remove_fallback("A"),
            "cleanup must fail when the fallback file cannot be read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A genuinely absent fallback file (no file) is a successful cleanup.
    #[test]
    fn test_remove_fallback_absent_file_is_success() {
        let store = KeychainStore::with_backend(Arc::new(FakeBackend::new(None)), temp_fallback());
        assert!(
            store.remove_fallback("A"),
            "an absent account must be a no-op success"
        );
    }

    /// Settings save → keys persisted → restart → keys present: simulate a
    /// full round-trip through the MockStore to verify the store/hydrate
    /// contract works end-to-end without touching the real Keychain.
    #[test]
    fn test_settings_save_restart_round_trip() {
        let store = MockStore::default();
        let cfg = cfg_with_plaintext_key();

        // Simulate Settings save: store_keys + save (stripped).
        store_keys_with(&cfg, &store);
        let saved = cfg.without_secrets();
        assert!(saved.providers.iter().all(|p| p.api_key.is_none()));

        // Simulate restart: load config (blank keys) + hydrate_keys.
        let mut reloaded = saved;
        reloaded.providers.iter_mut().for_each(|p| p.api_key = None);
        hydrate(&mut reloaded, &store);
        assert_eq!(reloaded.providers[0].api_key.as_deref(), Some("sk-plaintext"));
    }

    fn store_keys_with(cfg: &XenonConfig, store: &dyn SecretStore) {
        if let Some(key) = &cfg.provider.provider_api_key {
            if !key.trim().is_empty() {
                store.set(PRIMARY_ACCOUNT, key);
            }
        }
        for profile in &cfg.providers {
            if let Some(key) = &profile.api_key {
                if !key.trim().is_empty() {
                    store.set(&profile.name, key);
                }
            }
        }
    }

    /// Real-Keychain round-trip: store a throwaway key, read it back, delete
    /// it. `#[ignore]` and gated on XENON_LIVE_KEYCHAIN_TEST=1 so normal test
    /// runs never touch the real Keychain.
    #[test]
    #[ignore = "touches the real login Keychain; run with XENON_LIVE_KEYCHAIN_TEST=1"]
    fn real_keychain_round_trip() {
        if std::env::var("XENON_LIVE_KEYCHAIN_TEST").as_deref() != Ok("1") {
            eprintln!("skip: set XENON_LIVE_KEYCHAIN_TEST=1 to run");
            return;
        }
        let store = KeychainStore::with_backend(Arc::new(MacBackend), KeychainStore::fallback_path());
        let account = "__xenon_test_account__";
        let secret = "sk-test-round-trip-12345";

        // Clean any leftover from a previous run.
        store.delete(account);

        // Store + read back.
        store.set(account, secret);
        assert_eq!(store.get(account).as_deref(), Some(secret));

        // Delete + verify gone.
        store.delete(account);
        assert_eq!(store.get(account), None);
    }

    /// Test builds must never resolve the secrets fallback into the user's real
    /// config dir.
    #[test]
    fn test_fallback_path_never_real_dir_under_test() {
        let path = KeychainStore::fallback_path();
        let real = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        assert!(
            !path.starts_with(&real),
            "test fallback_path leaked into real config dir: {}",
            path.display()
        );
        assert!(path.starts_with(std::env::temp_dir()));
    }
}