use async_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;

use crate::agent::Agent;
use crate::config::XenonConfig;
use crate::events::{AgentAction, AudioEvent, PetState};
use crate::provider::{OpenAICompatProvider, Provider};
use crate::tools;
use crate::tts::TtsEngine;

pub struct Runtime {
    audio_tx: Sender<AudioEvent>,
    audio_rx: Receiver<AudioEvent>,
    action_tx: Sender<AgentAction>,
    action_rx: Receiver<AgentAction>,
    state: Arc<RwLock<PetState>>,
    config: Arc<RwLock<XenonConfig>>,
    /// `None` in tests (no Tauri app), `Some` in production.
    app: Option<AppHandle>,
    tts: Arc<TtsEngine>,
}

impl Runtime {
    pub fn new(config: Arc<RwLock<XenonConfig>>) -> Self {
        let (audio_tx, audio_rx) = bounded(32);
        let (action_tx, action_rx) = bounded(32);

        Self {
            audio_tx,
            audio_rx,
            action_tx,
            action_rx,
            state: Arc::new(RwLock::new(PetState::Idle)),
            config,
            app: None,
            tts: Arc::new(TtsEngine::new()),
        }
    }

    /// Attach the Tauri AppHandle so the presentation loop can emit
    /// `pet-state` / `pet-speech` events to the frontend.
    pub fn attach_app(&mut self, app: AppHandle) {
        self.app = Some(app);
    }

    /// Spawn the presentation and audio loops.  Call once after
    /// `attach_app`.
    pub fn start(&self) {
        let action_rx = self.action_rx.clone();
        let state = self.state.clone();
        let app = self.app.clone();
        let tts = self.tts.clone();
        tauri::async_runtime::spawn(async move {
            Self::presentation_loop(action_rx, state, app, tts).await;
        });

        let audio_rx = self.audio_rx.clone();
        let action_tx = self.action_tx.clone();
        let config = self.config.clone();
        tauri::async_runtime::spawn(async move {
            Self::audio_loop(audio_rx, action_tx, config).await;
        });
    }

    /// Consume `AgentAction`s, update shared pet state, emit `pet-state` /
    /// `pet-speech` to the frontend, and drive the TTS seam.
    async fn presentation_loop(
        action_rx: Receiver<AgentAction>,
        state: Arc<RwLock<PetState>>,
        app: Option<AppHandle>,
        tts: Arc<TtsEngine>,
    ) {
        while let Ok(action) = action_rx.recv().await {
            let next = match &action {
                AgentAction::Speak(_) => Some(PetState::Speaking),
                AgentAction::SetPetState(s) => Some(s.clone()),
                AgentAction::ExecuteTool(_) => Some(PetState::Agentic),
                AgentAction::ToolResult(_) => Some(PetState::Thinking),
                AgentAction::TurnFinished => Some(PetState::Idle),
            };

            if let Some(new_state) = next {
                let mut guard = state.write().await;
                if *guard != new_state {
                    // Barge-in: leaving Speaking toward Listening means the user
                    // wants to cut the pet off — ask the TTS engine to stop.
                    if *guard == PetState::Speaking && new_state == PetState::Listening {
                        drop(guard);
                        // TASK-3 HOOK: stop playback here.
                        tts.request_stop();
                        let mut g = state.write().await;
                        *g = new_state.clone();
                        drop(g);
                    } else {
                        *guard = new_state.clone();
                        drop(guard);
                    }
                    if let Some(app) = &app {
                        let _ = app.emit("pet-state", &new_state);
                    }
                } else {
                    drop(guard);
                }
            }

            if let AgentAction::Speak(text) = &action {
                // TTS seam: real synthesis is a future task.
                let _ = tts.speak(text);
                if let Some(app) = &app {
                    let _ = app.emit("pet-speech", text.as_str());
                }
            }
        }
    }

