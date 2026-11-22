//! Integration test: spawn the mock MCP server from this very test binary
//! (`current_exe --exact mcp_mock_server_entry --nocapture` with
//! `XENON_MCP_MOCK_CHILD=1`) and speak the stdio JSON-RPC protocol directly.
//! No external binary is built or bundled, so this works on a clean checkout.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Name of the guarded entry test (no module prefix in an integration test).
const ENTRY: &str = "mcp_mock_server_entry";
/// Env var the spawner sets; the entry test only serves when it is present.
const ENV: &str = "XENON_MCP_MOCK_CHILD";

/// Guarded entry point used as the mock server child process. Returns
/// immediately during a normal `cargo test` run.
#[test]
fn mcp_mock_server_entry() {
    let spawned = std::env::var_os(ENV).is_some()
        && std::env::args().any(|a| a == "--exact")
        && std::env::args().any(|a| a == "--nocapture");
    if spawned {
        xenon_lib::mcp_mock::run();
    }
}

fn spawn() -> (
    std::process::Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
) {
    let mut child = Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", ENTRY, "--nocapture"])
        .env(ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock server");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    (child, stdin, BufReader::new(stdout))
}

fn send(stdin: &mut std::process::ChildStdin, value: serde_json::Value) {
    let line = serde_json::to_string(&value).unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
}

/// Read the next JSON-RPC message, skipping libtest's banner/progress lines.
fn recv(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read response");
        assert!(n > 0, "mock server closed stdout");
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            return v;
        }
    }
}

fn id_is(v: &serde_json::Value, id: u64) -> bool {
    v.get("id").and_then(|i| i.as_u64()) == Some(id)
}

#[test]
fn initialize_then_list_then_call_echo() {
    let (mut child, mut stdin, mut stdout) = spawn();

    // initialize
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }}),
    );
    let resp = recv(&mut stdout);
    assert!(id_is(&resp, 1), "initialize response id: {resp}");
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["serverInfo"]["name"], "mock");

    // notifications/initialized (no response expected)
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    );

    // tools/list
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let resp = recv(&mut stdout);
    assert!(id_is(&resp, 2));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"echo"), "tools: {names:?}");
    assert!(names.contains(&"add"));

    // tools/call echo
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "echo", "arguments": {"text": "hello"}
        }}),
    );
    let resp = recv(&mut stdout);
    assert!(id_is(&resp, 3));
    let content = resp["result"]["content"].as_array().expect("content");
    assert_eq!(content[0]["text"], "hello");

    // tools/call add
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "add", "arguments": {"a": 2, "b": 40}
        }}),
    );
    let resp = recv(&mut stdout);
    assert_eq!(resp["result"]["content"][0]["text"], "42");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn sleep_tool_blocks_until_done() {
    let (mut child, mut stdin, mut stdout) = spawn();
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }}),
    );
    let _ = recv(&mut stdout);

    let started = std::time::Instant::now();
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "sleep", "arguments": {"secs": 1}
        }}),
    );
    let resp = recv(&mut stdout);
    let elapsed = started.elapsed();
    assert!(id_is(&resp, 2));
    assert_eq!(resp["result"]["content"][0]["text"], "slept");
    assert!(
        elapsed >= Duration::from_millis(900),
        "returned too soon: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(5), "took too long: {elapsed:?}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn crash_tool_exits_process() {
    let (mut child, mut stdin, mut stdout) = spawn();
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }}),
    );
    let _ = recv(&mut stdout);

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "crash", "arguments": {}
        }}),
    );
    // No response: the process exits. Wait for it.
    let status = child.wait().expect("wait for exit");
    assert!(!status.success(), "crash tool should exit non-zero: {status}");
}

#[test]
fn unknown_tool_returns_jsonrpc_error() {
    let (mut child, mut stdin, mut stdout) = spawn();
    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }}),
    );
    let _ = recv(&mut stdout);

    send(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "nope", "arguments": {}
        }}),
    );
    let resp = recv(&mut stdout);
    assert!(resp.get("error").is_some(), "expected error, got {resp}");
    assert_eq!(resp["error"]["code"], -32601);

    let _ = child.kill();
    let _ = child.wait();
}