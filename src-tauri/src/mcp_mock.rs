//! Built-in mock MCP server, used by Xenon's tests.
//!
//! Speaks the newline-delimited JSON-RPC 2.0 subset the client in `mcp.rs`
//! uses: `initialize` → `notifications/initialized` → `tools/list` →
//! `tools/call`. It runs in-process in the test executable (tests spawn
//! `current_exe` with a guarded entry test) or via `xenon --mcp-mock-server`,
//! so no separate binary is built and nothing extra lands in release bundles.
//!
//! Tools (the tool *name* selects the behaviour, so no extra config):
//!   - `echo`  {text}      → returns the text back
//!   - `add`   {a, b}      → returns a + b
//!   - `sleep` {secs}      → sleeps `secs`, then returns (timeout tests)
//!   - `crash`             → exits the process immediately (restart tests)

use std::io::{BufRead, Write};

/// Serve JSON-RPC on stdin/stdout until the peer closes the pipe.
pub fn run() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        handle(&mut out, &msg);
    }
}

fn handle<W: Write>(out: &mut W, msg: &serde_json::Value) {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => respond(
            out,
            id,
            Ok(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock", "version": env!("CARGO_PKG_VERSION") }
            })),
        ),
        "notifications/initialized" => {} // notification: no response
        "ping" => respond(out, id, Ok(serde_json::json!({}))),
        "tools/list" => respond(
            out,
            id,
            Ok(serde_json::json!({ "tools": [
                tool("echo", "Echo text back", "text"),
                tool("add", "Add two numbers", "a"),
                tool("sleep", "Sleep then return", "secs"),
                {
                    "name": "crash",
                    "description": "Exit the process",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ] })),
        ),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(serde_json::json!({}));
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match name {
                "echo" => {
                    let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    respond(out, id, Ok(text_block(text)));
                }
                "add" => {
                    let a = args.get("a").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let b = args.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    respond(out, id, Ok(text_block(&fmt_num(a + b))));
                }
                "sleep" => {
                    let secs = args.get("secs").and_then(|v| v.as_u64()).unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    respond(out, id, Ok(text_block("slept")));
                }
                "crash" => std::process::exit(1),
                other => respond(
                    out,
                    id,
                    Err(serde_json::json!({ "code": -32601, "message": format!("unknown tool {other}") })),
                ),
            }
        }
        other => respond(
            out,
            id,
            Err(serde_json::json!({ "code": -32601, "message": format!("unknown method {other}") })),
        ),
    }
}

/// A tool with a single required property (`name`).
fn tool(name: &str, description: &str, prop: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": { prop: { "type": if prop == "secs" { "number" } else { "string" } } },
            "required": [prop]
        }
    })
}

fn text_block(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Write one JSON-RPC response (skip notifications, which carry no id).
fn respond<W: Write>(
    out: &mut W,
    id: Option<serde_json::Value>,
    result: Result<serde_json::Value, serde_json::Value>,
) {
    let msg = match (id, result) {
        (Some(id), Ok(result)) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        (Some(id), Err(error)) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        (None, _) => return,
    };
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}