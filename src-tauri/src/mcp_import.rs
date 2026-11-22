//! Detect and import MCP servers from other apps already installed on this
//! Mac — read-only. Nothing is written to another app's config; Xenon copies
//! the server definitions into its own config.
//!
//! Each parser is a pure `&str -> Vec<McpServer>` function so it can be
//! unit-tested with fixture JSON. Env values whose *name* looks secret
//! (`*_TOKEN`, `*_KEY`, `*_SECRET`, `*_PASSWORD`) are listed in `env_secrets`
//! so the importer can move them into the Keychain instead of config.json.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::config::McpServer;

/// One server offered by an installed app. `safe` pre-checks the import box:
/// only obviously-safe definitions (absolute command, no args, no env).
#[derive(Debug, Clone)]
pub struct ImportCandidate {
    pub source: String,
    pub server: McpServer,
    pub safe: bool,
}

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["token", "key", "secret", "password", "passwd", "credential", "authorization"]
        .iter()
        .any(|s| n.contains(s))
}

fn parse_env(map: &Value) -> (BTreeMap<String, String>, Vec<String>) {
    let mut env = BTreeMap::new();
    let mut secrets = Vec::new();
    if let Some(obj) = map.as_object() {
        for (k, v) in obj {
            let Some(val) = v.as_str() else { continue };
            if is_secret_name(k) {
                env.insert(k.clone(), val.to_string());
                secrets.push(k.clone());
            } else {
                env.insert(k.clone(), val.to_string());
            }
        }
    }
    (env, secrets)
}

fn parse_headers(map: &Value) -> (BTreeMap<String, String>, Vec<String>) {
    let mut headers = BTreeMap::new();
    let mut secrets = Vec::new();
    if let Some(obj) = map.as_object() {
        for (k, v) in obj {
            let Some(val) = v.as_str() else { continue };
            if is_secret_name(k) {
                headers.insert(k.clone(), val.to_string());
                secrets.push(k.clone());
            } else {
                headers.insert(k.clone(), val.to_string());
            }
        }
    }
    (headers, secrets)
}

/// Convert one server entry (the value under a server name) into an `McpServer`.
/// Understands the common shapes:
/// - `{ "command": "…", "args": […], "env": {…} }` (Claude Desktop / Cursor)
/// - `{ "type": "stdio", … }` (explicit)
/// - `{ "type": "local", "command": ["npx","pkg"], "environment": {…} }` (OpenCode)
/// - `{ "type": "remote" | "http" | "sse", "url": "…", "headers": {…} }`
pub fn entry_to_server(name: &str, v: &Value, source: &str) -> Option<McpServer> {
    let obj = v.as_object()?;
    let kind = obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut server = McpServer {
        name: name.to_string(),
        source: source.to_string(),
        ..Default::default()
    };

    match kind.as_str() {
        "remote" | "http" | "sse" | "streamable-http" => {
            server.transport = "http".to_string();
            server.url = obj.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
            let (headers, header_secrets) = parse_headers(obj.get("headers").unwrap_or(&Value::Null));
            server.headers = headers;
            server.header_secrets = header_secrets;
        }
        "local" | "stdio" | "" => {
            // OpenCode stores the command as an array: ["npx", "-y", "pkg"].
            if let Some(arr) = obj.get("command").and_then(|c| c.as_array()) {
                let parts: Vec<String> = arr
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect();
                if parts.is_empty() {
                    return None;
                }
                server.command = parts[0].clone();
                server.args = parts[1..].to_vec();
            } else {
                server.command = obj
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(args) = obj.get("args").and_then(|a| a.as_array()) {
                    server.args = args
                        .iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect();
                }
            }
            let env_val = obj.get("env").or_else(|| obj.get("environment"));
            let (env, env_secrets) = parse_env(env_val.unwrap_or(&Value::Null));
            server.env = env;
            server.env_secrets = env_secrets;
            if server.command.trim().is_empty() {
                return None;
            }
        }
        _ => return None,
    }
    Some(server)
}

