//! Provider Hub presets and local-model discovery.
//!
//! Xenon speaks OpenAI-compatible HTTP only (ADR-0003), so every provider is
//! described by a name, a base URL and whether it needs an API key. "Detect
//! local" probes the local presets with `GET /models`; the answer is parsed
//! from either OpenAI (`{data:[{id}]}`) or llama-server/Ollama
//! (`{models:[{name|model}]}`) shape.

use serde::Serialize;
use std::time::Duration;

use crate::provider::normalize_base_url;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderPreset {
    pub name: &'static str,
    pub base_url: &'static str,
    pub needs_key: bool,
    pub notes: &'static str,
    /// On this machine; probed by "Detect local".
    pub local: bool,
}

/// Every provider the user asked for (PRD §3.4) plus the local servers this
/// machine runs. Base URLs that already carry a version segment are left
/// untouched by `normalize_base_url`.
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "LM Studio",
        base_url: "http://localhost:1234/v1",
        needs_key: false,
        notes: "Local server. Load a model in LM Studio first; any key works.",
        local: true,
    },
    ProviderPreset {
        name: "ToshLLM",
        base_url: "http://localhost:8080/v1",
        needs_key: false,
        notes: "Local llama-server (Qwen3-14B). No key.",
        local: true,
    },
    ProviderPreset {
        name: "XeonLLM",
        base_url: "http://localhost:18140/v1",
        needs_key: false,
        notes: "Local llama-server. The port changes per launch; edit if needed.",
        local: true,
    },
    ProviderPreset {
        name: "Ollama",
        base_url: "http://localhost:11434/v1",
        needs_key: false,
        notes: "Local. Model ids look like llama3.2 or llava.",
        local: true,
    },
    ProviderPreset {
        name: "9router",
        base_url: "http://localhost:20128/v1",
        needs_key: true,
        notes: "Local router. Paste the API key you configured in 9router.",
        local: true,
    },
    ProviderPreset {
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        needs_key: true,
        notes: "Cloud. Key is stored in the macOS Keychain, never in config.json.",
        local: false,
    },
    ProviderPreset {
        name: "NVIDIA",
        base_url: "https://integrate.api.nvidia.com/v1",
        needs_key: true,
        notes: "NVIDIA NIM cloud catalog (OpenAI-compatible).",
        local: false,
    },
    ProviderPreset {
        name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        needs_key: true,
        notes: "Cloud, OpenAI-compatible surface (Claude models).",
        local: false,
    },
    ProviderPreset {
        name: "Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        needs_key: true,
        notes: "Google AI Studio key; OpenAI-compatible endpoint.",
        local: false,
    },
    ProviderPreset {
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        needs_key: true,
        notes: "Cloud router for many vendors behind one key.",
        local: false,
    },
    ProviderPreset {
        name: "Custom",
        base_url: "",
        needs_key: false,
        notes: "Anything OpenAI-compatible: type the base URL yourself.",
        local: false,
    },
];

/// One reachable local endpoint and the model ids it serves.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedProvider {
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
}

/// Local presets, in probe order: LM Studio, ToshLLM, XeonLLM, Ollama, 9router
/// (ports 1234, 8080, 18140, 11434, 20128).
pub fn local_presets() -> Vec<&'static ProviderPreset> {
    PRESETS.iter().filter(|p| p.local && !p.base_url.is_empty()).collect()
}

/// Parse model ids out of a `/models` reply. Understands both the OpenAI shape
/// (`{"data":[{"id":…}]}`) and the llama-server / Ollama shape
/// (`{"models":[{"name"|"model"|"id"}]}` or `{"models":["id"]}`).
fn push_id(ids: &mut Vec<String>, v: &serde_json::Value) {
    for key in ["id", "name", "model"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            push_str(ids, s);
            return;
        }
    }
}

