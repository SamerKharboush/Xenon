//! Minimal MCP (Model Context Protocol) client: stdio + Streamable HTTP.
//!
//! Hand-rolled JSON-RPC 2.0 rather than the `rmcp` framework: it is Apache-2.0
//! but pulls ~336 crates, and the subset Xenon needs (initialize →
//! notifications/initialized → tools/list → tools/call) is small and testable
//! with a mock stdio server. No GPL dependencies.
//!
//! Lifecycle: servers connect lazily on the first tool call, restart on crash
//! with a short backoff, are killed on quit, and every `tools/call` is bounded
//! by a 60 s timeout with the output truncated.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::config::McpServer;
use crate::events::ToolResult;
use crate::tools::Tool;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_INFO: &str = "Xenon";
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const INIT_TIMEOUT: Duration = Duration::from_secs(15);
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const MAX_OUTPUT_CHARS: usize = 6000;
const MAX_ERR_CHARS: usize = 400;

// ---- transport -----------------------------------------------------------

struct StdioConn {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl Drop for StdioConn {
    fn drop(&mut self) {
        // Best-effort kill so a removed server does not outlive the app.
        let _ = self.child.start_kill();
    }
}

struct HttpConn {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session_id: Option<String>,
    next_id: u64,
}

enum Transport {
    Stdio(StdioConn),
    Http(HttpConn),
}

impl Transport {
    fn is_alive(&mut self) -> bool {
        match self {
            Transport::Stdio(c) => !c.child.try_wait().ok().flatten().is_some(),
            Transport::Http(_) => true,
        }
    }

    /// Send a request and wait for the matching response. For HTTP this needs
    /// the response body, so it is handled separately below.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(c) => {
                c.next_id += 1;
                let id = c.next_id;
                let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                let line = serde_json::to_string(&msg)?;
                c.stdin.write_all(line.as_bytes()).await?;
                c.stdin.write_all(b"\n").await?;
                c.stdin.flush().await?;
                loop {
                    let line = tokio::time::timeout(CALL_TIMEOUT, c.lines.next_line()).await
                        .map_err(|_| anyhow!("timeout waiting for {method}"))?
                        .map_err(|e| anyhow!("read: {e}"))?;
                    let Some(line) = line else {
                        return Err(anyhow!("server closed the connection during {method}"));
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                        continue; // notification or unrelated response
                    }
                    return response_result(v);
                }
            }
            Transport::Http(_) => Err(anyhow!("http request must go through request_http")),
        }
    }

    async fn request_http(&mut self, method: &str, params: Value) -> Result<Value> {
        let Transport::Http(h) = self else {
            return Err(anyhow!("not http"));
        };
        h.next_id += 1;
        let id = h.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut req = h
            .client
            .post(&h.url)
            .headers(h.headers.clone())
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream");
        if let Some(sid) = &h.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req
            .json(&msg)
            .send()
            .await
            .map_err(|e| anyhow!("http: {e}"))?;
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            h.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("http body: {e}"))?;
        if !status.is_success() {
            return Err(anyhow!("http {status}: {}", truncate(&body, MAX_ERR_CHARS)));
        }
        // Either a plain JSON-RPC response or an SSE stream of `data:` frames.
        let candidate = if ctype.contains("text/event-stream") || body.contains("data:") {
            sse_messages(&body)
                .into_iter()
                .find(|v| v.get("id").and_then(|i| i.as_u64()) == Some(id))
        } else {
            serde_json::from_str::<Value>(&body).ok()
        };
        match candidate {
            Some(v) => response_result(v),
            None => Err(anyhow!("no response for {method}")),
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        match self {
            Transport::Stdio(c) => {
                let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
                let line = serde_json::to_string(&msg)?;
                c.stdin.write_all(line.as_bytes()).await?;
                c.stdin.write_all(b"\n").await?;
                c.stdin.flush().await?;
                Ok(())
            }
            Transport::Http(h) => {
                let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
                let mut req = h
                    .client
                    .post(&h.url)
                    .headers(h.headers.clone())
                    .header(reqwest::header::ACCEPT, "application/json, text/event-stream");
                if let Some(sid) = &h.session_id {
                    req = req.header("Mcp-Session-Id", sid);
                }
                let _ = req.json(&msg).send().await;
                Ok(())
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Http(_) => self.request_http(method, params).await,
            Transport::Stdio(_) => self.request(method, params).await,
        }
    }
}

