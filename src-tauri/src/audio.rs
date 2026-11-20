//! The real Audio Pipeline: cpal capture -> downmix -> resample to 16 kHz ->
//! Silero VAD (whisper-rs) -> streaming segmenter -> whisper-rs STT ->
//! AudioEvent::Speech. Plus a global-hotkey force-listen and `audio-level`
//! events for the Listening waveform.
//!
//! Threading: cpal runs its input callback on its own thread and only
//! downmixes + pushes small f32 frames over a bounded mpsc channel. A dedicated
//! `std::thread` worker owns the resampler, the VAD context, and the whisper STT
//! context (lazily loaded / idle-unloaded) and all blocking inference, so the
//! tokio workers are never blocked. The worker speaks to the rest of the app
//! only through the `audio_tx` AudioEvent channel (pet-state changes are routed
//! through the runtime) and its `AppHandle` (for `audio-level` / `xenon-notice`).

use anyhow::{Context as _, Result};
use async_channel::Sender;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams,
};

use crate::audio_dsp::{resample_linear, Segmenter};
use crate::config::XenonConfig;
use crate::events::{AudioEvent, Utterance};
use crate::voice;

const TARGET_RATE: u32 = 16_000;
/// ~one Silero frame at 16 kHz (whisper.cpp uses a 512-sample window).
const VAD_WINDOW: usize = 512;
/// Run VAD once this much new audio has accumulated (milliseconds).
const VAD_STRIDE_MS: u32 = 300;
/// Pre-roll kept before an onset (milliseconds) for cleaner transcription.
const PRE_ROLL_MS: u32 = 300;
/// Hotkey force-listen closes after this long with no speech.
const FORCE_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
/// After the wake word, wait this long for the command before re-arming.
const WAKE_LISTEN_TIMEOUT: Duration = Duration::from_secs(12);
/// Wake-word segments are capped this short so a name is checked quickly.
const WAKE_MAX_SPEECH_MS: u32 = 2_500;
/// Emit `audio-level` at ~30 Hz.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);
/// Trim the in-session silence buffer past this length (bounds always-on RAM).
const SILENCE_TRIM_MAX_MS: u64 = 60_000;
/// Don't run VAD until we've buffered at least this many 16 kHz samples.
const MIN_VAD_SAMPLES: usize = VAD_WINDOW * 2;

#[derive(Serialize, Clone)]
struct AudioLevel {
    rms: f32,
}

enum Command {
    ForceListen,
}

/// Handle to the Audio Pipeline, kept in Tauri-managed state and used by the
/// hotkey handler. [`AudioPipeline::new`] spawns the worker thread.
pub struct AudioPipeline {
    cmd_tx: mpsc::Sender<Command>,
}

impl AudioPipeline {
    pub fn new(
        audio_tx: Sender<AudioEvent>,
        app: AppHandle,
        config: Arc<RwLock<XenonConfig>>,
        speaking: Arc<AtomicBool>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("xenon-audio".into())
            .spawn(move || worker(cmd_rx, app, config, audio_tx, speaking))
            .expect("spawn audio worker");
        Self { cmd_tx }
    }

    /// Global hotkey (Cmd+Shift+Space): force a listen window open and, via the
    /// runtime, barge-in out of Speaking into Listening.
    pub fn force_listen(&self) {
        let _ = self.cmd_tx.send(Command::ForceListen);
    }
}

/// Per-session worker state. Created once per `worker()` invocation.
struct Worker {
    cmd_rx: Receiver<Command>,
    audio_tx: Sender<AudioEvent>,
    app: AppHandle,
    config: Arc<RwLock<XenonConfig>>,
    cfg: XenonConfig,
    cfg_loaded_at: Instant,
    stream: Option<Stream>,
    frame_rx: Option<Receiver<Vec<f32>>>,
    native_rate: u32,
    input_buf: Vec<f32>,
    captured: Vec<f32>,
    seg: Segmenter,
    frames_committed: usize,
    since_vad: usize,
    /// Last computed captured-samples-per-VAD-frame ratio (for slicing audio).
    last_spf: usize,
    vad: Option<WhisperVadContext>,
    stt: Option<WhisperContext>,
    stt_last_used: Option<Instant>,
    force: bool,
    speaking: Arc<AtomicBool>,
    awake: bool,
    awake_until: Instant,
    /// Wake fired with a same-breath command; let the agent own the state
    /// instead of closing the window with a Silence event.
    awake_with_command: bool,
    wake_seg: Segmenter,
    wake_stt: Option<WhisperContext>,
    wake_notice_sent: bool,
    /// Capture is open but no hotkey session has started (lazy_audio = false).
    /// Frames are dropped until the hotkey flips this off.
    preloaded: bool,
    last_speech: Instant,
    last_level: Instant,
    model_notice_sent: bool,
}

