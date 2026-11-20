//! Voice lane: wake-word matching, the voice picker catalog, custom-voice
//! import, and experimental ZipVoice cloning.
//!
//! Kept free of Tauri types where possible: the matcher and validator are
//! pure functions with unit tests, and the `#[tauri::command]` wrappers live
//! at the bottom.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};

use crate::config::models_dir;

/// Kokoro v0.19 speaker table (English, 11 speakers).
pub const KOKORO_V019_SPEAKERS: &[(&str, u32)] = &[
    ("af", 0),
    ("af_bella", 1),
    ("af_nicole", 2),
    ("af_sarah", 3),
    ("af_sky", 4),
    ("am_adam", 5),
    ("am_michael", 6),
    ("bf_emma", 7),
    ("bf_isabella", 8),
    ("bm_george", 9),
    ("bm_lewis", 10),
];

/// Kokoro multi-language v1.0 speaker table (English + Chinese, 54 speakers).
pub const KOKORO_MULTI_SPEAKERS: &[(&str, u32)] = &[
    ("af_alloy", 0),
    ("af_aoede", 1),
    ("af_bella", 2),
    ("af_heart", 3),
    ("af_jessica", 4),
    ("af_kore", 5),
    ("af_nicole", 6),
    ("af_nova", 7),
    ("af_river", 8),
    ("af_sarah", 9),
    ("af_sky", 10),
    ("am_adam", 11),
    ("am_echo", 12),
    ("am_eric", 13),
    ("am_fenrir", 14),
    ("am_liam", 15),
    ("am_michael", 16),
    ("am_onyx", 17),
    ("am_puck", 18),
    ("am_santa", 19),
    ("bf_alice", 20),
    ("bf_emma", 21),
    ("bf_isabella", 22),
    ("bf_lily", 23),
    ("bm_daniel", 24),
    ("bm_fable", 25),
    ("bm_george", 26),
    ("bm_lewis", 27),
    ("ef_dora", 28),
    ("em_alex", 29),
    ("ff_siwis", 30),
    ("hf_alpha", 31),
    ("hf_beta", 32),
    ("hm_omega", 33),
    ("hm_psi", 34),
    ("if_sara", 35),
    ("im_nicola", 36),
    ("jf_alpha", 37),
    ("jf_gongitsune", 38),
    ("jf_nezumi", 39),
    ("jf_tebukuro", 40),
    ("jm_kumo", 41),
    ("pf_dora", 42),
    ("pm_alex", 43),
    ("pm_santa", 44),
    ("zf_xiaobei", 45),
    ("zf_xiaoni", 46),
    ("zf_xiaoxiao", 47),
    ("zf_xiaoyi", 48),
    ("zm_yunjian", 49),
    ("zm_yunxi", 50),
    ("zm_yunxia", 51),
    ("zm_yunyang", 52),
    ("em_santa", 53),
];



// ---------------------------------------------------------------------------
// Wake-phrase matching
// ---------------------------------------------------------------------------

/// Lowercase, strip punctuation, collapse whitespace.
pub fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Classic Levenshtein distance over chars. Small inputs only (wake phrases).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// One successful wake detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeMatch {
    /// Number of leading transcript words consumed by the wake phrase.
    pub matched_words: usize,
    /// Words after the wake phrase, if the command followed in the same
    /// breath. Empty when the user only said the wake phrase.
    pub trailing: String,
}

