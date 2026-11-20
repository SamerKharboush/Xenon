use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
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

/// OpenAI message content: either a plain string or a list of parts
/// (text + image_url) for vision messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: MessageContent,
}

impl ChatMessage {
    pub fn text(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: MessageContent::Text(content.into()),
        }
    }

    /// A user message carrying text plus an image data URI (vision input).
    pub fn user_with_image(text: impl Into<String>, data_uri: String) -> Self {
        Self {
            role: ChatRole::User,
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: text.into() },
                ContentPart::ImageUrl {
                    image_url: ImageUrl { url: data_uri },
                },
            ]),
        }
    }
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

/// Wrap tool schemas in the OpenAI wire shape: `{"type":"function","function":{…}}`.
/// Strict servers (e.g. llama.cpp) reject bare `{name,description,parameters}`
/// objects with "Missing tool type".
fn tool_entries(tools: &[ToolSchema]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
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
    /// Non-streaming completion (used for grounding / vision calls).
    async fn chat(&self, req: ChatRequest) -> Result<String>;
}

/// A 5xx from the provider: the next fallback profile may succeed.
#[derive(Debug, thiserror::Error)]
#[error("provider server error ({status}): {body}")]
struct ServerError {
    status: u16,
    body: String,
}

/// Retryable when the failure is transient: a 5xx, a connection error, or a
/// timeout. 4xx (bad key, bad request) and parse errors are not retried.
pub fn is_retryable(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<ServerError>().is_some() {
        return true;
    }
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        return re.is_timeout() || re.is_connect() || re.is_request() || re.is_decode() || re.is_body();
    }
    false
}

/// One send lock per base URL. A single-slot llama.cpp server cannot serve
/// the planning and action calls concurrently, so every request to the same
/// endpoint (including the stream's lifetime) takes this lock.
static PROVIDER_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn provider_lock(base_url: &str) -> Arc<tokio::sync::Mutex<()>> {
    let locks = PROVIDER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    locks
        .lock()
        .unwrap()
        .entry(base_url.to_string())
        .or_default()
        .clone()
}