fn worker(
    cmd_rx: Receiver<Command>,
    app: AppHandle,
    config: Arc<RwLock<XenonConfig>>,
    audio_tx: Sender<AudioEvent>,
    speaking: Arc<AtomicBool>,
) {
    // Start in hotkey mode by default; always-on opens below.
    let initial_cfg = config.try_read().map(|c| c.clone()).unwrap_or_default();
    let mut w = Worker {
        cmd_rx,
        audio_tx,
        app,
        config,
        cfg: initial_cfg.clone(),
        cfg_loaded_at: Instant::now(),
        stream: None,
        frame_rx: None,
        native_rate: TARGET_RATE,
        input_buf: Vec::new(),
        captured: Vec::new(),
        seg: Segmenter::new(0.5, 32.0, 30, initial_cfg.silence_ms as u32, 300, 30_000),
        frames_committed: 0,
        since_vad: 0,
        last_spf: VAD_WINDOW,
        vad: None,
        stt: None,
        stt_last_used: None,
        force: false,
        speaking,
        awake: false,
        awake_until: Instant::now(),
        awake_with_command: false,
        wake_seg: Segmenter::new(
            0.5,
            32.0,
            30,
            initial_cfg.silence_ms as u32,
            300,
            WAKE_MAX_SPEECH_MS,
        ),
        wake_stt: None,
        wake_notice_sent: false,
        preloaded: false,
        last_speech: Instant::now(),
        last_level: Instant::now(),
        model_notice_sent: false,
    };

    // Wake-word mode keeps capture + VAD warm but the main STT model lazy.
    if w.cfg.listen_mode == "always" || w.cfg.listen_mode == "wake_word" {
        w.open_session(false);
    } else if !w.cfg.lazy_audio {
        // Performance toggle off: warm the mic and VAD at startup.
        w.preload();
    }

    loop {
        if !w.poll_events() {
            break;
        }
    }
    log::info!("audio worker exiting");
}