/// Fuzzy-match `transcript` against `phrase`.
///
/// Accepts an exact word-prefix, or a near-miss prefix within an edit-distance
/// budget derived from `sensitivity` (0.0 exact .. 1.0 loose). The comparison
/// is on the first N words, so "hey xenon open safari" yields the trailing
/// "open safari", while "hey there" and "xerox" do not match "hey xenon".
pub fn match_wake(transcript: &str, phrase: &str, sensitivity: f32) -> Option<WakeMatch> {
    let phrase = normalize_text(phrase);
    if phrase.is_empty() {
        return None;
    }
    let phrase_words: Vec<&str> = phrase.split_whitespace().collect();
    let normalized = normalize_text(transcript);
    let words: Vec<&str> = normalized.split_whitespace().collect();
    if words.len() < phrase_words.len() {
        return None;
    }
    let budget = wake_distance_budget(sensitivity);

    for (word, expected) in words[..phrase_words.len()]
        .iter()
        .zip(phrase_words.iter())
    {
        if levenshtein(word, expected) > budget {
            return None;
        }
    }

    let trailing = if words.len() > phrase_words.len() {
        words[phrase_words.len()..].join(" ")
    } else {
        String::new()
    };
    Some(WakeMatch {
        matched_words: phrase_words.len(),
        trailing,
    })
}

/// How many edits the matcher tolerates at a given sensitivity.
pub fn wake_distance_budget(sensitivity: f32) -> usize {
    let s = sensitivity.clamp(0.0, 1.0);
    if s >= 0.85 {
        4
    } else if s >= 0.35 {
        2
    } else {
        0
    }
}

/// Remove a leading wake phrase from `transcript`, returning the command.
/// Falls back to [`match_wake`] when the caller does not have a `WakeMatch`.
#[cfg(test)]
pub fn strip_wake(transcript: &str, phrase: &str, sensitivity: f32) -> Option<String> {
    let m = match_wake(transcript, phrase, sensitivity)?;
    if m.trailing.is_empty() {
        None
    } else {
        Some(m.trailing)
    }
}

// ---------------------------------------------------------------------------
// Voice catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOption {
    /// Display name, e.g. "af_bella" or the imported folder name.
    pub name: String,
    /// Kokoro speaker id (ignored for custom/system voices).
    pub speaker_id: u32,
    /// Model directory, empty for system voices.
    pub model_dir: String,
    /// "kokoro" | "custom" | "system"
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCatalog {
    pub kokoro: Vec<VoiceOption>,
    pub custom: Vec<VoiceOption>,
    pub system: Vec<VoiceOption>,
}

/// Kokoro speakers + every imported voice under `models/voices/` + the
/// macOS `say` voices.
pub fn catalog() -> VoiceCatalog {
    let mut kokoro = Vec::new();
    let active = PathBuf::from(crate::config::default_tts_model_dir());
    let speakers = if active.join("lexicon-us-en.txt").exists() {
        KOKORO_MULTI_SPEAKERS
    } else {
        KOKORO_V019_SPEAKERS
    };
    for (name, id) in speakers {
        kokoro.push(VoiceOption {
            name: (*name).to_string(),
            speaker_id: *id,
            model_dir: String::new(),
            kind: "kokoro".into(),
        });
    }

    let mut custom = Vec::new();
    let voices_dir = models_dir().join("voices");
    if let Ok(entries) = fs::read_dir(&voices_dir) {
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            if dir.join("model.onnx").exists() {
                custom.push(VoiceOption {
                    name: dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    speaker_id: 0,
                    model_dir: dir.to_string_lossy().to_string(),
                    kind: "custom".into(),
                });
            }
        }
    }

    let mut system = Vec::new();
    if let Ok(out) = std::process::Command::new("/usr/bin/say").arg("-v").arg("?").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            if let Some(name) = line.split_whitespace().next() {
                if name != "?" {
                    system.push(VoiceOption {
                        name: name.to_string(),
                        speaker_id: 0,
                        model_dir: String::new(),
                        kind: "system".into(),
                    });
                }
            }
        }
    }

    VoiceCatalog {
        kokoro,
        custom,
        system,
    }
}

// ---------------------------------------------------------------------------
// Import validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    Kokoro,
    Vits,
    Zipvoice,
}

