//! Tauri commands backing Settings → Tools & MCP: server status/test, import
//! from installed apps, and writing MCP/search secrets to the Keychain.

use serde::Serialize;
use tauri::State;

use crate::config::{McpServer, XenonConfig};
use crate::ui::ArcRwLockConfig;

#[derive(Debug, Clone, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub enabled: bool,
    pub transport: String,
    pub source: String,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpTestResult {
    pub ok: bool,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportCandidateDto {
    pub source: String,
    pub source_label: String,
    pub name: String,
    pub transport: String,
    pub detail: String,
    pub safe: bool,
    pub server: McpServer,
}

async fn probe(server: &McpServer) -> (usize, Vec<String>, Option<String>) {
    let manager = crate::mcp::build_manager(vec![server.clone()], 100);
    let descs = manager.discover().await;
    let tools: Vec<String> = descs.iter().map(|d| d.tool.clone()).collect();
    if descs.is_empty() {
        return (0, tools, Some("no tools (server unreachable or returned none)".into()));
    }
    (descs.len(), tools, None)
}

/// Status of every configured MCP server (spawns each enabled server briefly).
#[tauri::command]
pub async fn mcp_status(config: State<'_, ArcRwLockConfig>) -> Result<Vec<McpServerStatus>, String> {
    let servers = {
        let guard = config.read().await;
        guard.mcp_servers.clone()
    };
    let mut out = Vec::new();
    for server in &servers {
        let (tool_count, tools, error) = probe(server).await;
        out.push(McpServerStatus {
            name: server.name.clone(),
            enabled: server.enabled,
            transport: server.transport.clone(),
            source: server.source.clone(),
            tool_count,
            tools,
            status: if error.is_some() { "error".into() } else { "ok".into() },
            error,
        });
    }
    Ok(out)
}

/// Test one server definition without saving it.
#[tauri::command]
pub async fn mcp_test(server: McpServer) -> McpTestResult {
    let (tool_count, tools, error) = probe(&server).await;
    McpTestResult {
        ok: error.is_none(),
        tool_count,
        tools,
        error,
    }
}

/// MCP servers discovered (read-only) in other installed apps.
#[tauri::command]
pub fn mcp_import_candidates() -> Vec<ImportCandidateDto> {
    let label = |s: &str| {
        crate::mcp_import::sources()
            .into_iter()
            .find(|(id, _)| *id == s)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| s.to_string())
    };
    crate::mcp_import::discover()
        .into_iter()
        .map(|c| {
            let detail = if c.server.transport == "http" {
                c.server.url.clone()
            } else {
                format!("{} {}", c.server.command, c.server.args.join(" ")).trim().to_string()
            };
            let source_label = label(&c.source);
            ImportCandidateDto {
                source: c.source.clone(),
                source_label,
                name: c.server.name.clone(),
                transport: c.server.transport.clone(),
                detail,
                safe: c.safe,
                server: c.server,
            }
        })
        .collect()
}

/// Import the given servers (secrets moved to the Keychain) and persist them.
#[tauri::command]
pub async fn mcp_import_servers(
    config: State<'_, ArcRwLockConfig>,
    servers: Vec<McpServer>,
) -> Result<usize, String> {
    let mut guard = config.write().await;
    let mut added = 0;
    for mut server in servers {
        // Move secret env/header values into the Keychain, never config.json.
        for name in &server.env_secrets {
            if let Some(val) = server.env.remove(name) {
                let account = format!("mcp:{}:{}", server.name, name);
                if crate::hub::secret_set(&account, &val) {
                    guard.unpersisted_secrets.remove(&account);
                } else {
                    // Could not persist: keep the value in the server env so a
                    // later save writes it (never silently lost).
                    guard.unpersisted_secrets.insert(account);
                    server.env.insert(name.clone(), val);
                }
            }
        }
        for name in &server.header_secrets {
            if let Some(val) = server.headers.remove(name) {
                let account = format!("mcp:{}:{}", server.name, name);
                if crate::hub::secret_set(&account, &val) {
                    guard.unpersisted_secrets.remove(&account);
                } else {
                    guard.unpersisted_secrets.insert(account);
                    server.headers.insert(name.clone(), val);
                }
            }
        }
        if guard.mcp_servers.iter().any(|s| s.name == server.name) {
            continue; // already present; don't clobber
        }
        guard.mcp_servers.push(server);
        added += 1;
    }
    guard.save().map_err(|e| e.to_string())?;
    Ok(added)
}

