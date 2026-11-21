//! Pure, unit-tested DSP building blocks for the Audio Pipeline:
//! linear resampling, a streaming VAD segmenter, and the hallucination
//! filter. Nothing here touches cpal, whisper, or the network — these are
//! the "pure functions" the task keeps testable.

/// Linear-interpolation resampler from `from` Hz to `to` Hz.
///
/// Output length is `round(input.len() * to / from)` so a sine stays a sine
/// at the same physical frequency. When downsampling, a short box low-pass is
/// applied first: plain linear interpolation would just decimate, and content
/// above the target Nyquist would fold back into the passband and garble
/// speech for STT (this is what made 48 kHz USB-mic capture transcribe
/// "Hey Jarvis" as "page harvest"). Streaming callers may invoke this per
/// block; a single stateless call is sufficient and keeps this trivial to test.
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }

    let ratio = from as f64 / to as f64;
    // Anti-alias before decimating. Upsampling needs no filter.
    let filtered: Vec<f32>;
    let src: &[f32] = if ratio > 1.0 {
        filtered = box_lowpass(input, ratio.round().max(1.0) as usize);
        &filtered
    } else {
        input
    };

    let out_len = ((input.len() as f64) * to as f64 / from as f64).round() as usize;
    let last = src.len() - 1;

    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let p = n as f64 * ratio;
        let i0 = p.floor() as usize;
        let frac = (p - i0 as f64) as f32;
        let s0 = src[i0.min(last)];
        let s1 = src[(i0 + 1).min(last)];
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// Centered moving-average low-pass with `width` taps (edges clamped). Cheap
/// anti-alias for small integer downsample ratios like 48k -> 16k.
fn box_lowpass(input: &[f32], width: usize) -> Vec<f32> {
    let w = width.max(1);
    let n = input.len();
    let mut out = vec![0.0f32; n];
    let half = (w - 1) / 2;
    for (i, o) in out.iter_mut().enumerate() {
        let lo = i.saturating_sub(half);
        let hi = (i + w - half).min(n);
        *o = input[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
    }
    out
}

/// RMS of a mono f32 slice. `0.0` for an empty slice.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Wake segments worth a whisper pass are 0.4-3.0 s long.
pub const WAKE_MIN_SEG_MS: u32 = 400;
pub const WAKE_MAX_SEG_MS: u32 = 3_000;
/// Only the first ~2.0 s of a wake segment is transcribed: the wake phrase
/// leads, the tail is the command (handled after the wake fires).
pub const WAKE_TRANSCRIBE_MS: u32 = 2_000;

/// A wake segment must be this many times the adaptive noise floor to be worth
/// an STT pass. Filters room/fan noise that the VAD alone lets through.
pub const WAKE_ENERGY_FACTOR: f32 = 2.5;
/// ...and above this absolute RMS, so near-silent digital noise never fires.
pub const WAKE_ENERGY_ABS_MIN: f32 = 0.005;

/// Adaptive noise floor + energy gate for wake-word segments. Cheap and
/// deterministic (same observe/pass sequence -> same result) so it unit-tests
/// without audio. The floor tracks the room; `passes` keeps only segments that
/// stand out from it.
#[derive(Debug, Clone)]
pub struct NoiseGate {
    floor: f32,
    init: bool,
}

impl Default for NoiseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseGate {
    pub fn new() -> Self {
        Self {
            floor: 0.0,
            init: false,
        }
    }

    pub fn floor(&self) -> f32 {
        self.floor
    }

    /// Learn the room level from one frame of input RMS. `in_speech` frames are
    /// skipped so the talker never inflates the floor.
    pub fn observe(&mut self, frame_rms: f32, in_speech: bool) {
        if !self.init {
            if frame_rms > 0.0 {
                self.floor = frame_rms;
                self.init = true;
            }
            return;
        }
        if in_speech || frame_rms <= 0.0 {
            return;
        }
        // Rise quickly toward a louder room, fall slowly so a brief quiet dip
        // doesn't crater the floor (which would make noise pass the gate).
        let a = if frame_rms > self.floor { 0.05 } else { 0.01 };
        self.floor += a * (frame_rms - self.floor);
    }

    /// Whether a segment of RMS `seg_rms` clears the energy gate.
    pub fn passes(&self, seg_rms: f32) -> bool {
        self.init && seg_rms >= self.floor * WAKE_ENERGY_FACTOR && seg_rms >= WAKE_ENERGY_ABS_MIN
    }
}

/// Streaming speech segmenter over a per-frame probability stream.
///
/// Feed it probabilities in order with `push`; it returns `Some((start, end))`
/// (frame indices) when a segment completes. Rules, all expressed in frames:
/// - a segment starts after `onset_frames` consecutive frames at/above threshold;
/// - it ends after `silence_frames` consecutive frames below threshold;
/// - segments shorter than `min_frames` are dropped (returns `None`);
/// - segments longer than `max_frames` are force-closed at the cap.
///
/// `frame_duration_ms` only converts the ms-based config into frames; the
/// segmenter itself is pure and frame-indexed so it is trivial to test.
pub struct Segmenter {
    threshold: f32,
    onset_frames: usize,
    silence_frames: usize,
    min_frames: usize,
    max_frames: usize,
    // streaming state
    active: bool,
    speech_run: usize,
    silence_run: usize,
    frames_seen: usize,
    seg_start: usize,
    seg_len: usize,
    /// Length (frames) of the segment closed by the most recent `push`, when
    /// it was dropped for being shorter than `min_frames`. `None` otherwise.
    /// Read by the audio worker for debug logging of dropped segments.
    last_too_short: Option<usize>,
}

impl Segmenter {
    pub fn new(
        threshold: f32,
        frame_duration_ms: f64,
        onset_ms: u32,
        silence_ms: u32,
        min_speech_ms: u32,
        max_speech_ms: u32,
    ) -> Self {
        let per_frame = |ms: u32| ((ms as f64) / frame_duration_ms).round().max(1.0) as usize;
        Self {
            threshold,
            onset_frames: per_frame(onset_ms),
            silence_frames: per_frame(silence_ms),
            min_frames: per_frame(min_speech_ms),
            max_frames: per_frame(max_speech_ms).max(per_frame(min_speech_ms)),
            active: false,
            speech_run: 0,
            silence_run: 0,
            frames_seen: 0,
            seg_start: 0,
            seg_len: 0,
            last_too_short: None,
        }
    }

    /// Whether a speech segment is currently open (mic is in an utterance).
    pub fn in_speech(&self) -> bool {
        self.active
    }

    /// Length (frames) of the segment dropped as too short by the most recent
    /// [`Self::push`], or `None` when that push did not close a short segment.
    pub fn last_too_short(&self) -> Option<usize> {
        self.last_too_short
    }

    /// Reset all state (e.g. when the listen session is torn down or the
    /// silence buffer is trimmed).
    pub fn reset(&mut self) {
        *self = Self {
            threshold: self.threshold,
            onset_frames: self.onset_frames,
            silence_frames: self.silence_frames,
            min_frames: self.min_frames,
            max_frames: self.max_frames,
            active: false,
            speech_run: 0,
            silence_run: 0,
            frames_seen: 0,
            seg_start: 0,
            seg_len: 0,
            last_too_short: None,
        };
    }

    /// Advance the segmenter by one frame worth of probability.
    /// Returns a completed `(start_frame, end_frame_exclusive)` when ready.
    pub fn push(&mut self, prob: f32) -> Option<(usize, usize)> {
        self.last_too_short = None;
        let idx = self.frames_seen;
        self.frames_seen += 1;
        let is_speech = prob >= self.threshold;

        if is_speech {
            self.speech_run += 1;
            self.silence_run = 0;
        } else {
            self.silence_run += 1;
            self.speech_run = 0;
        }

        if !self.active {
            if self.speech_run >= self.onset_frames {
                // Onset fires on the frame that completes the run.
                self.active = true;
                self.seg_start = idx + 1 - self.onset_frames;
                self.seg_len = self.onset_frames;
            }
            return None;
        }

        self.seg_len += 1;
        if self.seg_len >= self.max_frames {
            return self.close();
        }
        if self.silence_run >= self.silence_frames {
            return self.close();
        }
        None
    }

    /// Close the open segment, applying the min-length drop rule.
    fn close(&mut self) -> Option<(usize, usize)> {
        let start = self.seg_start;
        let len = self.seg_len;
        self.active = false;
        self.speech_run = 0;
        self.silence_run = 0;
        if len < self.min_frames {
            self.last_too_short = Some(len);
            None
        } else {
            Some((start, start + len))
        }
    }

    /// Force-finish any open segment (used when the listen window is closed
    /// by a hotkey timeout). Applies the same min-length rule.
    pub fn flush(&mut self) -> Option<(usize, usize)> {
        if self.active {
            self.close()
        } else {
            None
        }
    }
}

/// Known whisper.cpp hallucinations / soundtrack tags that mean "no real
/// speech". Matched case-insensitively on the trimmed transcript.
pub fn is_hallucination(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    if t.is_empty() {
        return true;
    }
    const EXACT: &[&str] = &[
        "[blank_audio]",
        "[music]",
        "(music)",
        "(upbeat music)",
        "(applause)",
        "(laughter)",
        "(silence)",
        "[silence]",
        "(blank_audio)",
        "thank you.",
        "thanks for watching!",
        "thanks for watching.",
        "please subscribe.",
        "subscribe to my channel.",
        "bye.",
        "you",
        ".",
    ];
    if EXACT.contains(&t.as_str()) {
        return true;
    }
    // Bracket/parenthesis tag spanning the whole transcript, e.g. "[Music]".
    (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resampler ----------------------------------------------------

    #[test]
    fn resample_identity_keeps_samples() {
        let x = vec![0.0, 0.5, 1.0, 0.5, 0.0];
        assert_eq!(resample_linear(&x, 16_000, 16_000), x);
    }

    #[test]
    fn resample_length_scales_by_ratio() {
        let x = vec![0.0f32; 48_000]; // 3 s at 48k
        let y = resample_linear(&x, 48_000, 16_000);
        assert_eq!(y.len(), 16_000);
    }

    #[test]
    fn resample_upsample_doubles_length() {
        let x = vec![0.0f32; 100];
        let y = resample_linear(&x, 8_000, 16_000);
        assert_eq!(y.len(), 200);
    }

    #[test]
    fn resample_preserves_sine_frequency() {
        // 100 Hz sine at 48k, 1 second. Downsample to 16k must still be 100 Hz.
        let n = 48_000;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * i as f32 / 48_000.0).sin())
            .collect();
        let y = resample_linear(&x, 48_000, 16_000);
        assert_eq!(y.len(), 16_000);
        // Count zero crossings (rising) to recover frequency.
        let mut crossings = 0;
        for w in 1..2_000 {
            if y[w - 1] < 0.0 && y[w] >= 0.0 {
                crossings += 1;
            }
        }
        // In the first 2000 samples (0.125s) expect ~12-13 crossings for 100 Hz.
        assert!((12..=14).contains(&crossings), "crossings={crossings}");
    }

    #[test]
    fn resample_empty_input() {
        assert!(resample_linear(&[], 48_000, 16_000).is_empty());
    }

    // ---- noise gate ---------------------------------------------------

    #[test]
    fn noise_gate_learns_floor_and_rejects_noise() {
        let mut g = NoiseGate::new();
        // Establish a room floor around 0.02 from quiet frames.
        for _ in 0..50 {
            g.observe(0.02, false);
        }
        assert!((g.floor() - 0.02).abs() < 0.01, "floor={}", g.floor());
        // Room-noise-level segment does not pass.
        assert!(!g.passes(0.025));
        // A loud speech-like segment does.
        assert!(g.passes(0.2));
    }

    #[test]
    fn noise_gate_ignores_speech_frames_when_learning() {
        let mut g = NoiseGate::new();
        for _ in 0..50 {
            g.observe(0.02, false);
        }
        let floor = g.floor();
        // Loud frames flagged as speech must not raise the floor.
        for _ in 0..50 {
            g.observe(0.5, true);
        }
        assert!((g.floor() - floor).abs() < 1e-3, "floor moved to {}", g.floor());
    }

    #[test]
    fn noise_gate_abs_min_blocks_near_silence() {
        let mut g = NoiseGate::new();
        g.observe(0.0001, false);
        // Even if the floor is tiny, near-silence never clears the abs min.
        assert!(!g.passes(0.001));
    }

    // ---- segmenter ----------------------------------------------------

    // 100 ms per frame keeps the arithmetic legible in the tests below.
    fn seg() -> Segmenter {
        // onset 100ms=1fr, silence 300ms=3fr, min 300ms=3fr, max 30_000ms=300fr
        Segmenter::new(0.5, 100.0, 100, 300, 300, 30_000)
    }

    #[test]
    fn segmenter_silence_never_emits() {
        let mut s = seg();
        for _ in 0..50 {
            assert!(s.push(0.0).is_none());
        }
        assert!(!s.in_speech());
    }

    #[test]
    fn segmenter_onset_then_trailing_silence() {
        let mut s = seg();
        // 10 frames of speech
        for _ in 0..10 {
            assert!(s.push(0.9).is_none());
        }
        assert!(s.in_speech());
        // 3 frames of trailing silence -> close
        assert!(s.push(0.0).is_none());
        assert!(s.push(0.0).is_none());
        let done = s.push(0.0).expect("segment completes after trailing silence");
        // onset at frame 0 (1-frame onset run), length = 10 speech + 3 silence = 13
        assert_eq!(done, (0, 13));
        assert!(!s.in_speech());
    }

    #[test]
    fn segmenter_drops_segments_shorter_than_min() {
        // One onset frame then long silence => length 1 speech + 3 silence = 4 <... 
        // min is 3 frames; length 4 >= 3 so it WOULD keep. Make it truly short:
        let mut s = Segmenter::new(0.5, 100.0, 100, 300, 500, 30_000); // min 500ms=5fr
        s.push(0.9); // onset, len=1
        assert!(s.push(0.0).is_none());
        assert!(s.push(0.0).is_none());
        assert!(s.push(0.0).is_none()); // len=1+3=4 < 5 -> dropped
        assert!(s.push(0.0).is_none());
    }

    #[test]
    fn segmenter_caps_at_max_length() {
        let mut s = Segmenter::new(0.5, 100.0, 100, 300, 300, 1000); // max 1000ms=10fr
        let mut emitted = None;
        for _ in 0..30 {
            if let Some(d) = s.push(0.9) {
                emitted = Some(d);
                break;
            }
        }
        let (start, end) = emitted.expect("forced close at cap");
        assert_eq!(start, 0);
        assert_eq!(end, 10);
        assert!(!s.in_speech());
    }

    #[test]
    fn segmenter_flush_finishes_open_speech() {
        let mut s = seg();
        for _ in 0..6 {
            s.push(0.9);
        }
        assert!(s.in_speech());
        let (start, end) = s.flush().expect("flush closes open segment");
        assert_eq!(start, 0);
        assert_eq!(end, 6);
    }

    #[test]
    fn segmenter_flush_returns_none_when_idle() {
        let mut s = seg();
        assert!(s.flush().is_none());
    }

    #[test]
    fn segmenter_reset_clears_state() {
        let mut s = seg();
        for _ in 0..6 {
            s.push(0.9);
        }
        s.reset();
        assert!(!s.in_speech());
        assert!(s.flush().is_none());
    }

    // ---- hallucination filter -----------------------------------------

    #[test]
    fn hallucination_filters_known_phrases() {
        for h in [
            "[BLANK_AUDIO]",
            "(Music)",
            "Thank you.",
            "Thanks for watching!",
            "  please subscribe.  ",
            "you",
            "",
        ] {
            assert!(is_hallucination(h), "should filter {h:?}");
        }
    }

    #[test]
    fn hallucination_keeps_real_speech() {
        for r in [
            "what's the weather today",
            "open spotify",
            "hello xenon",
            "the value of pi is 3.14",
        ] {
            assert!(!is_hallucination(r), "should keep {r:?}");
        }
    }
}