/// Find the `mcpServers` / `mcp.servers` / `servers` object inside a config and
/// parse every entry.
pub fn parse_servers(root: &Value, source: &str) -> Vec<McpServer> {
    let mut out = Vec::new();
    // OpenCode nests servers directly under `mcp`; VS Code nests them under
    // `mcp.servers`. Add the bare `mcp` object too and drop the bogus
    // "servers" entry when it appears (it has no command/url).
    let containers = [
        root.get("mcpServers").and_then(|v| v.as_object()),
        root.get("mcp")
            .and_then(|m| m.get("servers"))
            .and_then(|v| v.as_object()),
        root.get("mcp").and_then(|v| v.as_object()),
        root.get("servers").and_then(|v| v.as_object()),
    ];
    for container in containers.into_iter().flatten() {
        for (name, v) in container {
            if let Some(server) = entry_to_server(name, v, source) {
                out.push(server);
            }
        }
    }
    // De-duplicate by name (a config may list a server in two places).
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.name.clone()));
    out
}

// ---- per-app parsers -----------------------------------------------------

pub fn parse_claude_desktop(json: &str) -> Vec<McpServer> {
    parse_value(json, "claude-desktop")
}
pub fn parse_claude_code(json: &str) -> Vec<McpServer> {
    parse_value(json, "claude-code")
}
pub fn parse_cursor(json: &str) -> Vec<McpServer> {
    parse_value(json, "cursor")
}
pub fn parse_windsurf(json: &str) -> Vec<McpServer> {
    parse_value(json, "windsurf")
}
pub fn parse_vscode(json: &str) -> Vec<McpServer> {
    // VS Code settings.json is JSONC (comments); try strict, then stripped.
    let root = match serde_json::from_str::<Value>(json) {
        Ok(v) => v,
        Err(_) => match serde_json::from_str::<Value>(&strip_jsonc(json)) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        },
    };
    parse_servers(&root, "vscode")
}
pub fn parse_opencode(json: &str) -> Vec<McpServer> {
    parse_value(json, "opencode")
}

fn parse_value(json: &str, source: &str) -> Vec<McpServer> {
    match serde_json::from_str::<Value>(json) {
        Ok(v) => parse_servers(&v, source),
        Err(_) => Vec::new(),
    }
}

/// Naive JSONC → JSON: drop `//` line comments and `/* */` blocks that are not
/// inside a string. Good enough for settings.json written by VS Code.
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                Some('*') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

// ---- discovery -----------------------------------------------------------

struct AppConfig {
    source: &'static str,
    path: PathBuf,
    parser: fn(&str) -> Vec<McpServer>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Every supported app config that exists on this machine.
fn app_configs() -> Vec<AppConfig> {
    let h = home();
    vec![
        AppConfig {
            source: "claude-desktop",
            path: h.join("Library/Application Support/Claude/claude_desktop_config.json"),
            parser: parse_claude_desktop,
        },
        AppConfig {
            source: "claude-code",
            path: h.join(".claude.json"),
            parser: parse_claude_code,
        },
        AppConfig {
            source: "claude-code",
            path: h.join(".claude/settings.json"),
            parser: parse_claude_code,
        },
        AppConfig {
            source: "cursor",
            path: h.join(".cursor/mcp.json"),
            parser: parse_cursor,
        },
        AppConfig {
            source: "windsurf",
            path: h.join(".codeium/windsurf/mcp_config.json"),
            parser: parse_windsurf,
        },
        AppConfig {
            source: "vscode",
            path: h.join("Library/Application Support/Code/User/settings.json"),
            parser: parse_vscode,
        },
        AppConfig {
            source: "opencode",
            path: h.join(".config/opencode/opencode.json"),
            parser: parse_opencode,
        },
    ]
}

/// Read every supported app config (read-only) and return import candidates.
pub fn discover() -> Vec<ImportCandidate> {
    let mut out: Vec<ImportCandidate> = Vec::new();
    for app in app_configs() {
        let Ok(text) = std::fs::read_to_string(&app.path) else {
            continue;
        };
        for server in (app.parser)(&text) {
            let safe = server.transport == "stdio"
                && server.args.is_empty()
                && server.env.is_empty()
                && server.command.starts_with('/');
            out.push(ImportCandidate {
                source: app.source.to_string(),
                server,
                safe,
            });
        }
    }
    // De-duplicate by (source, name).
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert((c.source.clone(), c.server.name.clone())));
    out
}

