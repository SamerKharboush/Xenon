use anyhow::{anyhow, Result};
use async_trait::async_trait;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Serialize;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::events::ToolResult;
use crate::grounding::ground;
use crate::provider::Provider;
use crate::screenshot;

/// Shared dependencies a tool needs: the Tauri app (for `agent-focus`) and the
/// vision provider used by `click` to ground its target.
#[derive(Clone)]
pub struct ToolContext {
    pub app: Option<tauri::AppHandle>,
    pub provider: Option<Arc<dyn Provider>>,
    pub vision_model: String,
}

impl ToolContext {
    /// Emit the normalized 0..1 point the agent is looking at.
    fn emit_focus(&self, x: f64, y: f64) {
        if let Some(app) = &self.app {
            use tauri::Emitter;
            let _ = app.emit("agent-focus", FocusPoint { x, y });
        }
    }
}

#[derive(Serialize, Clone)]
struct FocusPoint {
    x: f64,
    y: f64,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<ToolResult>;
}

/// Attach a verification screenshot to a successful action result. A capture
/// failure (missing Screen Recording permission) degrades to a warning so the
/// action itself still succeeds.
fn attach_verification(mut result: Value) -> ToolResult {
    let mut tr = ToolResult {
        call_id: String::new(),
        result: json!({}),
        is_error: false,
        error_message: None,
        image: None,
    };
    match screenshot::capture() {
        Ok(shot) => {
            result["screenshot"] = json!({
                "width": shot.image_width(),
                "height": shot.image_height(),
            });
            tr.result = result;
            tr.image = Some(shot);
        }
        Err(e) => {
            result["screenshot"] = json!({ "error": e.to_string() });
            tr.result = result;
        }
    }
    tr
}

fn make_enigo() -> Result<Enigo> {
    Enigo::new(&Settings::default()).map_err(|e| anyhow!("input control unavailable: {e}"))
}

/// `look`: capture the screen and hand the model a screenshot.
pub struct LookTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for LookTool {
    fn name(&self) -> &str {
        "look"
    }

    fn description(&self) -> &str {
        "Take a screenshot of the screen so you can see what is currently displayed. \
         Call this before acting, and after an action to verify the result."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "monitor": {
                    "type": "integer",
                    "description": "Monitor index (0 = primary). Omit to use the monitor under the cursor."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let monitor = args["monitor"].as_u64().map(|v| v as usize);
        let shot = screenshot::capture_monitor(monitor)?;
        // `look` has no target: point the eyes at the centre of the screen.
        self.ctx.emit_focus(0.5, 0.5);
        let summary = format!(
            "Screenshot of the screen ({}x{} image).",
            shot.image_width(),
            shot.image_height()
        );
        let result = json!({
            "width": shot.image_width(),
            "height": shot.image_height(),
            "summary": summary,
        });
        Ok(ToolResult {
            call_id: String::new(),
            result,
            is_error: false,
            error_message: None,
            image: Some(shot),
        })
    }
}

/// `click`: ground *target* to a point, move there and click.
pub struct ClickTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for ClickTool {
    fn name(&self) -> &str {
        "click"
    }

    fn description(&self) -> &str {
        "Click a UI element described in words (e.g. \"the blue Spotify icon\"). \
         The element is located on a screenshot of the screen, then clicked."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Short description of the element to click."
                },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "default": "left",
                    "description": "Mouse button."
                },
                "double": {
                    "type": "boolean",
                    "default": false,
                    "description": "Double-click instead of single click."
                }
            },
            "required": ["target"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let target = args["target"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'target'"))?;
        let button = args["button"].as_str().unwrap_or("left");
        let double = args["double"].as_bool().unwrap_or(false);

        let provider = self
            .ctx
            .provider
            .as_ref()
            .ok_or_else(|| anyhow!("no provider configured for grounding"))?;
        let shot = screenshot::capture()?;
        let point = ground(provider.as_ref(), &self.ctx.vision_model, &shot, target).await?;
        let (sx, sy) = shot.map_point(point.x, point.y);
        let (fx, fy) = shot.focus_point(point.x, point.y);
        self.ctx.emit_focus(fx, fy);

        let btn = match button {
            "left" => Button::Left,
            "right" => Button::Right,
            "middle" => Button::Middle,
            other => return Err(anyhow!("invalid button: {other}")),
        };
        let mut enigo = make_enigo()?;
        enigo.move_mouse(sx, sy, Coordinate::Abs)?;
        if double {
            enigo.button(btn, Direction::Click)?;
            enigo.button(btn, Direction::Click)?;
        } else {
            enigo.button(btn, Direction::Click)?;
        }

        let summary = format!("Clicked {target:?} at ({sx}, {sy}).");
        let result = json!({
            "clicked": point.target,
            "screen": [sx, sy],
            "button": button,
            "double": double,
            "summary": summary,
        });
        Ok(attach_verification(result))
    }
}

