use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioEvent {
    Speech(Utterance),
    Silence,
    /// User tapped the pet: greet them by name.
    Greet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Utterance {
    pub text: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentAction {
    /// One sentence of the final spoken reply.
    Speak(String),
    /// Keep the pet in the given Pet State (used at turn start: Thinking).
    SetPetState(PetState),
    /// A tool is about to run (pet goes Agentic).
    ExecuteTool(ToolCall),
    /// A tool finished; `is_error` results are also fed back to the model.
    ToolResult(ToolResult),
    /// The agent turn is over; the pet returns to Idle.
    TurnFinished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PetState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Agentic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub result: serde_json::Value,
    pub is_error: bool,
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_event_serialization() {
        let speech = AudioEvent::Speech(Utterance {
            text: "hello".to_string(),
            confidence: 0.95,
        });
        let json = serde_json::to_string(&speech).unwrap();
        let deserialized: AudioEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            AudioEvent::Speech(u) => {
                assert_eq!(u.text, "hello");
                assert!((u.confidence - 0.95).abs() < 0.001);
            }
            _ => panic!("Expected Speech variant"),
        }
    }

    #[test]
    fn test_pet_state_serialization() {
        for state in [
            PetState::Idle,
            PetState::Listening,
            PetState::Thinking,
            PetState::Speaking,
            PetState::Agentic,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let _: PetState = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_tool_call_serialization() {
        let call = ToolCall {
            name: "take_screenshot".to_string(),
            args: serde_json::json!({}),
            call_id: "call_123".to_string(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "take_screenshot");
        assert_eq!(deserialized.call_id, "call_123");
    }

    #[test]
    fn test_tool_result_serialization() {
        let result = ToolResult {
            call_id: "call_123".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
            error_message: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.call_id, "call_123");
        assert!(!deserialized.is_error);
    }
}