/// Validate a user-chosen folder as a sherpa-onnx voice model.
///
/// Accepts:
/// - Kokoro: `model.onnx` + `voices.bin` + `tokens.txt`
/// - VITS/Piper: `model.onnx` + `tokens.txt`, or `model.onnx` + `model.onnx.json`
///   (the JSON's `phoneme_id_map` is converted to `tokens.txt` on import).
/// - ZipVoice: `encoder*.onnx` + `decoder*.onnx` + `tokens.txt`.
pub fn validate_voice_dir(dir: &Path) -> Result<VoiceKind> {
    let model = dir.join("model.onnx");
    if model.exists() {
        if dir.join("voices.bin").exists() {
            return Ok(VoiceKind::Kokoro);
        }
        if dir.join("tokens.txt").exists() || dir.join("model.onnx.json").exists() {
            return Ok(VoiceKind::Vits);
        }
        bail!(
            "{} has model.onnx but no tokens.txt (or model.onnx.json)",
            dir.display()
        );
    }
    if first_existing(dir, &["encoder.int8.onnx", "encoder.onnx"]).is_some()
        && first_existing(dir, &["decoder.int8.onnx", "decoder.onnx"]).is_some()
    {
        if dir.join("tokens.txt").exists() {
            return Ok(VoiceKind::Zipvoice);
        }
        bail!("ZipVoice model needs tokens.txt");
    }
    bail!(
        "not a voice model: expected model.onnx (Kokoro/VITS/Piper) or \
         encoder+decoder .onnx (ZipVoice) in {}",
        dir.display()
    )
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
}

/// Copy `src` into `models/voices/<name>/`, converting a Piper
/// `model.onnx.json` into `tokens.txt` and borrowing the shared
/// `espeak-ng-data` when the source omits it.
pub fn import_voice(src: &Path, name: &str) -> Result<PathBuf> {
    let kind = validate_voice_dir(src)?;
    let name = sanitize_voice_name(name)?;
    let dest = models_dir().join("voices").join(&name);
    if dest.exists() {
        bail!("a voice named '{name}' already exists at {}", dest.display());
    }
    fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
    copy_dir(src, &dest)?;

    if kind == VoiceKind::Vits && !dest.join("tokens.txt").exists() {
        if let Some(json) = first_existing(&dest, &["model.onnx.json"]) {
            let text = fs::read_to_string(&json)?;
            write_tokens_from_piper_json(&dest.join("tokens.txt"), &text)?;
        }
    }
    if !dest.join("espeak-ng-data").exists() {
        let shared = crate::config::default_tts_model_dir();
        let shared = PathBuf::from(shared);
        let src_data = if shared.join("espeak-ng-data").is_dir() {
            shared.join("espeak-ng-data")
        } else {
            models_dir().join("kokoro-en-v0_19/espeak-ng-data")
        };
        if src_data.is_dir() {
            copy_dir(&src_data, &dest.join("espeak-ng-data"))?;
        } else {
            bail!(
                "voice has no espeak-ng-data and none was found to borrow; \
                 install a sherpa-onnx voice model first (scripts/fetch-models.sh)"
            );
        }
    }
    Ok(dest)
}

fn sanitize_voice_name(name: &str) -> Result<String> {
    let mut out = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        bail!("invalid voice name");
    }
    Ok(out)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Piper stores its phoneme map in the ONNX sidecar; sherpa-onnx wants a