impl Worker {
    /// One loop tick. Returns `false` when the worker should exit.
    fn poll_events(&mut self) -> bool {
        // Block briefly for audio (keeps tokens flowing), then check commands.
        let timeout = if self.stream.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(200)
        };
        if let Some(rx) = &self.frame_rx {
            match rx.recv_timeout(timeout) {
                Ok(frames) => self.on_frames(&frames),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.close_session();
                    return true;
                }
            }
        } else {
            std::thread::sleep(timeout);
        }

        loop {
            match self.cmd_rx.try_recv() {
                Ok(Command::ForceListen) => self.on_force_listen(),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }

        self.refresh_config();
        self.tick_timers();
        true
    }

    /// Refresh the cached config snapshot at most once every second.
    fn wake_mode(&self) -> bool {
        self.cfg.listen_mode == "wake_word"
    }

    fn refresh_config(&mut self) {
        if self.cfg_loaded_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.cfg_loaded_at = Instant::now();
        if let Ok(guard) = self.config.try_read() {
            let new = guard.clone();
            // Rebuild the segmenters only if their timing inputs changed.
            if new.silence_ms != self.cfg.silence_ms {
                self.seg = Segmenter::new(0.5, 32.0, 30, new.silence_ms as u32, 300, 30_000);
                self.wake_seg = Segmenter::new(
                    0.5,
                    32.0,
                    30,
                    new.silence_ms as u32,
                    300,
                    WAKE_MAX_SPEECH_MS,
                );
            }
            self.cfg = new;
        }
    }

    fn on_force_listen(&mut self) {
        // A hotkey/tray/pet click wakes the pipeline directly.
        if self.wake_mode() {
            self.awake = true;
            self.awake_until = Instant::now() + WAKE_LISTEN_TIMEOUT;
            self.awake_with_command = false;
            self.wake_seg.reset();
        }
        // Barge-in: tell the runtime to swap Speaking -> Listening. If TTS is
        // playing, task 3 stops it here (see runtime presentation_loop hook).
        self.emit(AudioEvent::ForceListen);
        if self.preloaded && self.stream.is_some() {
            // Warm mic from preload(): reuse the stream instead of reopening.
            self.preloaded = false;
            self.force = true;
            self.input_buf.clear();
            self.captured.clear();
            self.seg.reset();
            self.frames_committed = 0;
            self.since_vad = 0;
            self.last_speech = Instant::now();
            self.emit(AudioEvent::Listening);
            log::info!("audio: hotkey using preloaded capture");
            return;
        }
        self.open_session(true);
    }

    /// Performance toggle off: open capture and load the VAD model at
    /// startup so the first hotkey press pays for neither. Nothing is
    /// resampled or transcribed until the hotkey opens a session, and no pet
    /// state changes.
    fn preload(&mut self) {
        self.input_buf.clear();
        self.captured.clear();
        self.seg.reset();
        self.wake_seg.reset();
        self.frames_committed = 0;
        self.since_vad = 0;
        self.force = false;
        if let Err(e) = self.open_capture() {
            log::error!("audio: preload capture failed: {e:#}");
            self.close_session();
            return;
        }
        self.preloaded = true;
        if self.vad.is_none() {
            if let Err(e) = self.load_vad() {
                // Keep the warm mic; the hotkey path loads VAD (and reports a
                // missing model) exactly as before.
                log::warn!("audio: preload VAD failed: {e:#}");
            }
        }
        log::info!("audio: capture + VAD preloaded (lazy_audio = false)");
    }

    fn open_session(&mut self, force: bool) {
        self.close_session();
        self.force = force;
        self.input_buf.clear();
        self.captured.clear();
        self.seg.reset();
        self.wake_seg.reset();
        self.frames_committed = 0;
        self.since_vad = 0;
        self.last_speech = Instant::now();

        if let Err(e) = self.open_capture() {
            log::error!("audio: failed to open capture: {e:#}");
            self.emit_notice(&format!(
                "Couldn't open the microphone ({e}). Grant access in System \
                 Settings -> Privacy & Security -> Microphone, then try the \
                 hotkey again."
            ));
            self.close_session();
            return;
        }
        if self.cfg.listen_mode == "hotkey" && force {
            // Hotkey mode: show Listening immediately. Wake mode gets its
            // Listening transition from the Wake event; always mode stays Idle.
            self.emit(AudioEvent::Listening);
        }
        log::info!(
            "audio: capture open (force={force}, rate={} Hz)",
            self.native_rate
        );
    }

    /// Transcribe any still-open (partial) utterance, then close the session.
    /// Used when a hotkey listen window times out mid-sentence.
    fn finish_and_close(&mut self) {
        if let Some((start, end)) = self.seg.flush() {
            let spf = self.last_spf.max(1);
            let pre = (PRE_ROLL_MS as usize * TARGET_RATE as usize) / 1000 / spf;
            let s0 = start.saturating_sub(pre);
            let lo = (s0 * spf).min(self.captured.len());
            let hi = (end * spf).min(self.captured.len());
            if hi > lo {
                let audio = self.captured[lo..hi].to_vec();
                self.on_segment(&audio);
            } else {
                self.emit(AudioEvent::Silence);
            }
        } else {
            self.emit(AudioEvent::Silence);
        }
        self.close_session();
    }

    fn close_session(&mut self) {
        // Dropping the stream + frame channel also detaches the cpal callback.
        self.stream = None;
        self.frame_rx = None;
        self.input_buf.clear();
        self.captured.clear();
        self.seg.reset();
        self.wake_seg.reset();
        self.frames_committed = 0;
        self.since_vad = 0;
        self.force = false;
        self.preloaded = false;
    }

    /// Build and play a cpal input stream, wiring a fresh frame channel.
    fn open_capture(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input device")?;
        let supported = device
            .default_input_config()
            .context("no supported input config")?;
        let cfg: StreamConfig = supported.config();
        let channels = cfg.channels as usize;
        let rate = cfg.sample_rate.0;
        self.native_rate = rate;

        let (frame_tx, frame_rx): (SyncSender<Vec<f32>>, Receiver<Vec<f32>>) =
            mpsc::sync_channel(64);
        self.frame_rx = Some(frame_rx);

        let app = self.app.clone();
        let err_cb = move |e: cpal::StreamError| {
            log::warn!("audio: stream error: {e}");
            let _ = app.emit("xenon-notice", format!("Microphone stream error: {e}"));
        };

        let stream = match supported.sample_format() {
            SampleFormat::F32 => {
                let tx = frame_tx.clone();
                device.build_input_stream::<f32, _, _>(
                    &cfg,
                    move |data: &[f32], _| push_mono(data, channels, &tx),
                    err_cb,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let tx = frame_tx.clone();
                device.build_input_stream::<i16, _, _>(
                    &cfg,
                    move |data: &[i16], _| {
                        let f: Vec<f32> =
                            data.iter().map(|&x| x as f32 / i16::MAX as f32).collect();
                        push_mono(&f, channels, &tx)
                    },
                    err_cb,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let tx = frame_tx.clone();
                device.build_input_stream::<u16, _, _>(
                    &cfg,
                    move |data: &[u16], _| {
                        let f: Vec<f32> = data
                            .iter()
                            .map(|&x| (x as f32 - 32_768.0) / 32_768.0)
                            .collect();
                        push_mono(&f, channels, &tx)
                    },
                    err_cb,
                    None,
                )?
            }
            SampleFormat::I8 => {
                let tx = frame_tx.clone();
                device.build_input_stream::<i8, _, _>(
                    &cfg,
                    move |data: &[i8], _| {
                        let f: Vec<f32> =
                            data.iter().map(|&x| x as f32 / i8::MAX as f32).collect();
                        push_mono(&f, channels, &tx)
                    },
                    err_cb,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported sample format: {other:?}"),
        };
        stream.play().context("start capture stream")?;
        self.stream = Some(stream);
        Ok(())
    }

    fn on_frames(&mut self, frames: &[f32]) {
        // Preloaded and no hotkey session: the mic is warm but idle. Drop the
        // frames so nothing is resampled, transcribed, or emitted.
        if self.preloaded {
            self.input_buf.clear();
            return;
        }
        self.input_buf.extend_from_slice(frames);
        // Enough new native-rate audio to make one resample block.
        let block_need = (self.native_rate as usize * VAD_STRIDE_MS as usize) / 1000;
        // VAD re-run cadence, measured in 16 kHz samples.
        let vad_need = (TARGET_RATE as usize * VAD_STRIDE_MS as usize) / 1000;
        if self.input_buf.len() >= block_need.max(1) {
            let block = std::mem::take(&mut self.input_buf);
            let r16 = resample_linear(&block, self.native_rate, TARGET_RATE);
            self.captured.extend_from_slice(&r16);
            self.since_vad += r16.len();
            if self.captured.len() >= MIN_VAD_SAMPLES && self.since_vad >= vad_need {
                self.run_vad();
            }
        }
        self.maybe_emit_level();
    }

    fn run_vad(&mut self) {
        self.since_vad = 0;
        if self.vad.is_none() {
            if let Err(e) = self.load_vad() {
                log::error!("audio: VAD load failed: {e:#}");
                self.emit_notice(&format!(
                    "Voice detection model missing ({e}). Speech transcription \
                     is disabled until it's configured."
                ));
                self.close_session();
                return;
            }
        }
        let vad = self.vad.as_mut().expect("vad loaded");
        if let Err(e) = vad.detect_speech(&self.captured) {
            log::error!("audio: VAD detect_speech failed: {e:#}");
            return;
        }
        // Clone so the `&mut self.vad` borrow ends before we call other
        // `&mut self` methods below.
        let probs = vad.probabilities().to_vec();
        if probs.is_empty() {
            return;
        }
        let spf = (self.captured.len() / probs.len()).max(1);
        self.last_spf = spf;
        let start_idx = self.frames_committed.min(probs.len());

        for p in &probs[start_idx..] {
            let wake_listening = self.wake_mode() && !self.awake;
            let was_speaking = self.seg.in_speech();
            let mut segment: Option<Vec<f32>> = None;
            let mut wake_segment = false;

            if !wake_listening {
                if let Some((start, end)) = self.seg.push(*p) {
                    let pre = (PRE_ROLL_MS as usize * TARGET_RATE as usize) / 1000 / spf;
                    let s0 = start.saturating_sub(pre);
                    let s1 = (end * spf).min(self.captured.len());
                    let lo = (s0 * spf).min(self.captured.len());
                    if s1 > lo {
                        segment = Some(self.captured[lo..s1].to_vec());
                    }
                    self.seg.reset();
                    self.frames_committed = 0;
                    self.captured.clear();
                    self.since_vad = 0;
                }
            } else if let Some((start, end)) = self.wake_seg.push(*p) {
                // Wake mode, not yet awake: feed a short-segment whisper
                // segmenter instead of the main one.
                let s1 = (end * spf).min(self.captured.len());
                let lo = (start * spf).min(self.captured.len());
                if s1 > lo {
                    segment = Some(self.captured[lo..s1].to_vec());
                    wake_segment = true;
                }
                self.wake_seg.reset();
                self.frames_committed = 0;
                self.captured.clear();
                self.since_vad = 0;
            }

            if let Some(audio) = segment {
                if wake_segment {
                    self.on_wake_segment(&audio);
                } else {
                    self.on_segment(&audio);
                }
                if self.force {
                    // Hotkey window ends once an utterance is transcribed.
                    self.emit(AudioEvent::Silence);
                    self.close_session();
                }
                return;
            }
            if !wake_listening && !was_speaking && self.seg.in_speech() {
                // Speech onset: start the Listening waveform via the runtime.
                self.emit(AudioEvent::Listening);
                self.last_speech = Instant::now();
            }
            self.frames_committed += 1;
        }
        self.frames_committed = probs.len();
    }

    fn on_segment(&mut self, audio: &[f32]) {
        let language = self.cfg.language.trim().to_string();
        let lang = if language.is_empty() || language.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(language.as_str())
        };
        match self.transcribe(audio, lang) {
            Ok(Some(utterance)) => self.emit(AudioEvent::Speech(utterance)),
            Ok(None) => self.emit(AudioEvent::Silence),
            Err(e) => {
                log::error!("audio: STT failed: {e:#}");
                if !self.model_notice_sent {
                    self.model_notice_sent = true;
                    self.emit_notice(&format!("Speech transcription failed: {e}"));
                }
                self.emit(AudioEvent::Silence);
            }
        }
    }

    /// Short VAD segments transcribed by the small wake model. Main STT stays
    /// unloaded until a wake phrase actually matches.
    fn on_wake_segment(&mut self, audio: &[f32]) {
        if self.awake {
            return;
        }
        // Echo guard: don't let the pet wake itself while it speaks. High
        // sensitivity deliberately allows barge-in.
        if self.speaking.load(Ordering::Relaxed) && self.cfg.wake_sensitivity < 0.85 {
            return;
        }
        let Some(text) = self.transcribe_wake(audio) else {
            return;
        };
        let phrase = self.cfg.effective_wake_phrase();
        if let Some(m) = voice::match_wake(&text, &phrase, self.cfg.wake_sensitivity) {
            self.awake = true;
            self.awake_until = Instant::now() + WAKE_LISTEN_TIMEOUT;
            self.awake_with_command = !m.trailing.is_empty();
            // Discard wake audio; the same-breath command starts fresh.
            self.seg.reset();
            self.wake_seg.reset();
            self.captured.clear();
            self.frames_committed = 0;
            self.since_vad = 0;
            self.emit(AudioEvent::Wake {
                text: (!m.trailing.is_empty()).then_some(m.trailing),
            });
            if self.cfg.wake_chime {
                play_chime();
            }
        }
    }

    /// Reuse the tiny wake STT context; return `None` on missing model or
    /// empty/hallucinated transcript.
    fn transcribe_wake(&mut self, audio: &[f32]) -> Option<String> {
        let path = self.wake_model_path();
        if self.wake_stt.is_none() {
            match WhisperContext::new_with_params(
                &path,
                WhisperContextParameters::new(),
            ) {
                Ok(ctx) => self.wake_stt = Some(ctx),
                Err(e) => {
                    if !self.wake_notice_sent {
                        self.wake_notice_sent = true;
                        log::warn!("audio: wake model load failed: {e:#}");
                        self.emit_notice(&format!(
                            "Wake-word model missing ({e}). Run scripts/fetch-models.sh \
                             or set wake_model_path in Settings -> Voice & Wake word."
                        ));
                    }
                    return None;
                }
            }
        }
        let ctx = self.wake_stt.as_ref()?;
        match run_stt(ctx, audio, Some("en")) {
            Ok(Some(u)) => Some(u.text),
            Ok(None) => None,
            Err(e) => {
                log::warn!("audio: wake STT failed: {e:#}");
                None
            }
        }
    }

    fn wake_model_path(&self) -> String {
        let p = self.cfg.wake_model_path.trim();
        if !p.is_empty() && std::path::Path::new(p).exists() {
            return p.to_string();
        }
        self.cfg.whisper_model_path.clone()
    }

    /// Lazily load / reuse the whisper STT context and transcribe *audio*.
    /// Returns `Ok(None)` for empty audio or a filtered hallucination.
    fn transcribe(&mut self, audio: &[f32], lang: Option<&str>) -> Result<Option<Utterance>> {
        if audio.is_empty() {
            return Ok(None);
        }
        if self.stt.is_none() {
            let path = self.cfg.whisper_model_path.clone();
            if !std::path::Path::new(&path).exists() {
                anyhow::bail!(
                    "whisper model not found at {path}. Set it in Settings -> \
                     Voice & Audio, or run scripts/fetch-models.sh."
                );
            }
            log::info!("audio: loading whisper STT model from {path}");
            self.stt = Some(WhisperContext::new_with_params(
                &path,
                WhisperContextParameters::default(),
            )?);
        }
        self.stt_last_used = Some(Instant::now());
        self.model_notice_sent = false;
        let ctx = self.stt.as_ref().expect("stt loaded");
        run_stt(ctx, audio, lang)
    }

    fn load_vad(&mut self) -> Result<()> {
        let path = self.cfg.vad_model_path.clone();
        if !std::path::Path::new(&path).exists() {
            anyhow::bail!(
                "VAD model not found at {path}. Set it in Settings -> Voice & \
                 Audio, or run scripts/fetch-models.sh."
            );
        }
        let mut pctx = WhisperVadContextParams::default();
        pctx.set_n_threads(2);
        pctx.set_use_gpu(false);
        self.vad = Some(WhisperVadContext::new(&path, pctx)?);
        Ok(())
    }

    fn tick_timers(&mut self) {
        // Idle-unload the STT model after N minutes.
        if let Some(last) = self.stt_last_used {
            let idle = Duration::from_secs(self.cfg.stt_unload_minutes.saturating_mul(60));
            if idle.as_secs() > 0 && last.elapsed() >= idle {
                log::info!("audio: unloading idle STT model (idle > {} min)", self.cfg.stt_unload_minutes);
                self.stt = None;
                self.stt_last_used = None;
            }
        }
        // Wake-word listening window: close it when the timer runs out and the
        // user did not already send a command (the agent owns the state then).
        if self.wake_mode()
            && self.awake
            && !self.awake_with_command
            && Instant::now() >= self.awake_until
        {
            self.awake = false;
            self.seg.reset();
            self.wake_seg.reset();
            self.captured.clear();
            self.frames_committed = 0;
            self.since_vad = 0;
            self.emit(AudioEvent::Silence);
        }
        // Hotkey window timeout: after FORCE_IDLE_TIMEOUT with no trailing silence,
        // transcribe whatever was captured, then close.
        if self.seg.in_speech() {
            // Keep the timer warm while speech is ongoing.
            self.last_speech = Instant::now();
        }
        if self.stream.is_some()
            && self.force
            && self.last_speech.elapsed() >= FORCE_IDLE_TIMEOUT
        {
            self.finish_and_close();
        }
        // Bound always-on silence memory (both segmenters).
        if self.stream.is_some() && !self.seg.in_speech() && !self.wake_seg.in_speech() {
            let cap = (TARGET_RATE as usize) * (SILENCE_TRIM_MAX_MS as usize / 1000);
            if self.captured.len() > cap {
                self.frames_committed = 0;
                self.captured.clear();
                self.seg.reset();
                self.wake_seg.reset();
                self.since_vad = 0;
            }
        }
    }

    fn maybe_emit_level(&mut self) {
        // ~30 Hz, and only while a speech segment is open (i.e. Listening).
        if self.last_level.elapsed() < LEVEL_INTERVAL {
            return;
        }
        self.last_level = Instant::now();
        if !self.seg.in_speech() {
            return;
        }
        let n = (TARGET_RATE as usize / 20).min(self.captured.len()); // last 50 ms
        if n == 0 {
            return;
        }
        let window = &self.captured[self.captured.len() - n..];
        let rms = (window.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        let rms = rms.clamp(0.0, 1.0);
        let _ = self.app.emit("audio-level", AudioLevel { rms });
    }

    fn emit(&self, event: AudioEvent) {
        if self.audio_tx.try_send(event).is_err() {
            log::warn!("audio: agent channel full, dropped event");
        }
    }

    fn emit_notice(&self, msg: &str) {
        log::warn!("audio notice: {msg}");
        let _ = self.app.emit("xenon-notice", msg.to_string());
    }
}

/// Play a soft two-note chime when the wake word fires. Best-effort: any
/// failure (no output device) is logged and ignored.
fn play_chime() {
    std::thread::spawn(|| {
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            log::warn!("audio: no output device for wake chime");
            return;
        };
        let Ok(support) = device.default_output_config() else {
            log::warn!("audio: no default output config for wake chime");
            return;
        };
        let rate = support.sample_rate().0 as f32;
        let spec = support.config();
        let total = (rate * 0.24) as usize;
        let note = 660.0f32;

        let mut i = 0usize;
        let write = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for sample in data.iter_mut() {
                if i >= total {
                    *sample = 0.0;
                    continue;
                }
                let t = i as f32 / rate;
                let env = (1.0 - (t / 0.24)).clamp(0.0, 1.0).min(1.0);
                let env = env * env;
                *sample = (2.0 * std::f32::consts::PI * note * t).sin() * 0.08 * env;
                i += 1;
            }
        };
        let stream = match support.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                &spec,
                write,
                |e| log::warn!("audio: wake chime stream error: {e}"),
                None,
            ),
            _ => {
                log::warn!("audio: unsupported output format for wake chime");
                return;
            }
        };
        let Ok(stream) = stream else {
            log::warn!("audio: failed to open wake chime stream");
            return;
        };
        if stream.play().is_err() {
            log::warn!("audio: failed to start wake chime stream");
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    });
}