fn push_str(ids: &mut Vec<String>, raw: &str) {
    let s = raw.trim();
    if !s.is_empty() && !ids.iter().any(|i| i == s) {
        ids.push(s.to_string());
    }
}

pub fn parse_model_ids(body: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();

    for key in ["data", "models"] {
        if let Some(serde_json::Value::Array(items)) = body.get(key) {
            for item in items {
                match item {
                    serde_json::Value::String(s) => push_str(&mut ids, s),
                    other => push_id(&mut ids, other),
                }
            }
        }
    }

    ids
}

/// `GET {base}/models` with a hard timeout. `Err` carries a short, log-safe
/// reason (never a key).
pub async fn probe_models(
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let base = normalize_base_url(base_url);
    if base.is_empty() {
        return Err("empty base URL".to_string());
    }
    let url = format!("{base}/models");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            req = req.bearer_auth(key);
        }
    }
    let response = req.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(parse_model_ids(&body))
}

/// Probe every local preset in order. Unreachable endpoints are skipped, not
/// reported as errors: a local server that is simply not running is normal.
pub async fn detect_local(timeout: Duration) -> Vec<DetectedProvider> {
    let mut found = Vec::new();
    for preset in local_presets() {
        match probe_models(preset.base_url, None, timeout).await {
            Ok(models) if !models.is_empty() => found.push(DetectedProvider {
                name: preset.name.to_string(),
                base_url: normalize_base_url(preset.base_url),
                models,
            }),
            Ok(_) => log::debug!("local provider {} reachable but serves no models", preset.name),
            Err(e) => log::debug!("local provider {} not reachable: {e}", preset.name),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_openai_shape() {
        let body = json!({"object": "list", "data": [{"id": "gpt-4o-mini"}, {"id": "gpt-4o"}]});
        assert_eq!(parse_model_ids(&body), vec!["gpt-4o-mini", "gpt-4o"]);
    }

    #[test]
    fn test_parse_llama_server_shape() {
        let body = json!({"models": [{"name": "qwen3-14b"}, {"model": "llama-3"}]});
        assert_eq!(parse_model_ids(&body), vec!["qwen3-14b", "llama-3"]);
    }

    #[test]
    fn test_parse_ollama_shape() {
        let body = json!({"models": ["llama3.2:latest", "llava:latest"]});
        assert_eq!(parse_model_ids(&body), vec!["llama3.2:latest", "llava:latest"]);
    }

    #[test]
    fn test_parse_dedupes_and_skips_blanks() {
        let body = json!({"data": [{"id": "a"}, {"id": "a"}, {"id": "  "}], "models": [{"name": "b"}]});
        assert_eq!(parse_model_ids(&body), vec!["a", "b"]);
    }

    #[test]
    fn test_parse_garbage_is_empty() {
        assert!(parse_model_ids(&json!({"nope": 1})).is_empty());
        assert!(parse_model_ids(&json!([])).is_empty());
    }

    #[test]
    fn test_local_presets_cover_the_expected_ports() {
        let urls: Vec<&str> = local_presets().iter().map(|p| p.base_url).collect();
        for expected in [
            "http://localhost:1234/v1",
            "http://localhost:8080/v1",
            "http://localhost:18140/v1",
            "http://localhost:11434/v1",
            "http://localhost:20128/v1",
        ] {
            assert!(urls.contains(&expected), "missing local preset {expected}");
        }
    }

    #[test]
    fn test_every_preset_has_notes_and_cloud_keys_flagged() {
        for p in PRESETS {
            assert!(!p.name.is_empty());
            assert!(!p.notes.is_empty());
            if !p.local && !p.base_url.is_empty() {
                assert!(p.base_url.starts_with("https://"), "cloud preset {} must be https", p.name);
            }
        }
        assert!(PRESETS.iter().any(|p| p.name == "OpenAI" && p.needs_key));
        assert!(PRESETS.iter().any(|p| p.name == "ToshLLM" && !p.needs_key));
    }
}