fn response_result(v: Value) -> Result<Value> {
    if let Some(err) = v.get("error") {
        return Err(anyhow!("rpc error: {}", err));
    }
    v.get("result").cloned().ok_or_else(|| anyhow!("no result"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Parse the `data:` JSON frames out of an SSE body.
fn sse_messages(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                out.push(v);
            }
        }
    }
    out
}

// ---- connection + manager ------------------------------------------------

pub struct McpConnection {
    transport: Transport,
    tools: Vec<McpToolDesc>,
    /// Monotonic clock for the restart backoff.
    failed_until: Option<std::time::Instant>,
}

impl McpConnection {
    async fn connect(server: &McpServer) -> Result<Self> {
        let mut transport = match server.transport.as_str() {
            "http" => {
                let url = server.url.trim();
                if url.is_empty() {
                    return Err(anyhow!("http server {:?} has no url", server.name));
                }
                let mut headers = reqwest::header::HeaderMap::new();
                for (k, v) in &server.headers {
                    if let (Ok(name), Ok(val)) = (
                        reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                        reqwest::header::HeaderValue::from_str(v),
                    ) {
                        headers.insert(name, val);
                    }
                }
                for name in &server.header_secrets {
                    let account = secret_account(&server.name, name);
                    if let Some(val) = crate::hub::secret_get(&account) {
                        if let (Ok(n), Ok(v)) = (
                            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                            reqwest::header::HeaderValue::from_str(&val),
                        ) {
                            headers.insert(n, v);
                        }
                    }
                }
                let client = reqwest::Client::builder()
                    .timeout(CALL_TIMEOUT)
                    .connect_timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|e| anyhow!("http client: {e}"))?;
                Transport::Http(HttpConn {
                    client,
                    url: url.to_string(),
                    headers,
                    session_id: None,
                    next_id: 0,
                })
            }
            _ => {
                let command = server.command.trim();
                if command.is_empty() {
                    return Err(anyhow!("stdio server {:?} has no command", server.name));
                }
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(&server.args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                for (k, v) in &server.env {
                    cmd.env(k, v);
                }
                for name in &server.env_secrets {
                    let account = secret_account(&server.name, name);
                    if let Some(val) = crate::hub::secret_get(&account) {
                        cmd.env(name, val);
                    }
                }
                let mut child = cmd.spawn().map_err(|e| anyhow!("spawn {}: {e}", command))?;
                let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
                let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
                Transport::Stdio(StdioConn {
                    child,
                    stdin,
                    lines: BufReader::new(stdout).lines(),
                    next_id: 0,
                })
            }
        };

        // initialize → notifications/initialized
        let init = tokio::time::timeout(INIT_TIMEOUT, transport.call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": CLIENT_INFO, "version": env!("CARGO_PKG_VERSION") }
            }),
        ))
        .await
        .map_err(|_| anyhow!("initialize timed out"))?
        .map_err(|e| anyhow!("initialize failed: {e}"))?;
        log::debug!("mcp {:?} initialized: {}", server.name, init);
        let _ = transport
            .notify("notifications/initialized", json!({}))
            .await;

        let list = tokio::time::timeout(INIT_TIMEOUT, transport.call("tools/list", json!({})))
            .await
            .map_err(|_| anyhow!("tools/list timed out"))?
            .map_err(|e| anyhow!("tools/list failed: {e}"))?;
        let tools = parse_tools(&server.name, &list);

        Ok(Self {
            transport,
            tools,
            failed_until: None,
        })
    }

    async fn ensure(&mut self, server: &McpServer) -> Result<()> {
        if self.transport.is_alive() {
            return Ok(());
        }
        // Crashed: back off before respawning.
        if let Some(until) = self.failed_until {
            let now = std::time::Instant::now();
            if now < until {
                let wait = until.duration_since(now);
                tokio::time::sleep(wait).await;
            }
        }
        *self = Self::connect(server).await?;
        Ok(())
    }
}

