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

use crate::audio_dsp::{
    rms, resample_linear, NoiseGate, Segmenter, WAKE_MAX_SEG_MS, WAKE_MIN_SEG_MS,
    WAKE_TRANSCRIBE_MS,
};
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
/// Emit `audio-level` at ~30 Hz.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);
/// When idle (no segment open) keep only this much recent audio. The VAD scans
/// the whole buffer every tick, so an ever-growing idle buffer makes wake-word
/// CPU grow without bound. A wake utterance can't be missed: onset is detected
/// from the recent window, and this is far longer than the 300 ms pre-roll.
const IDLE_KEEP_MS: u64 = 3_000;
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
    /// Last time an input RMS diagnostic was logged (5 s cadence).
    last_rms_log: Instant,
    model_notice_sent: bool,
    /// A wake_word/always session wants the mic open. When the stream is not
    /// open (startup failure, device unplugged, stream error) the worker keeps
    /// retrying open_capture() on a backoff instead of giving up until restart.
    mic_want_open: bool,
    /// When to next attempt open_capture(); None = no retry pending.
    mic_retry_at: Option<Instant>,
    /// Current retry delay (5 s → 30 s max, doubling).
    mic_retry_delay: Duration,
    /// Set by the cpal error callback (runs on cpal's thread) so the worker
    /// reopens the stream on the next tick.
    stream_failed: Arc<AtomicBool>,
    /// Last mic-open error, for a one-shot "permission denied" vs device notice.
    mic_error_notice_sent: bool,
    /// Adaptive noise floor + energy gate so wake STT only runs on segments
    /// that stand out from room/fan noise.
    noise_gate: NoiseGate,
    /// Rolling wall-clock window + time spent in wake STT, for the duty cap.
    wake_duty_window_start: Instant,
    wake_duty_stt: Duration,
}

/// Mic-open retry schedule: start at 5 s, double each failure, cap at 30 s.
/// Pure so it is unit-testable without a device.
fn next_mic_backoff(current: Duration) -> Duration {
    const START: Duration = Duration::from_secs(5);
    const MAX: Duration = Duration::from_secs(30);
    if current.is_zero() {
        return START;
    }
    let doubled = current.saturating_mul(2);
    doubled.min(MAX)
}

/// Classify a mic-open failure so the user gets a precise message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicErrorKind {
    /// Microphone TCC grant is missing/denied — point at System Settings.
    PermissionDenied,
    /// No input device, wrong format, unplugged — a device problem.
    Device,
}