/// Normalize an OpenAI-compatible base URL: drop a trailing slash and append
/// `/v1` only when the URL has no path (so legacy `http://host:port` configs
/// keep working while presets that already include `/v1` or
/// `/v1beta/openai` are left untouched). The endpoint is then
/// `{base_url}/chat/completions`.
pub fn normalize_base_url(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    match reqwest::Url::parse(trimmed) {
        Ok(url) if url.path() == "/" || url.path().is_empty() => format!("{trimmed}/v1"),
        _ => trimmed.to_string(),
    }
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
            base_url: normalize_base_url(&base_url),
            api_key,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
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
            if status.is_server_error() {
                return Err(ServerError {
                    status: status.as_u16(),
                    body: text,
                }
                .into());
            }
            return Err(anyhow!("API request failed: {} - {}", status, text));
        }

        Ok(response)
    }

    /// Bounded retry for transient transport / 5xx errors. Each attempt runs
    /// under the provider lock, so a slow retry blocks other requests to the
    /// same single-slot server instead of piling on.
    async fn send_json_retry(&self, url: String, body: Value) -> Result<reqwest::Response> {
        const ATTEMPTS: usize = 3;
        let mut delay = Duration::from_millis(200);
        for attempt in 1..=ATTEMPTS {
            match self.send_json(url.clone(), body.clone()).await {
                Ok(r) => return Ok(r),
                Err(e) if attempt < ATTEMPTS && is_retryable(&e) => {
                    log::warn!("provider request failed ({e}); retry {attempt}/{ATTEMPTS}");
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow!("provider retries exhausted"))
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

struct GatedStream {
    inner: ChatStream,
    /// Held until the stream is dropped, so planning and action calls to the
    /// same endpoint never overlap.
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Stream for GatedStream {
    type Item = Result<ChatChunk>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[async_trait]
impl Provider for OpenAICompatProvider {
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": true,
            "temperature": req.temperature.unwrap_or(0.7),
        });
        if let Some(tools) = &req.tools {
            body["tools"] = serde_json::json!(tool_entries(tools));
        }
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        let guard = provider_lock(&self.base_url).lock_owned().await;
        let response = self.send_json_retry(self.endpoint(), body).await?;
        Ok(Box::pin(GatedStream {
            inner: self.stream_response(response),
            _guard: guard,
        }))
    }

    async fn chat(&self, req: ChatRequest) -> Result<String> {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": false,
            "temperature": req.temperature.unwrap_or(0.7),
        });
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        let _guard = provider_lock(&self.base_url).lock_owned().await;
        let response = self.send_json_retry(self.endpoint(), body).await?;
        let json: Value = response.json().await?;
        Ok(json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }
}

/// Provider Hub fallback: try each profile in order, transparently moving on
/// when the primary hits a connection error, timeout or 5xx. A non-retryable
/// error (e.g. 4xx) is surfaced immediately. A `xenon-notice` is emitted at
/// most once per instance when a fallback actually happens.
pub struct FallbackProvider {
    providers: Vec<Arc<dyn Provider>>,
    names: Vec<String>,
    app: Option<tauri::AppHandle>,
    notified: AtomicBool,
}

impl FallbackProvider {
    pub fn new(
        providers: Vec<Arc<dyn Provider>>,
        names: Vec<String>,
        app: Option<tauri::AppHandle>,
    ) -> Self {
        Self {
            providers,
            names,
            app,
            notified: AtomicBool::new(false),
        }
    }

    fn name_at(&self, i: usize) -> &str {
        self.names.get(i).map(|s| s.as_str()).unwrap_or("?")
    }

    fn notify_once(&self, tried: &str) {
        if self.notified.swap(true, Ordering::SeqCst) {
            return;
        }
        let msg = format!(
            "Primary provider unavailable; falling back to another profile ({tried})."
        );
        log::warn!("{msg}");
        if let Some(app) = &self.app {
            use tauri::Emitter;
            let _ = app.emit("xenon-notice", msg);
        }
    }

    /// Try each provider in order. `attempt` is `chat_stream` or `chat`.
    async fn retry_all<T, F, Fut>(&self, mut attempt: F) -> Result<T>
    where
        F: FnMut(usize) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last_err: Option<anyhow::Error> = None;
        for i in 0..self.providers.len() {
            match attempt(i).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_retryable(&e) {
                        return Err(e);
                    }
                    log::warn!("provider {} failed ({}), trying next", self.name_at(i), e);
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            let tried = self.names.join(" -> ");
            self.notify_once(&tried);
            return Err(e);
        }
        Err(anyhow!("no providers configured"))
    }
}

