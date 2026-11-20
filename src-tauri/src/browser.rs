//! Browser tools driven through AppleScript (Safari + Chromium browsers:
//! Chrome, Chromium, Arc, Brave, Edge).
//!
//! Reading the page and clicking by selector needs "Allow JavaScript from
//! Apple Events" (Safari: Develop menu; Chrome: View > Developer > Allow
//! JavaScript from Apple Events). When that is off, AppleScript answers with a
//! JavaScript error; we turn it into a readable notice and the agent falls
//! back to vision-only control.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Command;

use crate::events::ToolResult;
use crate::tools::{attach_verification, Tool, ToolContext};

/// The browsers we know how to drive, and whether they are Chromium-based.
const SUPPORTED: &[(&str, bool)] = &[
    ("Safari", false),
    ("Google Chrome", true),
    ("Chromium", true),
    ("Arc", true),
    ("Brave Browser", true),
    ("Microsoft Edge", true),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Safari,
    Chromium,
}

#[derive(Debug, Clone)]
pub struct Browser {
    pub name: &'static str,
    pub kind: Kind,
}

impl Browser {
    fn is_running(&self) -> bool {
        osascript(&format!("application \"{}\" is running", self.name))
            .map(|s| s.trim() == "true")
            .unwrap_or(false)
    }

    fn activate(&self) {
        let _ = osascript(&format!("tell application \"{}\" to activate", self.name));
    }
}

