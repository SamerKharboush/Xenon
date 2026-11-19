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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XenonConfig {
    pub pet_name: String,
    #[serde(flatten)]
    pub provider: ProviderConfig,
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

impl Default for XenonConfig {
    fn default() -> Self {
        Self {
            pet_name: "Xenon".to_string(),
            provider: ProviderConfig::default(),
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