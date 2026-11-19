use async_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;

use crate::agent::Agent;
use crate::config::XenonConfig;
use crate::events::{AgentAction, AudioEvent, PetState};
use crate::provider::{OpenAICompatProvider, Provider};
use crate::tools;
use crate::tts::Tts;

pub struct Runtime {
    audio_tx: Sender<AudioEvent>,
    audio_rx: Receiver<AudioEvent>,
    action_tx: Sender<AgentAction>,
    action_rx: Receiver<AgentAction>,
    state: Arc<RwLock<PetState>>,
    config: Arc<RwLock<XenonConfig>>,
    /// `None` in tests (no Tauri app), `Some` in production.
    app: Option<AppHandle>,
    tts: Arc<Tts>,
}

impl Runtime {
    pub fn new(config: Arc<RwLock<XenonConfig>>) -> Self {
        let (audio_tx, audio_rx) = bounded(32);
        let (action_tx, action_rx) = bounded(32);
        let tts = Arc::new(Tts::new(config.clone()));

        Self {
            audio_tx,
            audio_rx,
            action_tx,
            action_rx,
            state: Arc::new(RwLock::new(PetState::Idle)),
            config,
            app: None,
            tts,
        }
    }

    /// Attach the Tauri AppHandle so the presentation loop can emit
    /// `pet-state` / `pet-speech` events to the frontend.
    pub fn attach_app(&mut self, app: AppHandle) {
        self.tts.attach_app(app.clone());
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
        let state = self.state.clone();
        tauri::async_runtime::spawn(async move {
            Self::audio_loop(audio_rx, action_tx, config, state).await;
        });
    }

    /// Consume `AgentAction`s, update shared pet state, emit `pet-state` /
    /// `pet-speech` to the frontend, and drive the TTS seam.
    async fn presentation_loop(
        action_rx: Receiver<AgentAction>,
        state: Arc<RwLock<PetState>>,
        app: Option<AppHandle>,
        tts: Arc<Tts>,
    ) {
        while let Ok(action) = action_rx.recv().await {
            let next = match &action {
                AgentAction::Speak(_) => Some(PetState::Speaking),
                AgentAction::SetPetState(s) => Some(s.clone()),
                AgentAction::ExecuteTool(_) => Some(PetState::Agentic),
                AgentAction::ToolResult(_) => Some(PetState::Thinking),
                // Idle only once the speech queue has drained; the drain
                // watcher below emits Idle later when audio is still playing.
                AgentAction::TurnFinished => turn_finished_next_state(tts.is_busy()),
            };

            match next {
                Some(new_state) => {
                    let mut guard = state.write().await;
                    if *guard != new_state {
                        // Barge-in: leaving Speaking toward Listening means the
                        // user wants to cut the pet off — stop TTS playback now.
                        if *guard == PetState::Speaking && new_state == PetState::Listening {
                            drop(guard);
                            tts.stop();
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
                    }
                }
                None => Self::watch_tts_drain(tts.clone(), state.clone(), app.clone()),
            }

            if let AgentAction::Speak(text) = &action {
                // TTS seam: enqueue the sentence; the dedicated worker
                // synthesizes it while the previous sentence keeps playing.
                let _ = tts.speak(text);
                if let Some(app) = &app {
                    let _ = app.emit("pet-speech", text.as_str());
                }
            }
        }
    }

    /// TurnFinished arrived while audio is still playing: poll (async, never
    /// blocking a tokio worker) until the TTS queue drains, then emit Idle.
    fn watch_tts_drain(tts: Arc<Tts>, state: Arc<RwLock<PetState>>, app: Option<AppHandle>) {
        tauri::async_runtime::spawn(async move {
            for _ in 0..1200 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if !tts.is_busy() {
                    break;
                }
            }
            let mut guard = state.write().await;
            if *guard == PetState::Speaking {
                *guard = PetState::Idle;
                drop(guard);
                if let Some(app) = &app {
                    let _ = app.emit("pet-state", &PetState::Idle);
                }
            }
        });
    }

    /// Stop any current playback (Tauri command + barge-in share this).
    pub fn stop_speaking(&self) {
        self.tts.stop();
    }

    /// Consume `AudioEvent`s, run the agent for each utterance / greeting,
    /// and forward resulting `AgentAction`s to the presentation loop.
    async fn audio_loop(
        audio_rx: Receiver<AudioEvent>,
        action_tx: Sender<AgentAction>,
        config: Arc<RwLock<XenonConfig>>,
        state: Arc<RwLock<PetState>>,
    ) {
        while let Ok(event) = audio_rx.recv().await {
            match event {
                AudioEvent::Speech(utterance) => {
                    // Half-duplex echo guard: in "always" listen mode the mic
                    // hears the pet's own TTS while it is Speaking. Drop
                    // VAD-triggered events then. The hotkey barge-in flips to
                    // Listening before user speech arrives, so real speech
                    // after a barge-in is never suppressed.
                    let echoing = Self::echo_guard(&config, &state).await;
                    if echoing {
                        log::debug!(
                            "tts echo guard: dropping transcribed speech while Speaking"
                        );
                        continue;
                    }
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
                // Hotkey always wins; VAD onset is suppressed by the echo
                // guard so the pet cannot barge itself out of Speaking.
                AudioEvent::ForceListen => {
                    let _ = action_tx
                        .send(AgentAction::SetPetState(PetState::Listening))
                        .await;
                }
                AudioEvent::Listening => {
                    if Self::echo_guard(&config, &state).await {
                        log::debug!("tts echo guard: dropping VAD onset while Speaking");
                        continue;
                    }
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

    /// Half-duplex echo guard: true while the pet is Speaking in "always"
    /// listen mode, where the mic would otherwise hear the pet's own TTS.
    /// Documented behaviour: VAD onset and transcribed Speech are dropped
    /// then; the Cmd+Shift+Space hotkey still works because it emits
    /// ForceListen, which is never guarded.
    async fn echo_guard(
        config: &Arc<RwLock<XenonConfig>>,
        state: &Arc<RwLock<PetState>>,
    ) -> bool {
        let always = {
            let cfg = config.read().await;
            cfg.listen_mode == "always"
        };
        always && matches!(*state.read().await, PetState::Speaking)
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

/// TurnFinished must become Idle only when nothing is left to play; while the
/// speech queue is non-empty or audio is playing the drain watcher defers it.
fn turn_finished_next_state(tts_busy: bool) -> Option<PetState> {
    if tts_busy {
        None
    } else {
        Some(PetState::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AudioEvent, Utterance};

    /// TurnFinished defers Idle only while audio is still playing.
    #[test]
    fn turn_finished_defers_idle_while_busy() {
        assert_eq!(turn_finished_next_state(true), None);
    }

    #[test]
    fn turn_finished_is_idle_once_drained() {
        assert_eq!(turn_finished_next_state(false), Some(PetState::Idle));
    }

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
        // tts disabled: keep tests silent (no fallback `say`); TEST-NET-1
        // provider URL keeps Thinking observable while HTTP times out.
        let cfg = XenonConfig {
            tts_enabled: false,
            provider: crate::config::ProviderConfig {
                provider_url: "http://192.0.2.1:9".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
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
        let cfg = XenonConfig {
            tts_enabled: false, // keep tests silent (no fallback `say`)
            provider: crate::config::ProviderConfig {
                provider_url: "http://192.0.2.1:9".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
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