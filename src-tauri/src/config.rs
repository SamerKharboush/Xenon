use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process sequence so concurrent/repeated saves never collide on a temp
/// file name (`path.with_extension` only sees pid, which is constant).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique temp path next to *path*: `<stem>.tmp.<pid>.<seq>`. Two saves in a
/// row get different names, so neither can hit `AlreadyExists` on the other's
/// leftover or an in-flight file.
pub(crate) fn unique_tmp_path(path: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
    path.with_extension(format!("json.tmp.{}.{}", std::process::id(), seq))
}

/// Create *tmp* exclusively (never truncating an existing file) with mode 0600
/// on Unix, write *content*, flush it to stable storage (F_FULLFSYNC on macOS,
/// `sync_all` elsewhere), and return the file.
pub(crate) fn write_tmp_file(tmp: &Path, content: &[u8]) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let mut f = fs::OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create_new(true)
        .open(tmp)?;
    #[cfg(not(unix))]
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    use std::io::Write;
    f.write_all(content)?;
    f.sync_all()?;
    fsync_file(&f)?;
    Ok(f)
}

/// macOS: force the file's data to stable storage (F_FULLFSYNC). Elsewhere
/// `sync_all` already did that.
#[cfg(target_os = "macos")]
fn fsync_file(f: &fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn fsync_file(_f: &fs::File) -> std::io::Result<()> {
    Ok(())
}

/// Flush the directory holding *path* so a preceding rename is durable. A
/// failure is a real durability error and must be surfaced to the caller.
pub(crate) fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

/// Default whisper STT model path: on Apple Silicon the large turbo model is
/// fast enough (Metal); on Intel the GPU backend is broken for large models,
/// so use base.en (CPU) instead. First candidate that exists, else a placeholder.
fn default_whisper_model_path() -> String {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if cfg!(target_arch = "aarch64") {
        candidates.push(home_dir().join("models/whisper/ggml-large-v3-turbo.bin"));
    }
    candidates.push(app_support_models().join("ggml-base.en.bin"));
    candidates.push(home_dir().join("models/whisper/ggml-large-v3-turbo.bin"));
    for c in &candidates {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| app_support_models().join("ggml-base.en.bin"))
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

/// Models directory, shared with the voice lane.
pub(crate) fn models_dir() -> PathBuf {
    app_support_models()
}

/// Default neural voice directory, populated by scripts/fetch-models.sh.
/// Prefer the multi-language Kokoro (ships lexicons); v0.19 is the fallback.
pub(crate) fn default_tts_model_dir() -> String {
    for name in ["kokoro-multi-lang-v1_0", "kokoro-en-v0_19"] {
        let dir = app_support_models().join(name);
        if dir.join("model.onnx").exists() {
            return dir.to_string_lossy().to_string();
        }
    }
    app_support_models()
        .join("kokoro-multi-lang-v1_0")
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
    /// Model understands OpenAI-style `tools` (Provider Hub Test probes this).
    /// `false` makes the agent use the JSON text-action protocol instead.
    #[serde(default = "default_true")]
    pub supports_tools: bool,
    /// Model accepts image parts (screenshots). Needed for grounding.
    /// Defaults to `false`: capability is proven only by a Hub Test.
    #[serde(default)]
    pub supports_vision: bool,
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
            supports_tools: true,
            supports_vision: false,
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

/// One MCP (Model Context Protocol) server the user enabled. Secrets (env
/// values, HTTP headers) are never stored here: `env_secrets` / `header_secrets`
/// name the keys whose values live in the Keychain (`mcp:<server>:<NAME>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// "stdio" or "http".
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Plain env values (safe to keep in config.json).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Env var names whose values live in the Keychain, not in config.json.
    #[serde(default)]
    pub env_secrets: Vec<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Header names whose values live in the Keychain.
    #[serde(default)]
    pub header_secrets: Vec<String>,
    /// "manual", or the app this server was imported from ("claude-desktop"…).
    #[serde(default = "default_mcp_source")]
    pub source: String,
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

fn default_mcp_source() -> String {
    "manual".to_string()
}

impl Default for McpServer {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            transport: default_mcp_transport(),
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            env_secrets: Vec::new(),
            url: String::new(),
            headers: Default::default(),
            header_secrets: Vec::new(),
            source: default_mcp_source(),
        }
    }
}

/// Which profile fills which agent role. Empty string = use the primary
/// (first in fallback order) profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRoles {
    /// Planning / plain chat model.
    #[serde(default)]
    pub chat: String,
    /// Agent loop model (native tool calls or JSON text actions).
    #[serde(default)]
    pub agent: String,
    /// Grounding / vision model used to locate elements on a screenshot.
    #[serde(default)]
    pub vision: String,
}