/// Record `seconds` of mono 16 kHz audio from the default input device. Used by
/// the experimental voice-clone flow (Settings -> Voice & Wake word).
pub(crate) fn record_samples(seconds: u32) -> Result<Vec<f32>> {
    let seconds = seconds.clamp(3, 15);
    let device = cpal::default_host()
        .default_input_device()
        .context("no default input device")?;
    let cfg = device.default_input_config()?.clone();
    let native_rate = cfg.sample_rate().0;
    let channels = cfg.channels() as usize;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(512);
    let push = move |frames: &[f32], channels: usize| {
        let f: Vec<f32> = if channels <= 1 {
            frames.to_vec()
        } else {
            let mut v = Vec::with_capacity(frames.len() / channels + 1);
            let mut i = 0;
            while i + channels <= frames.len() {
                let mut s = 0.0;
                for c in 0..channels {
                    s += frames[i + c];
                }
                v.push(s / channels as f32);
                i += channels;
            }
            v
        };
        let _ = tx.try_send(f);
    };
    let err_cb = |e| log::warn!("audio: record stream error: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &cfg.config(),
            move |data: &[f32], _| push(data, channels),
            err_cb,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            &cfg.config(),
            move |data: &[i16], _| {
                let f: Vec<f32> = data.iter().map(|&x| x as f32 / i16::MAX as f32).collect();
                push(&f, channels);
            },
            err_cb,
            None,
        )?,
        SampleFormat::I32 => device.build_input_stream(
            &cfg.config(),
            move |data: &[i32], _| {
                let f: Vec<f32> = data.iter().map(|&x| x as f32 / i32::MAX as f32).collect();
                push(&f, channels);
            },
            err_cb,
            None,
        )?,
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    };
    stream.play().context("start record stream")?;

    let deadline = Instant::now() + Duration::from_secs(seconds as u64);
    let mut raw: Vec<f32> = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frames) => raw.extend_from_slice(&frames),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stream);
    if raw.is_empty() {
        anyhow::bail!("recorded no audio");
    }
    Ok(resample_linear(&raw, native_rate, TARGET_RATE))
}

