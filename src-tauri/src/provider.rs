use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

use crate::events::ToolCall;

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequestWithTools {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSchema>,
    pub stream: bool,
    pub temperature: Option<f32>,
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
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream>;
    async fn chat_with_tools(&self, req: ChatRequestWithTools) -> Result<ChatResponse>;
    async fn chat_vision(&self, image_base64: &str, prompt: &str) -> Result<String>;
}

pub struct OpenAICompatProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAICompatProvider {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            client: Client::new(),
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

    async fn stream_response(&self, response: reqwest::Response) -> Result<ChatStream> {
        use futures_util::StreamExt;

        let stream = response
            .bytes_stream()
            .scan(String::new(), |buffer, chunk| {
                let chunk = chunk?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let lines: Vec<String> = buffer
                    .split('\n')
                    .filter(|line| !line.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                *buffer = String::new();
                futures::future::ready(Some(Ok(lines)))
            })
            .flat_map(|lines| {
                futures::stream::iter(lines.into_iter().filter_map(|line| {
                    if line.starts_with("data: ") {
                        let data = line.trim_start_matches("data: ").trim();
                        if data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<Value>(data) {
                            Ok(json) => {
                                let content = json["choices"][0]["delta"]["content"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();

                                let tool_calls = json["choices"][0]["delta"]["tool_calls"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|tc| {
                                                let name =
                                                    tc["function"]["name"].as_str()?.to_string();
                                                let args = tc["function"]["arguments"]
                                                    .as_str()
                                                    .unwrap_or("{}")
                                                    .to_string();
                                                let call_id = tc["id"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string();
                                                Some(ToolCall {
                                                    name,
                                                    args: serde_json::from_str(&args).ok()?,
                                                    call_id,
                                                })
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();

                                let finish_reason = json["choices"][0]["finish_reason"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                Some(Ok(ChatChunk {
                                    content,
                                    tool_calls,
                                    finish_reason,
                                }))
                            }
                            Err(e) => Some(Err(anyhow!("Failed to parse chunk: {}", e))),
                        }
                    } else {
                        None
                    }
                }))
            });

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl Provider for OpenAICompatProvider {
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": true,
            "temperature": req.temperature.unwrap_or(0.7),
            "max_tokens": req.max_tokens.unwrap_or(2048),
        });

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

        self.stream_response(response).await
    }

    async fn chat_with_tools(&self, req: ChatRequestWithTools) -> Result<ChatResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "tools": req.tools,
            "stream": false,
            "temperature": req.temperature.unwrap_or(0.7),
        });

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

        let json: Value = response.json().await?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let tool_calls = json["choices"][0]["message"]["tool_calls"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        let name = tc["function"]["name"].as_str()?.to_string();
                        let args = tc["function"]["arguments"].as_str()?.to_string();
                        let call_id = tc["id"].as_str()?.to_string();
                        Some(ToolCall {
                            name,
                            args: serde_json::from_str(&args).ok()?,
                            call_id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let usage = json["usage"].as_object().map(|u| Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
        });

        Ok(ChatResponse {
            content,
            tool_calls,
            usage,
        })
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
            return Err(anyhow!("Vision API request failed: {} - {}", status, text));
        }

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
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
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
}