/// The three agent roles a profile can be assigned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Chat,
    Agent,
    Vision,
}

impl Role {
    fn field(self, roles: &AgentRoles) -> &str {
        match self {
            Role::Chat => &roles.chat,
            Role::Agent => &roles.agent,
            Role::Vision => &roles.vision,
        }
    }
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
    /// "hotkey" (default for existing configs), "always", or "wake_word"
    /// (default for new configs).
    #[serde(default = "default_listen_mode_serde")]
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
    /// whisper GPU on Apple Silicon. `None` = auto (GPU only on aarch64).
    #[serde(default)]
    pub stt_use_gpu: Option<bool>,
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
    /// Ship the 320x320 sprites and decode rare poses on first use. `false`
    /// restores the 448x448 originals, all preloaded (Settings -> Performance).
    #[serde(default = "default_true")]
    pub light_sprites: bool,
    /// Open the mic/VAD/TTS only when needed instead of at startup.
    /// `false` preloads them (Settings -> Performance).
    #[serde(default = "default_true")]
    pub lazy_audio: bool,
    /// Draw Idle at 15 FPS instead of 60 (Settings -> Performance).
    #[serde(default = "default_true")]
    pub idle_power_saver: bool,

    // --- release ---
    /// Check GitHub Releases for a newer Xenon build (Settings -> Updates).
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
    /// Unix timestamp (seconds) of the last update check; 0 = never.
    #[serde(default)]
    pub last_update_check: i64,
    /// Version the user chose to skip (e.g. "v0.3.0"); None = skip nothing.
    #[serde(default)]
    pub skipped_version: Option<String>,
    /// Onboarding dismissed for this app version. Empty = not dismissed.
    #[serde(default)]
    pub onboarding_dismissed_for_version: String,
    // --- agent ---
    /// Provider profile per agent role (chat / agent / vision). Empty = primary.
    #[serde(default)]
    pub roles: AgentRoles,
    /// `run_script` (AppleScript / shell) tool. Off by default; when on, every
    /// call is confirmed by the user in a native dialog.
    #[serde(default)]
    pub allow_scripts: bool,
    /// `browser_eval` (arbitrary JavaScript through Apple Events). Off by
    /// default: it needs a per-browser macOS permission the user must grant.
    #[serde(default)]
    pub browser_eval_enabled: bool,
    // --- voice ---
    /// Wake phrase. Empty means the built-in default: "hey {pet_name}", which
    /// live-updates when the pet is renamed. A non-empty value is treated as
    /// a user customisation and never rewritten.
    #[serde(default)]
    pub wake_phrase: String,
    /// Wake-match tolerance, 0.0 (exact) .. 1.0 (loose).
    #[serde(default = "default_wake_sensitivity")]
    pub wake_sensitivity: f32,
    /// Play a soft chime when the wake word fires.
    #[serde(default = "default_true")]
    pub wake_chime: bool,
    /// Single click on the pet starts listening instead of the old tap greet.
    #[serde(default = "default_true")]
    pub click_to_talk: bool,
    /// Small whisper model used only for wake-word spotting. Falls back to
    /// `whisper_model_path` when this file is absent.
    #[serde(default = "default_wake_model_path")]
    pub wake_model_path: String,
    /// macOS system voice for the `say` fallback (empty = system default).
    #[serde(default)]
    pub system_voice: String,
    // --- tools ---
    /// MCP servers the user enabled. Empty = built-in tools only.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// Start enabled MCP servers at launch instead of on the first agent turn.
    #[serde(default)]
    pub mcp_start_at_launch: bool,
    /// Cap on MCP tools sent per request (built-ins are never dropped).
    #[serde(default = "default_max_mcp_tools")]
    pub max_mcp_tools: usize,
    /// Built-in web tools (web_search / web_extract / web_research / …).
    #[serde(default = "default_true")]
    pub web_tools_enabled: bool,
    /// Optional SearXNG base URL used before the public fallbacks.
    #[serde(default)]
    pub searxng_url: String,
    /// Search API keys live in the Keychain; these hold only the fact that one
    /// is configured (`true`) so Settings can show "key set" without a value.
    #[serde(default)]
    pub brave_key_set: bool,
    #[serde(default)]
    pub tavily_key_set: bool,
    #[serde(default)]
    pub serper_key_set: bool,
    /// Accounts whose plaintext secret could not be persisted to the Keychain
    /// or fallback file (migration failed). In-memory only: `without_secrets`
    /// keeps these values so a later `save` cannot drop them. Cleared as each
    /// secret is stored successfully.
    #[serde(skip)]
    pub unpersisted_secrets: std::collections::HashSet<String>,
}