/// `type_text`: type a string at the current focus.
pub struct TypeTextTool;

#[async_trait]
impl Tool for TypeTextTool {
    fn name(&self) -> &str {
        "type_text"
    }

    fn description(&self) -> &str {
        "Type text into the currently focused element. Click the target field first."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to type." }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'text'"))?;
        let mut enigo = make_enigo()?;
        enigo.text(text)?;
        let summary = format!("Typed {text:?}.");
        Ok(attach_verification(json!({ "typed": text, "summary": summary })))
    }
}

/// `press_keys`: press a chord such as "cmd+space" or "ctrl+alt+delete".
pub struct PressKeysTool;

#[async_trait]
impl Tool for PressKeysTool {
    fn name(&self) -> &str {
        "press_keys"
    }

    fn description(&self) -> &str {
        "Press a keyboard chord. Keys are joined with '+' (e.g. \"cmd+space\", \
         \"ctrl+shift+esc\"). Use names like cmd, ctrl, alt, shift, enter, tab, esc."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "keys": {
                    "type": "string",
                    "description": "Chord, e.g. \"cmd+space\" or \"enter\"."
                }
            },
            "required": ["keys"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let keys = args["keys"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'keys'"))?;
        let parsed: Vec<Key> = keys
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(parse_key)
            .collect::<Result<Vec<_>>>()?;
        if parsed.is_empty() {
            return Err(anyhow!("no keys parsed from {keys:?}"));
        }
        let mut enigo = make_enigo()?;
        for key in &parsed {
            enigo.key(*key, Direction::Press)?;
        }
        for key in parsed.iter().rev() {
            enigo.key(*key, Direction::Release)?;
        }
        let summary = format!("Pressed {keys}.");
        Ok(attach_verification(json!({ "keys": keys, "summary": summary })))
    }
}

/// `scroll`: scroll the mouse wheel in a direction.
pub struct ScrollTool;

#[async_trait]
impl Tool for ScrollTool {
    fn name(&self) -> &str {
        "scroll"
    }

    fn description(&self) -> &str {
        "Scroll the mouse wheel up, down, left or right by a number of clicks."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "default": "down"
                },
                "amount": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 3,
                    "description": "Number of wheel clicks."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let direction = args["direction"].as_str().unwrap_or("down");
        let amount = args["amount"].as_i64().unwrap_or(3).clamp(1, 100) as i32;
        let (length, axis) = match direction {
            "up" => (amount, Axis::Vertical),
            "down" => (-amount, Axis::Vertical),
            "left" => (amount, Axis::Horizontal),
            "right" => (-amount, Axis::Horizontal),
            other => return Err(anyhow!("invalid direction: {other}")),
        };
        let mut enigo = make_enigo()?;
        enigo.scroll(length, axis)?;
        let summary = format!("Scrolled {direction} {amount} clicks.");
        Ok(attach_verification(
            json!({ "direction": direction, "amount": amount, "summary": summary }),
        ))
    }
}

/// `open_app`: launch a macOS application by name.
pub struct OpenAppTool;

#[async_trait]
impl Tool for OpenAppTool {
    fn name(&self) -> &str {
        "open_app"
    }