/// The sources Xenon can import from, for the Settings UI labels.
pub fn sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-desktop", "Claude Desktop"),
        ("claude-code", "Claude Code"),
        ("cursor", "Cursor"),
        ("windsurf", "Windsurf"),
        ("vscode", "VS Code"),
        ("opencode", "OpenCode"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_claude_desktop_fixture() {
        let json = r#"{
            "mcpServers": {
                "files": { "command": "/usr/bin/cat", "args": [], "env": {} },
                "remote": { "type": "http", "url": "https://x/mcp", "headers": { "Authorization": "Bearer t" } }
            }
        }"#;
        let servers = parse_claude_desktop(json);
        assert_eq!(servers.len(), 2);
        let files = servers.iter().find(|s| s.name == "files").unwrap();
        assert_eq!(files.command, "/usr/bin/cat");
        assert_eq!(files.transport, "stdio");
        let remote = servers.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url, "https://x/mcp");
        assert!(remote.header_secrets.contains(&"Authorization".to_string()));
    }

    #[test]
    fn test_parse_cursor_fixture() {
        let json = r#"{"mcpServers": {"fs": {"command": "npx", "args": ["-y","server"], "env": {"TOKEN":"abc"}}}}"#;
        let servers = parse_cursor(json);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].command, "npx");
        assert_eq!(servers[0].args, vec!["-y", "server"]);
        assert!(servers[0].env_secrets.contains(&"TOKEN".to_string()));
    }

    #[test]
    fn test_parse_opencode_local_array_command() {
        let json = r#"{"mcp": {"weather": {"type": "local", "command": ["npx","-y","weather-mcp"], "environment": {"API_KEY":"k"}}}}"#;
        let servers = parse_opencode(json);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].command, "npx");
        assert_eq!(servers[0].args, vec!["-y", "weather-mcp"]);
        assert!(servers[0].env_secrets.contains(&"API_KEY".to_string()));
    }

    #[test]
    fn test_parse_opencode_remote() {
        let json = r#"{"mcp": {"r": {"type": "remote", "url": "https://h/mcp", "headers": {}}}}"#;
        let servers = parse_opencode(json);
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[0].url, "https://h/mcp");
    }

    #[test]
    fn test_parse_vscode_jsonc() {
        let json = r#"{
            // line comment
            "mcp": { "servers": { "s": { "command": "node", "args": ["s.js"] } } }
        }"#;
        let servers = parse_vscode(json);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].command, "node");
    }

    #[test]
    fn test_parse_claude_code_fixture() {
        let json = r#"{"mcpServers": {"x": {"command": "/bin/echo"}}}"#;
        assert_eq!(parse_claude_code(json).len(), 1);
    }

    #[test]
    fn test_garbage_is_empty() {
        assert!(parse_cursor("not json").is_empty());
        assert!(parse_claude_desktop("{}").is_empty());
    }

    #[test]
    fn test_secret_name_detection() {
        assert!(is_secret_name("GITHUB_TOKEN"));
        assert!(is_secret_name("api_key"));
        assert!(is_secret_name("DB_PASSWORD"));
        assert!(!is_secret_name("HOME"));
        assert!(!is_secret_name("PATH"));
    }
}