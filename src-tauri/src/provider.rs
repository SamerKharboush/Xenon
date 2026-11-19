use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use crate::events::ToolCall;

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    /// Tool schemas for tool-capable models. `None` = plain chat completion.
    #[serde(default)]
    pub tools: Option<Vec<ToolSchema>>,
    pub stream: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    pub content: String,
    /// Complete tool calls. Streaming providers send these as fragments
    /// (`index`, partial `function.arguments`); the provider accumulates them
    /// and only emits fully-received calls here, on the final chunk.
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Streams a chat completion. When `req.tools` is set, tool calls are
    /// delivered as complete `ToolCall`s in the final chunk.
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream>;
    async fn chat_vision(&self, image_base64: &str, prompt: &str) -> Result<String>;
}

pub struct OpenAICompatProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAICompatProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        // Timeouts so a dead endpoint fails the turn instead of hanging it.
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("XENON_PROVIDER_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let api_key = std::env::var("XENON_PROVIDER_API_KEY").ok();

        Ok(Self::new(base_url, api_key))
    }

    fn build_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        if let Some(ref key) = self.api_key {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", key)) {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }

        headers
    }

    /// POST a JSON body and turn any non-2xx into an Err carrying status + body.
    async fn send_json(&self, url: String, body: Value) -> Result<reqwest::Response> {
        let response = self
            .client
            .post(&url)
            .headers(self.build_headers())
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await?;
            return Err(anyhow!("API request failed: {} - {}", status, text));
        }

        Ok(response)
    }

    /// Parses an OpenAI-style SSE body into ChatChunks: line-buffered,
    /// `data:`-prefixed JSON only, tool-call fragments accumulated by index.
    fn stream_response(&self, response: reqwest::Response) -> ChatStream {
        #[derive(Default)]
        struct Acc {
            buf: String,
            /// tool-call index -> (id, name, argument fragments)
            calls: BTreeMap<usize, (String, String, String)>,
            finish_pending: bool,
            calls_emitted: bool,
        }

        fn complete_calls(acc: &Acc) -> Vec<ToolCall> {
            acc.calls
                .iter()
                .map(|(_, (id, name, args))| {
                    let args = if args.trim().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        serde_json::from_str(args).unwrap_or(Value::Object(Default::default()))
                    };
                    ToolCall {
                        name: name.clone(),
                        args,
                        call_id: id.clone(),
                    }
                })
                .collect()
        }

        let stream = futures::stream::unfold(
            (Box::pin(response.bytes_stream()), Acc::default()),
            |(mut bytes, mut acc)| {
                Box::pin(async move {
                    let mut out: Option<ChatChunk> = None;
                    let mut err: Option<anyhow::Error> = None;

                    loop {
                        if acc.finish_pending {
                            break;
                        }
                        if let Some(pos) = acc.buf.find('\n') {
                            let line: String = acc.buf.drain(..=pos).collect();
                            let line = line.trim();
                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if data == "[DONE]" {
                                    break;
                                }
                                match serde_json::from_str::<Value>(data) {
                                    Ok(json) => {
                                        let choice = &json["choices"][0];
                                        let content = choice["delta"]["content"]
                                            .as_str()
                                            .unwrap_or("")
                                            .to_string();
                                        if let Some(calls) =
                                            choice["delta"]["tool_calls"].as_array()
                                        {
                                            for tc in calls {
                                                let idx =
                                                    tc["index"].as_u64().unwrap_or(0) as usize;
                                                let entry = acc.calls.entry(idx).or_default();
                                                if let Some(id) = tc["id"].as_str() {
                                                    if !id.is_empty() {
                                                        entry.0 = id.to_string();
                                                    }
                                                }
                                                if let Some(name) =
                                                    tc["function"]["name"].as_str()
                                                {
                                                    if !name.is_empty() {
                                                        entry.1 = name.to_string();
                                                    }
                                                }
                                                if let Some(args) =
                                                    tc["function"]["arguments"].as_str()
                                                {
                                                    entry.2.push_str(args);
                                                }
                                            }
                                        }
                                        let finish_reason = choice["finish_reason"]
                                            .as_str()
                                            .map(String::from);
                                        if finish_reason.is_some() {
                                            acc.finish_pending = true;
                                            if content.is_empty() {
                                                break;
                                            }
                                        }
                                        if !content.is_empty() {
                                            out = Some(ChatChunk {
                                                content,
                                                tool_calls: vec![],
                                                finish_reason,
                                            });
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        err = Some(anyhow!("Failed to parse chunk: {}", e));
                                        break;
                                    }
                                }
                            }
                            continue;
                        }

                        // No complete line buffered: pull more bytes.
                        let mut exhausted = false;
                        match bytes.next().await {
                            Some(Ok(chunk)) => {
                                acc.buf.push_str(&String::from_utf8_lossy(&chunk))
                            }
                            Some(Err(e)) => {
                                err = Some(anyhow!("Stream error: {}", e));
                                break;
                            }
                            None => exhausted = true,
                        }
                        if exhausted {
                            if !acc.buf.is_empty() {
                                // Flush a trailing partial line.
                                acc.buf.push('\n');
                                continue;
                            }
                            break;
                        }
                    }

                    if let Some(e) = err {
                        return Some((Err(e), (bytes, acc)));
                    }

                    // Emit fully-received tool calls once, at the end.
                    if !acc.calls.is_empty() && !acc.calls_emitted {
                        acc.calls_emitted = true;
                        return Some((
                            Ok(ChatChunk {
                                content: out.map(|c| c.content).unwrap_or_default(),
                                tool_calls: complete_calls(&acc),
                                finish_reason: None,
                            }),
                            (bytes, acc),
                        ));
                    }

                    out.map(|chunk| (Ok(chunk), (bytes, acc)))
                })
            },
        );

        Box::pin(stream)
    }
}