/// Remove one MCP server (and its Keychain secrets).
#[tauri::command]
pub async fn mcp_remove_server(
    config: State<'_, ArcRwLockConfig>,
    name: String,
) -> Result<(), String> {
    let mut guard = config.write().await;
    if let Some(server) = guard.mcp_servers.iter().find(|s| s.name == name) {
        let accounts: Vec<String> = server
            .env_secrets
            .iter()
            .chain(server.header_secrets.iter())
            .map(|key| format!("mcp:{name}:{key}"))
            .collect();
        for account in &accounts {
            crate::hub::secret_delete(account);
            guard.unpersisted_secrets.remove(account);
        }
    }
    guard.mcp_servers.retain(|s| s.name != name);
    guard.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// Set (or clear) a secret env var / header value for an MCP server.
#[tauri::command]
pub async fn mcp_set_secret(
    config: State<'_, ArcRwLockConfig>,
    server: String,
    name: String,
    value: String,
) -> Result<(), String> {
    let account = format!("mcp:{server}:{name}");
    let mut guard = config.write().await;
    if value.trim().is_empty() {
        crate::hub::secret_delete(&account);
        guard.unpersisted_secrets.remove(&account);
        // Drop any value a prior failed write parked in the server env/headers,
        // otherwise the stdio/http transport would keep sending the cleared
        // credential to the child process / request.
        if let Some(s) = guard.mcp_servers.iter_mut().find(|s| s.name == server) {
            s.env.remove(&name);
            s.headers.remove(&name);
        }
    } else if crate::hub::secret_set(&account, &value) {
        guard.unpersisted_secrets.remove(&account);
        // Now durable: if a prior failure parked it in env or headers, drop it.
        if let Some(s) = guard.mcp_servers.iter_mut().find(|s| s.name == server) {
            s.env.remove(&name);
            s.headers.remove(&name);
        }
    } else {
        // Could not persist: park the value where the transport will read it.
        // Header secrets are read from `headers` (HTTP transport ignores env);
        // everything else is read from `env`.
        guard.unpersisted_secrets.insert(account.clone());
        if let Some(s) = guard.mcp_servers.iter_mut().find(|s| s.name == server) {
            if s.header_secrets.contains(&name) {
                s.headers.insert(name.clone(), value);
            } else {
                s.env.insert(name.clone(), value);
            }
        }
    }
    // Ensure the name is registered so the client resolves it.
    if let Some(s) = guard.mcp_servers.iter_mut().find(|s| s.name == server) {
        if !s.env_secrets.contains(&name) && !s.header_secrets.contains(&name) {
            s.env_secrets.push(name);
        }
    }
    guard.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// Set a built-in search API key ("brave" | "tavily" | "serper") in the
/// Keychain and mirror the fact on the config.
#[tauri::command]
pub async fn set_search_key(
    config: State<'_, ArcRwLockConfig>,
    kind: String,
    value: String,
) -> Result<(), String> {
    let account = format!("search:{kind}");
    let present = !value.trim().is_empty();
    if present {
        crate::hub::secret_set(&account, value.trim());
    } else {
        crate::hub::secret_delete(&account);
    }
    let mut guard = config.write().await;
    guard.unpersisted_secrets.remove(&account);
    match kind.as_str() {
        "brave" => guard.brave_key_set = present,
        "tavily" => guard.tavily_key_set = present,
        "serper" => guard.serper_key_set = present,
        _ => {}
    }
    guard.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// Convenience for the Settings page: the full config (secrets already
/// stripped by `without_secrets`).
#[tauri::command]
pub async fn tools_config(config: State<'_, ArcRwLockConfig>) -> Result<XenonConfig, String> {
    Ok(config.read().await.clone())
}