fn parse_tools(server: &str, list: &Value) -> Vec<McpToolDesc> {
    let mut out = Vec::new();
    if let Some(arr) = list.get("tools").and_then(|t| t.as_array()) {
        for t in arr {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            out.push(McpToolDesc {
                qualified: format!("mcp__{server}__{name}"),
                server: server.to_string(),
                tool: name.to_string(),
                description,
                schema,
            });
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct McpToolDesc {
    pub qualified: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
}

/// Owns one connection per server and bridges MCP tools into the agent.
pub struct McpManager {
    servers: Vec<McpServer>,
    max_tools: usize,
    /// Per-`tools/call` timeout. Defaults to [`CALL_TIMEOUT`]; tests shrink it
    /// via [`McpManager::with_timeout`] so the timeout path is fast to exercise.
    timeout: Duration,
    conns: Mutex<HashMap<String, Arc<Mutex<McpConnection>>>>,
}

impl McpManager {
    pub fn new(servers: Vec<McpServer>, max_tools: usize) -> Self {
        let servers = servers.into_iter().filter(|s| s.enabled).collect();
        Self {
            servers,
            max_tools,
            timeout: CALL_TIMEOUT,
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// Same as [`McpManager::new`] but with a custom per-call timeout. Used by
    /// tests to exercise the timeout path without waiting a minute.
    #[cfg(test)]
    pub fn with_timeout(servers: Vec<McpServer>, max_tools: usize, timeout: Duration) -> Self {
        let mut m = Self::new(servers, max_tools);
        m.timeout = timeout;
        m
    }

    fn server(&self, name: &str) -> Option<&McpServer> {
        self.servers.iter().find(|s| s.name == name)
    }

    async fn connection(&self, name: &str) -> Result<Arc<Mutex<McpConnection>>> {
        if let Some(c) = self.conns.lock().await.get(name).cloned() {
            return Ok(c);
        }
        let server = self
            .server(name)
            .ok_or_else(|| anyhow!("unknown MCP server {name:?}"))?
            .clone();
        let conn = Arc::new(Mutex::new(McpConnection::connect(&server).await?));
        let mut guard = self.conns.lock().await;
        let entry = guard.entry(name.to_string()).or_insert_with(|| conn.clone());
        Ok(entry.clone())
    }

    /// Discover every tool across enabled servers, capped at `max_tools`.
    pub async fn discover(&self) -> Vec<McpToolDesc> {
        let mut out = Vec::new();
        for server in &self.servers {
            match self.connection(&server.name).await {
                Ok(conn) => {
                    let guard = conn.lock().await;
                    out.extend(guard.tools.iter().cloned());
                }
                Err(e) => log::warn!("mcp server {:?} unavailable: {e}", server.name),
            }
        }
        out.truncate(self.max_tools);
        out
    }

    async fn call(&self, server: &str, tool: &str, args: Value) -> Result<Value> {
        let cfg = self
            .server(server)
            .ok_or_else(|| anyhow!("unknown MCP server {server:?}"))?
            .clone();
        let conn = self.connection(server).await?;
        let mut guard = conn.lock().await;
        guard.ensure(&cfg).await?;
        let result = tokio::time::timeout(
            self.timeout,
            guard.transport.call(
                "tools/call",
                json!({ "name": tool, "arguments": args }),
            ),
        )
        .await
        .map_err(|_| anyhow!("mcp call timed out"))?;
        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                guard.failed_until = Some(std::time::Instant::now() + RESTART_BACKOFF);
                Err(e)
            }
        }
    }

}

fn secret_account(server: &str, name: &str) -> String {
    format!("mcp:{server}:{name}")
}

/// Bridge discovered tools into the agent's `Tool` trait, sharing *manager*.
pub fn build_tools(manager: Arc<McpManager>, descs: Vec<McpToolDesc>) -> Vec<Box<dyn Tool>> {
    descs
        .into_iter()
        .map(|desc| {
            Box::new(McpTool {
                manager: manager.clone(),
                desc,
            }) as Box<dyn Tool>
        })
        .collect()
}

/// Build the shared manager for *servers* (enabled ones only).
pub fn build_manager(servers: Vec<McpServer>, max_tools: usize) -> Arc<McpManager> {
    Arc::new(McpManager::new(servers, max_tools))
}

/// Bridge one MCP tool into the agent's `Tool` trait.
pub struct McpTool {
    manager: Arc<McpManager>,
    desc: McpToolDesc,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.desc.qualified
    }
    fn description(&self) -> &str {
        &self.desc.description
    }
    fn schema(&self) -> Value {
        self.desc.schema.clone()
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let result = self
            .manager
            .call(&self.desc.server, &self.desc.tool, args)
            .await?;
        // MCP returns content blocks; flatten text blocks into a string.
        let text = result_text(&result);
        Ok(ToolResult {
            call_id: String::new(),
            result: json!({
                "server": self.desc.server,
                "tool": self.desc.tool,
                "text": truncate(&text, MAX_OUTPUT_CHARS),
                "summary": format!("mcp {}:{} done", self.desc.server, self.desc.tool),
            }),
            is_error: false,
            error_message: None,
            image: None,
        })
    }
}

fn result_text(result: &Value) -> String {
    if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
                out.push('\n');
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    serde_json::to_string(result).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_messages() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let msgs = sse_messages(body);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], 1);
    }

    #[test]
    fn test_parse_tools_qualifies_names() {
        let list = json!({"tools": [
            {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}},
            {"name": "add"}
        ]});
        let tools = parse_tools("mock", &list);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].qualified, "mcp__mock__echo");
        assert_eq!(tools[0].server, "mock");
        assert_eq!(tools[0].tool, "echo");
    }

    #[test]
    fn test_response_result_unwraps_error() {
        assert!(response_result(json!({"error": {"code": -1, "message": "nope"}})).is_err());
        assert_eq!(
            response_result(json!({"result": {"ok": 1}})).unwrap()["ok"],
            1
        );
    }

    #[test]
    fn test_secret_account() {
        assert_eq!(secret_account("mock", "TOKEN"), "mcp:mock:TOKEN");
    }
}