#[async_trait]
impl Provider for OpenAICompatProvider {
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": true,
            "temperature": req.temperature.unwrap_or(0.7),
        });
        if let Some(tools) = &req.tools {
            body["tools"] = serde_json::to_value(tools)?;
        }
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        let response = self.send_json(url, body).await?;
        Ok(self.stream_response(response))
    }

    async fn chat_vision(&self, image_base64: &str, prompt: &str) -> Result<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let body = serde_json::json!({
            "model": "gpt-4-vision-preview",
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": prompt
                    },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:image/png;base64,{}", image_base64)
                        }
                    }
                ]
            }],
            "max_tokens": 500,
        });

        let response = self.send_json(url, body).await?;
        let json: Value = response.json().await?;
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_provider_builds_correct_request() {
        let provider = OpenAICompatProvider::new(
            "http://localhost:11434".to_string(),
            Some("test-key".to_string()),
        );

        let req = ChatRequest {
            model: "llama2".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "Hello".to_string(),
            }],
            tools: None,
            stream: true,
            temperature: Some(0.7),
            max_tokens: Some(100),
        };

        assert_eq!(provider.base_url, "http://localhost:11434");
        assert_eq!(provider.api_key, Some("test-key".to_string()));
        assert_eq!(req.model, "llama2");
    }

    #[test]
    fn test_tool_schema_serialization() {
        let schema = ToolSchema {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        };

        let json = serde_json::to_string(&schema).unwrap();
        let deserialized: ToolSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "test_tool");
    }

    /// Chat messages serialize with OpenAI's lowercase role strings.
    #[test]
    fn test_chat_message_role_is_lowercase() {
        let json = serde_json::to_string(&ChatMessage {
            role: ChatRole::Tool,
            content: "{}".to_string(),
        })
        .unwrap();
        assert_eq!(json, r#"{"role":"tool","content":"{}"}"#);
    }
}