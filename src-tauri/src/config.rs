use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Default whisper STT model path: the first candidate that exists, else the
/// first candidate (so the UI shows a meaningful placeholder).
fn default_whisper_model_path() -> String {
    for c in [
        home_dir().join("models/whisper/ggml-large-v3-turbo.bin"),
        app_support_models().join("ggml-base.en.bin"),
    ] {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    home_dir()
        .join("models/whisper/ggml-large-v3-turbo.bin")
        .to_string_lossy()
        .to_string()
}

/// Default Silero VAD model path (whisper.cpp ggml format).
fn default_vad_model_path() -> String {
    for c in [
        home_dir().join("models/whisper/ggml-silero-v6.2.0.bin"),
        app_support_models().join("ggml-silero-v6.2.0.bin"),
    ] {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    home_dir()
        .join("models/whisper/ggml-silero-v6.2.0.bin")
        .to_string_lossy()
        .to_string()
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn app_support_models() -> PathBuf {
    // ~/Library/Application Support/com.xenon.app/models
    home_dir()
        .join("Library/Application Support/com.xenon.app/models")
}

/// Default neural voice directory, populated by scripts/fetch-models.sh.
fn default_tts_model_dir() -> String {
    app_support_models()
        .join("kokoro-en-v0_19")
        .to_string_lossy()
        .to_string()
}

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

/// One OpenAI-compatible endpoint in the Provider Hub. `vision_model` is the
/// VLM used for Grounding; it defaults to `model` when blank.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub vision_model: String,
}

impl ProviderProfile {
    #[allow(dead_code)]
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        vision_model: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            vision_model: vision_model.into(),
        }
    }

    fn from_flat(flat: &ProviderConfig) -> Self {
        Self::new(
            "Primary",
            flat.provider_url.clone(),
            flat.provider_api_key.clone(),
            flat.model_name.clone(),
            flat.model_name.clone(),
        )
    }

    /// Effective VLM: explicit vision model, else the chat model.
    pub fn effective_vision_model(&self) -> &str {
        if self.vision_model.trim().is_empty() {
            &self.model
        } else {
            &self.vision_model
        }
    }
}