/// Transcribe already-captured mono 16 kHz samples with the small wake model
/// (falls back to the main STT model). Returns an empty string on failure.
pub(crate) fn transcribe_samples(samples: &[f32], cfg: &XenonConfig) -> Result<String> {
    let mut path = cfg.wake_model_path.clone();
    if path.trim().is_empty() || !std::path::Path::new(&path).exists() {
        path = cfg.whisper_model_path.clone();
    }
    if path.trim().is_empty() || !std::path::Path::new(&path).exists() {
        anyhow::bail!("no whisper model available for transcription");
    }
    let ctx = WhisperContext::new_with_params(&path, WhisperContextParameters::new())?;
    match run_stt(&ctx, samples, None) {
        Ok(Some(u)) => Ok(u.text),
        Ok(None) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Downmix interleaved *frames* to mono f32 and push to the cpal -> worker
/// channel (dropping rather than blocking the audio thread).
fn push_mono(frames: &[f32], channels: usize, tx: &SyncSender<Vec<f32>>) {
    let out = if channels <= 1 {
        frames.to_vec()
    } else {
        let mut v = Vec::with_capacity(frames.len() / channels + 1);
        let mut i = 0;
        while i + channels <= frames.len() {
            let mut s = 0.0;
            for c in 0..channels {
                s += frames[i + c];
            }
            v.push(s / channels as f32);
            i += channels;
        }
        v
    };
    let _ = tx.try_send(out);
}

/// Run whisper STT over *samples* (16 kHz mono f32) and return a cleaned
/// `Utterance`, or `None` when the transcript is empty or a known
/// hallucination. Pure inference: no state, no model loading, no emitting.
fn run_stt(
    ctx: &WhisperContext,
    samples: &[f32],
    language: Option<&str>,
) -> Result<Option<Utterance>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
        .min(8);
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_language(language);
    params.set_translate(false);
    params.set_no_context(true);
    params.set_single_segment(true);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    let mut state = ctx.create_state()?;
    state.full(params, samples)?;

    let mut text = String::new();
    let mut psum = 0.0f32;
    let mut pn = 0u32;
    for seg in state.as_iter() {
        text.push_str(&seg.to_str_lossy()?);
        for t in 0..seg.n_tokens() {
            if let Some(tok) = seg.get_token(t) {
                let p = tok.token_probability();
                if p > 0.0 {
                    psum += p;
                    pn += 1;
                }
            }
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() || crate::audio_dsp::is_hallucination(&text) {
        return Ok(None);
    }
    let confidence = if pn > 0 { (psum / pn as f32).clamp(0.0, 1.0) } else { 0.0 };
    Ok(Some(Utterance { text, confidence }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_mono_averages_channels() {
        let (tx, rx) = mpsc::sync_channel(4);
        // Two stereo frames: (1.0, -1.0) -> 0.0 ; (0.4, 0.8) -> 0.6
        push_mono(&[1.0, -1.0, 0.4, 0.8], 2, &tx);
        let v = rx.try_recv().unwrap();
        assert_eq!(v.len(), 2);
        assert!((v[0] - 0.0).abs() < 1e-6);
        assert!((v[1] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn push_mono_mono_passthrough() {
        let (tx, rx) = mpsc::sync_channel(4);
        push_mono(&[0.1, 0.2, 0.3], 1, &tx);
        assert_eq!(rx.try_recv().unwrap(), vec![0.1, 0.2, 0.3]);
    }

    /// End-to-end check with the real ggml models at the default paths:
    /// 1 s of generated silence must produce no speech — neither a VAD
    /// speech segment nor a non-hallucination STT utterance. Skipped
    /// gracefully if the models aren't present on this machine.
    #[test]
    #[ignore = "loads real multi-GB whisper + VAD models; run with -- --ignored"]
    fn real_models_silence_yields_no_speech() {
        let cfg = crate::config::XenonConfig::default();
        if !std::path::Path::new(&cfg.vad_model_path).exists()
            || !std::path::Path::new(&cfg.whisper_model_path).exists()
        {
            eprintln!(
                "skip: models absent (vad={}, stt={})",
                cfg.vad_model_path, cfg.whisper_model_path
            );
            return;
        }

        // VAD: feed 1 s of silence, expect no completed speech segment.
        let mut pctx = WhisperVadContextParams::default();
        pctx.set_n_threads(2);
        pctx.set_use_gpu(false);
        let mut vad = WhisperVadContext::new(&cfg.vad_model_path, pctx)
            .expect("load VAD model");
        let silence = vec![0.0f32; TARGET_RATE as usize];
        vad.detect_speech(&silence).expect("vad detect");
        let probs = vad.probabilities().to_vec();
        let mut seg = Segmenter::new(0.5, 32.0, 30, 700, 300, 30_000);
        let vad_spoke = probs.iter().any(|&p| seg.push(p).is_some());
        assert!(!vad_spoke, "silence should not yield a VAD speech segment");

        // STT: 1 s of silence should transcribe to empty/hallucination.
        let ctx = WhisperContext::new_with_params(
            &cfg.whisper_model_path,
            WhisperContextParameters::default(),
        )
        .expect("load STT model");
        let out = run_stt(&ctx, &silence, Some("en")).expect("stt run");
        assert!(out.is_none(), "silence should yield no Speech utterance");
    }
}