#[cfg(test)]
mod mock_tests {
    //! End-to-end tests against the built-in mock MCP server (`mcp_mock`).
    //! They cover the client's full path: initialize → tools/list →
    //! tools/call, the per-call timeout, and crash-then-restart.
    //!
    //! The server runs inside this very test executable: the tests spawn
    //! `current_exe()` with a filter that selects `mcp_mock_server_entry`,
    //! which serves JSON-RPC on stdio instead of asserting. Nothing outside
    //! the test binary is needed, so this works on a clean checkout.

    use super::*;
    use crate::config::McpServer;
    use crate::events::{AgentAction, ToolCall};
    use crate::provider::{ChatChunk, ChatRequest, ChatStream, Provider};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Name of the guarded entry test below (also the libtest filter).
    const ENTRY: &str = "mcp::mock_tests::mcp_mock_server_entry";
    /// Env var the spawner sets; the entry test only serves when it is present.
    const ENV: &str = "XENON_MCP_MOCK_CHILD";

    /// Guarded entry point used as the mock server child process.
    ///
    /// Run as `current_exe --exact mcp::mock_tests::mcp_mock_server_entry
    /// --nocapture` with `XENON_MCP_MOCK_CHILD=1`, it serves JSON-RPC on
    /// stdio. In a normal `cargo test` run it returns immediately.
    #[test]
    fn mcp_mock_server_entry() {
        let spawned = std::env::var_os(ENV).is_some()
            && std::env::args().any(|a| a == "--exact")
            && std::env::args().any(|a| a == "--nocapture");
        if spawned {
            crate::mcp_mock::run();
        }
    }