    /// Consume `AudioEvent`s, run the agent for each utterance / greeting,
    /// and forward resulting `AgentAction`s to the presentation loop.
    async fn audio_loop(
        audio_rx: Receiver<AudioEvent>,
        action_tx: Sender<AgentAction>,
        config: Arc<RwLock<XenonConfig>>,
    ) {
        while let Ok(event) = audio_rx.recv().await {
            match event {
                AudioEvent::Speech(utterance) => {
                    Self::run_agent(&utterance.text, &action_tx, &config).await;
                }
                AudioEvent::Greet => {
                    let cfg = config.read().await;
                    let greeting = format!(
                        "The user just tapped you. Greet them warmly in one short sentence and mention you are {} ready to help.",
                        cfg.pet_name
                    );
                    drop(cfg);
                    Self::run_agent(&greeting, &action_tx, &config).await;
                }
                // Hotkey barge-in / speech onset: swap the pet to Listening.
                // TTS-stop on barge-in is wired here in task 3.
                AudioEvent::ForceListen | AudioEvent::Listening => {
                    let _ = action_tx
                        .send(AgentAction::SetPetState(PetState::Listening))
                        .await;
                }
                // A listen window closed with nothing to transcribe: back to Idle.
                AudioEvent::Silence => {
                    let _ = action_tx
                        .send(AgentAction::SetPetState(PetState::Idle))
                        .await;
                }
            }
        }
    }

    async fn run_agent(text: &str, action_tx: &Sender<AgentAction>, config: &Arc<RwLock<XenonConfig>>) {
        let cfg = config.read().await.clone();
        let provider: Arc<dyn Provider> = Arc::new(OpenAICompatProvider::new(
            cfg.provider.provider_url.clone(),
            cfg.provider.provider_api_key.clone(),
        ));
        let agent_tools = tools::create_default_tools();
        let agent = Agent::new(provider, agent_tools, cfg);

        if let Err(e) = agent.run_utterance(text, action_tx.clone()).await {
            log::error!("Agent error: {}", e);
            let _ = action_tx
                .send(AgentAction::Speak(
                    "I encountered an error processing that request.".to_string(),
                ))
                .await;
            let _ = action_tx.send(AgentAction::TurnFinished).await;
        }
    }

    /// Clone of the audio channel, so Tauri commands can inject utterances
    /// (pet taps, text input, wake word) without owning the Runtime.
    pub fn audio_sender(&self) -> Sender<AudioEvent> {
        self.audio_tx.clone()
    }

    fn state(&self) -> Arc<RwLock<PetState>> {
        self.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AudioEvent, Utterance};

    fn test_runtime() -> Runtime {
        Runtime::new(Arc::new(RwLock::new(XenonConfig::default())))
    }

    #[tokio::test]
    async fn test_runtime_creation() {
        let runtime = test_runtime();
        // state starts at Idle
        let s = runtime.state().read().await.clone();
        assert!(matches!(s, PetState::Idle));
    }

    #[tokio::test]
    async fn test_runtime_start() {
        let runtime = test_runtime();
        runtime.start();
        // No panic = success.
    }

    /// A Greet event must reach the agent loop and come back as pet state
    /// changes, proving the audio channel is wired to the agent (ticket 10).
    ///
    /// Uses a black-hole provider URL so the Thinking state stays observable
    /// while the HTTP connection times out.
    #[tokio::test]
    async fn test_greet_event_reaches_agent_loop() {
        let mut cfg = XenonConfig::default();
        cfg.provider.provider_url = "http://192.0.2.1:9".to_string(); // TEST-NET-1, non-routable
        let config = Arc::new(RwLock::new(cfg));
        let runtime = Runtime::new(config);
        runtime.start();

        let state = runtime.state();
        runtime
            .audio_sender()
            .send(AudioEvent::Greet)
            .await
            .unwrap();

        let mut saw_thinking = false;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let s = state.read().await.clone();
            if matches!(s, PetState::Thinking) {
                saw_thinking = true;
                break;
            }
        }
        assert!(saw_thinking, "Greet should push the pet into Thinking");
    }

    /// A Speech event must also drive the state machine through Thinking.
    #[tokio::test]
    async fn test_speech_event_reaches_agent_loop() {
        let mut cfg = XenonConfig::default();
        cfg.provider.provider_url = "http://192.0.2.1:9".to_string();
        let config = Arc::new(RwLock::new(cfg));
        let runtime = Runtime::new(config);
        runtime.start();

        let state = runtime.state();
        runtime
            .audio_sender()
            .send(AudioEvent::Speech(Utterance {
                text: "Hello".to_string(),
                confidence: 1.0,
            }))
            .await
            .unwrap();

        let mut saw_thinking = false;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let s = state.read().await.clone();
            if matches!(s, PetState::Thinking) {
                saw_thinking = true;
                break;
            }
        }
        assert!(saw_thinking, "Speech should push the pet into Thinking");
    }
}