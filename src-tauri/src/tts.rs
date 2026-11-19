use anyhow::Result;

pub struct TtsEngine;

impl TtsEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn synthesize(&self, text: &str) -> Result<Vec<f32>> {
        log::info!("TTS: {}", text);
        // Placeholder: needs sherpa-onnx TTS integration
        // For now, just log and return silence
        Ok(vec![0.0; 1000])
    }

    pub fn speak(&self, text: &str) -> Result<()> {
        log::info!("Speaking: {}", text);
        let _ = self.synthesize(text)?;
        // Placeholder: needs cpal output stream + audio playback
        Ok(())
    }

    /// Barge-in hook: asked to stop playing (e.g. the user pressed the
    /// force-listen hotkey mid-sentence). Real playback stop is wired in
    /// task 3; today this only logs so the seam is explicit.
    pub fn request_stop(&self) {
        // ponytail: no-op barge-in hook; task 3 stops the cpal output stream.
        log::debug!("TTS: barge-in requested (stop playback)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tts_synthesize() {
        let engine = TtsEngine::new();
        let samples = engine.synthesize("Hello world").unwrap();
        assert!(!samples.is_empty());
    }

    #[test]
    fn test_tts_speak() {
        let engine = TtsEngine::new();
        let result = engine.speak("Test");
        assert!(result.is_ok());
    }
}
