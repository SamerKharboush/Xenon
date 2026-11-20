//! Streaming TTS: `Speak(sentence)` enqueues, a dedicated std::thread
//! synthesizes each sentence with sherpa-onnx while the previous one plays
//! through a cpal output stream, and the playback cursor drives `speech-level`
//! at ~30 Hz for lip-sync. When no neural model is present, `say` is used as
//! a killable fallback with a synthetic amplitude envelope.
//!
//! Threading: synthesis and playback never touch tokio workers. The cpal
//! output callback only copies samples and advances a cursor under a short
//! mutex; a small level ticker computes RMS from that cursor and emits events.
//! `stop()` clears the queue and the playback buffer immediately, and the
//! worker kills the fallback process on its next poll.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use serde::Serialize;
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig, OfflineTtsVitsModelConfig, OfflineTtsZipvoiceModelConfig,
};
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;

use crate::audio_dsp::resample_linear;
use crate::config::XenonConfig;

/// Emit `speech-level` at ~30 Hz.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);
/// Worker idle poll / playback drain poll cadence.
const POLL: Duration = Duration::from_millis(10);
/// Last 50 ms of played samples feed the RMS window.
const LEVEL_WINDOW_MS: u32 = 50;

#[derive(Serialize, Clone)]
struct SpeechLevel {
    rms: f32,
}

enum TtsCommand {
    Speak(String),
    /// Voice picker Preview: one-off speaker/speed, never touches the queue.
    Preview {
        text: String,
        speaker: Option<u32>,
        speed: Option<f32>,
    },
    Stop,
    /// Performance toggle off: load the neural model now instead of at the
    /// first sentence.
    Preload,
}

/// FIFO of sentences waiting to be synthesized. Kept separate so ordering
/// and stop-clears-queue are testable without spawning threads.
#[derive(Default)]
struct SpeechQueue {
    inner: Mutex<VecDeque<String>>,
}

impl SpeechQueue {
    fn push(&self, text: String) {
        self.inner.lock().unwrap().push_back(text);
    }

    fn pop(&self) -> Option<String> {
        self.inner.lock().unwrap().pop_front()
    }

    fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

/// Shared between the synthesis worker, the level ticker and the cpal
/// callback. `cursor` is the index of the next sample to be played.
struct Playback {
    samples: Vec<f32>,
    cursor: usize,
    /// Resampled playback rate, used to size the RMS window.
    rate: u32,
}

impl Playback {
    fn is_playing(&self) -> bool {
        self.cursor < self.samples.len()
    }
}

type Pb = Arc<Mutex<Playback>>;

/// Handle kept in Tauri state. `speak()` and `stop()` are non-blocking; the
/// worker + level ticker are spawned lazily on first `speak()`.
pub struct Tts {
    config: Arc<RwLock<XenonConfig>>,
    cmd_tx: Mutex<Option<mpsc::Sender<TtsCommand>>>,
    stop_flag: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    playback: Pb,
    app: Arc<OnceLock<AppHandle>>,
}

impl Default for Tts {
    fn default() -> Self {
        Self::new(Arc::new(RwLock::new(XenonConfig::default())))
    }
}

impl Tts {
    pub fn new(config: Arc<RwLock<XenonConfig>>) -> Self {
        Self {
            config,
            cmd_tx: Mutex::new(None),
            stop_flag: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            playback: Arc::new(Mutex::new(Playback {
                samples: Vec::new(),
                cursor: 0,
                rate: 48_000,
            })),
            app: Arc::new(OnceLock::new()),
        }
    }

