use crate::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_events_types() {
        let audio_event = AudioEvent::Speech(Utterance {
            text: "Hello".to_string(),
            confidence: 0.9,
        });
        assert!(matches!(audio_event, AudioEvent::Speech(_)));

        let agent_action = AgentAction::Speak("Hello".to_string());
        assert!(matches!(agent_action, AgentAction::Speak(_)));
    }
}
