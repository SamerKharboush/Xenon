use anyhow::Result;
use serde_json::Value;

use crate::events::{PetState, ToolCall, ToolResult};
use crate::provider::{ChatMessage, ChatRequestWithTools, Provider};
use crate::tools::{tools_to_schemas, Tool};

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    max_iterations: usize,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>, tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            provider,
            tools,
            max_iterations: 5,
        }
    }

    pub async fn run_utterance(&mut self, text: &str) -> Result<Vec<crate::events::AgentAction>> {
        let mut actions = Vec::new();
        let mut messages = vec![ChatMessage {
            role: "user".to_string(),
            content: text.to_string(),
        }];

        let schemas = tools_to_schemas(&self.tools);

        for iteration in 0..self.max_iterations {
            let request = ChatRequestWithTools {
                model: "local-model".to_string(),
                messages: messages.clone(),
                tools: schemas.clone(),
                stream: false,
                temperature: Some(0.7),
            };

            let response = self.provider.chat_with_tools(request).await?;

            if !response.tool_calls.is_empty() {
                let tool_call = &response.tool_calls[0];

                actions.push(crate::events::AgentAction::ExecuteTool(ToolCall {
                    name: tool_call.name.clone(),
                    args: tool_call.args.clone(),
                    call_id: tool_call.call_id.clone(),
                }));

                let tool = self
                    .tools
                    .iter()
                    .find(|t| t.name() == tool_call.name)
                    .ok_or_else(|| anyhow::anyhow!("Tool not found: {}", tool_call.name))?;

                let result = tool.execute(tool_call.args.clone()).await?;

                let result_value = serde_json::to_value(&result)?;

                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: format!(
                        "Called tool {} with args: {}",
                        tool_call.name, tool_call.args
                    ),
                });

                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: serde_json::to_string(&result_value)?,
                });

                actions.push(crate::events::AgentAction::ToolResult(result));

                if result.is_error {
                    break;
                }

                continue;
            }

            actions.push(crate::events::AgentAction::Speak(response.content.clone()));
            actions.push(crate::events::AgentAction::SetPetState(PetState::Idle));
            break;
        }

        if iteration == self.max_iterations - 1 {
            actions.push(crate::events::AgentAction::Speak(
                "I reached the maximum number of iterations. Please try a simpler request."
                    .to_string(),
            ));
        }

        Ok(actions)
    }

    pub fn get_tools(&self) -> &Vec<Box<dyn Tool>> {
        &self.tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ToolCall;
    use crate::provider::{ChatChunk, ChatRequest, ChatResponse, ChatStream, ToolSchema};
    use async_trait::async_trait;
    use futures_util::StreamExt;

    struct MockProvider {
        responses: Vec<MockResponse>,
        call_count: std::cell::RefCell<usize>,
    }

    struct MockResponse {
        content: String,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            unimplemented!()
        }

        async fn chat_with_tools(
            &self,
            _req: ChatRequestWithTools,
        ) -> Result<ChatResponse> {
            let mut count = self.call_count.borrow_mut();
            let idx = *count;
            *count += 1;

            if idx < self.responses.len() {
                let resp = &self.responses[idx];
                Ok(ChatResponse {
                    content: resp.content.clone(),
                    tool_calls: resp.tool_calls.clone(),
                    usage: None,
                })
            } else {
                Ok(ChatResponse {
                    content: "Done".to_string(),
                    tool_calls: vec![],
                    usage: None,
                })
            }
        }

        async fn chat_vision(&self, _image: &str, _prompt: &str) -> Result<String> {
            Ok("{}".to_string())
        }
    }

    struct MockTool {
        name: String,
        result: Value,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Mock tool"
        }

        fn schema(&self) -> Value {
            serde_json::json!({})
        }

        async fn execute(&self, _args: Value) -> Result<ToolResult> {
            Ok(ToolResult {
                call_id: "test".to_string(),
                result: self.result.clone(),
                is_error: false,
                error_message: None,
            })
        }
    }

    #[tokio::test]
    async fn test_agent_handles_tool_call() {
        let provider = Box::new(MockProvider {
            responses: vec![
                MockResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        name: "test_tool".to_string(),
                        args: serde_json::json!({"key": "value"}),
                        call_id: "call_1".to_string(),
                    }],
                },
                MockResponse {
                    content: "Tool executed successfully".to_string(),
                    tool_calls: vec![],
                },
            ],
            call_count: std::cell::RefCell::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
        });

        let mut agent = Agent::new(provider, vec![tool]);
        let actions = agent.run_utterance("Test").await.unwrap();

        assert!(actions.iter().any(|a| matches!(a, crate::events::AgentAction::ExecuteTool(_))));
        assert!(actions
            .iter()
            .any(|a| matches!(a, crate::events::AgentAction::Speak(_))));
    }

    #[tokio::test]
    async fn test_agent_handles_direct_response() {
        let provider = Box::new(MockProvider {
            responses: vec![MockResponse {
                content: "Direct answer".to_string(),
                tool_calls: vec![],
            }],
            call_count: std::cell::RefCell::new(0),
        });

        let mut agent = Agent::new(provider, vec![]);
        let actions = agent.run_utterance("Test").await.unwrap();

        assert!(actions
            .iter()
            .any(|a| matches!(a, crate::events::AgentAction::Speak(s) if s == "Direct answer")));
    }

    #[tokio::test]
    async fn test_agent_max_iterations() {
        let provider = Box::new(MockProvider {
            responses: vec![MockResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    name: "test_tool".to_string(),
                    args: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                }],
            }],
            call_count: std::cell::RefCell::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
        });

        let mut agent = Agent::new(provider, vec![tool]);
        let actions = agent.run_utterance("Test").await.unwrap();

        let speak_actions: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let crate::events::AgentAction::Speak(s) = a {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();

        assert!(!speak_actions.is_empty());
    }
}