    /// Called once from Runtime::attach_app so the worker can emit
    /// `speech-level` and `xenon-notice`.
    pub fn attach_app(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    /// Performance toggle off: start the TTS worker and load the neural model
    /// at startup. Never blocks on the load itself.
    pub fn preload(&self) {
        if !self
            .config
            .try_read()
            .map(|c| c.tts_enabled)
            .unwrap_or(true)
        {
            return;
        }
        let _ = self.sender().send(TtsCommand::Preload);
    }

    /// Enqueue one sentence. Never blocks: the worker does synthesis.
    pub fn speak(&self, text: &str) -> Result<()> {
        let enabled = self
            .config
            .try_read()
            .map(|c| c.tts_enabled)
            .unwrap_or(true);
        if !enabled || text.trim().is_empty() {
            return Ok(());
        }
        self.busy.store(true, Ordering::SeqCst);
        self.sender()
            .send(TtsCommand::Speak(text.to_string()))
            .context("tts worker stopped")?;
        Ok(())
    }

    /// Voice picker Preview: speak one sentence with a one-off Kokoro speaker
    /// and/or speed. Bypasses the normal speech queue and the pet state.
    pub fn preview_neural(
        &self,
        text: &str,
        speaker: Option<u32>,
        speed: Option<f32>,
    ) -> Result<()> {
        self.sender()
            .send(TtsCommand::Preview {
                text: text.to_string(),
                speaker,
                speed,
            })
            .context("tts worker stopped")
    }

    /// Preview a macOS system voice through `say` (no neural model).
    pub fn preview_system(&self, voice: &str, text: &str) -> Result<()> {
        std::process::Command::new("/usr/bin/say")
            .arg("-v")
            .arg(voice)
            .arg(text)
            .spawn()
            .context("spawn /usr/bin/say for voice preview")?;
        Ok(())
    }

    /// Barge-in / mute: clear the queue, silence playback immediately and let
    /// the worker kill any fallback `say` process.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.busy.store(false, Ordering::SeqCst);
        {
            let mut pb = self.playback.lock().unwrap();
            pb.samples.clear();
            pb.cursor = 0;
        }
        if let Some(tx) = self.cmd_tx.lock().unwrap().as_ref() {
            let _ = tx.send(TtsCommand::Stop);
        }
    }

    /// True while the queue is non-empty or audio is still playing. Runtime
    /// uses this to defer Idle until after the last sentence has drained.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    fn sender(&self) -> mpsc::Sender<TtsCommand> {
        let mut guard = self.cmd_tx.lock().unwrap();
        if guard.is_none() {
            let (tx, rx) = mpsc::channel();
            let worker_config = self.config.clone();
            let stop_flag = self.stop_flag.clone();
            let busy = self.busy.clone();
            let pb = self.playback.clone();
            let app = self.app.clone();
            std::thread::Builder::new()
                .name("xenon-tts".into())
                .spawn(move || {
                    worker(rx, worker_config, stop_flag, busy, pb, app);
                })
                .expect("spawn tts worker");

            let pb = self.playback.clone();
            let app = self.app.clone();
            std::thread::Builder::new()
                .name("xenon-tts-level".into())
                .spawn(move || level_ticker(pb, app))
                .expect("spawn tts level ticker");

            *guard = Some(tx.clone());
        }
        guard.as_ref().expect("tts sender").clone()
    }
}

enum SynthOut {
    Neural(Vec<f32>, u32),
    Say(String),
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TtsKind {
    Kokoro,
    Vits,
    Zipvoice,
}

/// Autodetect from file layout unless the user pinned a kind in Settings.
fn detect_kind(kind: &str, dir: &Path) -> TtsKind {
    match kind {
        "kokoro" => TtsKind::Kokoro,
        "vits" => TtsKind::Vits,
        "zipvoice" => TtsKind::Zipvoice,
        _ if dir.join("voices.bin").exists() => TtsKind::Kokoro,
        _ if first_with_prefix(dir, "encoder").is_some()
            && first_with_prefix(dir, "decoder").is_some() =>
        {
            TtsKind::Zipvoice
        }
        _ => TtsKind::Vits,
    }
}

fn first_with_prefix(dir: &Path, prefix: &str) -> Option<String> {
    let entries = dir.read_dir().ok()?;
    let mut names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".onnx"))
        .collect();
    names.sort();
    names
        .into_iter()
        .next()
        .map(|n| dir.join(n).to_string_lossy().to_string())
}

fn opt_file(dir: &Path, name: &str) -> Option<String> {
    let p = dir.join(name);
    p.exists().then(|| p.to_string_lossy().to_string())
}

/// Kokoro lexicon files. Multi-language v1_0 ships one or more
/// `lexicon-*.txt` files, comma-joined for sherpa-onnx; single-language
/// v0.19 ships none and falls back to the system voice.
fn kokoro_lexicon(dir: &Path) -> Option<String> {
    let mut files: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("lexicon") && n.ends_with(".txt"))
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    files.sort_by_key(|path| {
        match Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
        {
            "lexicon-us-en.txt" | "lexicon-en-us.txt" => 0,
            "lexicon-gb-en.txt" => 1,
            "lexicon-zh.txt" => 2,
            _ => 3,
        }
    });
    (!files.is_empty()).then(|| files.join(","))
}

