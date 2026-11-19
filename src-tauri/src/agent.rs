use std::sync::Arc;

use anyhow::Result;
use async_channel::Sender;
use futures_util::StreamExt;

use crate::config::XenonConfig;
use crate::events::{AgentAction, PetState, ToolCall, ToolResult};
use crate::provider::{ChatMessage, ChatRequest, ChatRole, Provider};
use crate::tools::{tools_to_schemas, Tool};

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    max_iterations: usize,
    config: XenonConfig,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Box<dyn Tool>>, config: XenonConfig) -> Self {
        Self {
            provider,
            tools,
            max_iterations: 5,
            config,
        }
    }

    /// Run a full ReAct turn for *text*, streaming sentences to *emit* as
    /// they are recognised. Tool errors are fed back into the conversation
    /// so the model can react; the loop caps at `max_iterations`.
    pub async fn run_utterance(&self, text: &str, emit: Sender<AgentAction>) -> Result<()> {
        let _ = emit
            .send(AgentAction::SetPetState(PetState::Thinking))
            .await;

        let mut messages = vec![
            ChatMessage {
                role: ChatRole::System,
                content: self.config.system_prompt.clone(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: text.to_string(),
            },
        ];

        let schemas = tools_to_schemas(&self.tools);

        for _ in 0..self.max_iterations {
            let req = ChatRequest {
                model: self.config.provider.model_name.clone(),
                messages: messages.clone(),
                tools: Some(schemas.clone()),
                stream: true,
                temperature: Some(0.8),
                max_tokens: None,
            };

            let stream = self.provider.chat_stream(req).await?;
            let mut stream = stream;
            let mut buffer = String::new();
            let mut full_content = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if !chunk.content.is_empty() {
                    full_content.push_str(&chunk.content);
                    buffer.push_str(&chunk.content);
                    for sentence in split_sentences(&mut buffer) {
                        let _ = emit.send(AgentAction::Speak(sentence)).await;
                    }
                }
                if !chunk.tool_calls.is_empty() {
                    tool_calls = chunk.tool_calls;
                }
            }

            // Flush any remaining partial sentence.
            let remainder = buffer.trim().to_string();
            if !remainder.is_empty() {
                let _ = emit.send(AgentAction::Speak(remainder)).await;
            }

            if !tool_calls.is_empty() {
                let tool_call = tool_calls.into_iter().next().unwrap();
                let _ = emit
                    .send(AgentAction::ExecuteTool(tool_call.clone()))
                    .await;

                let result = match self.tools.iter().find(|t| t.name() == tool_call.name) {
                    Some(tool) => match tool.execute(tool_call.args.clone()).await {
                        Ok(mut r) => {
                            r.call_id = tool_call.call_id.clone();
                            r
                        }
                        Err(e) => ToolResult {
                            call_id: tool_call.call_id.clone(),
                            result: serde_json::json!({}),
                            is_error: true,
                            error_message: Some(e.to_string()),
                        },
                    },
                    None => ToolResult {
                        call_id: tool_call.call_id.clone(),
                        result: serde_json::json!({}),
                        is_error: true,
                        error_message: Some(format!("Tool not found: {}", tool_call.name)),
                    },
                };

                let is_error = result.is_error;
                let _ = emit.send(AgentAction::ToolResult(result.clone())).await;

                // Feed result back into conversation so the model can react.
                messages.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: if full_content.is_empty() {
                        format!("Called tool {} with args: {}", tool_call.name, tool_call.args)
                    } else {
                        full_content.clone()
                    },
                });

                let result_json = if is_error {
                    serde_json::json!({ "error": result.error_message })
                } else {
                    result.result.clone()
                };

                messages.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: serde_json::to_string(&result_json)?,
                });

                continue;
            }

            // No tool calls — turn is done.
            break;
        }

        let _ = emit.send(AgentAction::TurnFinished).await;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_tools(&self) -> &Vec<Box<dyn Tool>> {
        &self.tools
    }
}

/// Split *buffer* at sentence boundaries, returning complete sentences and
/// leaving the incomplete tail in *buffer*.
///
/// Boundaries are `.` / `!` / `?` followed by whitespace (or end-of-input)
/// and newlines.  Decimals (`3.14`), common abbreviations (`Mr.`, `Dr.`)
/// and single-letter initials (`J.`) are skipped.
fn split_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut sentence_start = 0;
    {
        let bytes = buffer.as_bytes();
        for i in 0..bytes.len() {
            if is_sentence_end(bytes, i) {
                let s = buffer[sentence_start..=i].trim().to_string();
                if !s.is_empty() {
                    sentences.push(s);
                }
                sentence_start = i + 1;
            }
        }
    }
    let remainder = buffer[sentence_start..].to_string();
    buffer.clear();
    buffer.push_str(&remainder);
    sentences
}

