//! Provider Hub: Keychain-backed API keys, per-profile capability probing, and
//! the Tauri commands the Settings UI calls.
//!
//! Keys never reach `config.json`: `XenonConfig::save` strips them and
//! `update_config` writes them here, under service `com.xenon.app` with the
//! profile name as the account.

use std::time::Instant;

use serde::Serialize;

use crate::config::{ProviderProfile, XenonConfig};
use crate::presets::{self, DetectedProvider, ProviderPreset};
use crate::provider::normalize_base_url;

/// Keychain service for every Xenon secret.
pub const KEYCHAIN_SERVICE: &str = "com.xenon.app";
/// Account used by the legacy flat provider config.
pub const PRIMARY_ACCOUNT: &str = "Primary";

/// Minimal secret storage, so migration is unit-testable without touching the
/// real Keychain.
pub trait SecretStore {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, secret: &str);
}

/// macOS Keychain via the `keyring` crate.
pub struct KeychainStore;

impl SecretStore for KeychainStore {
    fn get(&self, account: &str) -> Option<String> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, account) {
            Ok(entry) => match entry.get_password() {
                Ok(pw) => Some(pw),
                // NotFound is the normal "no key stored yet" case.
                Err(keyring::Error::NoEntry) => None,
                Err(e) => {
                    log::warn!("keychain read failed for {account}: {e}");
                    None
                }
            },
            Err(e) => {
                log::warn!("keychain unavailable: {e}");
                None
            }
        }
    }

    fn set(&self, account: &str, secret: &str) {
        match keyring::Entry::new(KEYCHAIN_SERVICE, account) {
            Ok(entry) => {
                if let Err(e) = entry.set_password(secret) {
                    // Never log the secret itself.
                    log::error!("keychain write failed for {account}: {e}");
                }
            }
            Err(e) => log::error!("keychain unavailable: {e}"),
        }
    }
}

/// Move plaintext keys from an old `config.json` into *store* and clear them
/// from the in-memory config. Empty keys are dropped.
pub fn migrate_plaintext(cfg: &mut XenonConfig, store: &dyn SecretStore) {
    if let Some(key) = cfg.provider.provider_api_key.take() {
        if !key.trim().is_empty() {
            store.set(PRIMARY_ACCOUNT, &key);
        }
    }
    for profile in &mut cfg.providers {
        if let Some(key) = profile.api_key.take() {
            if !key.trim().is_empty() {
                store.set(&profile.name, &key);
            }
        }
    }
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

pub fn migrate_plaintext_keys(cfg: &mut XenonConfig) {
    migrate_plaintext(cfg, &KeychainStore);
}

pub fn hydrate_keys(cfg: &mut XenonConfig) {
    hydrate(cfg, &KeychainStore);
}

/// Persist every non-empty key in *cfg* to the Keychain. Called by
/// `update_config` before the config is written (stripped) to disk.
pub fn store_keys(cfg: &XenonConfig) {
    let store = KeychainStore;
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
        fn set(&self, account: &str, secret: &str) {
            self.inner
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
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
            supports_vision: true,
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
}