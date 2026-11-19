use anyhow::Result;
use async_channel::Sender;
use std::sync::Arc;

use crate::events::AudioEvent;

/// Placeholder Audio Pipeline. Real capture (cpal) + VAD (sherpa-onnx) +
/// STT (whisper-rs) is a later task; for now this is a marked no-op seam
/// that never emits events.
pub struct AudioPipeline {
    #[allow(dead_code)]
    audio_tx: Arc<Sender<AudioEvent>>,
}

impl AudioPipeline {
    pub fn new(audio_tx: Sender<AudioEvent>) -> Self {
        Self {
            audio_tx: Arc::new(audio_tx),
        }
    }

    pub async fn start(&self) -> Result<()> {
        // No-op: real microphone capture + VAD + STT is a future task.
        // ponytail: no-op seam; replace with cpal + sherpa-onnx + whisper-rs.
        Ok(())
    }
}