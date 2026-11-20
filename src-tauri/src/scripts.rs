//! `run_script`: AppleScript or shell, off by default, and always gated by an
//! explicit confirmation dialog that shows the exact code. Never auto-confirmed.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::Arc;

use crate::events::ToolResult;
use crate::tools::{attach_verification, Tool, ToolContext};

/// Run one command through the OS and return its combined output.
fn run_program(program: &str, args: &[&str]) -> Result<(bool, String)> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to run {program}: {e}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), text))
}

/// Ask the user to approve `code` in a native dialog. `Ok(true)` = Run.
fn confirm(code: &str) -> Result<bool> {
    let escaped = serde_json::to_string(code)?;
    let script = format!(
        "set r to display dialog \"Xenon wants to run this code:\n\n{code}\n\n\
         Only click Run if you asked Xenon to do this.\" buttons {{\"Cancel\", \"Run\"}} \
         default button \"Cancel\" with title \"Xenon: run script?\" with icon caution\n\
         return button returned of r",
        code = escaped.trim_matches('"'),
    );
    let out = run_program("osascript", &["-e", &script])?;
    if !out.0 {
        return Err(anyhow!("confirmation dialog failed: {}", out.1.trim()));
    }
    Ok(out.1.contains("Run"))
}

pub struct RunScriptTool {
    allow: Arc<std::sync::atomic::AtomicBool>,
}

impl RunScriptTool {
    pub fn new(_ctx: ToolContext, allow_scripts: bool) -> Self {
        Self {
            allow: Arc::new(std::sync::atomic::AtomicBool::new(allow_scripts)),
        }
    }
}

#[async_trait]
impl Tool for RunScriptTool {
    fn name(&self) -> &str {
        "run_script"
    }

    fn description(&self) -> &str {
        "Run AppleScript or shell code. Asks the user to confirm every single call. \
         Only offer this when the task truly needs it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["applescript", "shell"],
                    "description": "Language of the code."
                },
                "code": { "type": "string", "description": "The exact code to run." }
            },
            "required": ["kind", "code"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        if !self.allow.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(anyhow!("run_script is disabled (Settings → Agent → allow_scripts)"));
        }
        let kind = args["kind"].as_str().unwrap_or("").to_string();
        let code = args["code"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing 'code'"))?
            .to_string();
        if kind != "applescript" && kind != "shell" {
            return Err(anyhow!("invalid kind: {kind:?} (applescript or shell)"));
        }

        let approved = tokio::task::spawn_blocking({
            let code = code.clone();
            move || confirm(&code)
        })
        .await
        .map_err(|e| anyhow!("confirmation dialog crashed: {e}"))??;
        if !approved {
            return Err(anyhow!("the user declined to run the script"));
        }

        let run_kind = kind.clone();
        let result = tokio::task::spawn_blocking(move || match run_kind.as_str() {
            "applescript" => run_program("osascript", &["-e", &code]),
            _ => run_program("/bin/zsh", &["-c", &code]),
        })
        .await
        .map_err(|e| anyhow!("script task crashed: {e}"))??;

        let (ok, output) = result;
        let summary = if ok {
            format!("Ran {kind} code.")
        } else {
            format!("{kind} code failed.")
        };
        let mut tr = attach_verification(json!({
            "kind": kind,
            "output": truncate(&output, 4000),
            "success": ok,
            "summary": summary,
        }));
        if !ok {
            tr.is_error = true;
            tr.error_message = Some("script exited with a non-zero status".to_string());
        }
        Ok(tr)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use std::sync::{Arc, Mutex};

    fn ctx() -> ToolContext {
        ToolContext {
            app: None,
            provider: None,
            vision_model: "vl".to_string(),
            vision: false,
            notes: Arc::new(Mutex::new(Vec::new())),
            allow_scripts: false,
            browser_eval: false,
        }
    }

    #[tokio::test]
    async fn test_run_script_disabled_by_default() {
        let tool = RunScriptTool::new(ctx(), false);
        assert!(tool
            .execute(json!({ "kind": "shell", "code": "echo hi" }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_run_script_requires_code_and_kind() {
        let tool = RunScriptTool::new(ctx(), true);
        assert!(tool.execute(json!({ "kind": "shell" })).await.is_err());
        assert!(tool.execute(json!({ "kind": "perl", "code": "x" })).await.is_err());
    }

    #[test]
    fn test_run_program_captures_output() {
        let (ok, out) = run_program("/bin/echo", &["hello"]).unwrap();
        assert!(ok);
        assert!(out.contains("hello"));
    }
}