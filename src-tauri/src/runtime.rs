use anyhow::Result;
use async_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::Agent;
use crate::audio::AudioPipeline;
use crate::events::{AgentAction, AudioEvent, PetState};
use crate::grounding;
use crate::provider::{OpenAICompatProvider, Provider};
use crate::tools;
use crate::tts::TtsEngine;

pub struct Runtime {
    audio_tx: Sender<AudioEvent>,
    audio_rx: Receiver<AudioEvent>,
    agent_tx: Sender<AgentAction>,
    agent_rx: Receiver<AgentAction>,
    state: Arc<RwLock<PetState>>,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn tools::Tool>>,
    tts: Arc<TtsEngine>,
}

impl Runtime {
    pub fn new() -> Result<Self> {
        let (audio_tx, audio_rx) = bounded(32);
        let (agent_tx, agent_rx) = bounded(32);

        let provider: Arc<dyn Provider> = Arc::new(OpenAICompatProvider::from_env()?);

        let default_tools = tools::create_default_tools();

        let tts = Arc::new(TtsEngine::new());

        Ok(Self {
            audio_tx,
            audio_rx,
            agent_tx,
            agent_rx,
            state: Arc::new(RwLock::new(PetState::Idle)),
            provider,
            tools: default_tools,
            tts,
        })
    }

    pub async fn start(&self) -> Result<()> {
        let audio_pipeline = AudioPipeline::new(self.audio_tx.clone());
        audio_pipeline.start().await?;

        let agent_rx = self.agent_rx.clone();
        let agent_tx = self.agent_tx.clone();
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let tts = self.tts.clone();

        tokio::spawn(async move {
            Self::agent_loop(agent_rx, agent_tx, provider, tools, tts).await;
        });

        let audio_rx = self.audio_rx.clone();
        let agent_tx = self.agent_tx.clone();
        let provider = self.provider.clone();
        let tools = self.tools.clone();

        tokio::spawn(async move {
            Self::audio_loop(audio_rx, agent_tx, provider, tools).await;
        });

        Ok(())
    }

    async fn agent_loop(
        agent_rx: Receiver<AgentAction>,
        agent_tx: Sender<AgentAction>,
        _provider: Arc<dyn Provider>,
        _tools: Vec<Box<dyn tools::Tool>>,
        tts: Arc<TtsEngine>,
    ) {
        while let Ok(action) = agent_rx.recv().await {
            match action {
                AgentAction::Speak(text) => {
                    let _ = tts.speak(&text);
                    let _ = agent_tx.send(AgentAction::SetPetState(PetState::Speaking)).await;
                }
                AgentAction::SetPetState(state) => {
                    let _ = agent_tx.send(AgentAction::SetPetState(state)).await;
                }
                AgentAction::ExecuteTool(_) => {
                    let _ = agent_tx.send(AgentAction::SetPetState(PetState::Agentic)).await;
                }
                AgentAction::ToolResult(_) => {
                    let _ = agent_tx.send(AgentAction::SetPetState(PetState::Thinking)).await;
                }
            }
        }
    }

    async fn audio_loop(
        audio_rx: Receiver<AudioEvent>,
        agent_tx: Sender<AgentAction>,
        provider: Arc<dyn Provider>,
        tools: Vec<Box<dyn tools::Tool>>,
    ) {
        while let Ok(event) = audio_rx.recv().await {
            match event {
                AudioEvent::Speech(utterance) => {
                    let _ = agent_tx.send(AgentAction::SetPetState(PetState::Thinking)).await;

                    let mut agent = Agent::new(
                        Box::new(OpenAICompatProvider {
                            client: reqwest::Client::new(),
                            base_url: std::env::var("XENON_PROVIDER_URL")
                                .unwrap_or_else(|_| "http://localhost:11434".to_string()),
                            api_key: std::env::var("XENON_PROVIDER_API_KEY").ok(),
                        }),
                        tools.clone(),
                    );

                    let actions = agent.run_utterance(&utterance.text).await;

                    if let Ok(actions) = actions {
                        for action in actions {
                            let _ = agent_tx.send(action).await;
                        }
                    }
                }
                AudioEvent::Silence => {
                    let _ = agent_tx.send(AgentAction::SetPetState(PetState::Idle)).await;
                }
            }
        }
    }

    pub fn get_state(&self) -> Arc<RwLock<PetState>> {
        self.state.clone()
    }

    pub async fn set_state(&self, state: PetState) {
        let mut s = self.state.write().await;
        *s = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_runtime_creation() {
        let runtime = Runtime::new();
        assert!(runtime.is_ok());
    }

    #[tokio::test]
    async fn test_runtime_start() {
        let runtime = Runtime::new().unwrap();
        let result = runtime.start().await;
        assert!(result.is_ok());
    }
}
