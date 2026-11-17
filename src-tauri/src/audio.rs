use anyhow::Result;
use async_channel::Sender;

use crate::events::AudioEvent;

pub struct AudioPipeline {
    audio_tx: Sender<AudioEvent>,
}

impl AudioPipeline {
    pub fn new(audio_tx: Sender<AudioEvent>) -> Self {
        Self { audio_tx }
    }

    pub async fn start(&self) -> Result<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                let _ = self.audio_tx.send(AudioEvent::Silence).await;
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel;

    #[tokio::test]
    async fn test_audio_pipeline_silence() {
        let (tx, rx) = async_channel::bounded(10);
        let pipeline = AudioPipeline::new(tx);

        pipeline.start().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        let event = rx.recv().await;
        assert!(event.is_ok());
    }
}