    fn description(&self) -> &str {
        "Open a macOS application by name. Use this to launch apps like Spotify, Safari, etc."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Application name (e.g. 'Spotify', 'Safari', 'Notes')."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'name'"))?;
        let output = Command::new("open")
            .arg("-a")
            .arg(name)
            .output()
            .map_err(|e| anyhow!("failed to run 'open': {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("failed to open {name:?}: {stderr}"));
        }
        let summary = format!("Opened {name}.");
        Ok(attach_verification(
            json!({ "opened": true, "name": name, "summary": summary }),
        ))
    }
}

/// `wait`: pause for a number of milliseconds (e.g. for an animation).
pub struct WaitTool;

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        "Wait for a number of milliseconds before taking the next screenshot."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ms": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 1000,
                    "description": "Milliseconds to wait."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let ms = args["ms"].as_u64().unwrap_or(1000).clamp(0, 10_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let summary = format!("Waited {ms} ms.");
        Ok(attach_verification(json!({ "waited_ms": ms, "summary": summary })))
    }
}

/// Map a key name to an enigo `Key`.
fn parse_key(name: &str) -> Result<Key> {
    let n = name.trim().to_ascii_lowercase();
    let key = match n.as_str() {
        "cmd" | "command" | "super" | "win" => Key::Meta,
        "ctrl" | "control" => Key::Control,
        "alt" | "option" => Key::Alt,
        "shift" => Key::Shift,
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // A single character (letters, digits, punctuation) is typed literally.
        single if single.chars().count() == 1 => Key::Unicode(single.chars().next().unwrap()),
        other => return Err(anyhow!("unknown key: {other}")),
    };
    Ok(key)
}

pub fn create_default_tools(ctx: ToolContext) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(LookTool { ctx: ctx.clone() }),
        Box::new(ClickTool { ctx: ctx.clone() }),
        Box::new(TypeTextTool),
        Box::new(PressKeysTool),
        Box::new(ScrollTool),
        Box::new(OpenAppTool),
        Box::new(WaitTool),
    ]
}

pub fn tools_to_schemas(tools: &[Box<dyn Tool>]) -> Vec<crate::provider::ToolSchema> {
    tools
        .iter()
        .map(|tool| crate::provider::ToolSchema {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.schema(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            app: None,
            provider: None,
            vision_model: "vl".to_string(),
        }
    }

    #[test]
    fn test_parse_key_names() {
        assert!(matches!(parse_key("cmd").unwrap(), Key::Meta));
        assert!(matches!(parse_key("Command").unwrap(), Key::Meta));
        assert!(matches!(parse_key("ctrl").unwrap(), Key::Control));
        assert!(matches!(parse_key("alt").unwrap(), Key::Alt));
        assert!(matches!(parse_key("shift").unwrap(), Key::Shift));
        assert!(matches!(parse_key("enter").unwrap(), Key::Return));
        assert!(matches!(parse_key("esc").unwrap(), Key::Escape));
        assert!(matches!(parse_key("f5").unwrap(), Key::F5));
        assert!(matches!(parse_key("a").unwrap(), Key::Unicode('a')));
    }

    #[test]
    fn test_parse_key_unknown_is_error() {
        assert!(parse_key("nope").is_err());
        assert!(parse_key("ab").is_err());
    }

    #[test]
    fn test_all_tools_have_schemas() {
        let tools = create_default_tools(ctx());
        assert_eq!(tools.len(), 7);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for expected in [
            "look",
            "click",
            "type_text",
            "press_keys",
            "scroll",
            "open_app",
            "wait",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        for t in &tools {
            assert!(!t.description().is_empty());
            assert!(t.schema().is_object());
        }
    }

    #[tokio::test]
    async fn test_click_requires_target() {
        let tool = ClickTool { ctx: ctx() };
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_click_without_provider_errors() {
        let tool = ClickTool { ctx: ctx() };
        assert!(tool.execute(json!({ "target": "button" })).await.is_err());
    }

    #[tokio::test]
    async fn test_type_text_requires_text() {
        let tool = TypeTextTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_press_keys_requires_keys() {
        let tool = PressKeysTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_open_app_requires_name() {
        let tool = OpenAppTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_scroll_invalid_direction_errors() {
        let tool = ScrollTool;
        assert!(tool.execute(json!({ "direction": "sideways" })).await.is_err());
    }
}