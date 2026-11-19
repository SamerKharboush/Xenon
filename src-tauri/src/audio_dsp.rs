//! Pure, unit-tested DSP building blocks for the Audio Pipeline:
//! linear resampling, a streaming VAD segmenter, and the hallucination
//! filter. Nothing here touches cpal, whisper, or the network — these are
//! the "pure functions" the task keeps testable.

/// Linear-interpolation resampler from `from` Hz to `to` Hz.
///
/// Output length is `round(input.len() * to / from)` so a sine stays a sine
/// at the same physical frequency. Streaming callers may invoke this per
/// block; sub-sample drift at block seams is corrected upstream by recomputing
/// the samples-per-frame ratio from the live buffer, so a single stateless
/// call is sufficient and keeps this function trivial to test.
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }

    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) * to as f64 / from as f64).round() as usize;
    let last = input.len() - 1;

    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let p = n as f64 * ratio;
        let i0 = p.floor() as usize;
        let frac = (p - i0 as f64) as f32;
        let s0 = input[i0.min(last)];
        let s1 = input[(i0 + 1).min(last)];
        out.push(s0 + (s1 - s0) * frac);
    }
    out
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
        }
    }

    /// Whether a speech segment is currently open (mic is in an utterance).
    pub fn in_speech(&self) -> bool {
        self.active
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
        };
    }

    /// Advance the segmenter by one frame worth of probability.
    /// Returns a completed `(start_frame, end_frame_exclusive)` when ready.
    pub fn push(&mut self, prob: f32) -> Option<(usize, usize)> {
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