#[async_trait]
impl Provider for FallbackProvider {
    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        // Each attempt clones the request; only the profile that accepts it
        // consumes it. Streaming failures mid-stream are not retried (the
        // model already saw partial output).
        self.retry_all(|i| {
            let p = self.providers[i].clone();
            let req = req.clone();
            async move { p.chat_stream(req).await }
        })
        .await
    }

    async fn chat(&self, req: ChatRequest) -> Result<String> {
        self.retry_all(|i| {
            let p = self.providers[i].clone();
            let req = req.clone();
            async move { p.chat(req).await }
        })
        .await
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
            messages: vec![ChatMessage::text(ChatRole::User, "Hello")],
            tools: None,
            stream: true,
            temperature: Some(0.7),
            max_tokens: Some(100),
        };

        assert_eq!(provider.base_url, "http://localhost:11434/v1");
        assert_eq!(provider.endpoint(), "http://localhost:11434/v1/chat/completions");
        assert_eq!(provider.api_key, Some("test-key".to_string()));
        assert_eq!(req.model, "llama2");
    }

    #[test]
    fn test_normalize_base_url() {
        assert_eq!(normalize_base_url("http://localhost:11434"), "http://localhost:11434/v1");
        assert_eq!(normalize_base_url("http://localhost:1234/v1/"), "http://localhost:1234/v1");
        assert_eq!(normalize_base_url("https://api.openai.com/v1"), "https://api.openai.com/v1");
        assert_eq!(
            normalize_base_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(normalize_base_url("https://api.anthropic.com/v1"), "https://api.anthropic.com/v1");
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

    /// Tool schemas go over the wire wrapped in `type:function` — llama.cpp and
    /// other strict OpenAI-compatible servers reject bare schema objects.
    #[test]
    fn test_tool_entries_use_openai_function_wrapper() {
        let entries = tool_entries(&[ToolSchema {
            name: "ping".into(),
            description: "pong".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["type"], "function");
        assert_eq!(entries[0]["function"]["name"], "ping");
        assert_eq!(entries[0]["function"]["description"], "pong");
        assert_eq!(entries[0]["function"]["parameters"]["type"], "object");
        assert!(entries[0]["name"].is_null(), "bare name leaked to wire");
    }

    /// Chat messages serialize with OpenAI's lowercase role strings.
    #[test]
    fn test_chat_message_role_is_lowercase() {
        let json = serde_json::to_string(&ChatMessage {
            role: ChatRole::Tool,
            content: MessageContent::Text("{}".to_string()),
        })
        .unwrap();
        assert_eq!(json, r#"{"role":"tool","content":"{}"}"#);
    }

    /// Vision messages serialize to OpenAI's content-parts shape.
    #[test]
    fn test_vision_message_serializes_parts() {
        let msg = ChatMessage::user_with_image("find the button", "data:image/png;base64,AAAA".to_string());
        let json = serde_json::to_string(&msg).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        let parts = value["content"].as_array().expect("content parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "find the button");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn test_text_message_serializes_string() {
        let msg = ChatMessage::text(ChatRole::User, "hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
    }

    // ---- fallback ordering -------------------------------------------------

    /// A provider that always fails. `retryable` picks a 5xx (retryable) or a
    /// plain client error (not retryable).
    struct FailingProvider {
        retryable: bool,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FailingProvider {
        fn err(&self) -> anyhow::Error {
            if self.retryable {
                ServerError {
                    status: 503,
                    body: "unavailable".to_string(),
                }
                .into()
            } else {
                anyhow::anyhow!("client error")
            }
        }
    }

    #[async_trait]
    impl Provider for FailingProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.err())
        }

        async fn chat(&self, _req: ChatRequest) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.err())
        }
    }

    /// An OK provider that returns a fixed chat result.
    struct OkProvider {
        name: String,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for OkProvider {
        async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                content: "ok".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
            })])))
        }

        async fn chat(&self, _req: ChatRequest) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("ok:{}", self.name))
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: None,
            stream: false,
            temperature: None,
            max_tokens: None,
        }
    }

    #[tokio::test]
    async fn test_fallback_order_on_retryable_error() {
        let c1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback = FallbackProvider::new(
            vec![
                Arc::new(FailingProvider {
                    retryable: true,
                    calls: c1.clone(),
                }),
                Arc::new(OkProvider {
                    name: "second".into(),
                    calls: c2.clone(),
                }),
            ],
            vec!["first".into(), "second".into()],
            None,
        );

        let out = fallback.chat(req()).await.unwrap();
        assert_eq!(out, "ok:second");
        assert_eq!(c1.load(Ordering::SeqCst), 1, "first tried once");
        assert_eq!(c2.load(Ordering::SeqCst), 1, "second tried once");
    }

    #[tokio::test]
    async fn test_fallback_skips_retry_on_client_error() {
        let c1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback = FallbackProvider::new(
            vec![
                Arc::new(FailingProvider {
                    retryable: false,
                    calls: c1.clone(),
                }),
                Arc::new(OkProvider {
                    name: "second".into(),
                    calls: c2.clone(),
                }),
            ],
            vec!["first".into(), "second".into()],
            None,
        );

        let err = fallback.chat(req()).await.unwrap_err();
        assert!(err.to_string().contains("client error"));
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0, "second must not be tried on 4xx");
    }

    #[tokio::test]
    async fn test_fallback_stops_after_first_success() {
        let c1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fallback = FallbackProvider::new(
            vec![
                Arc::new(OkProvider {
                    name: "first".into(),
                    calls: c1.clone(),
                }),
                Arc::new(OkProvider {
                    name: "second".into(),
                    calls: c2.clone(),
                }),
            ],
            vec!["first".into(), "second".into()],
            None,
        );

        let out = fallback.chat(req()).await.unwrap();
        assert_eq!(out, "ok:first");
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0, "second must not be tried");
    }

    #[tokio::test]
    async fn test_fallback_returns_last_error_when_all_fail() {
        let fallback = FallbackProvider::new(
            vec![
                Arc::new(FailingProvider {
                    retryable: true,
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                Arc::new(FailingProvider {
                    retryable: true,
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
            ],
            vec!["a".into(), "b".into()],
            None,
        );

        let err = fallback.chat(req()).await.unwrap_err();
        assert!(err.to_string().contains("503"));
    }

    /// Harness 5: dead primary port must fall back to a live ToshLLM server
    /// with one notice (no Tauri app attached, so we only measure timing).
    #[tokio::test]
    #[ignore = "uses local model server; run with -- --ignored stress_provider_fallback"]
    async fn stress_provider_fallback_dead_port() {
        let dead = OpenAICompatProvider::new("http://127.0.0.1:1".to_string(), None);
        let live = OpenAICompatProvider::new("http://127.0.0.1:8080/v1".to_string(), None);
        let fallback = FallbackProvider::new(
            vec![Arc::new(dead), Arc::new(live)],
            vec!["dead-port".into(), "ToshLLM".into()],
            None,
        );
        let mut req = req();
        req.model = std::env::var("XENON_MODEL").unwrap_or_else(|_| "local".into());
        req.messages = vec![ChatMessage::text(ChatRole::User, "Say OK.")];
        let started = std::time::Instant::now();
        match fallback.chat(req).await {
            Ok(out) => eprintln!(
                "fallback ok in {} ms: {}",
                started.elapsed().as_millis(),
                out
            ),
            Err(e) => eprintln!("fallback skipped (server down?): {e}"),
        }
    }

    /// Harness 5: malformed 200 body and HTTP 500 are surfaced as errors.
    #[tokio::test]
    #[ignore = "spawns local mock server; run with -- --ignored stress_provider_malformed"]
    async fn stress_provider_malformed_and_500() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn serve(body: &'static str, status: &'static str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                for _ in 0..4 {
                    let Ok((mut sock, _)) = listener.accept().await else { break };
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
            format!("http://{addr}")
        }

        let bad = OpenAICompatProvider::new(serve("not json", "200 OK").await, None);
        let err = bad.chat(req()).await.unwrap_err();
        eprintln!("malformed 200 -> {err}");
        assert!(!err.to_string().is_empty());

        let err500 = OpenAICompatProvider::new(serve("boom", "500 Internal Server Error").await, None)
            .chat(req())
            .await
            .unwrap_err();
        eprintln!("500 -> {err500}");
        assert!(is_retryable(&err500), "500 must stay retryable");
    }

    #[tokio::test]
    async fn test_fallback_empty_is_error() {
        let fallback = FallbackProvider::new(vec![], vec![], None);
        assert!(fallback.chat(req()).await.is_err());
    }

    #[test]
    fn test_provider_lock_is_per_base_url() {
        let a1 = provider_lock("http://localhost:8080/v1");
        let a2 = provider_lock("http://localhost:8080/v1");
        let b = provider_lock("http://localhost:8081/v1");
        assert!(Arc::ptr_eq(&a1, &a2), "same endpoint shares a lock");
        assert!(!Arc::ptr_eq(&a1, &b), "different endpoints do not share");
    }

    #[tokio::test]
    async fn test_send_json_retry_recovers_from_503() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = if attempt == 0 {
                    "unavailable"
                } else {
                    r#"{"choices":[{"message":{"content":"ok"}}]}"#
                };
                let status = if attempt == 0 {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let provider = OpenAICompatProvider::new(format!("http://{addr}"), None);
        let resp = provider
            .send_json_retry(provider.endpoint(), serde_json::json!({}))
            .await
            .unwrap();
        assert!(resp.status().is_success(), "second attempt should succeed");
    }
}