fn default_max_mcp_tools() -> usize {
    40
}

fn default_true() -> bool {
    true
}

fn default_listen_mode() -> String {
    // New configs (Default::default) get the wake word; old config files that
    // lack the field use `default_listen_mode_serde` below so they keep the
    // behaviour they already had.
    "wake_word".to_string()
}
fn default_listen_mode_serde() -> String {
    "hotkey".to_string()
}
fn default_wake_sensitivity() -> f32 {
    0.5
}
fn default_wake_model_path() -> String {
    // Same resolution as the app: tiny.en (smallest), then base.en, then the
    // main STT model.
    let candidates = [
        app_support_models().join("ggml-tiny.en.bin"),
        app_support_models().join("ggml-base.en.bin"),
        home_dir().join("models/whisper/ggml-large-v3-turbo.bin"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.to_string_lossy().to_string();
        }
    }
    candidates[0].to_string_lossy().to_string()
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
            system_prompt: "You are Xenon, a witty and intelligent desktop companion. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural.".to_string(),
            listen_mode: default_listen_mode(),
            language: default_language(),
            silence_ms: default_silence_ms(),
            stt_unload_minutes: default_stt_unload_minutes(),
            stt_use_gpu: None,
            whisper_model_path: default_whisper_model_path(),
            vad_model_path: default_vad_model_path(),
            tts_enabled: default_tts_enabled(),
            tts_model_dir: default_tts_model_dir(),
            tts_kind: String::new(),
            tts_speaker_id: 0,
            tts_speed: default_tts_speed(),
            tts_unload_minutes: default_tts_unload_minutes(),
            light_sprites: true,
            lazy_audio: true,
            idle_power_saver: true,
            // release
            auto_check_updates: true,
            last_update_check: 0,
            skipped_version: None,
            onboarding_dismissed_for_version: String::new(),
            roles: AgentRoles::default(),
            allow_scripts: false,
            browser_eval_enabled: false,
            wake_phrase: String::new(),
            wake_sensitivity: default_wake_sensitivity(),
            wake_chime: true,
            click_to_talk: true,
            wake_model_path: default_wake_model_path(),
            system_voice: String::new(),
            mcp_servers: Vec::new(),
            mcp_start_at_launch: false,
            max_mcp_tools: default_max_mcp_tools(),
            web_tools_enabled: true,
            searxng_url: String::new(),
            brave_key_set: false,
            tavily_key_set: false,
            serper_key_set: false,
            unpersisted_secrets: std::collections::HashSet::new(),
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
            // Plaintext secrets written by older builds move into the
            // Keychain; then every profile gets its key back from there.
            // If any were actually stored, rewrite config.json immediately so
            // the stored plaintext does not linger until the next Settings
            // save. Secrets that failed to store stay in the file (tracked in
            // `unpersisted_secrets`) so no value is lost.
            // Use one store for both migration and hydration (shared cache).
            let store = crate::hub::KeychainStore::for_migration();
            store.migrate_legacy_accounts();
            let outcome = crate::hub::migrate_plaintext_checked(&mut cfg, &store);
            for account in &outcome.failed {
                cfg.unpersisted_secrets.insert(account.clone());
            }
            if !outcome.stored.is_empty() {
                if let Err(e) = cfg.save_to(&path) {
                    log::warn!("failed to rewrite config after secret migration: {e}");
                }
            }
            crate::hub::hydrate(&mut cfg, &store);
            cfg
        } else {
            Self::default()
        }
    }

    /// Copy with every secret removed, so `save` can never write a secret to
    /// disk (provider keys and MCP secret values live in the macOS Keychain).
    ///
    /// Exceptions: accounts in `unpersisted_secrets` (a migration/store write
    /// failed) keep their plaintext value, because it exists nowhere else.
    pub(crate) fn without_secrets(&self) -> Self {
        let mut c = self.clone();
        let unpersisted = &c.unpersisted_secrets;
        if !unpersisted.contains(crate::hub::PRIMARY_ACCOUNT) {
            c.provider.provider_api_key = None;
        }
        for p in &mut c.providers {
            if !unpersisted.contains(&p.name) {
                p.api_key = None;
            }
        }
        for s in &mut c.mcp_servers {
            for name in &s.env_secrets {
                let account = crate::mcp::secret_account(&s.name, name);
                if !unpersisted.contains(&account) {
                    s.env.remove(name);
                }
            }
            for name in &s.header_secrets {
                let account = crate::mcp::secret_account(&s.name, name);
                if !unpersisted.contains(&account) {
                    s.headers.remove(name);
                }
            }
        }
        c
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::config_path())
    }

    /// Atomic, 0600-from-creation config write: a unique temp file in the same
    /// directory, fully written and flushed to stable storage, then renamed
    /// into place; finally the directory entry is flushed so the rename
    /// survives a power loss.
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content =
            serde_json::to_string_pretty(&self.without_secrets()).map_err(|e| e.to_string())?;
        let tmp = unique_tmp_path(path);
        let write_res = write_tmp_file(&tmp, content.as_bytes()).map(|_| ());
        match write_res {
            Ok(()) => match fs::rename(&tmp, path) {
                Ok(()) => {
                    // Flush the directory entry (the rename) as well.
                    if let Some(parent) = path.parent() {
                        if let Err(e) = sync_parent_dir(parent) {
                            log::warn!("config dir sync failed: {e}");
                            return Err(e.to_string());
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(e.to_string())
                }
            },
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.to_string())
            }
        }
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

        // Migrate old Jarvis default to new Xenon default.
        const OLD_DEFAULT: &str = "You are Xenon, a witty and intelligent desktop companion inspired by Jarvis. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural.";
        if self.system_prompt == OLD_DEFAULT {
            self.system_prompt = "You are Xenon, a witty and intelligent desktop companion. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural.".to_string();
        }
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

    /// Profile assigned to *role*, or the primary profile when unassigned.
    pub fn role_profile(&self, role: Role) -> ProviderProfile {
        let name = role.field(&self.roles);
        self.profiles_named(name)
            .or_else(|| Some(self.primary()))
            .unwrap_or_else(|| ProviderProfile::from_flat(&self.provider))
    }

    /// All profiles for *role*: the assigned profile first (so it is tried
    /// before the fallbacks), then the rest in fallback order.
    pub fn role_profiles(&self, role: Role) -> Vec<ProviderProfile> {
        let chosen = self.role_profile(role);
        let mut out = vec![chosen.clone()];
        for p in self.ordered_profiles() {
            if p.name != chosen.name {
                out.push(p);
            }
        }
        out
    }

    fn profiles_named(&self, name: &str) -> Option<ProviderProfile> {
        if name.trim().is_empty() {
            return None;
        }
        self.providers.iter().find(|p| p.name == name).cloned()
    }

    /// True when *role*'s model is known not to support native tool calling.
    pub fn role_lacks_tools(&self, role: Role) -> bool {
        !self.role_profile(role).supports_tools
    }

    /// The phrase the wake detector listens for. An empty `wake_phrase` means
    /// "hey {pet_name}", so renaming the pet updates it automatically; a
    /// non-empty value is the user's own phrase and is left alone.
    pub fn effective_wake_phrase(&self) -> String {
        let phrase = self.wake_phrase.trim();
        if phrase.is_empty() {
            format!("hey {}", self.pet_name)
        } else {
            phrase.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use crate::hub::SecretStore;

    #[derive(Default)]
    struct FakeStore {
        secrets: Mutex<std::collections::HashMap<String, String>>,
        fail: bool,
        /// Account ids whose `set` fails (all fail when `fail` is true).
        fail_accounts: std::collections::HashSet<String>,
    }

    impl SecretStore for FakeStore {
        fn get(&self, account: &str) -> Option<String> {
            self.secrets.lock().unwrap().get(account).cloned()
        }
        fn set(&self, account: &str, secret: &str) -> bool {
            if self.fail || self.fail_accounts.contains(account) {
                return false;
            }
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

    fn temp_config_dir() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xenon-cfg-migrate-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plaintext_config_json() -> &'static str {
        r#"{
            "pet_name": "Xenon",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": "sk-plaintext",
            "model_name": "m",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi",
            "mcp_servers": [{
                "name": "demo",
                "transport": "stdio",
                "command": "demo",
                "env": {"PLAIN": "keep", "API_TOKEN": "sk-env"},
                "env_secrets": ["API_TOKEN"],
                "headers": {"X-Key": "sk-header"},
                "header_secrets": ["X-Key"]
            }]
        }"#
    }

    fn load_temp_config(path: &Path) -> XenonConfig {
        let mut cfg: XenonConfig = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        cfg.migrate();
        cfg
    }

    /// Legacy plaintext provider + MCP secrets move to the store, and load
    /// rewrites config.json immediately instead of waiting for Settings save.
    #[cfg(unix)]
    #[test]
    fn test_migration_strips_plaintext_and_rewrites_config_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_config_dir();
        let path = dir.join("config.json");
        fs::write(&path, plaintext_config_json()).unwrap();
        let mut cfg = load_temp_config(&path);
        let store = FakeStore::default();

        assert!(crate::hub::migrate_plaintext(&mut cfg, &store));
        cfg.save_to(&path).unwrap();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("sk-plaintext"));
        assert!(!on_disk.contains("sk-env"));
        assert!(!on_disk.contains("sk-header"));
        assert!(on_disk.contains("keep"));
        assert_eq!(store.get("Primary").as_deref(), Some("sk-plaintext"));
        assert_eq!(store.get("mcp:demo:API_TOKEN").as_deref(), Some("sk-env"));
        assert_eq!(store.get("mcp:demo:X-Key").as_deref(), Some("sk-header"));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    /// If the store refuses a secret, migration reports nothing migrated and
    /// the plaintext remains on disk (no data loss).
    #[test]
    fn test_migration_store_failure_keeps_plaintext() {
        let dir = temp_config_dir();
        let path = dir.join("config.json");
        fs::write(&path, plaintext_config_json()).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let mut cfg = load_temp_config(&path);
        let store = FakeStore {
            fail: true,
            ..Default::default()
        };

        assert!(!crate::hub::migrate_plaintext(&mut cfg, &store));

        // Load only rewrites when migration succeeded.
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("sk-plaintext"));
        assert!(on_disk.contains("sk-env"));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        assert_eq!(cfg.providers[0].api_key.as_deref(), Some("sk-plaintext"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Partial migration: the secrets that stored are scrubbed from the file,
    /// the ones that failed stay (no data loss), and `without_secrets` keeps
    /// exactly the unpersisted values on the next save.
    #[test]
    fn test_partial_migration_scrubs_stored_keeps_failed() {
        let dir = temp_config_dir();
        let path = dir.join("config.json");
        fs::write(&path, plaintext_config_json()).unwrap();
        let mut cfg = load_temp_config(&path);
        // Only the env secret fails to store.
        let store = FakeStore {
            fail_accounts: ["mcp:demo:API_TOKEN".to_string()].into_iter().collect(),
            ..Default::default()
        };

        let outcome = crate::hub::migrate_plaintext_checked(&mut cfg, &store);
        assert!(outcome.stored.contains(&"Primary".to_string()));
        assert!(outcome.failed.contains(&"mcp:demo:API_TOKEN".to_string()));

        // Emulate load(): record failures as unpersisted, then rewrite.
        for account in &outcome.failed {
            cfg.unpersisted_secrets.insert(account.clone());
        }
        cfg.save_to(&path).unwrap();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("sk-plaintext"), "stored provider key must be scrubbed");
        assert!(!on_disk.contains("sk-header"), "stored header secret must be scrubbed");
        assert!(on_disk.contains("sk-env"), "failed env secret must survive");
        assert!(on_disk.contains("keep"));

        // In-memory without_secrets keeps the failed value too.
        let stripped = cfg.without_secrets();
        let demo = stripped.mcp_servers.iter().find(|s| s.name == "demo").unwrap();
        assert_eq!(demo.env.get("API_TOKEN").map(String::as_str), Some("sk-env"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A value that failed to persist must survive later saves, and stop being
    /// kept once a later store succeeds.
    #[test]
    fn test_unpersisted_value_survives_later_saves() {
        let mut cfg: XenonConfig = serde_json::from_str(plaintext_config_json()).unwrap();
        cfg.migrate();
        cfg.unpersisted_secrets.insert("Primary".to_string());
        assert_eq!(
            cfg.without_secrets().provider.provider_api_key.as_deref(),
            Some("sk-plaintext"),
            "unpersisted provider key must be kept"
        );

        // A later successful store of the same account clears the marker.
        cfg.unpersisted_secrets.remove("Primary");
        assert!(cfg.without_secrets().provider.provider_api_key.is_none());
    }

    /// F3: two saves in a row both succeed (unique temp names) and leave no
    /// `.tmp` files behind.
    #[test]
    fn test_save_to_unique_temp_names_no_leftovers() {
        let dir = temp_config_dir();
        let path = dir.join("config.json");
        let cfg = XenonConfig::default();
        cfg.save_to(&path).expect("first save");
        cfg.save_to(&path).expect("second save");
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Already-clean configs are never rewritten (mtime unchanged).
    #[test]
    fn test_no_plaintext_leaves_config_untouched() {
        let dir = temp_config_dir();
        let path = dir.join("config.json");
        let clean = plaintext_config_json()
            .replace("\"sk-plaintext\"", "null")
            .replace("\"API_TOKEN\": \"sk-env\"", "\"PLAIN2\": \"safe\"")
            .replace("\"env_secrets\": [\"API_TOKEN\"]", "\"env_secrets\": []")
            .replace("\"X-Key\": \"sk-header\"", "\"X-Plain\": \"safe\"")
            .replace("\"header_secrets\": [\"X-Key\"]", "\"header_secrets\": []");
        fs::write(&path, clean).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let mut cfg = load_temp_config(&path);
        let store = FakeStore::default();

        assert!(!crate::hub::migrate_plaintext(&mut cfg, &store));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Settings must survive a save/load round-trip, otherwise renaming the pet
    /// or pointing at a new provider silently does nothing.
    #[test]
    fn test_config_round_trip() {
        let dir = temp_config_dir();
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

    /// The three Performance toggles default to on (budget-meeting config).
    #[test]
    fn test_performance_defaults_are_on() {
        let cfg = XenonConfig::default();
        assert!(cfg.light_sprites);
        assert!(cfg.lazy_audio);
        assert!(cfg.idle_power_saver);
    }

    /// Old configs with the Jarvis default system prompt must be migrated to
    /// the new Xenon default on load.
    #[test]
    fn test_old_jarvis_default_prompt_is_migrated() {
        let old = r#"{
            "pet_name": "Jarvis",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": null,
            "model_name": "qwen3",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "You are Xenon, a witty and intelligent desktop companion inspired by Jarvis. Be helpful, concise, and occasionally playful. You can control the computer, answer questions, and assist with tasks. Keep responses conversational and natural."
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(old).unwrap();
        cfg.migrate();
        assert!(
            !cfg.system_prompt.contains("Jarvis"),
            "old Jarvis default should be replaced: {}",
            cfg.system_prompt
        );
        assert!(cfg.system_prompt.starts_with("You are Xenon"));
    }

    /// Custom system prompts that are not the old default must NOT be touched.
    #[test]
    fn test_custom_prompt_not_migrated() {
        let old = r#"{
            "pet_name": "Xenon",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": null,
            "model_name": "qwen3",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "You are a helpful pirate. Arrr."
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(old).unwrap();
        cfg.migrate();
        assert_eq!(cfg.system_prompt, "You are a helpful pirate. Arrr.");
    }

    /// Config files written before the Performance section existed must load
    /// with the toggles on rather than failing or defaulting to false.
    #[test]
    fn test_old_config_without_performance_defaults_to_on() {
        let old = r#"{
            "pet_name": "Jarvis",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": null,
            "model_name": "qwen3",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let cfg: XenonConfig = serde_json::from_str(old).expect("old config must deserialize");
        assert_eq!(cfg.pet_name, "Jarvis");
        assert!(cfg.light_sprites, "light_sprites must default to true");
        assert!(cfg.lazy_audio, "lazy_audio must default to true");
        assert!(cfg.idle_power_saver, "idle_power_saver must default to true");
        // release lane fields: old configs must default cleanly too.
        assert!(cfg.auto_check_updates, "auto_check_updates must default to true");
        assert_eq!(cfg.last_update_check, 0);
        assert!(cfg.skipped_version.is_none());
    }

    /// New configs get the wake-word lane; old configs keep their hotkey mode.
    #[test]
    fn test_listen_mode_new_vs_old() {
        assert_eq!(XenonConfig::default().listen_mode, "wake_word");
        let old = r#"{
            "pet_name": "Jarvis",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": null,
            "model_name": "qwen3",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let cfg: XenonConfig = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.listen_mode, "hotkey");
    }

    /// Voice-lane defaults apply to old configs and empty wake phrases follow
    /// the pet name live.
    #[test]
    fn test_voice_defaults_and_effective_phrase() {
        let old = r#"{
            "pet_name": "Jarvis",
            "provider_url": "http://127.0.0.1:8080",
            "provider_api_key": null,
            "model_name": "qwen3",
            "voice_enabled": true,
            "wake_word_enabled": true,
            "wake_word": "Xenon",
            "system_prompt": "hi"
        }"#;
        let mut cfg: XenonConfig = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.wake_phrase, "");
        assert_eq!(cfg.wake_sensitivity, 0.5);
        assert!(cfg.wake_chime);
        assert!(cfg.click_to_talk);
        assert_eq!(cfg.effective_wake_phrase(), "hey Jarvis");
        cfg.pet_name = "Nova".into();
        assert_eq!(cfg.effective_wake_phrase(), "hey Nova");
        cfg.wake_phrase = "yo computer".into();
        assert_eq!(cfg.effective_wake_phrase(), "yo computer");
    }
}