/// `tokens.txt` of `token id` lines.
fn write_tokens_from_piper_json(path: &Path, json: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let map = value
        .get("phoneme_id_map")
        .and_then(|m| m.as_object())
        .context("model.onnx.json has no phoneme_id_map")?;
    let mut pairs: Vec<(u32, &str)> = map
        .iter()
        .filter_map(|(k, v)| v.as_u64().map(|id| (id as u32, k.as_str())))
        .collect();
    pairs.sort_by_key(|(id, _)| *id);
    let mut out = String::new();
    for (id, token) in pairs {
        out.push_str(token);
        out.push(' ');
        out.push_str(&id.to_string());
        out.push('\n');
    }
    fs::write(path, out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Experimental cloning (ZipVoice)
// ---------------------------------------------------------------------------

/// Result of recording a reference clip.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneRecording {
    pub wav: String,
    pub transcript: String,
    pub seconds: f32,
}

/// True when `dir` looks like a ZipVoice model with a usable reference clip.
pub fn has_clone_reference(dir: &Path) -> bool {
    dir.join("reference.wav").exists() && dir.join("reference.txt").exists()
}

/// Load the reference clip + transcript for ZipVoice synthesis.
pub fn clone_reference(dir: &Path) -> Option<(Vec<f32>, i32, String)> {
    if !has_clone_reference(dir) {
        return None;
    }
    let wav = sherpa_onnx::Wave::read(&dir.join("reference.wav").to_string_lossy())?;
    let text = fs::read_to_string(dir.join("reference.txt"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    Some((wav.samples().to_vec(), wav.sample_rate(), text))
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

use crate::audio::AudioPipeline;
use crate::ui::ArcRwLockConfig;
use crate::runtime::Runtime;
use tauri::{AppHandle, Emitter, Manager, State};

/// Start listening — the single code path shared by the pet click, the tray
/// Talk item, and the right-click Talk item (same as Cmd+Shift+Space).
#[tauri::command]
pub fn start_listening(pipeline: State<'_, std::sync::Arc<AudioPipeline>>) -> Result<(), String> {
    pipeline.force_listen();
    Ok(())
}

#[tauri::command]
pub fn list_voices() -> VoiceCatalog {
    catalog()
}

#[tauri::command]
pub fn import_voice_folder(path: String, name: String) -> Result<String, String> {
    let dest = import_voice(Path::new(&path), &name).map_err(|e| format!("{e:#}"))?;
    Ok(dest.to_string_lossy().to_string())
}

/// Speak a sample with a one-off voice/speed, without changing the saved
/// config. Empty `system_voice` means the neural voice.
#[tauri::command]
pub fn preview_voice(
    runtime: State<'_, Runtime>,
    text: String,
    speaker_id: Option<u32>,
    speed: Option<f32>,
    system_voice: Option<String>,
) -> Result<(), String> {
    let sample = if text.trim().is_empty() {
        "Hi, I'm your Xenon voice.".to_string()
    } else {
        text
    };
    match system_voice {
        Some(v) if !v.trim().is_empty() => runtime
            .preview_system(&v, &sample)
            .map_err(|e| format!("{e:#}")),
        _ => runtime
            .preview_neural(&sample, speaker_id, speed)
            .map_err(|e| format!("{e:#}")),
    }
}

/// Record 5–10 s of reference audio and transcribe it with whisper. The clip
/// and transcript are written into the active voice model directory.
#[tauri::command]
pub fn record_clone(
    config: State<'_, ArcRwLockConfig>,
    seconds: Option<f32>,
) -> Result<CloneRecording, String> {
    let cfg = config.blocking_read().clone();
    let dir = PathBuf::from(cfg.tts_model_dir.trim());
    let has_zipvoice = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("encoder"));
    if !dir.join("model.onnx").exists() && !has_zipvoice {
        return Err("select a voice model directory first".into());
    }
    let seconds = seconds.unwrap_or(7.0).clamp(5.0, 10.0);
    let samples = crate::audio::record_samples(seconds.round() as u32)
        .map_err(|e| format!("{e:#}"))?;
    let wav_path = dir.join("reference.wav");
    if !sherpa_onnx::write(&wav_path.to_string_lossy(), &samples, 16_000) {
        return Err("failed to write reference.wav".into());
    }

    let transcript =
        crate::audio::transcribe_samples(&samples, &cfg).map_err(|e| format!("{e:#}"))?;
    fs::write(dir.join("reference.txt"), &transcript).map_err(|e| e.to_string())?;
    Ok(CloneRecording {
        wav: wav_path.to_string_lossy().to_string(),
        transcript,
        seconds,
    })
}

/// Show/focus the pet and open the typed-message input.
#[tauri::command]
pub fn open_typed_message(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "pet window missing".to_string())?;
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    crate::ui::set_typing(true);
    app.emit("typed-message-open", ()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Hit-test thread must stop forcing click-through while the text field is up.
#[tauri::command]
pub fn set_typing(active: bool) {
    crate::ui::set_typing(active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::XenonConfig;
    use sherpa_onnx::{GenerationConfig, OfflineTts};

    #[test]
    fn wake_positive_exact_and_accent() {
        let p = "hey xenon";
        for t in [
            "hey xenon",
            "Hey Xenon!",
            "HEY XENON",
            "hey zenon",
            "hi xenon",
            "hey xendon",
        ] {
            assert!(match_wake(t, p, 0.5).is_some(), "should match {t:?}");
        }
    }

    #[test]
    fn wake_heavy_accent_loose() {
        let p = "hey xenon";
        for t in ["hello xenon", "hey xendon please"] {
            assert!(match_wake(t, p, 1.0).is_some(), "should match {t:?}");
        }
    }

    #[test]
    fn wake_negative_lookalikes() {
        let p = "hey xenon";
        for t in ["hey there", "xerox", "xenon", "hello", "hey xenons twin brother"] {
            assert!(match_wake(t, p, 0.0).is_none(), "should not match {t:?}");
        }
    }

    #[test]
    fn wake_custom_name() {
        let p = "hey jarvis";
        assert!(match_wake("hey jarvis open safari", p, 0.5).is_some());
        assert!(match_wake("hey jarviss", p, 0.5).is_some());
        assert!(match_wake("hey javert", p, 1.0).is_some());
        assert!(match_wake("hey other", p, 0.5).is_none());
    }

    #[test]
    fn wake_sensitivity_exact_vs_loose() {
        let p = "hey xenon";
        assert!(match_wake("hi xenon", p, 0.0).is_none());
        assert!(match_wake("hi xenon", p, 0.5).is_some());
        assert!(match_wake("hi zenon", p, 0.5).is_some());
        assert!(match_wake("hi zenon", p, 1.0).is_some());
    }

    #[test]
    fn wake_strips_phrase_and_keeps_command() {
        let m = match_wake("hey xenon open safari", "hey xenon", 0.5).unwrap();
        assert_eq!(m.matched_words, 2);
        assert_eq!(m.trailing, "open safari");
        assert_eq!(
            strip_wake("hey xenon open safari", "hey xenon", 0.5).as_deref(),
            Some("open safari")
        );
        assert_eq!(strip_wake("hey xenon", "hey xenon", 0.5), None);
    }

    #[test]
    fn wake_phrase_live_default() {
        let cfg = XenonConfig {
            pet_name: "Jarvis".into(),
            wake_phrase: String::new(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_wake_phrase(), "hey Jarvis");
        let cfg2 = XenonConfig {
            wake_phrase: "yo computer".into(),
            ..cfg
        };
        assert_eq!(cfg2.effective_wake_phrase(), "yo computer");
    }

    #[test]
    fn validate_kokoro_dir() {
        let dir = std::env::temp_dir().join(format!("xenon-voice-kokoro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.onnx"), b"").unwrap();
        fs::write(dir.join("voices.bin"), b"").unwrap();
        fs::write(dir.join("tokens.txt"), b"").unwrap();
        assert_eq!(validate_voice_dir(&dir).unwrap(), VoiceKind::Kokoro);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_empty_dir() {
        let dir = std::env::temp_dir().join(format!("xenon-voice-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(validate_voice_dir(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn piper_json_becomes_tokens() {
        let dir = std::env::temp_dir().join(format!("xenon-voice-piper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.onnx"), b"").unwrap();
        fs::write(
            dir.join("model.onnx.json"),
            r#"{"phoneme_id_map":{"a":0,"b":1," ":2}}"#,
        )
        .unwrap();
        assert_eq!(validate_voice_dir(&dir).unwrap(), VoiceKind::Vits);
        write_tokens_from_piper_json(&dir.join("tokens.txt"), r#"{"phoneme_id_map":{"a":0,"b":1," ":2}}"#).unwrap();
        let toks = fs::read_to_string(dir.join("tokens.txt")).unwrap();
        assert!(toks.contains("a 0"));
        assert!(toks.contains("b 1"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Synthesize *text* with `say`, convert to 16 kHz mono WAV, return f32.
    fn say_wav(text: &str) -> Option<Vec<f32>> {
        let dir = std::env::temp_dir().join(format!(
            "xenon-wake-{}-{}",
            std::process::id(),
            text.chars().filter(|c| c.is_alphanumeric()).collect::<String>()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).ok()?;
        let aiff = dir.join("a.aiff");
        let wav = dir.join("a.wav");
        let ok = std::process::Command::new("/usr/bin/say")
            .arg("-o")
            .arg(&aiff)
            .arg(text)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let ok = std::process::Command::new("/usr/bin/afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
            .arg(&aiff)
            .arg(&wav)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let wave = sherpa_onnx::Wave::read(&wav.to_string_lossy())?;
        let samples: Vec<f32> = wave.samples().to_vec();
        let _ = fs::remove_dir_all(&dir);
        Some(samples)
    }

    /// Real audio wake test: "Hey Xenon" through `say` must match.
    #[test]
    #[ignore = "synthesizes real audio; run with -- --ignored wake_audio"]
    fn wake_audio_hey_xenon_fires() {
        let Some(samples) = say_wav("Hey Xenon") else {
            eprintln!("skip: say/afconvert unavailable");
            return;
        };
        let mut cfg = XenonConfig::default();
        if let Ok(path) = std::env::var("XENON_WAKE_MODEL") {
            cfg.wake_model_path = path;
        }
        let Some(text) = crate::audio::transcribe_samples(&samples, &cfg)
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skip: no whisper model available");
            return;
        };
        eprintln!("transcript: {text:?}");
        assert!(
            match_wake(&text, "hey xenon", cfg.wake_sensitivity).is_some(),
            "expected a wake match, got {text:?}"
        );
    }

    /// Negative real-audio wake test: "hello there" must not match.
    #[test]
    #[ignore = "synthesizes real audio; run with -- --ignored wake_audio"]
    fn wake_audio_hello_there_does_not_fire() {
        let Some(samples) = say_wav("hello there") else {
            eprintln!("skip: say/afconvert unavailable");
            return;
        };
        let mut cfg = XenonConfig::default();
        if let Ok(path) = std::env::var("XENON_WAKE_MODEL") {
            cfg.wake_model_path = path;
        }
        let text = crate::audio::transcribe_samples(&samples, &cfg).unwrap_or_default();
        eprintln!("transcript: {text:?}");
        assert!(
            match_wake(&text, "hey xenon", cfg.wake_sensitivity).is_none(),
            "expected no wake match, got {text:?}"
        );
    }

    /// Harness 2: 30 positives + 30 negatives through the wake matcher.
    #[test]
    #[ignore = "synthesizes and transcribes audio; run with -- --ignored stress_wake_harness"]
    fn stress_wake_harness() {
        use std::io::Write;

        let cfg = XenonConfig::default();
        let model = if Path::new(&cfg.wake_model_path).exists() {
            cfg.wake_model_path.clone()
        } else {
            cfg.whisper_model_path.clone()
        };
        if !Path::new(&model).exists() {
            eprintln!("skip: no whisper model");
            return;
        }
        let ctx = whisper_rs::WhisperContext::new_with_params(
            &model,
            whisper_rs::WhisperContextParameters::new(),
        )
        .expect("load whisper");

        let positives = [
            "Hey Xenon", "Hey Xenon.", "Hey Xenon!", "Hey Xenon, are you there?",
            "Hey Xenon, what time is it?", "Hey Xenon, open Safari", "Hey Xenon, stop",
            "Hey Xenon, hello", "Hey Xenon, can you hear me?", "Hey Xenon, thanks",
            "Hey Xenon, do the thing", "Hey Xenon, read this", "Hey Xenon, look here",
            "Hey Xenon, remember this", "Hey Xenon, tell me a joke", "Hey Xenon, play music",
            "Hey Xenon, turn it down", "Hey Xenon, turn it up", "Hey Xenon, next",
            "Hey Xenon, done", "Hey Xenon, yes", "Hey Xenon, no", "Hey Xenon, maybe",
            "Hey Xenon, wait", "Hey Xenon, go", "Hey Xenon, come back",
            "Hey Xenon, I need help", "Hey Xenon, are you listening?",
            "Hey Xenon, good morning", "Hey Xenon, good night",
        ];
        let negatives = [
            "Hello there", "Hey you", "Hello Xenon", "Hey Xenon's friend",
            "What time is it?", "Open Safari", "Stop", "Thanks", "Good morning",
            "Good night", "How are you?", "What is the weather?", "Tell me a joke",
            "Read this page", "Remember this", "Do the thing", "Can you hear me?",
            "Are you there?", "Hey, listen", "Hey, stop", "Hey, go", "Wait a minute",
            "I need help", "Please help me", "This is a test", "Testing one two three",
            "The quick brown fox", "Something completely different", "Never mind",
            "Never say that word",
        ];

        let out = std::env::var_os("XENON_STRESS_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../scripts/stress/out")
            });
        std::fs::create_dir_all(&out).unwrap();
        let mut f = std::fs::File::create(out.join("wake.tsv")).unwrap();
        let mut pos_hits = 0;
        let mut neg_hits = 0;

        for (label, texts, expected) in [
            ("pos", &positives[..], true),
            ("neg", &negatives[..], false),
        ] {
            for text in texts {
                let Some(samples) = say_wav(text) else { continue };
                let transcript =
                    crate::audio::transcribe_with_context(&ctx, &samples).unwrap_or_default();
                let matched = match_wake(&transcript, "hey xenon", cfg.wake_sensitivity).is_some();
                if matched && expected {
                    pos_hits += 1;
                }
                if matched && !expected {
                    neg_hits += 1;
                }
                writeln!(f, "{label}\t{text}\t{transcript}\t{matched}").unwrap();
            }
        }
        eprintln!(
            "wake: positives fired {pos_hits}/{}; negatives fired {neg_hits}/{}",
            positives.len(),
            negatives.len()
        );
    }

    /// Real Kokoro preview: every speaker must produce non-silent audio.
    #[test]
    #[ignore = "loads the real Kokoro model; run with -- --ignored kokoro_speakers"]
    fn kokoro_all_speakers_non_silent() {
        let cfg = XenonConfig::default();
        let dir = Path::new(&cfg.tts_model_dir);
        if !dir.join("model.onnx").exists() || !dir.join("voices.bin").exists() {
            eprintln!("skip: Kokoro model absent at {}", dir.display());
            return;
        }
        let tts = OfflineTts::create(&crate::tts::offline_tts_config(dir, crate::tts::TtsKind::Kokoro))
            .expect("create Kokoro TTS");
        let speakers = if dir.join("lexicon-us-en.txt").exists() {
            KOKORO_MULTI_SPEAKERS
        } else {
            KOKORO_V019_SPEAKERS
        };
        for (_, sid) in speakers {
            let gen = GenerationConfig {
                speed: cfg.tts_speed,
                sid: *sid as i32,
                ..Default::default()
            };
            let audio = tts
                .generate_with_config("Hello from Xenon", &gen, None::<fn(&[f32], f32) -> bool>)
                .expect("synthesize");
            let peak = audio
                .samples()
                .iter()
                .fold(0.0f32, |m, &s| m.max(s.abs()));
            assert!(!audio.samples().is_empty() && peak > 1e-3, "speaker {sid} silent");
            eprintln!("speaker {sid}: {} samples", audio.samples().len());
        }
    }
}