fn osascript(script: &str) -> Result<String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| anyhow!("failed to run osascript: {e}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "osascript failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The frontmost app if it is a browser we can drive.
pub fn frontmost_browser() -> Option<Browser> {
    let front = osascript(
        r#"tell application "System Events" to get name of first process whose frontmost is true"#,
    )
    .ok()?;
    supported().into_iter().find(|b| b.name == front)
}

/// First running supported browser (fallback when the frontmost app is not one,
/// or when System Events is blocked by Accessibility permissions).
pub fn any_running_browser() -> Option<Browser> {
    supported().into_iter().find(|b| b.is_running())
}

pub fn detect_browser() -> Option<Browser> {
    frontmost_browser().or_else(any_running_browser)
}

fn supported() -> Vec<Browser> {
    SUPPORTED
        .iter()
        .map(|(name, chromium)| Browser {
            name,
            kind: if *chromium { Kind::Chromium } else { Kind::Safari },
        })
        .collect()
}

const JS_PERMISSION_HINT: &str = "JavaScript from Apple Events is off. Enable it with: \
Safari → Develop menu → Allow JavaScript from Apple Events; Chrome/Arc/Brave → View → \
Developer → Allow JavaScript from Apple Events. Then ask me again, or let me use the \
vision tools instead.";

fn is_js_permission_error(err: &anyhow::Error) -> bool {
    let m = err.to_string().to_ascii_lowercase();
    m.contains("javascript")
        || m.contains("-1743") // not authorized
        || m.contains("access is denied")
        || m.contains("isn't allowed")
}

impl Browser {
    /// Ask the browser for `url` + `title` via native AppleScript properties.
    fn read_url_title(&self) -> Result<(String, String)> {
        let script = match self.kind {
            Kind::Safari => "tell application \"Safari\" to return (URL of front document) & linefeed & (name of front document)".to_string(),
            Kind::Chromium => format!(
                "tell application \"{}\" to return (URL of active tab of front window) & linefeed & (title of active tab of front window)",
                self.name
            ),
        };
        let out = osascript(&script)?;
        let mut lines = out.splitn(2, '\n');
        let url = lines.next().unwrap_or("").trim().to_string();
        let title = lines.next().unwrap_or("").trim().to_string();
        Ok((url, title))
    }

    /// Evaluate JavaScript in the front document and return its JSON result.
    fn eval_js(&self, js: &str) -> Result<String> {
        let literal = serde_json::to_string(js)?;
        let script = match self.kind {
            Kind::Safari => {
                format!("tell application \"Safari\" to do JavaScript {literal} in front document")
            }
            Kind::Chromium => format!(
                "tell application \"{}\" to execute front window's active tab javascript {literal}",
                self.name
            ),
        };
        osascript(&script)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Read,
    Click,
    Fill,
    Eval,
}

pub struct BrowserTool {
    ctx: ToolContext,
    action: Action,
}

impl BrowserTool {
    pub fn new(ctx: ToolContext, action: Action) -> Self {
        Self { ctx, action }
    }

    fn notice(&self, msg: &str) {
        if let Some(app) = &self.ctx.app {
            use tauri::Emitter;
            let _ = app.emit("xenon-notice", msg.to_string());
        }
    }

    fn browser(&self) -> Result<Browser> {
        detect_browser().ok_or_else(|| {
            anyhow!(
                "no supported browser is running (Safari, Chrome, Chromium, Arc, Brave, Edge)"
            )
        })
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        match self.action {
            Action::Open => "browser_open",
            Action::Read => "browser_read",
            Action::Click => "browser_click",
            Action::Fill => "browser_fill",
            Action::Eval => "browser_eval",
        }
    }

    fn description(&self) -> &str {
        match self.action {
            Action::Open => "Open a URL in the default/frontmost browser.",
            Action::Read => {
                "Read the current page: URL, title and visible text (up to 6000 chars)."
            }
            Action::Click => {
                "Click an element on the page by CSS selector, or by visible text if the \
                 selector does not match."
            }
            Action::Fill => "Type text into an input or contenteditable element by CSS selector.",
            Action::Eval => "Run JavaScript in the page and return its result (off by default).",
        }
    }

    fn schema(&self) -> Value {
        match self.action {
            Action::Open => json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Full URL, e.g. https://example.com" }
                },
                "required": ["url"]
            }),
            Action::Read => json!({ "type": "object", "properties": {}, "required": [] }),
            Action::Click => json!({
                "type": "object",
                "properties": {
                    "css_or_text": {
                        "type": "string",
                        "description": "CSS selector (\"button.ok\") or visible text (\"Sign in\")."
                    }
                },
                "required": ["css_or_text"]
            }),
            Action::Fill => json!({
                "type": "object",
                "properties": {
                    "css": { "type": "string", "description": "CSS selector of the field." },
                    "text": { "type": "string", "description": "Text to type." }
                },
                "required": ["css", "text"]
            }),
            Action::Eval => json!({
                "type": "object",
                "properties": {
                    "js": { "type": "string", "description": "JavaScript expression to evaluate." }
                },
                "required": ["js"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let browser = self.browser()?;
        browser.activate();

        match self.action {
            Action::Open => {
                let url = args["url"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("missing 'url'"))?;
                let output = Command::new("open")
                    .arg("-a")
                    .arg(browser.name)
                    .arg(url)
                    .output()
                    .map_err(|e| anyhow!("failed to run 'open': {e}"))?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "failed to open {url:?}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                let summary = format!("Opened {url} in {}.", browser.name);
                Ok(attach_verification(json!({
                    "browser": browser.name,
                    "url": url,
                    "summary": summary,
                })))
            }
            Action::Read => {
                let (url, title) = browser.read_url_title()?;
                let text = match browser.eval_js(
                    "JSON.stringify({t: (document.body ? document.body.innerText : '')})",
                ) {
                    Ok(raw) => {
                        let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                        let t = parsed["t"].as_str().unwrap_or("").to_string();
                        truncate_chars(&t, 6000)
                    }
                    Err(e) => {
                        let msg = if is_js_permission_error(&e) {
                            JS_PERMISSION_HINT.to_string()
                        } else {
                            format!("could not read the page text: {e}")
                        };
                        self.notice(&msg);
                        format!("(text unavailable: {msg})")
                    }
                };
                Ok(attach_verification(json!({
                    "browser": browser.name,
                    "url": url,
                    "title": title,
                    "text": text,
                    "summary": format!("Read {}: {}", browser.name, title),
                })))
            }
            Action::Click => {
                let selector = args["css_or_text"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("missing 'css_or_text'"))?;
                let lit = serde_json::to_string(selector)?;
                let js = format!(
                    "(function(){{\
                       var q={lit};\
                       var el=document.querySelector(q);\
                       if(!el){{\
                         var all=document.querySelectorAll('a,button,input,[role=button],div,span,p,li,h1,h2,h3');\
                         for(var i=0;i<all.length;i++){{\
                           var t=(all[i].innerText||'').trim();\
                           if(t && t.toLowerCase()===q.toLowerCase()){{el=all[i];break;}}\
                         }}\
                       }}\
                       if(!el){{return 'not found';}}\
                       el.scrollIntoView({{block:'center'}});el.click();return 'clicked';\
                     }})()"
                );
                let result = browser.eval_js(&js)?;
                if result.contains("not found") {
                    return Err(anyhow!("no element matched {selector:?}"));
                }
                Ok(attach_verification(json!({
                    "browser": browser.name,
                    "selector": selector,
                    "summary": format!("Clicked {selector} in {}.", browser.name),
                })))
            }
            Action::Fill => {
                if crate::tools::looks_like_password(args["css"].as_str().unwrap_or("")) {
                    return Err(anyhow!("refusing to fill a password field"));
                }
                let css = args["css"].as_str().unwrap_or("");
                let text = args["text"].as_str().unwrap_or("");
                let css_lit = serde_json::to_string(css)?;
                let text_lit = serde_json::to_string(text)?;
                let js = format!(
                    "(function(){{\
                       var el=document.querySelector({css_lit});\
                       if(!el){{return 'not found';}}\
                       el.focus();\
                       if('value' in el){{\
                         var p=window.HTMLInputElement.prototype;\
                         var setter=Object.getOwnPropertyDescriptor(p,'value');\
                         if(setter){{setter.set.call(el,{text_lit});}}\
                         else{{el.value={text_lit};}}\
                         el.dispatchEvent(new Event('input',{{bubbles:true}}));\
                         el.dispatchEvent(new Event('change',{{bubbles:true}}));\
                       }}else{{el.textContent={text_lit};}}\
                       return 'filled';\
                     }})()"
                );
                let result = browser.eval_js(&js)?;
                if result.contains("not found") {
                    return Err(anyhow!("no element matched {css:?}"));
                }
                Ok(attach_verification(json!({
                    "browser": browser.name,
                    "css": css,
                    "summary": format!("Filled {css} in {}.", browser.name),
                })))
            }
            Action::Eval => {
                if !self.ctx.browser_eval {
                    return Err(anyhow!(
                        "browser_eval is disabled. Turn it on in Settings → Agent."
                    ));
                }
                let js = args["js"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing 'js'"))?;
                let wrapped = format!("JSON.stringify({js})");
                let result = browser.eval_js(&wrapped)?;
                Ok(attach_verification(json!({
                    "browser": browser.name,
                    "result": result,
                    "summary": "Evaluated JavaScript.",
                })))
            }
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_chars_counts_chars_not_bytes() {
        let s = "é".repeat(10);
        assert_eq!(truncate_chars(&s, 4), "é".repeat(4));
    }

    #[test]
    fn test_supported_browsers_include_safari_and_chromium() {
        let names: Vec<&str> = supported().iter().map(|b| b.name).collect();
        for expected in ["Safari", "Google Chrome", "Arc", "Brave Browser"] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert_eq!(supported()[0].kind, Kind::Safari);
        assert_eq!(supported()[1].kind, Kind::Chromium);
    }

    #[test]
    fn test_js_permission_error_detection() {
        let e = anyhow::anyhow!("execution error: -1743 doesn't understand the message");
        assert!(is_js_permission_error(&e));
        assert!(!is_js_permission_error(&anyhow::anyhow!("no supported browser")));
    }
}