/// Classify by the underlying error text plus the live AVFoundation mic grant.
/// cpal surfaces a missing macOS mic grant as "no supported input config …
/// Unknown property"; the AV status is the authoritative signal.
pub fn classify_mic_error(err_text: &str, mic_granted: bool) -> MicErrorKind {
    let lower = err_text.to_ascii_lowercase();
    let looks_like_permission =
        lower.contains("no supported input config") || lower.contains("unknown property");
    if !mic_granted || looks_like_permission {
        MicErrorKind::PermissionDenied
    } else {
        MicErrorKind::Device
    }
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
            WAKE_MIN_SEG_MS,
            WAKE_MAX_SEG_MS,
        ),
        wake_stt: None,
        wake_notice_sent: false,
        preloaded: false,
        last_speech: Instant::now(),
        last_level: Instant::now(),
        last_rms_log: Instant::now(),
        model_notice_sent: false,
        mic_want_open: false,
        mic_retry_at: None,
        mic_retry_delay: Duration::from_secs(5),
        stream_failed: Arc::new(AtomicBool::new(false)),
        mic_error_notice_sent: false,
        noise_gate: NoiseGate::new(),
        wake_duty_window_start: Instant::now(),
        wake_duty_stt: Duration::ZERO,
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
        self.tick_mic_retry();
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
                    WAKE_MIN_SEG_MS,
                    WAKE_MAX_SEG_MS,
                );
            }
            let listen_mode_changed = new.listen_mode != self.cfg.listen_mode;
            let device_changed = new.whisper_model_path != self.cfg.whisper_model_path
                || new.vad_model_path != self.cfg.vad_model_path;
            self.cfg = new;
            if listen_mode_changed {
                // Switching into/out of wake/always mode changes whether a
                // warm mic is wanted. Reopen (or stop retrying) immediately.
                if self.cfg.listen_mode == "always" || self.cfg.listen_mode == "wake_word" {
                    if self.stream.is_none() {
                        self.mic_want_open = true;
                        self.mic_retry_at = Some(Instant::now());
                    }
                } else {
                    self.mic_want_open = false;
                    self.mic_retry_at = None;
                }
            }
            if device_changed && self.stream.is_some() {
                // Model paths changing mid-run: drop the stream so it reopens
                // fresh against the (possibly new) device config.
                self.stream_failed.store(true, Ordering::SeqCst);
            }
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

        // Wake-word / always mode keeps a warm mic: mark it wanted so the
        // retry loop reopens on failure. Hotkey/always-with-force sessions do
        // not keep retrying after the window closes.
        self.mic_want_open = self.cfg.listen_mode == "always" || self.cfg.listen_mode == "wake_word";

        match self.open_capture() {
            Ok(()) => {
                self.mic_retry_at = None;
                self.mic_retry_delay = Duration::from_secs(5);
                self.mic_error_notice_sent = false;
            }
            Err(e) => {
                log::error!("audio: failed to open capture: {e:#}");
                self.mic_error_notice(&e);
                if self.mic_want_open {
                    // Schedule a retry instead of giving up until restart.
                    self.mic_retry_at = Some(Instant::now() + self.mic_retry_delay);
                    self.mic_retry_delay = next_mic_backoff(self.mic_retry_delay);
                    log::info!(
                        "audio: will retry microphone in {:?}",
                        self.mic_retry_delay
                    );
                }
                self.close_session();
            }
        }
        if self.cfg.listen_mode == "hotkey" && force {
            // Hotkey mode: show Listening immediately. Wake mode gets its
            // Listening transition from the Wake event; always mode stays Idle.
            self.emit(AudioEvent::Listening);
        }
        if self.stream.is_some() {
            log::info!(
                "audio: capture open (force={force}, rate={} Hz)",
                self.native_rate
            );
        }
    }

    /// One-shot user notice for a mic-open failure, distinguishing a missing
    /// permission (→ System Settings) from a device error (unplugged/busy).
    fn mic_error_notice(&mut self, err: &anyhow::Error) {
        if self.mic_error_notice_sent {
            return;
        }
        self.mic_error_notice_sent = true;
        let mic_granted = crate::permissions::get_permissions().microphone.granted;
        let text = err.to_string();
        let msg = match classify_mic_error(&text, mic_granted) {
            MicErrorKind::PermissionDenied => format!(
                "Microphone access is off. Open System Settings → Privacy & Security → \
                 Microphone and enable Xenon, then it will connect automatically. ({text})"
            ),
            MicErrorKind::Device => format!(
                "Couldn't open the microphone ({text}). Check that a microphone is \
                 connected and not in use by another app; Xenon will keep retrying."
            ),
        };
        self.emit_notice(&msg);
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
        // Note: mic_want_open is intentionally NOT cleared here — open_session
        // sets it and the retry loop owns it. Only a config change to hotkey
        // mode or worker exit clears it.
    }

    /// Build and play a cpal input stream, wiring a fresh frame channel.
    fn open_capture(&mut self) -> Result<()> {
        log::info!("audio: opening microphone…");
        let t0 = std::time::Instant::now();
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
        if rate == TARGET_RATE {
            log::debug!("audio: capture at native {rate} Hz, no resample (channels={channels})");
        } else {
            log::debug!(
                "audio: resample path {rate} Hz -> {TARGET_RATE} Hz (channels={channels})"
            );
        }

        // The first mic open can block for a long time while macOS shows the
        // permission prompt (or while a device is being configured). Watch for
        // a slow open and surface a "waiting for permission" notice from a
        // background thread, so the pet bubble explains the wait instead of
        // looking frozen. Logged at info so it is visible without debug logs.
        let opened = Arc::new(AtomicBool::new(false));
        let _open_done = OpenDone(opened.clone());
        if self.mic_want_open {
            let app = self.app.clone();
            let flag = opened.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                if !flag.load(Ordering::SeqCst) {
                    log::info!(
                        "audio: microphone open still pending after 2s; emitting permission-wait notice"
                    );
                    let _ = app.emit(
                        "xenon-notice",
                        "Waiting for microphone permission — accept the system prompt or check \
                         System Settings → Privacy & Security → Microphone.",
                    );
                }
            });
        }

        let (frame_tx, frame_rx): (SyncSender<Vec<f32>>, Receiver<Vec<f32>>) =
            mpsc::sync_channel(64);
        self.frame_rx = Some(frame_rx);

        let app = self.app.clone();
        let failed = self.stream_failed.clone();
        let err_cb = move |e: cpal::StreamError| {
            log::warn!("audio: stream error: {e}");
            // Wake/always mode: ask the worker to reopen on the next tick.
            failed.store(true, Ordering::SeqCst);
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
        let elapsed = t0.elapsed();
        log::info!("audio: microphone open in {:.2}s", elapsed.as_secs_f64());
        if elapsed.as_secs_f64() > 2.0 {
            let secs = elapsed.as_secs_f64();
            let msg =
                format!("Microphone took {secs:.1}s to open — slow, check focus/input devices.");
            log::info!("audio: {msg}");
            let _ = self.app.emit("xenon-notice", msg);
        }
        Ok(())
    }

    fn on_frames(&mut self, frames: &[f32]) {
        // Preloaded and no hotkey session: the mic is warm but idle. Drop the
        // frames so nothing is resampled, transcribed, or emitted.
        if self.preloaded {
            self.input_buf.clear();
            return;
        }
        // Track the room's noise floor (skipping frames while a segment is open
        // so the talker never inflates it). Used to gate wake STT below.
        let in_speech = self.seg.in_speech() || self.wake_seg.in_speech();
        self.noise_gate.observe(rms(frames), in_speech);
        self.input_buf.extend_from_slice(frames);
        self.log_input_rms(frames);
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
            let was_wake_speaking = self.wake_seg.in_speech();
            let mut segment: Option<Vec<f32>> = None;
            let mut wake_segment = false;

            if !wake_listening {
                if let Some((start, end)) = self.seg.push(*p) {
                    let dur_ms = (end - start) as f32 * spf as f32 / TARGET_RATE as f32 * 1000.0;
                    log::debug!("vad: speech end dur={dur_ms:.0}ms frames={start}..{end}");
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
                } else if let Some(len) = self.seg.last_too_short() {
                    log::debug!("vad: speech segment dropped (too short: {len} frames)");
                } else if !was_speaking && self.seg.in_speech() {
                    log::debug!("vad: speech start");
                }
            } else if let Some((start, end)) = self.wake_seg.push(*p) {
                // Wake mode, not yet awake: feed a short-segment whisper
                // segmenter instead of the main one.
                let dur_ms = (end - start) as f32 * spf as f32 / TARGET_RATE as f32 * 1000.0;
                log::debug!("vad: wake speech end dur={dur_ms:.0}ms frames={start}..{end}");
                let pre = (PRE_ROLL_MS as usize * TARGET_RATE as usize) / 1000 / spf;
                let s0 = start.saturating_sub(pre);
                let s1 = (end * spf).min(self.captured.len());
                let lo = (s0 * spf).min(self.captured.len());
                if s1 > lo {
                    segment = Some(self.captured[lo..s1].to_vec());
                    wake_segment = true;
                }
                self.wake_seg.reset();
                self.frames_committed = 0;
                self.captured.clear();
                self.since_vad = 0;
            } else if let Some(len) = self.wake_seg.last_too_short() {
                log::debug!("vad: wake speech segment dropped (too short: {len} frames)");
            } else if !was_wake_speaking && self.wake_seg.in_speech() {
                log::debug!("vad: wake speech start");
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
            log::debug!("wake: segment dropped (echo guard: pet is speaking)");
            return;
        }
        // Duty cap: never let wake STT exceed ~25% of wall time. Skip this
        // segment if the rolling window is already over budget (we drop, never
        // queue, so a backlog can't build up).
        if self.wake_duty_over_budget() {
            log::debug!("wake: segment dropped (duty cap ~25% reached)");
            return;
        }
        // Energy gate: only transcribe segments that stand out from the room's
        // adaptive noise floor. This is what keeps idle CPU near zero when the
        // VAD fires on fan/room noise.
        let seg_rms = rms(audio);
        if !self.noise_gate.passes(seg_rms) {
            log::debug!(
                "wake: segment dropped (energy gate rms={seg_rms:.4} floor={:.4})",
                self.noise_gate.floor()
            );
            return;
        }
        // Only the first ~2 s carries the wake phrase; the tail is the command.
        let cap = (WAKE_TRANSCRIBE_MS as usize * TARGET_RATE as usize) / 1000;
        let head = &audio[..audio.len().min(cap)];
        let started = Instant::now();
        let result = self.transcribe_wake(head);
        self.wake_duty_stt += started.elapsed();
        let Some(text) = result else {
            log::debug!("wake: segment dropped (empty/hallucinated wake transcript)");
            return;
        };
        let phrase = self.cfg.effective_wake_phrase();
        let sens = self.cfg.wake_sensitivity;
        let budgets = voice::wake_distance_budgets(sens, phrase.split_whitespace().count());
        match voice::match_wake(&text, &phrase, sens) {
            Some(m) => {
                log::debug!(
                    "wake: transcript={text:?} phrase={phrase:?} matched=true dist={} budgets={budgets:?}",
                    m.distance
                );
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
            None => {
                log::debug!(
                    "wake: transcript={text:?} phrase={phrase:?} matched=false budgets={budgets:?}"
                );
            }
        }
    }

    /// True when wake STT has used more than ~25% of the current 10 s window.
    /// Rolls the window over when it elapses. Cheap: two Instant compares.
    fn wake_duty_over_budget(&mut self) -> bool {
        const WINDOW: Duration = Duration::from_secs(10);
        let now = Instant::now();
        if now.duration_since(self.wake_duty_window_start) >= WINDOW {
            self.wake_duty_window_start = now;
            self.wake_duty_stt = Duration::ZERO;
            return false;
        }
        let elapsed = now.duration_since(self.wake_duty_window_start);
        self.wake_duty_stt * 100 > elapsed * 25
    }

    /// Reuse the tiny wake STT context; return `None` on missing model or
    /// empty/hallucinated transcript.
    fn transcribe_wake(&mut self, audio: &[f32]) -> Option<String> {
        let path = self.wake_model_path();
        if self.wake_stt.is_none() {
            let mut params = WhisperContextParameters::new();
            // whisper GPU only on Apple Silicon (Intel Metal is broken for
            // large models); an explicit stt_use_gpu overrides the auto choice.
            params.use_gpu(self.cfg.stt_use_gpu.unwrap_or(cfg!(target_arch = "aarch64")));
            match WhisperContext::new_with_params(&path, params) {
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
        // Do NOT seed whisper with the wake phrase as the initial prompt:
        // whisper treats the prompt as already-transcribed context and drops
        // the matching prefix from its output (e.g. returns "What is two plus
        // two?" for "Hey Jarvis. What is two plus two?"), so the matcher never
        // sees the wake phrase and the wake never fires. The wake harness
        // transcribes without a prompt (30/30 positives), so match that path.
        match run_stt(ctx, audio, Some("en"), None, 2) {
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
            let mut params = WhisperContextParameters::default();
            params.use_gpu(self.cfg.stt_use_gpu.unwrap_or(cfg!(target_arch = "aarch64")));
            self.stt = Some(WhisperContext::new_with_params(&path, params)?);
        }
        self.stt_last_used = Some(Instant::now());
        self.model_notice_sent = false;
        let ctx = self.stt.as_ref().expect("stt loaded");
        run_stt(ctx, audio, lang, None, 8)
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
        // Bound the silence buffer so the VAD (which re-scans the whole buffer
        // every tick) can't make idle CPU grow without bound. When no segment is
        // open, keep only the last few seconds. A segment in progress is bounded
        // by its own max_frames cap and clears on close, so it needs no trim.
        if self.stream.is_some() && !self.seg.in_speech() && !self.wake_seg.in_speech() {
            let keep = ((TARGET_RATE as u64) * IDLE_KEEP_MS / 1000) as usize;
            if self.captured.len() > keep {
                let drop = self.captured.len() - keep;
                self.captured.drain(..drop);
                // Dropped samples shift every frame index; the segmenters index
                // into `captured`, so rebase them (idle: no open segment).
                self.seg.reset();
                self.wake_seg.reset();
                self.frames_committed = 0;
                self.since_vad = 0;
            }
        }
    }

    /// Reopen the microphone on a backoff while a wake/always session wants it
    /// open. Also reopens immediately when the cpal stream reports an error
    /// (device unplugged / default input changed). Reset the backoff on success.
    fn tick_mic_retry(&mut self) {
        // A stream error (device unplugged, format change) triggers an
        // immediate reopen, not waiting for the timer.
        let stream_broken = self.stream_failed.swap(false, Ordering::SeqCst);

        // Config may have switched us out of wake/always mode.
        if self.cfg.listen_mode != "always" && self.cfg.listen_mode != "wake_word" {
            self.mic_want_open = false;
            self.mic_retry_at = None;
            return;
        }

        // If the mic is already open, nothing to do.
        if self.stream.is_some() && !stream_broken {
            return;
        }

        let due = match self.mic_retry_at {
            Some(at) => Instant::now() >= at,
            None => true,
        };
        if !due && !stream_broken {
            return;
        }

        // Device change / stream error: reopen now. If a stream is somehow
        // still present, drop it first.
        if self.stream.is_some() {
            self.stream = None;
            self.frame_rx = None;
        }

        match self.open_capture() {
            Ok(()) => {
                self.mic_retry_at = None;
                self.mic_retry_delay = Duration::from_secs(5);
                self.mic_error_notice_sent = false;
                log::info!("audio: microphone reopened");
            }
            Err(e) => {
                log::warn!("audio: microphone reopen failed: {e:#}");
                self.mic_error_notice(&e);
                let delay = self.mic_retry_delay;
                self.mic_retry_at = Some(Instant::now() + delay);
                self.mic_retry_delay = next_mic_backoff(self.mic_retry_delay);
                log::info!("audio: will retry microphone in {:?}", self.mic_retry_delay);
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

    /// Log the raw input RMS once every 5 s while capturing. Debug-only so
    /// transcripts stay private; lets a live run show whether the mic is
    /// actually delivering signal (vs. a dead device / muted channel).
    fn log_input_rms(&mut self, frames: &[f32]) {
        if self.last_rms_log.elapsed() < Duration::from_secs(5) || frames.is_empty() {
            return;
        }
        self.last_rms_log = Instant::now();
        let rms = (frames.iter().map(|s| s * s).sum::<f32>() / frames.len() as f32).sqrt();
        log::debug!(
            "audio: input rms={rms:.4} over {} native frames at {} Hz",
            frames.len(),
            self.native_rate
        );
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

/// Cancels the slow-open watchdog: flips the flag to `true` when the open
/// attempt finishes (success or failure), so a watchdog that has not fired yet
/// stays quiet.
struct OpenDone(Arc<AtomicBool>);

impl Drop for OpenDone {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
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
    let mut params = WhisperContextParameters::new();
    params.use_gpu(cfg.stt_use_gpu.unwrap_or(cfg!(target_arch = "aarch64")));
    let ctx = WhisperContext::new_with_params(&path, params)?;
    let prompt = cfg.effective_wake_phrase();
    transcribe_with_prompt(&ctx, samples, Some(&prompt))
}

/// Transcribe with an already-loaded context (harnesses reuse one context for
/// many utterances instead of paying model load per call).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn transcribe_with_context(ctx: &WhisperContext, samples: &[f32]) -> Result<String> {
    transcribe_with_prompt(ctx, samples, None)
}

/// Like [`transcribe_with_context`], but seeds whisper with an initial prompt
/// (used by the wake harness to bias the transcript toward the wake phrase).
pub(crate) fn transcribe_with_prompt(
    ctx: &WhisperContext,
    samples: &[f32],
    prompt: Option<&str>,
) -> Result<String> {
    match run_stt(ctx, samples, None, prompt, 8) {
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
    initial_prompt: Option<&str>,
    max_threads: i32,
) -> Result<Option<Utterance>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
        .min(max_threads.max(1));
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_language(language);
    if let Some(prompt) = initial_prompt {
        params.set_initial_prompt(prompt);
    }
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
    fn mic_backoff_starts_at_5s_doubles_and_caps_at_30s() {
        // 0 (unset) -> 5s
        assert_eq!(next_mic_backoff(Duration::ZERO), Duration::from_secs(5));
        // 5 -> 10 -> 20 -> 30 (capped) -> 30 ...
        let mut d = next_mic_backoff(Duration::ZERO);
        assert_eq!(d, Duration::from_secs(5));
        d = next_mic_backoff(d);
        assert_eq!(d, Duration::from_secs(10));
        d = next_mic_backoff(d);
        assert_eq!(d, Duration::from_secs(20));
        d = next_mic_backoff(d);
        assert_eq!(d, Duration::from_secs(30));
        d = next_mic_backoff(d);
        assert_eq!(d, Duration::from_secs(30), "must cap at 30s");
    }

    #[test]
    fn classify_mic_error_distinguishes_permission_from_device() {
        // Missing grant -> permission denied even with a generic cpal message.
        let cpal_perm = "no supported input config: A backend-specific error has occurred: Unknown property";
        assert_eq!(
            classify_mic_error(cpal_perm, false),
            MicErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_mic_error(cpal_perm, true),
            MicErrorKind::PermissionDenied,
            "the 'unknown property' signature means permission regardless of the bool"
        );
        // Grant present + a real device error -> device.
        assert_eq!(
            classify_mic_error("no default input device", true),
            MicErrorKind::Device
        );
        assert_eq!(
            classify_mic_error("device not found", true),
            MicErrorKind::Device
        );
    }

    #[test]
    fn push_mono_mono_passthrough() {
        let (tx, rx) = mpsc::sync_channel(4);
        push_mono(&[0.1, 0.2, 0.3], 1, &tx);
        assert_eq!(rx.try_recv().unwrap(), vec![0.1, 0.2, 0.3]);
    }

    /// Harness 1: ~20 generated utterances through base.en and large-v3-turbo,
    /// recording first-load and per-utterance latency. Audio is produced by
    /// scripts/stress/gen-utterances.py (raw f32le 16 kHz + expected text).
    #[test]
    #[ignore = "loads real whisper models; run with -- --ignored stress_stt_harness"]
    fn stress_stt_harness() {
        use std::io::Write;

        let dir = std::env::var_os("XENON_STT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("xenon-stt"));
        let out = std::env::var_os("XENON_STRESS_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../scripts/stress/out")
            });
        std::fs::create_dir_all(&out).unwrap();
        let summary = out.join("stt-summary.tsv");
        let _ = std::fs::remove_file(&summary);
        let load_only = std::env::var_os("XENON_STT_LOAD_ONLY").is_some();

        let cfg = XenonConfig::default();
        let home = dirs::home_dir().unwrap_or_default();
        let models = [
            (
                "base.en",
                home.join("Library/Application Support/com.xenon.app/models/ggml-base.en.bin"),
            ),
            ("large-v3-turbo", std::path::PathBuf::from(&cfg.whisper_model_path)),
        ];

        for (name, path) in models {
            if !path.exists() {
                eprintln!("skip: {name} absent at {}", path.display());
                continue;
            }
            let started = Instant::now();
            let mut params = WhisperContextParameters::new();
            // This host's Metal backend aborts on large-v3 (ggml-metal
            // `GGML_ASSERT(buf_dst)` on the old AMD FirePro D700). The
            // harness still measures transcription, so use CPU for that model.
            if name.contains("large") {
                params.use_gpu(false);
            }
            let ctx = WhisperContext::new_with_params(&path, params).expect("load whisper context");
            let first_load_ms = started.elapsed().as_millis();
            if load_only {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&summary)
                    .unwrap();
                writeln!(f, "{name}\t{first_load_ms}").unwrap();
                eprintln!("STT {name}: first_load={first_load_ms}ms (load-only)");
                continue;
            }

            let mut files: Vec<_> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("f32"))
                .collect();
            files.sort();

            let mut rows = Vec::new();
            let mut times = Vec::new();
            for wav in &files {
                let Ok(bytes) = std::fs::read(wav) else { continue };
                let samples: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                let expected = std::fs::read_to_string(wav.with_extension("txt"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let t0 = Instant::now();
                let transcript = transcribe_with_context(&ctx, &samples).unwrap_or_default();
                let ms = t0.elapsed().as_millis();
                times.push(ms as f64);
                rows.push(format!(
                    "{}\t{}\t{}\t{}",
                    wav.file_name().unwrap().to_string_lossy(),
                    expected,
                    transcript,
                    ms
                ));
            }

            if times.is_empty() {
                eprintln!("STT {name}: no utterances found in {}", dir.display());
                continue;
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            let p95 = times
                .get((times.len() * 95 / 100).min(times.len() - 1))
                .copied()
                .unwrap_or(0.0);
            let mut f = std::fs::File::create(out.join(format!("stt-{name}.tsv"))).unwrap();
            for row in &rows {
                writeln!(f, "{row}").unwrap();
            }
            let mut sf = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&summary)
                .unwrap();
            writeln!(
                sf,
                "{name}\t{}\t{first_load_ms}\t{avg:.0}\t{p95:.0}",
                rows.len()
            )
            .unwrap();
            eprintln!(
                "STT {name}: n={} first_load={}ms avg={:.0}ms p95={:.0}ms",
                rows.len(),
                first_load_ms,
                avg,
                p95
            );
        }
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
        let out = run_stt(&ctx, &silence, Some("en"), None, 8).expect("stt run");
        assert!(out.is_none(), "silence should yield no Speech utterance");
    }

    /// Synthesize *text* at an arbitrary sample rate as mono f32 via `say` +
    /// `afconvert` (the 48 kHz path needs a native-rate source).
    fn say_wav_rate(text: &str, rate: u32) -> Option<Vec<f32>> {
        let dir = std::env::temp_dir().join(format!(
            "xenon-live-{rate}-{}-{}",
            std::process::id(),
            text.chars().filter(|c| c.is_alphanumeric()).collect::<String>()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
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
            .args(["-f", "WAVE", "-d"])
            .arg(format!("LEI16@{rate}"))
            .args(["-c", "1"])
            .arg(&aiff)
            .arg(&wav)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let wave = sherpa_onnx::Wave::read(&wav.to_string_lossy())?;
        let samples = wave.samples().to_vec();
        let _ = std::fs::remove_dir_all(&dir);
        Some(samples)
    }

    /// Harness 3: the live 48 kHz capture path. A USB mic opens at 48 kHz, but
    /// the VAD/STT run at 16 kHz, so the worker resamples each native block.
    /// This feeds a synthesized "Hey Jarvis. What is two plus two?" at 48 kHz
    /// through the SAME native-block accumulation -> resample -> Silero VAD ->
    /// short-segment wake path the worker uses (`on_frames` / `run_vad` wake
    /// branch) and asserts the wake fires. The 16 kHz shortcut harness
    /// (`wake_audio_hey_xenon_fires`) would miss a resampling, channel, or
    /// wake-window bug, which is exactly what this guards.
    #[test]
    #[ignore = "synthesizes 48 kHz audio + loads real models; run with -- --ignored stress_wake_live_rate"]
    fn stress_wake_live_rate() {
        let mut cfg = XenonConfig {
            pet_name: "Jarvis".into(),
            wake_phrase: String::new(),
            ..Default::default()
        };
        if let Ok(path) = std::env::var("XENON_WAKE_MODEL") {
            cfg.wake_model_path = path;
        }
        // Resolve the wake model exactly like Worker::wake_model_path().
        let wake_path = {
            let p = cfg.wake_model_path.trim();
            if !p.is_empty() && std::path::Path::new(p).exists() {
                p.to_string()
            } else {
                cfg.whisper_model_path.clone()
            }
        };
        if !std::path::Path::new(&wake_path).exists()
            || !std::path::Path::new(&cfg.vad_model_path).exists()
        {
            eprintln!(
                "skip: models absent (wake={wake_path}, vad={})",
                cfg.vad_model_path
            );
            return;
        }
        let Some(mut samples48) = say_wav_rate("Hey Jarvis. What is two plus two?", 48_000) else {
            eprintln!("skip: say/afconvert unavailable");
            return;
        };
        // The mic keeps capturing after the phrase; append trailing silence so
        // the segmenter sees the 700 ms gap that closes the wake segment.
        samples48.extend(std::iter::repeat(0.0f32).take(48_000 * 2));
        let native_rate = 48_000u32;
        let phrase = cfg.effective_wake_phrase();

        // --- capture -> resample (mirror Worker::on_frames) ---
        let block_need = (native_rate as usize * VAD_STRIDE_MS as usize) / 1000;
        let vad_need = (TARGET_RATE as usize * VAD_STRIDE_MS as usize) / 1000;
        let mut input_buf: Vec<f32> = Vec::new();
        let mut captured: Vec<f32> = Vec::new();
        let mut since_vad = 0usize;

        // --- VAD + wake segmenter (mirror Worker::run_vad wake branch) ---
        let mut pctx = WhisperVadContextParams::default();
        pctx.set_n_threads(2);
        pctx.set_use_gpu(false);
        let mut vad = WhisperVadContext::new(&cfg.vad_model_path, pctx).expect("load VAD model");
        let mut seg = Segmenter::new(0.5, 32.0, 30, cfg.silence_ms as u32, WAKE_MIN_SEG_MS, WAKE_MAX_SEG_MS);

        let mut wparams = WhisperContextParameters::new();
        wparams.use_gpu(false);
        let ctx = WhisperContext::new_with_params(&wake_path, wparams).expect("load wake model");

        let mut frames_committed = 0usize;
        let mut fired = false;

        // cpal delivers ~10 ms chunks at the device rate.
        'outer: for chunk in samples48.chunks(480) {
            input_buf.extend_from_slice(chunk);
            if input_buf.len() < block_need.max(1) {
                continue;
            }
            let block = std::mem::take(&mut input_buf);
            let r16 = resample_linear(&block, native_rate, TARGET_RATE);
            captured.extend_from_slice(&r16);
            since_vad += r16.len();
            if captured.len() < MIN_VAD_SAMPLES || since_vad < vad_need {
                continue;
            }
            since_vad = 0;
            vad.detect_speech(&captured).expect("vad detect");
            let probs = vad.probabilities().to_vec();
            if probs.is_empty() {
                continue;
            }
            let spf = (captured.len() / probs.len()).max(1);
            let mut completed = false;
            for p in &probs[frames_committed.min(probs.len())..] {
                if let Some((start, end)) = seg.push(*p) {
                    // Mirror Worker: keep PRE_ROLL_MS before the VAD onset so
                    // whisper hears the full first word (slicing at the exact
                    // onset garbled "Hey Jarvis" -> "page harvest").
                    let pre = (PRE_ROLL_MS as usize * TARGET_RATE as usize) / 1000 / spf;
                    let s0 = start.saturating_sub(pre);
                    let s1 = (end * spf).min(captured.len());
                    let lo = (s0 * spf).min(captured.len());
                    if s1 > lo {
                        let audio = captured[lo..s1].to_vec();
                        let dur_ms =
                            (end - start) as f32 * spf as f32 / TARGET_RATE as f32 * 1000.0;
                        // Same call the worker makes in transcribe_wake (no
                        // initial prompt; seeding the phrase makes whisper drop
                        // the wake phrase from the transcript).
                        let text =
                            transcribe_with_context(&ctx, &audio).unwrap_or_default();
                        eprintln!(
                            "wake segment frames={start}..{end} dur={dur_ms:.0}ms transcript={text:?}"
                        );
                        if crate::voice::match_wake(&text, &phrase, cfg.wake_sensitivity).is_some()
                        {
                            fired = true;
                        }
                    }
                    seg.reset();
                    frames_committed = 0;
                    captured.clear();
                    since_vad = 0;
                    completed = true;
                    break;
                }
            }
            if completed {
                if fired {
                    break 'outer;
                }
                continue 'outer;
            }
            frames_committed = probs.len();
        }
        assert!(
            fired,
            "48 kHz wake phrase did not fire; capture -> resample -> VAD -> wake path broken"
        );
    }

    /// Deterministic brown-noise generator (2 min of "room") at 48 kHz. Cheap;
    /// no external deps so the CPU harness is reproducible.
    fn brown_noise_48k(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        let mut last = 0.0f32;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            // xorshift white noise -> brown via leaky integration.
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let white = ((s >> 40) as f32 / (1u64 << 24) as f32) - 1.0; // ~[-1,1]
            last = (last + 0.02 * white) / 1.02;
            out.push(last);
        }
        // Normalize to a realistic room level (~0.03 RMS).
        let rms = rms(&out).max(1e-6);
        let g = 0.03 / rms;
        for v in &mut out {
            *v *= g;
        }
        out
    }

    /// Harness 4: 2 minutes of room noise + a few non-wake sentences through
    /// the SAME 48 kHz capture -> resample -> VAD -> wake-gate -> STT path the
    /// worker uses. Reports wake-STT invocations, CPU seconds, and average CPU
    /// as a fraction of one core over the 120 s of audio. Also asserts 30/30 on
    /// a positive wake set. Set XENON_WAKE_NO_GATE=1 to measure the pre-fix
    /// behaviour (no energy/duty gate) for before/after comparison.
    #[test]
    #[ignore = "synthesizes 2 min noise + loads real models; run with -- --ignored stress_wake_cpu"]
    fn stress_wake_cpu() {
        use std::time::Instant;

        let mut cfg = XenonConfig {
            pet_name: "Jarvis".into(),
            wake_phrase: String::new(),
            ..Default::default()
        };
        if let Ok(path) = std::env::var("XENON_WAKE_MODEL") {
            cfg.wake_model_path = path;
        }
        let wake_path = {
            let p = cfg.wake_model_path.trim();
            if !p.is_empty() && std::path::Path::new(p).exists() {
                p.to_string()
            } else {
                cfg.whisper_model_path.clone()
            }
        };
        if !std::path::Path::new(&wake_path).exists()
            || !std::path::Path::new(&cfg.vad_model_path).exists()
        {
            eprintln!("skip: models absent (wake={wake_path})");
            return;
        }
        let gate_on = std::env::var_os("XENON_WAKE_NO_GATE").is_none();
        let native_rate = 48_000u32;
        let phrase = cfg.effective_wake_phrase();
        let total_audio_s = 120.0f64;
        let total48 = (native_rate as f64 * total_audio_s) as usize;

        // --- 2 min of room noise with a few non-wake sentences sprinkled in ---
        let mut audio48 = brown_noise_48k(total48, 0x1234_5678_9abc_def0);
        let non_wake = ["What is the weather?", "Hello there", "Testing one two three"];
        let mut cursor = native_rate as usize * 10; // start after 10 s
        for s in non_wake {
            if let Some(mut clip) = say_wav_rate(s, 48_000) {
                // Scale clip to a natural level over the noise floor.
                for v in &mut clip {
                    *v = *v * 0.5 + 0.02;
                }
                let end = (cursor + clip.len()).min(audio48.len());
                let n = end - cursor;
                audio48[cursor..end].copy_from_slice(&clip[..n]);
                cursor += native_rate as usize * 15; // space them out
            }
        }

        // --- VAD + wake segmenter + gate (mirror Worker) ---
        let mut pctx = WhisperVadContextParams::default();
        pctx.set_n_threads(2);
        pctx.set_use_gpu(false);
        let mut vad = WhisperVadContext::new(&cfg.vad_model_path, pctx).expect("load VAD model");
        let mut seg = Segmenter::new(0.5, 32.0, 30, cfg.silence_ms as u32, WAKE_MIN_SEG_MS, WAKE_MAX_SEG_MS);
        let mut noise_gate = NoiseGate::new();

        let mut wparams = WhisperContextParameters::new();
        wparams.use_gpu(false);
        let ctx = WhisperContext::new_with_params(&wake_path, wparams).expect("load wake model");

        let block_need = (native_rate as usize * VAD_STRIDE_MS as usize) / 1000;
        let vad_need = (TARGET_RATE as usize * VAD_STRIDE_MS as usize) / 1000;
        let mut input_buf: Vec<f32> = Vec::new();
        let mut captured: Vec<f32> = Vec::new();
        let mut since_vad = 0usize;
        let mut frames_committed = 0usize;

        let mut stt_calls = 0u32;
        let mut stt_cpu = Duration::ZERO;
        let mut vad_cpu = Duration::ZERO;
        // Duty-cap window (mirror Worker).
        let mut duty_start = Instant::now();
        let mut duty_stt = Duration::ZERO;

        for chunk in audio48.chunks(480) {
            let in_speech = seg.in_speech();
            noise_gate.observe(rms(chunk), in_speech);
            input_buf.extend_from_slice(chunk);
            if input_buf.len() < block_need.max(1) {
                continue;
            }
            let block = std::mem::take(&mut input_buf);
            let r16 = resample_linear(&block, native_rate, TARGET_RATE);
            captured.extend_from_slice(&r16);
            since_vad += r16.len();
            if captured.len() < MIN_VAD_SAMPLES || since_vad < vad_need {
                continue;
            }
            since_vad = 0;
            let vt = Instant::now();
            vad.detect_speech(&captured).expect("vad detect");
            vad_cpu += vt.elapsed();
            let probs = vad.probabilities().to_vec();
            if probs.is_empty() {
                continue;
            }
            let spf = (captured.len() / probs.len()).max(1);
            let mut completed = false;
            for p in &probs[frames_committed.min(probs.len())..] {
                if let Some((start, end)) = seg.push(*p) {
                    let pre = (PRE_ROLL_MS as usize * TARGET_RATE as usize) / 1000 / spf;
                    let s0 = start.saturating_sub(pre);
                    let s1 = (end * spf).min(captured.len());
                    let lo = (s0 * spf).min(captured.len());
                    if s1 > lo {
                        let audio = captured[lo..s1].to_vec();
                        // Gate decision (mirror on_wake_segment). No-gate =
                        // pre-fix behaviour: every VAD segment gets an STT pass.
                        let mut run = true;
                        if gate_on {
                            let now = Instant::now();
                            if now.duration_since(duty_start) >= Duration::from_secs(10) {
                                duty_start = now;
                                duty_stt = Duration::ZERO;
                            } else {
                                let el = now.duration_since(duty_start);
                                if duty_stt * 100 > el * 25 {
                                    run = false;
                                }
                            }
                            if !noise_gate.passes(rms(&audio)) {
                                run = false;
                            }
                        }
                        if run {
                            let cap = (WAKE_TRANSCRIBE_MS as usize * TARGET_RATE as usize) / 1000;
                            let head = &audio[..audio.len().min(cap)];
                            let t = Instant::now();
                            let text =
                                transcribe_with_context(&ctx, head).unwrap_or_default();
                            let dt = t.elapsed();
                            stt_calls += 1;
                            stt_cpu += dt;
                            duty_stt += dt;
                            let _ = crate::voice::match_wake(&text, &phrase, cfg.wake_sensitivity);
                        }
                    }
                    seg.reset();
                    frames_committed = 0;
                    captured.clear();
                    since_vad = 0;
                    completed = true;
                    break;
                }
            }
            if completed {
                continue;
            }
            frames_committed = probs.len();
            // Mirror Worker tick_timers: cap the idle buffer so the VAD window
            // (and thus per-tick CPU) stays bounded.
            if !seg.in_speech() {
                let keep = ((TARGET_RATE as u64) * IDLE_KEEP_MS / 1000) as usize;
                if captured.len() > keep {
                    let drop = captured.len() - keep;
                    captured.drain(..drop);
                    seg.reset();
                    frames_committed = 0;
                    since_vad = 0;
                }
            }
        }

        let audio_s = total_audio_s;
        let stt_cpu_s = stt_cpu.as_secs_f64();
        let total_cpu_s = stt_cpu_s + vad_cpu.as_secs_f64();
        let avg_cpu_pct = total_cpu_s / audio_s * 100.0;
        eprintln!(
            "wake-cpu: model={} gate={} invocations={} stt_cpu={:.2}s vad_cpu={:.2}s avg_cpu={:.1}%",
            std::path::Path::new(&wake_path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            gate_on,
            stt_calls,
            stt_cpu_s,
            vad_cpu.as_secs_f64(),
            avg_cpu_pct,
        );

        // --- Positive set: 30/30 must still wake through the gated path ---
        let positives = [
            "Hey Jarvis",
            "Hey Jarvis, are you there?",
            "Hey Jarvis, what time is it?",
            "Hey Jarvis, open Safari",
            "Hey Jarvis, stop",
            "Hey Jarvis, hello",
            "Hey Jarvis, can you hear me?",
            "Hey Jarvis, thanks",
            "Hey Jarvis, tell me a joke",
            "Hey Jarvis, play music",
            "Hey Jarvis, turn it down",
            "Hey Jarvis, turn it up",
            "Hey Jarvis, next",
            "Hey Jarvis, done",
            "Hey Jarvis, yes",
            "Hey Jarvis, no",
            "Hey Jarvis, wait",
            "Hey Jarvis, come back",
            "Hey Jarvis, I need help",
            "Hey Jarvis, good morning",
            "Hey Jarvis, good night",
            "Hey Jarvis, read this",
            "Hey Jarvis, look here",
            "Hey Jarvis, remember this",
            "Hey Jarvis, do the thing",
            "Hey Jarvis, thanks a lot",
            "Hey Jarvis, help me",
            "Hey Jarvis, listen up",
            "Hey Jarvis, over here",
            "Hey Jarvis, what is two plus two?",
        ];
        let mut pos_hits = 0u32;
        for text in positives {
            let Some(samples) = say_wav_rate(text, 16_000) else {
                continue;
            };
            // Same gate: only the first ~2 s, then match.
            let cap = (WAKE_TRANSCRIBE_MS as usize * TARGET_RATE as usize) / 1000;
            let head = &samples[..samples.len().min(cap)];
            let transcript = transcribe_with_context(&ctx, head).unwrap_or_default();
            if crate::voice::match_wake(&transcript, &phrase, cfg.wake_sensitivity).is_some() {
                pos_hits += 1;
            }
        }
        eprintln!(
            "wake-cpu: positive {pos_hits}/{} (cap {}ms head)",
            positives.len(),
            WAKE_TRANSCRIBE_MS
        );
        assert_eq!(
            pos_hits,
            positives.len() as u32,
            "positive wake recall dropped below 30/30 with the gate"
        );
    }
}