    /// Spawn the mock server as a child of this test binary. No external
    /// binary is involved, so the path is always valid.
    fn mock_server() -> McpServer {
        let mut env = std::collections::BTreeMap::new();
        env.insert(ENV.to_string(), "1".to_string());
        McpServer {
            name: "mock".to_string(),
            enabled: true,
            transport: "stdio".to_string(),
            command: std::env::current_exe().unwrap().to_string_lossy().to_string(),
            args: vec![
                "--exact".to_string(),
                ENTRY.to_string(),
                "--nocapture".to_string(),
            ],
            env,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn discover_lists_mock_tools() {
        let mgr = build_manager(vec![mock_server()], 100);
        let descs = mgr.discover().await;
        let names: Vec<&str> = descs.iter().map(|d| d.qualified.as_str()).collect();
        assert!(
            names.contains(&"mcp__mock__echo"),
            "echo missing from {names:?}"
        );
        assert!(names.contains(&"mcp__mock__add"), "add missing");
        assert!(names.iter().all(|n| n.starts_with("mcp__mock__")));
    }

    #[tokio::test]
    async fn call_echo_roundtrips_text() {
        let mgr = build_manager(vec![mock_server()], 100);
        let result = mgr
            .call("mock", "echo", serde_json::json!({ "text": "hello from mcp" }))
            .await
            .expect("echo call");
        let text = result_text(&result);
        assert!(text.contains("hello from mcp"), "got {text:?}");
    }

    #[tokio::test]
    async fn call_add_returns_sum() {
        let mgr = build_manager(vec![mock_server()], 100);
        let result = mgr
            .call("mock", "add", serde_json::json!({ "a": 2, "b": 40 }))
            .await
            .expect("add call");
        assert!(result_text(&result).contains("42"), "got {result:?}");
    }

    /// A `sleep` tool longer than the manager timeout must return a timeout
    /// error well under the tool's own sleep.
    #[tokio::test]
    async fn call_times_out() {
        let mgr = McpManager::with_timeout(vec![mock_server()], 100, Duration::from_secs(1));
        let started = std::time::Instant::now();
        let err = mgr
            .call("mock", "sleep", serde_json::json!({ "secs": 30 }))
            .await
            .expect_err("sleep must time out");
        let ms = started.elapsed().as_millis();
        eprintln!("sleep call timed out in {ms} ms: {err}");
        assert!(ms < 5000, "timeout took {ms} ms");
        assert!(err.to_string().contains("timed out"), "got {err}");
    }

    /// After a server crashes, the next call respawns it and succeeds.
    #[tokio::test]
    async fn crash_then_restart_recovers() {
        let mgr = build_manager(vec![mock_server()], 100);
        // First call crashes the server process.
        let err = mgr
            .call("mock", "crash", serde_json::json!({}))
            .await
            .expect_err("crash call should fail");
        eprintln!("crash call failed as expected: {err}");
        // Give the OS a moment to reap the dead child, then call again: the
        // manager should respawn the server (after its backoff) and succeed.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let result = mgr
            .call("mock", "echo", serde_json::json!({ "text": "back online" }))
            .await
            .expect("echo after crash should reconnect");
        assert!(result_text(&result).contains("back online"));
    }

    /// End to end: the agent loop calls `mcp__mock__echo` through a scripted
    /// OpenAI-compatible provider, and the MCP result reaches the tool result.
    #[tokio::test]
    async fn agent_loop_calls_mcp_mock_echo() {
        struct ScriptedProvider {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl Provider for ScriptedProvider {
            async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
                let idx = self.calls.fetch_add(1, Ordering::Relaxed);
                let chunks = if idx == 0 {
                    // First turn: request the MCP echo tool.
                    vec![ChatChunk {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            name: "mcp__mock__echo".to_string(),
                            args: serde_json::json!({ "text": "ping via mcp" }),
                            call_id: "call_1".to_string(),
                        }],
                        finish_reason: Some("tool_calls".to_string()),
                    }]
                } else {
                    // Second turn: final answer.
                    vec![ChatChunk {
                        content: "MCP echo done".to_string(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".to_string()),
                    }]
                };
                Ok(Box::pin(futures::stream::iter(
                    chunks.into_iter().map(Ok),
                )))
            }
            async fn chat(&self, _req: ChatRequest) -> Result<String> {
                Ok(String::new())
            }
        }

        let mgr = build_manager(vec![mock_server()], 100);
        let descs = mgr.discover().await;
        assert!(!descs.is_empty(), "mock server exposed no tools");
        let tools = build_tools(mgr, descs);

        let agent = crate::agent::Agent::new(
            Arc::new(ScriptedProvider {
                calls: AtomicUsize::new(0),
            }),
            tools,
            crate::config::XenonConfig::default(),
        );
        let (tx, rx) = async_channel::bounded(64);
        agent.run_utterance("ping", tx).await.expect("turn");

        let mut saw_echo_result = false;
        while let Ok(a) = rx.try_recv() {
            if let AgentAction::ToolResult(tr) = a {
                assert!(!tr.is_error, "tool result errored: {:?}", tr.error_message);
                if tr.result["tool"].as_str() == Some("echo") {
                    let text = tr.result["text"].as_str().unwrap_or("");
                    assert!(text.contains("ping via mcp"), "got {text:?}");
                    saw_echo_result = true;
                }
            }
        }
        assert!(saw_echo_result, "no echo tool result observed");
    }
}