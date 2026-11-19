use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// LLM endpoint settings. Flattened into XenonConfig so the config file keeps
/// its historical flat shape (`provider_url`, `provider_api_key`,
/// `model_name` at the top level).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_url: String,
    pub provider_api_key: Option<String>,
    pub model_name: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            provider_url: "http://localhost:11434".to_string(),
            provider_api_key: None,
            model_name: "local-model".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XenonConfig {
    pub pet_name: String,
    #[serde(flatten)]
    pub provider: ProviderConfig,
    pub voice_enabled: bool,
    pub wake_word_enabled: bool,
    pub wake_word: String,
    pub system_prompt: String,
}

impl Default for XenonConfig {
    fn default() -> Self {
        Self {
            pet_name: "Xenon".to_string(),
            provider: ProviderConfig::default(),
            voice_enabled: true,
            wake_word_enabled: true,
            wake_word: "Xenon".to_string(),
            system_prompt: "You are Xenon, a witty and intelligent desktop companion inspired by Jarvis. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural.".to_string(),
        }
    }
}

impl XenonConfig {
    fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        let mut path = base;
        path.push("xenon");
        path.push("config.json");
        path
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&path, content).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings must survive a save/load round-trip, otherwise renaming the pet
    /// or pointing at a new provider silently does nothing.
    #[test]
    fn test_config_round_trip() {
        let dir = std::env::temp_dir().join(format!("xenon-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let cfg = XenonConfig {
            pet_name: "Jarvis".to_string(),
            provider: ProviderConfig {
                provider_url: "http://127.0.0.1:8080".to_string(),
                model_name: "qwen3".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.save().expect("save");

        let loaded = XenonConfig::load();
        assert_eq!(loaded.pet_name, "Jarvis");
        assert_eq!(loaded.provider.provider_url, "http://127.0.0.1:8080");
        assert_eq!(loaded.provider.model_name, "qwen3");

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Config files written before ProviderConfig existed keep their flat
    /// provider fields; serde(flatten) must still read them.
    #[test]
    fn test_config_deserializes_legacy_flat_provider_fields() {
        let json = r#"{
            "pet_name": "Xenon",
            "provider_url": "http://127.0.0.1:9999",
            "provider_api_key": "sk-test",
            "model_name": "legacy-model",
            "voice_enabled": false,
            "wake_word_enabled": false,
            "wake_word": "Xenon",
            "system_prompt": "be nice"
        }"#;
        let cfg: XenonConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider.provider_url, "http://127.0.0.1:9999");
        assert_eq!(cfg.provider.provider_api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.provider.model_name, "legacy-model");
        assert!(!cfg.voice_enabled);
    }

    #[test]
    fn test_default_name_is_xenon() {
        assert_eq!(XenonConfig::default().pet_name, "Xenon");
        assert_eq!(XenonConfig::default().wake_word, "Xenon");
    }
}