fn is_sentence_end(bytes: &[u8], i: usize) -> bool {
    let b = bytes[i];
    if b == b'\n' {
        return true;
    }
    if b != b'.' && b != b'!' && b != b'?' {
        return false;
    }

    // Must be at end or followed by whitespace / closing quote.
    match bytes.get(i + 1) {
        None => {}
        Some(&c) if c == b' ' || c == b'\t' || c == b'\n' || c == b'"' => {}
        Some(_) => return false,
    }

    if b == b'.' {
        // Decimal: digit.digit
        let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
        let next_digit = bytes
            .get(i + 1)
            .map(|&c| c.is_ascii_digit())
            .unwrap_or(false);
        if prev_digit && next_digit {
            return false;
        }

        // Walk back to find the word before the period.
        let mut ws = i;
        while ws > 0 && bytes[ws - 1].is_ascii_alphanumeric() {
            ws -= 1;
        }
        let word = &bytes[ws..i];
        // Single-letter initial (J. Smith).
        if word.len() == 1 {
            return false;
        }
        // Common abbreviations.
        const ABBREVS: &[&[u8]] = &[
            b"mr", b"mrs", b"ms", b"dr", b"prof", b"sr", b"jr", b"st", b"vs", b"etc",
        ];
        let lower: Vec<u8> = word.iter().map(|b| b.to_ascii_lowercase()).collect();
        if ABBREVS.contains(&lower.as_slice()) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatChunk, ChatRequest, ChatStream};
    use async_trait::async_trait;
    use serde_json::Value;

    // ---- sentence splitter --------------------------------------------------

    #[test]
    fn test_split_simple() {
        let mut buf = "Hello world.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_two_sentences() {
        let mut buf = "Hello world. How are you?".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world.", "How are you?"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_decimal_not_boundary() {
        let mut buf = "3.14 is pi.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["3.14 is pi."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_abbreviation_not_boundary() {
        let mut buf = "Mr. Smith went home.".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Mr. Smith went home."]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_split_newline_boundary() {
        let mut buf = "Hello\nWorld".to_string();
        let s = split_sentences(&mut buf);
        assert_eq!(s, vec!["Hello"]);
        assert_eq!(buf, "World");
    }

    #[test]
    fn test_split_no_boundary_buffers_all() {
        let mut buf = "No ending period".to_string();
        let s = split_sentences(&mut buf);
        assert!(s.is_empty());
        assert_eq!(buf, "No ending period");
    }

    // ---- agent streaming ----------------------------------------------------

    struct MockProvider {
        chunks: Vec<Vec<ChatChunk>>,
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            use std::sync::atomic::Ordering;
            let idx = self.call_count.fetch_add(1, Ordering::Relaxed);
            let chunks = self.chunks.get(idx).cloned().unwrap_or_else(|| {
                vec![ChatChunk {
                    content: "done".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }]
            });
            Ok(Box::pin(futures::stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        async fn chat_vision(&self, _image: &str, _prompt: &str) -> Result<String> {
            Ok("{}".to_string())
        }
    }

    struct MockTool {
        name: String,
        result: Value,
        is_error: bool,
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
                is_error: self.is_error,
                error_message: if self.is_error {
                    Some("mock error".to_string())
                } else {
                    None
                },
            })
        }
    }

    #[tokio::test]
    async fn test_agent_handles_tool_call() {
        let provider = Arc::new(MockProvider {
            chunks: vec![
                vec![ChatChunk {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        name: "test_tool".to_string(),
                        args: serde_json::json!({"key": "value"}),
                        call_id: "call_1".to_string(),
                    }],
                    finish_reason: Some("tool_calls".to_string()),
                }],
                vec![
                    ChatChunk {
                        content: "Tool executed successfully".to_string(),
                        tool_calls: vec![],
                        finish_reason: None,
                    },
                    ChatChunk {
                        content: ".".to_string(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".to_string()),
                    },
                ],
            ],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::ExecuteTool(_))));
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s == "Tool executed successfully.")));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    #[tokio::test]
    async fn test_agent_handles_direct_response() {
        let provider = Arc::new(MockProvider {
            chunks: vec![vec![
                ChatChunk {
                    content: "Direct answer".to_string(),
                    tool_calls: vec![],
                    finish_reason: None,
                },
                ChatChunk {
                    content: ".".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                },
            ]],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let agent = Agent::new(provider, vec![], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s == "Direct answer.")));
    }

    #[tokio::test]
    async fn test_agent_max_iterations() {
        let provider = Arc::new(MockProvider {
            chunks: vec![vec![ChatChunk {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    name: "test_tool".to_string(),
                    args: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                }],
                finish_reason: Some("tool_calls".to_string()),
            }]],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({"status": "ok"}),
            is_error: false,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        assert!(actions.iter().any(|a| matches!(a, AgentAction::Speak(_))));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::TurnFinished)));
    }

    /// A tool that errors must still produce a TurnFinished so the pet
    /// does not get stuck in Thinking.
    #[tokio::test]
    async fn test_tool_error_fed_back_and_turn_finishes() {
        let provider = Arc::new(MockProvider {
            chunks: vec![
                vec![ChatChunk {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        name: "test_tool".to_string(),
                        args: serde_json::json!({}),
                        call_id: "call_1".to_string(),
                    }],
                    finish_reason: Some("tool_calls".to_string()),
                }],
                vec![ChatChunk {
                    content: "I see the error.".to_string(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".to_string()),
                }],
            ],
            call_count: std::sync::atomic::AtomicUsize::new(0),
        });

        let tool = Box::new(MockTool {
            name: "test_tool".to_string(),
            result: serde_json::json!({}),
            is_error: true,
        });

        let agent = Agent::new(provider, vec![tool], crate::config::XenonConfig::default());
        let (tx, rx) = async_channel::bounded(32);

        agent.run_utterance("Test", tx).await.unwrap();

        let actions: Vec<AgentAction> = {
            let mut v = Vec::new();
            while let Ok(a) = rx.try_recv() {
                v.push(a);
            }
            v
        };

        // ToolResult with is_error must be present
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::ToolResult(tr) if tr.is_error)));
        // The loop must have continued after the error and produced speech
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Speak(s) if s.contains("error"))));
        // And the turn must have finished
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::TurnFinished)));
    }
}