/// Built-in presets for the Provider Hub (PRD §3.4). Base URLs include their
/// own version segment — `normalize_base_url` leaves those untouched.
#[allow(dead_code)]
pub fn preset_profiles() -> Vec<ProviderProfile> {
    vec![
        ProviderProfile::new("Ollama", "http://localhost:11434/v1", None, "llama3.2", "llava"),
        ProviderProfile::new("LM Studio", "http://localhost:1234/v1", None, "local-model", "local-model"),
        ProviderProfile::new("OpenAI", "https://api.openai.com/v1", None, "gpt-4o-mini", "gpt-4o-mini"),
        ProviderProfile::new(
            "Gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            None,
            "gemini-2.0-flash",
            "gemini-2.0-flash",
        ),
        ProviderProfile::new(
            "Anthropic",
            "https://api.anthropic.com/v1",
            None,
            "claude-3-5-sonnet-20241022",
            "claude-3-5-sonnet-20241022",
        ),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XenonConfig {
    pub pet_name: String,
    #[serde(flatten)]
    pub provider: ProviderConfig,
    /// Provider Hub profiles. Empty means "use the flat `provider` config";
    /// `load()` migrates that into a single profile on first run.
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
    /// Order in which profiles are tried as fallbacks. Empty means
    /// "the `providers` list order".
    #[serde(default)]
    pub fallback_order: Vec<String>,
    /// Maximum ReAct iterations per utterance.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    pub voice_enabled: bool,
    pub wake_word_enabled: bool,
    pub wake_word: String,
    pub system_prompt: String,
    /// "hotkey" (default) or "always".
    #[serde(default = "default_listen_mode")]
    pub listen_mode: String,
    /// STT language code, or "auto"/"" to auto-detect.
    #[serde(default = "default_language")]
    pub language: String,
    /// Trailing silence (ms) that ends a speech segment.
    #[serde(default = "default_silence_ms")]
    pub silence_ms: u64,
    /// Whisper model is unloaded from RAM after this many idle minutes.
    #[serde(default = "default_stt_unload_minutes")]
    pub stt_unload_minutes: u64,
    /// Path to the ggml whisper STT model.
    #[serde(default = "default_whisper_model_path")]
    pub whisper_model_path: String,
    /// Path to the ggml Silero VAD model.
    #[serde(default = "default_vad_model_path")]
    pub vad_model_path: String,
    /// Neural voice output (TTS) enabled.
    #[serde(default = "default_tts_enabled")]
    pub tts_enabled: bool,
    /// Directory containing the neural voice model (Kokoro or VITS/Piper).
    #[serde(default = "default_tts_model_dir")]
    pub tts_model_dir: String,
    /// "kokoro", "vits", or "" to autodetect from the model directory.
    #[serde(default)]
    pub tts_kind: String,
    /// Voice/speaker id within the model.
    #[serde(default)]
    pub tts_speaker_id: u32,
    /// Synthesis speed multiplier.
    #[serde(default = "default_tts_speed")]
    pub tts_speed: f32,
    /// TTS model is unloaded from RAM after this many idle minutes.
    #[serde(default = "default_tts_unload_minutes")]
    pub tts_unload_minutes: u64,
}

fn default_listen_mode() -> String {
    "hotkey".to_string()
}
fn default_language() -> String {
    "en".to_string()
}
fn default_silence_ms() -> u64 {
    700
}
fn default_stt_unload_minutes() -> u64 {
    5
}
fn default_tts_enabled() -> bool {
    true
}
fn default_tts_speed() -> f32 {
    1.0
}
fn default_tts_unload_minutes() -> u64 {
    5
}
fn default_max_steps() -> usize {
    15
}

impl Default for XenonConfig {
    fn default() -> Self {
        Self {
            pet_name: "Xenon".to_string(),
            provider: ProviderConfig::default(),
            providers: vec![],
            fallback_order: vec![],
            max_steps: default_max_steps(),
            voice_enabled: true,
            wake_word_enabled: true,
            wake_word: "Xenon".to_string(),
            system_prompt: "You are Xenon, a witty and intelligent desktop companion inspired by Jarvis. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural.".to_string(),
            listen_mode: default_listen_mode(),
            language: default_language(),
            silence_ms: default_silence_ms(),
            stt_unload_minutes: default_stt_unload_minutes(),
            whisper_model_path: default_whisper_model_path(),
            vad_model_path: default_vad_model_path(),
            tts_enabled: default_tts_enabled(),
            tts_model_dir: default_tts_model_dir(),
            tts_kind: String::new(),
            tts_speaker_id: 0,
            tts_speed: default_tts_speed(),
            tts_unload_minutes: default_tts_unload_minutes(),
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
            let mut cfg: XenonConfig = match fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str(&content) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        // A corrupt config would silently wipe every setting
                        // (API keys, provider profiles, hotkeys) on
                        // unwrap_or_default. Keep a copy for manual recovery
                        // and start fresh instead.
                        let backup = path.with_extension("json.bak");
                        let _ = fs::copy(&path, &backup);
                        log::error!(
                            "config parse failed ({e}); backed up to {} and using defaults",
                            backup.display()
                        );
                        Self::default()
                    }
                },
                Err(e) => {
                    log::error!("config read failed ({e}); using defaults");
                    Self::default()
                }
            };
            cfg.migrate();
            cfg
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

    /// Backfill fields that are empty on first load: flat provider config
    /// becomes the first profile, fallback order mirrors the list, blank vision
    /// models fall back to their chat model.
    pub fn migrate(&mut self) {
        if self.providers.is_empty() {
            self.providers = vec![ProviderProfile::from_flat(&self.provider)];
        }
        if self.fallback_order.is_empty() {
            self.fallback_order = self.providers.iter().map(|p| p.name.clone()).collect();
        }
        for p in &mut self.providers {
            if p.vision_model.trim().is_empty() {
                p.vision_model = p.model.clone();
            }
            if p.base_url.trim().is_empty() {
                p.base_url = self.provider.provider_url.clone();
            }
        }
        // Any profile not named in fallback_order goes last, in list order.
        for p in &self.providers {
            if !self.fallback_order.contains(&p.name) {
                self.fallback_order.push(p.name.clone());
            }
        }
        self.sync_flat_from_primary();
    }

    /// Keep the legacy flat fields mirroring the primary profile so old
    /// config readers (and tests) still see sensible values.
    fn sync_flat_from_primary(&mut self) {
        if let Some(p) = self.providers.first() {
            self.provider.provider_url = p.base_url.clone();
            self.provider.provider_api_key = p.api_key.clone();
            self.provider.model_name = p.model.clone();
        }
    }

    /// Profiles in fallback order (names not in `fallback_order` last).
    pub fn ordered_profiles(&self) -> Vec<ProviderProfile> {
        let mut ps = self.providers.clone();
        ps.sort_by_key(|p| {
            self.fallback_order
                .iter()
                .position(|n| n == &p.name)
                .unwrap_or(usize::MAX)
        });
        ps
    }

    /// The profile the agent talks to first.
    pub fn primary(&self) -> ProviderProfile {
        self.ordered_profiles()
            .into_iter()
            .next()
            .unwrap_or_else(|| ProviderProfile::from_flat(&self.provider))
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

        let mut cfg = XenonConfig {
            pet_name: "Jarvis".to_string(),
            provider: ProviderConfig {
                provider_url: "http://127.0.0.1:8080".to_string(),
                model_name: "qwen3".to_string(),
                ..Default::default()
            },
            providers: vec![],
            fallback_order: vec![],
            ..Default::default()
        };
        // Legacy-shaped config: flat provider only, migrate into a profile.
        cfg.migrate();
        cfg.save().expect("save");

        let loaded = XenonConfig::load();
        assert_eq!(loaded.pet_name, "Jarvis");
        assert_eq!(loaded.provider.provider_url, "http://127.0.0.1:8080");
        assert_eq!(loaded.provider.model_name, "qwen3");
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].base_url, "http://127.0.0.1:8080");
        assert_eq!(loaded.providers[0].model, "qwen3");
        assert_eq!(loaded.providers[0].vision_model, "qwen3");

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
        // Not migrated yet: migration happens in load().
        assert!(cfg.providers.is_empty());
    }

    /// A legacy flat config must become a first profile on load, preserving the
    /// endpoint and key, and mirroring a vision model.
    #[test]
    fn test_config_migrates_flat_provider_into_profile() {
        let json = r#"{
            "pet_name": "Xenon",
            "provider_url": "http://127.0.0.1:9999",
            "provider_api_key": "sk-test",
            "model_name": "legacy-model",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "be nice"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(json).unwrap();
        cfg.migrate();

        assert_eq!(cfg.providers.len(), 1);
        let p = &cfg.providers[0];
        assert_eq!(p.base_url, "http://127.0.0.1:9999");
        assert_eq!(p.api_key.as_deref(), Some("sk-test"));
        assert_eq!(p.model, "legacy-model");
        assert_eq!(p.vision_model, "legacy-model");
        assert_eq!(cfg.fallback_order, vec!["Primary".to_string()]);
        // Flat fields stay in sync with the primary profile.
        assert_eq!(cfg.provider.provider_url, "http://127.0.0.1:9999");
    }

    /// fallback_order wins over list order; profiles missing from it go last.
    #[test]
    fn test_ordered_profiles_honors_fallback_order() {
        let cfg = XenonConfig {
            providers: vec![
                ProviderProfile::new("a", "http://a/v1", None, "m", "m"),
                ProviderProfile::new("b", "http://b/v1", None, "m", "m"),
                ProviderProfile::new("c", "http://c/v1", None, "m", "m"),
            ],
            fallback_order: vec!["c".into(), "a".into()],
            ..Default::default()
        };
        let order: Vec<String> = cfg.ordered_profiles().into_iter().map(|p| p.name).collect();
        assert_eq!(order, vec!["c", "a", "b"]);
        assert_eq!(cfg.primary().name, "c");
    }

    #[test]
    fn test_default_max_steps_is_15() {
        assert_eq!(XenonConfig::default().max_steps, 15);
        assert_eq!(default_max_steps(), 15);
    }

    #[test]
    fn test_default_name_is_xenon() {
        assert_eq!(XenonConfig::default().pet_name, "Xenon");
        assert_eq!(XenonConfig::default().wake_word, "Xenon");
    }
}