/// Single-word keys from every Kokoro lexicon file. Used only to decide
/// whether a word needs the MIT-safe spelling fallback.
fn load_lexicon_words(dir: &Path) -> HashSet<String> {
    let mut words = HashSet::new();
    let Some(lexicon) = kokoro_lexicon(dir) else {
        return words;
    };
    for path in lexicon.split(',') {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = line
                .split_once("||")
                .map(|(k, _)| k)
                .unwrap_or(line);
            if let Some(word) = key.split_whitespace().next() {
                words.insert(word.to_lowercase());
            }
        }
    }
    words
}

const SMALL_NUMBERS: [&str; 20] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen",
    "seventeen", "eighteen", "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

fn number_word(n: u32) -> String {
    if n < 20 {
        return SMALL_NUMBERS[n as usize].to_string();
    }
    if n < 100 {
        let tens = TENS[(n / 10) as usize];
        return if n % 10 == 0 {
            tens.to_string()
        } else {
            format!("{tens}-{}", SMALL_NUMBERS[(n % 10) as usize])
        };
    }
    if n < 1000 {
        let hundred = SMALL_NUMBERS[(n / 100) as usize];
        return match n % 100 {
            0 => format!("{hundred} hundred"),
            rest => format!("{hundred} hundred {}", number_word(rest)),
        };
    }
    // Longer numbers: spell the digits instead of silently dropping them.
    n.to_string()
        .chars()
        .filter_map(|d| d.to_digit(10))
        .map(number_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn flush_kokoro_run(
    run: &mut String,
    digits: bool,
    words: &HashSet<String>,
    pieces: &mut Vec<String>,
) {
    if run.is_empty() {
        return;
    }
    if digits {
        let spoken = run
            .parse::<u32>()
            .map(number_word)
            .unwrap_or_else(|_| run.clone());
        pieces.push(spoken);
    } else if words.contains(&run.to_lowercase()) {
        pieces.push(run.clone());
    } else {
        pieces.push(
            run.to_lowercase()
                .chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    run.clear();
}

/// Expand OOV words/numbers before sherpa-onnx sees them. sherpa-onnx 2.0's
/// Kokoro lexicon path only handles hyphen splits and CJK characters; English
/// OOV words and plain digits are otherwise skipped.
fn expand_kokoro_token(token: &str, words: &HashSet<String>) -> String {
    let Some(start) = token
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
    else {
        return token.to_string();
    };
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(token.len());
    let core = &token[start..end];
    if words.contains(&core.to_lowercase()) {
        return token.to_string();
    }

    let prefix = &token[..start];
    let suffix = &token[end..];
    let mut pieces = Vec::new();
    let mut run = String::new();
    let mut digits = false;

    for c in core.chars() {
        if c.is_ascii_digit() {
            if !digits && !run.is_empty() {
                flush_kokoro_run(&mut run, digits, words, &mut pieces);
            }
            digits = true;
            run.push(c);
        } else if c.is_alphabetic() {
            if digits && !run.is_empty() {
                flush_kokoro_run(&mut run, digits, words, &mut pieces);
            }
            digits = false;
            run.push(c);
        } else {
            flush_kokoro_run(&mut run, digits, words, &mut pieces);
            digits = false;
            pieces.push(c.to_string());
        }
    }
    flush_kokoro_run(&mut run, digits, words, &mut pieces);

    let core = pieces.join(" ");
    if core.is_empty() {
        return token.to_string();
    }
    if prefix.is_empty() && suffix.is_empty() {
        core
    } else {
        format!("{prefix} {core} {suffix}")
    }
}

fn prepare_kokoro_text(text: &str, words: &HashSet<String>) -> String {
    text.split_whitespace()
        .map(|token| expand_kokoro_token(token, words))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the sherpa-onnx config used both by the worker and the ignored
/// real-model test. Missing optional files are simply left unset so Piper /
/// VITS model directories work too.
pub(crate) fn offline_tts_config(dir: &Path, kind: TtsKind) -> OfflineTtsConfig {
    let model = opt_file(dir, "model.onnx");
    let tokens = opt_file(dir, "tokens.txt");
    let data_dir = dir
        .join("espeak-ng-data")
        .exists()
        .then(|| dir.join("espeak-ng-data").to_string_lossy().to_string());
    let mut model_cfg = OfflineTtsModelConfig {
        num_threads: std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(2)
            .min(4),
        ..Default::default()
    };
    match kind {
        TtsKind::Kokoro => {
            model_cfg.kokoro = OfflineTtsKokoroModelConfig {
                model,
                voices: opt_file(dir, "voices.bin"),
                tokens,
                lexicon: kokoro_lexicon(dir),
                ..Default::default()
            };
        }
        TtsKind::Vits => {
            model_cfg.vits = OfflineTtsVitsModelConfig {
                model,
                tokens,
                data_dir,
                ..Default::default()
            };
        }
        TtsKind::Zipvoice => {
            model_cfg.zipvoice = OfflineTtsZipvoiceModelConfig {
                tokens,
                encoder: first_with_prefix(dir, "encoder"),
                decoder: first_with_prefix(dir, "decoder"),
                vocoder: first_with_prefix(dir, "vocoder"),
                lexicon: opt_file(dir, "lexicon.txt"),
                ..Default::default()
            };
        }
    }
    OfflineTtsConfig {
        model: model_cfg,
        ..Default::default()
    }
}

struct Worker {
    cmd_rx: mpsc::Receiver<TtsCommand>,
    config: Arc<RwLock<XenonConfig>>,
    stop_flag: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    playback: Pb,
    app: Arc<OnceLock<AppHandle>>,
    queue: SpeechQueue,
    cfg: XenonConfig,
    cfg_loaded_at: Instant,
    model: Option<OfflineTts>,
    model_dir_loaded: String,
    model_last_used: Option<Instant>,
    lexicon_words: Option<HashSet<String>>,
    lexicon_dir_loaded: String,
    stream: Option<Stream>,
    device_rate: u32,
    notice_sent: bool,
}

#[allow(clippy::too_many_arguments)]
fn worker(
    cmd_rx: mpsc::Receiver<TtsCommand>,
    config: Arc<RwLock<XenonConfig>>,
    stop_flag: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    playback: Pb,
    app: Arc<OnceLock<AppHandle>>,
) {
    let cfg = config.try_read().map(|c| c.clone()).unwrap_or_default();
    // A stop() before the first speak() leaves the flag set; clear it so the
    // first real sentence is not discarded.
    stop_flag.store(false, Ordering::SeqCst);
    let mut w = Worker {
        cmd_rx,
        config,
        stop_flag,
        busy,
        playback,
        app,
        queue: SpeechQueue::default(),
        cfg,
        cfg_loaded_at: Instant::now(),
        model: None,
        model_dir_loaded: String::new(),
        model_last_used: None,
        lexicon_words: None,
        lexicon_dir_loaded: String::new(),
        stream: None,
        device_rate: 48_000,
        notice_sent: false,
    };

    loop {
        if !w.poll_commands() {
            break;
        }
        if let Some(text) = w.queue.pop() {
            // Synthesize here, while the previous sentence is still playing
            // through the cpal callback.
            let out = w.synthesize(&text);
            w.wait_drained();
            if w.stop_flag.load(Ordering::SeqCst) {
                continue;
            }
            match out {
                SynthOut::Neural(pcm, rate) => w.start_playback(&pcm, rate),
                SynthOut::Say(text) => w.run_say(&text),
                SynthOut::Skip => {}
            }
        } else {
            w.wait_drained();
            w.busy.store(false, Ordering::SeqCst);
        }
        w.tick_timers();
    }
    log::info!("tts worker exiting");
}

impl Worker {
    /// Drain all pending commands, then wait briefly for the next one.
    /// Returns false when the handle was dropped.
    fn poll_commands(&mut self) -> bool {
        loop {
            match self.cmd_rx.try_recv() {
                Ok(cmd) => self.handle_command(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
        match self.cmd_rx.recv_timeout(POLL) {
            Ok(cmd) => self.handle_command(cmd),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
        true
    }

    fn handle_command(&mut self, cmd: TtsCommand) {
        match cmd {
            TtsCommand::Speak(text) => {
                self.queue.push(text);
                self.busy.store(true, Ordering::SeqCst);
            }
            TtsCommand::Preview {
                text,
                speaker,
                speed,
            } => {
                // Blocking one-off preview: finish it, then return to the
                // normal queue. Playback is isolated from the queue state.
                let out = self.synthesize_with(&text, speaker, speed);
                self.wait_drained();
                match out {
                    SynthOut::Neural(pcm, rate) => self.start_playback(&pcm, rate),
                    SynthOut::Say(text) => self.run_say(&text),
                    SynthOut::Skip => {}
                }
            }
            TtsCommand::Stop => self.reset_stop(),
            TtsCommand::Preload => {
                if let Err(e) = self.ensure_model() {
                    log::warn!("tts: preload failed: {e:#}");
                }
            }
        }
    }

    fn reset_stop(&self) {
        self.stop_flag.store(false, Ordering::SeqCst);
        self.queue.clear();
        self.stop_playback();
    }

    fn stop_playback(&self) {
        let mut pb = self.playback.lock().unwrap();
        pb.samples.clear();
        pb.cursor = 0;
    }

    /// Wait for the current sentence to finish playing. Polls the cursor so
    /// barge-in remains responsive; never sleeps on a tokio worker. On stop
    /// it silences playback but leaves the stop flag set for the caller.
    fn wait_drained(&self) {
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                self.stop_playback();
                return;
            }
            let active = {
                let mut pb = self.playback.lock().unwrap();
                if !pb.samples.is_empty() && pb.cursor >= pb.samples.len() {
                    pb.samples.clear();
                    pb.cursor = 0;
                }
                pb.is_playing()
            };
            if !active {
                return;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Lazy-load the model, then synthesize one sentence. A missing or broken
    /// model falls back to `say` with a one-time notice.
    fn synthesize(&mut self, text: &str) -> SynthOut {
        self.synthesize_with(text, None, None)
    }

    fn synthesize_with(
        &mut self,
        text: &str,
        speaker: Option<u32>,
        speed: Option<f32>,
    ) -> SynthOut {
        if !self.cfg.tts_enabled {
            return SynthOut::Skip;
        }
        let model_dir = self.cfg.tts_model_dir.trim().to_string();
        let dir = Path::new(&model_dir);
        let kind = detect_kind(&self.cfg.tts_kind, dir);
        if kind == TtsKind::Kokoro && kokoro_lexicon(dir).is_none() {
            log::warn!(
                "tts: Kokoro model at {} has no lexicon; run scripts/fetch-models.sh",
                dir.display()
            );
            if !self.notice_sent {
                self.notice_sent = true;
                self.emit_notice(
                    "Neural voice needs its lexicon. Run scripts/fetch-models.sh, then restart.",
                );
            }
            return SynthOut::Say(text.to_string());
        }
        let prepared = if kind == TtsKind::Kokoro {
            if self.lexicon_dir_loaded != model_dir {
                self.lexicon_words = Some(load_lexicon_words(dir));
                self.lexicon_dir_loaded = model_dir.clone();
            }
            prepare_kokoro_text(text, self.lexicon_words.as_ref().expect("lexicon loaded"))
        } else {
            String::new()
        };
        let text = if kind == TtsKind::Kokoro {
            prepared.as_str()
        } else {
            text
        };
        let model = match self.ensure_model() {
            Ok(()) => self.model.as_ref().expect("model loaded"),
            Err(e) => {
                if !self.notice_sent {
                    self.notice_sent = true;
                    self.emit_notice(&format!(
                        "Neural voice unavailable ({e}). Using the system voice for now \
                         — run scripts/fetch-models.sh to install the neural voice."
                    ));
                }
                return SynthOut::Say(text.to_string());
            }
        };
        let mut gen = GenerationConfig {
            speed: speed.unwrap_or(self.cfg.tts_speed),
            sid: speaker.map(|s| s as i32).unwrap_or(self.cfg.tts_speaker_id as i32),
            ..Default::default()
        };
        if kind == TtsKind::Zipvoice {
            if let Some((audio, rate, reference_text)) =
                crate::voice::clone_reference(Path::new(self.cfg.tts_model_dir.trim()))
            {
                gen.reference_audio = Some(audio);
                gen.reference_sample_rate = rate;
                gen.reference_text = Some(reference_text);
            }
        }
        match model.generate_with_config(text, &gen, None::<fn(&[f32], f32) -> bool>) {
            Some(audio) if !audio.samples().is_empty() => {
                self.model_last_used = Some(Instant::now());
                SynthOut::Neural(audio.samples().to_vec(), audio.sample_rate() as u32)
            }
            _ => {
                log::warn!(
                    "tts: neural synthesis returned no audio (kind={kind:?}, dir={}, text={text:?})",
                    dir.display()
                );
                if !self.notice_sent {
                    self.notice_sent = true;
                    self.emit_notice(
                        "Neural voice returned no audio. Using the system voice for now.",
                    );
                }
                SynthOut::Say(text.to_string())
            }
        }
    }

    fn ensure_model(&mut self) -> Result<()> {
        let dir = self.cfg.tts_model_dir.trim().to_string();
        if self.model.is_some() && self.model_dir_loaded == dir {
            return Ok(());
        }
        let path = Path::new(&dir);
        let kind = detect_kind(&self.cfg.tts_kind, path);
        if kind == TtsKind::Zipvoice {
            anyhow::ensure!(
                first_with_prefix(path, "encoder").is_some()
                    && first_with_prefix(path, "decoder").is_some(),
                "ZipVoice encoder/decoder not found in {} (run scripts/fetch-models.sh)",
                path.display()
            );
        } else {
            anyhow::ensure!(
                path.join("model.onnx").exists(),
                "TTS model not found at {} (run scripts/fetch-models.sh)",
                path.join("model.onnx").display()
            );
        }
        let cfg = offline_tts_config(path, kind);
        let model = OfflineTts::create(&cfg).context("sherpa-onnx could not load the TTS model")?;
        log::info!("tts: loaded {:?} model from {dir} (rate {} Hz)", kind, model.sample_rate());
        self.model = Some(model);
        self.model_dir_loaded = dir;
        self.model_last_used = Some(Instant::now());
        Ok(())
    }

    /// Resample with the Audio Pipeline's shared resampler, then hand the
    /// buffer to the cpal callback.
    fn start_playback(&mut self, pcm: &[f32], source_rate: u32) {
        if source_rate == 0 || pcm.is_empty() {
            return;
        }
        if self.stream.is_none() {
            if let Err(e) = self.open_output() {
                log::error!("tts: cannot open output device: {e:#}");
                self.emit_notice(&format!("Couldn't open audio output: {e}"));
                return;
            }
        }
        let samples = resample_linear(pcm, source_rate, self.device_rate);
        let mut pb = self.playback.lock().unwrap();
        pb.samples = samples;
        pb.cursor = 0;
        pb.rate = self.device_rate;
    }

    fn open_output(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default output device")?;
        let supported = device
            .default_output_config()
            .context("no supported output config")?;
        self.device_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let cfg = supported.config();
        let pb = self.playback.clone();

        let stream = match supported.sample_format() {
            SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
                &cfg,
                move |data: &mut [f32], _| write_frames(data, channels, &pb),
                |e| log::warn!("tts: output stream error: {e}"),
                None,
            )?,
            SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                &cfg,
                move |data: &mut [i16], _| write_frames_i16(data, channels, &pb),
                |e| log::warn!("tts: output stream error: {e}"),
                None,
            )?,
            other => anyhow::bail!("unsupported output sample format: {other:?}"),
        };
        stream.play().context("start tts output stream")?;
        self.stream = Some(stream);
        log::info!("tts: output stream open ({} Hz, {channels} ch)", self.device_rate);
        Ok(())
    }

    /// `say` fallback: spawn once, poll for exit while emitting a synthetic
    /// envelope at ~30 Hz, and kill it immediately on barge-in.
    fn run_say(&mut self, text: &str) {
        let voice = self.cfg.system_voice.trim().to_string();
        let mut command = std::process::Command::new("/usr/bin/say");
        if !voice.is_empty() {
            command.arg("-v").arg(&voice);
        }
        let mut child = match command.arg(text).spawn() {
            Ok(child) => child,
            Err(e) => {
                log::error!("tts: /usr/bin/say failed: {e}");
                return;
            }
        };
        log::info!("tts: fallback say: {text:?}");
        let start = Instant::now();
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                self.reset_stop();
                return;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(e) => {
                    log::warn!("tts: say wait failed: {e}");
                    break;
                }
            }
            let t = start.elapsed().as_secs_f32();
            let envelope = 0.25 + 0.55 * (t * 7.0).sin().abs() * (t * 3.2).cos().abs();
            self.emit_level(envelope.clamp(0.0, 1.0));
            std::thread::sleep(LEVEL_INTERVAL);
        }
        self.emit_level(0.0);
    }

    fn refresh_config(&mut self) {
        if self.cfg_loaded_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.cfg_loaded_at = Instant::now();
        if let Ok(guard) = self.config.try_read() {
            self.cfg = guard.clone();
        }
        if self.model.is_some() && self.model_dir_loaded != self.cfg.tts_model_dir.trim() {
            log::info!("tts: model dir changed, reloading on next sentence");
            self.model = None;
            self.model_last_used = None;
            self.lexicon_words = None;
            self.lexicon_dir_loaded.clear();
        }
    }

    fn tick_timers(&mut self) {
        self.refresh_config();
        if let Some(last) = self.model_last_used {
            let idle = Duration::from_secs(self.cfg.tts_unload_minutes.saturating_mul(60));
            if idle.as_secs() > 0 && last.elapsed() >= idle && self.model.is_some() {
                log::info!(
                    "tts: unloading idle model (> {} min)",
                    self.cfg.tts_unload_minutes
                );
                self.model = None;
                self.model_last_used = None;
                self.lexicon_words = None;
                self.lexicon_dir_loaded.clear();
            }
        }
    }

    fn emit_level(&self, rms: f32) {
        if let Some(app) = self.app.get() {
            let _ = app.emit("speech-level", SpeechLevel { rms });
        }
    }

    fn emit_notice(&self, msg: &str) {
        log::warn!("tts notice: {msg}");
        if let Some(app) = self.app.get() {
            let _ = app.emit("xenon-notice", msg.to_string());
        }
    }
}

/// Copy one sample per output frame into every channel; silence past the end
/// of the current sentence. The cursor is what the level ticker reads.
fn write_frames(data: &mut [f32], channels: usize, pb: &Pb) {
    let mut p = pb.lock().unwrap();
    let mut i = 0;
    while i < data.len() {
        let sample = if p.cursor < p.samples.len() {
            let s = p.samples[p.cursor];
            p.cursor += 1;
            s
        } else {
            0.0
        };
        for c in 0..channels {
            if i + c < data.len() {
                data[i + c] = sample;
            }
        }
        i += channels;
    }
}

fn write_frames_i16(data: &mut [i16], channels: usize, pb: &Pb) {
    let mut p = pb.lock().unwrap();
    let mut i = 0;
    while i < data.len() {
        let sample = if p.cursor < p.samples.len() {
            let s = p.samples[p.cursor];
            p.cursor += 1;
            s
        } else {
            0.0
        };
        let q = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        for c in 0..channels {
            if i + c < data.len() {
                data[i + c] = q;
            }
        }
        i += channels;
    }
}

/// RMS of recent *played* samples, 0..1. Pure so it can be unit tested.
/// Accumulates in f64 so long windows do not lose precision.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s * s)).sum();
    (sum / samples.len() as f64).sqrt().clamp(0.0, 1.0) as f32
}

/// Light smoothing so the lip-sync motion does not jitter frame to frame.
fn smooth_level(previous: f32, rms: f32) -> f32 {
    (previous * 0.7 + rms * 0.3).clamp(0.0, 1.0)
}

/// Compute the RMS window ending at the playback cursor.
fn level_at_cursor(pb: &Playback) -> f32 {
    if !pb.is_playing() {
        return 0.0;
    }
    let window = ((pb.rate * LEVEL_WINDOW_MS) / 1000).max(1) as usize;
    let lo = pb.cursor.saturating_sub(window);
    rms(&pb.samples[lo..pb.cursor])
}

/// ~30 Hz emitter. Computes level from the playback cursor, not from
/// synthesis time, and sends one zero when playback stops.
fn level_ticker(pb: Pb, app: Arc<OnceLock<AppHandle>>) {
    let mut previous = 0.0f32;
    let mut was_playing = false;
    loop {
        std::thread::sleep(LEVEL_INTERVAL);
        let (level, active) = {
            let p = pb.lock().unwrap();
            let active = p.is_playing();
            if active {
                (smooth_level(previous, level_at_cursor(&p)), true)
            } else {
                (0.0, false)
            }
        };
        previous = level;
        if !active && !was_playing {
            continue;
        }
        was_playing = active;
        if let Some(app) = app.get() {
            let _ = app.emit("speech-level", SpeechLevel { rms: level });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_preserves_sentence_order() {
        let q = SpeechQueue::default();
        for s in ["First.", "Second.", "Third."] {
            q.push(s.to_string());
        }
        assert_eq!(q.pop().as_deref(), Some("First."));
        assert_eq!(q.pop().as_deref(), Some("Second."));
        assert_eq!(q.pop().as_deref(), Some("Third."));
        assert!(q.pop().is_none());
    }

    #[test]
    fn kokoro_oov_fallback_spells_unknown_words_and_numbers() {
        let words: HashSet<String> = [
            "xenon", "safari", "pm", "three", "forty-five", "for", "at", "s", "a", "m",
            "e", "r", "o", "p", "n", "d", "t", "h", "f", "i", "v", "y", "x",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let out = prepare_kokoro_text("Xenon opened Safari for Samer at 3:45pm", &words);
        assert!(out.contains("Xenon"), "{out}");
        assert!(out.contains("o p e n e d"), "{out}");
        assert!(out.contains("Safari"), "{out}");
        assert!(out.contains("s a m e r"), "{out}");
        assert!(out.contains("three : forty-five pm"), "{out}");
    }

    #[test]
    fn kokoro_lexicon_detects_single_and_multi_lang_files() {
        let dir = std::env::temp_dir().join(format!("xenon-kokoro-lexicon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lexicon-en-us.txt"), b"").unwrap();
        let one = kokoro_lexicon(&dir).unwrap();
        let names: Vec<_> = one
            .split(',')
            .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["lexicon-en-us.txt"]);
        std::fs::write(dir.join("lexicon-zh.txt"), b"").unwrap();
        let two = kokoro_lexicon(&dir).unwrap();
        let names: Vec<_> = two
            .split(',')
            .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["lexicon-en-us.txt", "lexicon-zh.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_clears_queued_sentences() {
        let q = SpeechQueue::default();
        q.push("one".into());
        q.push("two".into());
        q.clear();
        assert!(q.pop().is_none());
    }

    #[test]
    fn rms_level_computation() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 512]), 0.0);
        assert!((rms(&[0.5; 512]) - 0.5).abs() < 1e-6);
        assert!((rms(&[1.0; 512]) - 1.0).abs() < 1e-6);
        let clipped = rms(&[2.0; 512]);
        assert_eq!(clipped, 1.0);
    }

    #[test]
    fn level_smoothing_moves_toward_rms() {
        let next = smooth_level(0.0, 1.0);
        assert!(next > 0.0 && next < 1.0);
        assert!(smooth_level(next, 1.0) > next);
    }

    #[test]
    fn level_uses_played_cursor_window() {
        // Only the samples before the cursor count; future samples must not
        // leak into the lip-sync level.
        let mut samples = vec![0.0f32; 4_800];
        for s in samples.iter_mut().take(2_400) {
            *s = 0.8;
        }
        let pb = Playback {
            samples,
            cursor: 2_400,
            rate: 48_000,
        };
        assert!((level_at_cursor(&pb) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn playback_resampler_is_shared() {
        // Playback must call audio_dsp::resample_linear, not a duplicate.
        let pcm = vec![0.25f32; 24_000]; // 1 s at Kokoro's 24 kHz
        let out = resample_linear(&pcm, 24_000, 48_000);
        assert_eq!(out.len(), 48_000);
        assert!((out[0] - 0.25).abs() < 1e-6);
    }

    /// Real Kokoro synthesis of "Hello from Xenon" at the default path.
    /// Skips only when the model itself is not installed.
    /// Run with: cargo test -- --ignored tts
    #[test]
    #[ignore = "loads the real Kokoro model; run with -- --ignored tts"]
    fn tts_kokoro_synthesizes_hello() {
        let cfg = XenonConfig::default();
        let dir = Path::new(&cfg.tts_model_dir);
        if !dir.join("model.onnx").exists() || !dir.join("voices.bin").exists() {
            eprintln!("skip: Kokoro model absent at {}", dir.display());
            return;
        }
        let tts = OfflineTts::create(&offline_tts_config(dir, TtsKind::Kokoro))
            .expect("create Kokoro TTS");
        let gen = GenerationConfig {
            speed: cfg.tts_speed,
            sid: cfg.tts_speaker_id as i32,
            ..Default::default()
        };
        let audio = tts
            .generate_with_config("Hello from Xenon", &gen, None::<fn(&[f32], f32) -> bool>)
            .expect("synthesize");
        assert!(!audio.samples().is_empty(), "empty synthesis result");
        assert!(audio.sample_rate() > 0);
        eprintln!(
            "synthesized {} samples at {} Hz",
            audio.samples().len(),
            audio.sample_rate()
        );
    }

    /// Real OOV probe: names and a clock time must not silently vanish.
    #[test]
    #[ignore = "loads the real Kokoro model; run with -- --ignored oov"]
    fn tts_kokoro_oov_sentence() {
        let cfg = XenonConfig::default();
        let dir = Path::new(&cfg.tts_model_dir);
        if !dir.join("model.onnx").exists() || !dir.join("voices.bin").exists() {
            eprintln!("skip: Kokoro model absent at {}", dir.display());
            return;
        }
        let tts = OfflineTts::create(&offline_tts_config(dir, TtsKind::Kokoro))
            .expect("create Kokoro TTS");
        let text = "Xenon opened Safari for Samer at 3:45pm";
        let prepared = prepare_kokoro_text(text, &load_lexicon_words(dir));
        eprintln!("oov prepared: {prepared:?}");
        let gen = GenerationConfig {
            speed: cfg.tts_speed,
            sid: cfg.tts_speaker_id as i32,
            ..Default::default()
        };
        let audio = tts
            .generate_with_config(&prepared, &gen, None::<fn(&[f32], f32) -> bool>)
            .expect("synthesize OOV sentence");
        assert!(!audio.samples().is_empty(), "empty OOV synthesis result");
        eprintln!(
            "oov synthesized {} samples at {} Hz",
            audio.samples().len(),
            audio.sample_rate()
        );
    }
}
