use anyhow::{anyhow, Result};
use async_trait::async_trait;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Serialize;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::events::ToolResult;
use crate::grounding::ground;
use crate::provider::Provider;
use crate::screenshot::{self, Screenshot};

/// Task-scoped notes written by `remember`, shared with the agent loop.
pub type Notes = Arc<Mutex<Vec<String>>>;

/// Shared dependencies a tool needs: the Tauri app (for `agent-focus`), the
/// vision provider used by `click` to ground its target, and the notes list.
#[derive(Clone)]
pub struct ToolContext {
    pub app: Option<tauri::AppHandle>,
    pub provider: Option<Arc<dyn Provider>>,
    pub vision_model: String,
    /// True when the agent-role model accepts image parts.
    pub vision: bool,
    pub notes: Notes,
    /// `run_script` is registered only when the user enabled scripts.
    pub allow_scripts: bool,
    /// `browser_eval` (arbitrary JavaScript) is off by default.
    pub browser_eval: bool,
    /// Built-in web tools configuration. `None` (tests) registers none.
    pub web: Option<crate::webtools::WebConfig>,
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
pub(crate) fn attach_verification(mut result: Value) -> ToolResult {
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

/// Ground *target* on a fresh screenshot. Returns the shot and the image-pixel
/// point the vision model chose.
async fn ground_point(ctx: &ToolContext, target: &str) -> Result<(Screenshot, f64, f64)> {
    let provider = ctx
        .provider
        .as_ref()
        .ok_or_else(|| anyhow!("no provider configured for grounding"))?;
    let shot = screenshot::capture()?;
    let point = ground(provider.as_ref(), &ctx.vision_model, &shot, target).await?;
    Ok((shot, point.x, point.y))
}

/// Map an image-pixel point to a logical screen point.
fn screen_point(shot: &Screenshot, x: f64, y: f64) -> (i32, i32) {
    shot.map_point(x, y)
}

/// Click at an already-grounded screen point.
fn click_at(enigo: &mut Enigo, x: i32, y: i32, button: Button, clicks: u32) -> Result<()> {
    enigo.move_mouse(x, y, Coordinate::Abs)?;
    for _ in 0..clicks.max(1) {
        enigo.button(button, Direction::Click)?;
    }
    Ok(())
}

fn parse_button(name: &str) -> Result<Button> {
    match name {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(anyhow!("invalid button: {other}")),
    }
}

/// Run `osascript -e <script>` and return stdout. Used for the AppleScript
/// tools (app switching, window lists, browser control).
fn osascript(script: &str) -> Result<String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| anyhow!("failed to run osascript: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!("osascript failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Element descriptions that must never receive typed text: a password field
/// would leak the secret into a screenshot and the transcript.
pub(crate) fn looks_like_password(target: &str) -> bool {
    let t = target.to_ascii_lowercase();
    ["password", "passwd", "passphrase", "passcode", "密码"]
        .iter()
        .any(|needle| t.contains(needle))
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
        if !self.ctx.vision {
            // Text-only model: no image can be understood, so return the
            // useful parts of the screen as text instead.
            let front = osascript(
                r#"tell application "System Events" to get name of first process whose frontmost is true"#,
            )
            .unwrap_or_else(|_| "unknown".to_string());
            let windows = osascript(
                r#"tell application "System Events"
                    set out to ""
                    repeat with p in (every process whose background only is false)
                        set out to out & (name of p) & " | "
                        try
                            repeat with w in (every window of p)
                                set out to out & (name of w) & " ;; "
                            end repeat
                        end try
                        set out to out & linefeed
                    end repeat
                    return out"#,
            )
            .unwrap_or_default();
            return Ok(ToolResult {
                call_id: String::new(),
                result: json!({
                    "frontmost_app": front,
                    "windows": windows,
                    "summary": "Text-only screen view (this model cannot see images).",
                }),
                is_error: false,
                error_message: None,
                image: None,
            });
        }
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
         The element is located on a screenshot of the screen, then clicked. \
         Supports repeated clicks and holding modifier keys during the click."
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
                "clicks": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 3,
                    "default": 1,
                    "description": "Number of clicks (2 = double-click)."
                },
                "hold_keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys held during the click, e.g. [\"shift\"]."
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
        let btn = parse_button(button)?;
        let clicks = args["clicks"].as_u64().unwrap_or(1).clamp(1, 3) as u32;
        let hold: Vec<Key> = args["hold_keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| k.as_str())
                    .map(parse_key)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();

        let (shot, px, py) = ground_point(&self.ctx, target).await?;
        let (sx, sy) = screen_point(&shot, px, py);
        let (fx, fy) = shot.focus_point(px, py);
        self.ctx.emit_focus(fx, fy);

        let mut enigo = make_enigo()?;
        for key in &hold {
            enigo.key(*key, Direction::Press)?;
        }
        let click_result = click_at(&mut enigo, sx, sy, btn, clicks);
        for key in hold.iter().rev() {
            enigo.key(*key, Direction::Release)?;
        }
        click_result?;

        let summary = format!("Clicked {target:?} at ({sx}, {sy}), {clicks} click(s).");
        let result = json!({
            "clicked": target,
            "screen": [sx, sy],
            "button": button,
            "clicks": clicks,
            "summary": summary,
        });
        Ok(attach_verification(result))
    }
}

/// `type_text`: type a string, optionally into a grounded target field.
pub struct TypeTextTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for TypeTextTool {
    fn name(&self) -> &str {
        "type_text"
    }

    fn description(&self) -> &str {
        "Type text. With a target, the element is located and clicked first; \
         overwrite clears the field, enter presses Return afterwards. \
         Password fields are refused."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to type." },
                "target": {
                    "type": "string",
                    "description": "Optional description of the field to type into."
                },
                "overwrite": {
                    "type": "boolean",
                    "default": false,
                    "description": "Select the existing content first (Cmd+A)."
                },
                "enter": {
                    "type": "boolean",
                    "default": false,
                    "description": "Press Return after typing."
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'text'"))?;
        let target = args["target"].as_str().unwrap_or("");
        let overwrite = args["overwrite"].as_bool().unwrap_or(false);
        let enter = args["enter"].as_bool().unwrap_or(false);

        if looks_like_password(target) {
            return Err(anyhow!(
                "refusing to type into {target:?}: it looks like a password field"
            ));
        }

        let mut enigo = make_enigo()?;
        if !target.is_empty() {
            let (shot, px, py) = ground_point(&self.ctx, target).await?;
            let (sx, sy) = screen_point(&shot, px, py);
            let (fx, fy) = shot.focus_point(px, py);
            self.ctx.emit_focus(fx, fy);
            click_at(&mut enigo, sx, sy, Button::Left, 1)?;
        }
        if overwrite {
            for key in [Key::Meta, Key::Unicode('a')] {
                enigo.key(key, Direction::Press)?;
            }
            for key in [Key::Unicode('a'), Key::Meta] {
                enigo.key(key, Direction::Release)?;
            }
        }
        enigo.text(text)?;
        if enter {
            enigo.key(Key::Return, Direction::Click)?;
        }
        let summary = format!("Typed {text:?}.");
        Ok(attach_verification(
            json!({ "typed": text, "target": target, "summary": summary }),
        ))
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
        if keys.is_empty() {
            return Err(anyhow!("missing 'keys'"));
        }
        press_chord(keys)?;
        let summary = format!("Pressed {keys}.");
        Ok(attach_verification(json!({ "keys": keys, "summary": summary })))
    }
}

/// `drag`: press at one grounded element, move to another, release.
pub struct DragTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for DragTool {
    fn name(&self) -> &str {
        "drag"
    }

    fn description(&self) -> &str {
        "Drag from one UI element to another, both described in words \
         (e.g. drag a file onto the Trash)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "start_target": { "type": "string", "description": "Element to drag from." },
                "end_target": { "type": "string", "description": "Element to drop on." }
            },
            "required": ["start_target", "end_target"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let start = args["start_target"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'start_target'"))?;
        let end = args["end_target"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'end_target'"))?;

        let (shot, px, py) = ground_point(&self.ctx, start).await?;
        let (_, dx, dy) = ground_point(&self.ctx, end).await?;
        let (sx, sy) = screen_point(&shot, px, py);
        let (ex, ey) = screen_point(&shot, dx, dy);
        let (fx, fy) = shot.focus_point(px, py);
        self.ctx.emit_focus(fx, fy);

        let mut enigo = make_enigo()?;
        enigo.move_mouse(sx, sy, Coordinate::Abs)?;
        enigo.button(Button::Left, Direction::Press)?;
        enigo.move_mouse(ex, ey, Coordinate::Abs)?;
        enigo.button(Button::Left, Direction::Release)?;

        let summary = format!("Dragged {start:?} onto {end:?}.");
        Ok(attach_verification(json!({
            "start": start,
            "end": end,
            "summary": summary,
        })))
    }
}

/// `hotkey`: press a chord given as a list of keys.
pub struct HotkeyTool;

#[async_trait]
impl Tool for HotkeyTool {
    fn name(&self) -> &str {
        "hotkey"
    }

    fn description(&self) -> &str {
        "Press a keyboard shortcut given as a list of keys, e.g. [\"cmd\", \"c\"] \
         to copy. Names: cmd, ctrl, alt, shift, enter, tab, esc."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keys to press together, e.g. [\"cmd\", \"space\"]."
                }
            },
            "required": ["keys"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let keys: Vec<String> = args["keys"]
            .as_array()
            .map(|k| {
                k.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if keys.is_empty() {
            return Err(anyhow!("missing 'keys'"));
        }
        press_chord(&keys.join("+"))?;
        Ok(attach_verification(json!({
            "keys": keys,
            "summary": format!("Pressed {}.", keys.join("+")),
        })))
    }
}

/// Shared chord press, used by both `press_keys` and `hotkey`.
fn press_chord(keys: &str) -> Result<()> {
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
    Ok(())
}

/// `switch_app`: bring an application to the front (AppleScript).
pub struct SwitchAppTool;

#[async_trait]
impl Tool for SwitchAppTool {
    fn name(&self) -> &str {
        "switch_app"
    }

    fn description(&self) -> &str {
        "Bring an application to the front by name (e.g. \"Safari\")."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Application name." }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'name'"))?;
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        osascript(&format!(
            "tell application \"{escaped}\" to activate"
        ))?;
        let summary = format!("Switched to {name}.");
        Ok(attach_verification(json!({ "app": name, "summary": summary })))
    }
}

/// `list_windows`: window titles of the running apps.
pub struct ListWindowsTool;

#[async_trait]
impl Tool for ListWindowsTool {
    fn name(&self) -> &str {
        "list_windows"
    }

    fn description(&self) -> &str {
        "List the open windows (application and title) so you can pick one."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        let output = osascript(
            r#"tell application "System Events"
                set out to ""
                repeat with p in (every process whose background only is false)
                    set out to out & (name of p) & " | "
                    try
                        repeat with w in (every window of p)
                            set out to out & (name of w) & " ;; "
                        end repeat
                    end try
                    set out to out & linefeed
                end repeat
                return out
            end tell"#,
        )?;
        let summary = if output.trim().is_empty() {
            "No windows found (System Events may be blocked by Accessibility)." 
        } else {
            "Open windows."
        };
        Ok(attach_verification(json!({
            "windows": output,
            "summary": summary,
        })))
    }
}

/// `frontmost_app`: the name of the frontmost application.
pub struct FrontmostAppTool;

#[async_trait]
impl Tool for FrontmostAppTool {
    fn name(&self) -> &str {
        "frontmost_app"
    }

    fn description(&self) -> &str {
        "Name of the application that currently has focus."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult> {
        let name = osascript(
            r#"tell application "System Events" to get name of first process whose frontmost is true"#,
        )?;
        Ok(attach_verification(json!({
            "frontmost_app": name,
            "summary": format!("Frontmost app: {name}."),
        })))
    }
}

/// `remember`: store a task-scoped note the agent sees in later turns.
pub struct RememberTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Store a short note for the rest of this task (e.g. a password you were \
         told to use, or the user's preference). Notes come back to you."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note": { "type": "string", "description": "Short note to remember." }
            },
            "required": ["note"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let note = args["note"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'note'"))?
            .trim()
            .to_string();
        if note.is_empty() {
            return Err(anyhow!("empty note"));
        }
        let count = {
            let mut notes = self.ctx.notes.lock().unwrap();
            notes.push(note.clone());
            notes.len()
        };
        Ok(attach_verification(json!({
            "remembered": note,
            "note_count": count,
            "summary": format!("Noted: {note}"),
        })))
    }
}

/// `done`: explicit terminal action with a short summary.
pub struct DoneTool;

#[async_trait]
impl Tool for DoneTool {
    fn name(&self) -> &str {
        "done"
    }

    fn description(&self) -> &str {
        "The task is complete. Give a short summary of what you did."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "What you did, one sentence." }
            },
            "required": ["summary"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let summary = args["summary"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'summary'"))?
            .to_string();
        Ok(ToolResult {
            call_id: String::new(),
            result: json!({ "summary": summary, "done": true }),
            is_error: false,
            error_message: None,
            image: None,
        })
    }
}

/// `fail`: explicit terminal action with a reason.
pub struct FailTool;

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "The task cannot be completed. Give the reason."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "Why the task cannot be done." }
            },
            "required": ["reason"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let reason = args["reason"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'reason'"))?
            .to_string();
        Ok(ToolResult {
            call_id: String::new(),
            result: json!({ "reason": reason, "failed": true }),
            is_error: false,
            error_message: None,
            image: None,
        })
    }
}

/// `scroll`: scroll the mouse wheel in a direction, optionally over an element.
pub struct ScrollTool {
    ctx: ToolContext,
}

#[async_trait]
impl Tool for ScrollTool {
    fn name(&self) -> &str {
        "scroll"
    }

    fn description(&self) -> &str {
        "Scroll the mouse wheel up, down, left or right by a number of clicks. \
         With a target, the pointer moves over that element first."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Optional element to scroll over (e.g. \"the article\")."
                },
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
        let target = args["target"].as_str().unwrap_or("");
        let (length, axis) = match direction {
            "up" => (amount, Axis::Vertical),
            "down" => (-amount, Axis::Vertical),
            "left" => (amount, Axis::Horizontal),
            "right" => (-amount, Axis::Horizontal),
            other => return Err(anyhow!("invalid direction: {other}")),
        };
        let mut enigo = make_enigo()?;
        if !target.is_empty() {
            let (shot, px, py) = ground_point(&self.ctx, target).await?;
            let (sx, sy) = screen_point(&shot, px, py);
            let (fx, fy) = shot.focus_point(px, py);
            self.ctx.emit_focus(fx, fy);
            enigo.move_mouse(sx, sy, Coordinate::Abs)?;
        }
        enigo.scroll(length, axis)?;
        let summary = format!("Scrolled {direction} {amount} clicks.");
        Ok(attach_verification(json!({
            "target": target,
            "direction": direction,
            "amount": amount,
            "summary": summary,
        })))
    }
}

/// `open`: launch an app, open a file, or open a URL (macOS `open`).
pub struct OpenTool;

#[async_trait]
impl Tool for OpenTool {
    fn name(&self) -> &str {
        "open"
    }

    fn description(&self) -> &str {
        "Open an application by name (e.g. \"Safari\"), a file by path, or a URL. \
         Use this to launch apps, open documents and go to websites."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "app_or_file_or_url": {
                    "type": "string",
                    "description": "Application name, file path, or URL (https://…)."
                }
            },
            "required": ["app_or_file_or_url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let target = args["app_or_file_or_url"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing 'app_or_file_or_url'"))?;

        let is_url = target.contains("://") || target.starts_with("www.");
        let is_path = target.starts_with('/') || target.starts_with('~') || target.contains('/');
        let mut cmd = Command::new("open");
        if !is_url && !is_path {
            cmd.arg("-a");
        }
        let output = cmd
            .arg(target)
            .output()
            .map_err(|e| anyhow!("failed to run 'open': {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("failed to open {target:?}: {stderr}"));
        }
        let summary = format!("Opened {target}.");
        Ok(attach_verification(json!({
            "opened": target,
            "is_url": is_url,
            "summary": summary,
        })))
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

/// The full tool vocabulary (Open-APA parity, PRD §3.3). Browser tools are
/// always present; `run_script` only when the user enabled scripts.
pub fn create_default_tools(ctx: ToolContext) -> Vec<Box<dyn Tool>> {
    use crate::browser::{Action, BrowserTool};

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(LookTool { ctx: ctx.clone() }),
        Box::new(ClickTool { ctx: ctx.clone() }),
        Box::new(TypeTextTool { ctx: ctx.clone() }),
        Box::new(DragTool { ctx: ctx.clone() }),
        Box::new(HotkeyTool),
        Box::new(PressKeysTool),
        Box::new(ScrollTool { ctx: ctx.clone() }),
        Box::new(OpenTool),
        Box::new(SwitchAppTool),
        Box::new(ListWindowsTool),
        Box::new(FrontmostAppTool),
        Box::new(RememberTool { ctx: ctx.clone() }),
        Box::new(WaitTool),
        Box::new(DoneTool),
        Box::new(FailTool),
    ];

    for action in [
        Action::Open,
        Action::Read,
        Action::Click,
        Action::Fill,
        Action::Eval,
    ] {
        tools.push(Box::new(BrowserTool::new(ctx.clone(), action)));
    }

    // Built-in web tools (web_search / web_extract / web_research / …).
    if let Some(web) = &ctx.web {
        if web.enabled {
            tools.extend(crate::webtools::create_web_tools(web.clone()));
        }
    }

    if ctx.allow_scripts {
        tools.push(Box::new(crate::scripts::RunScriptTool::new(
            ctx.clone(),
            true,
        )));
    }

    tools
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
            vision: false,
            notes: Arc::new(Mutex::new(Vec::new())),
            allow_scripts: false,
            browser_eval: false,
            web: None,
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
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for expected in [
            "look",
            "click",
            "type_text",
            "drag",
            "hotkey",
            "press_keys",
            "scroll",
            "open",
            "switch_app",
            "list_windows",
            "frontmost_app",
            "remember",
            "wait",
            "done",
            "fail",
            "browser_open",
            "browser_read",
            "browser_click",
            "browser_fill",
            "browser_eval",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        // Scripts are opt-in, so the tool must be absent by default.
        assert!(!names.contains(&"run_script"));
        for t in &tools {
            assert!(!t.description().is_empty());
            assert!(t.schema().is_object());
        }
    }

    #[test]
    fn test_allow_scripts_adds_run_script() {
        let mut c = ctx();
        c.allow_scripts = true;
        let tools = create_default_tools(c);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"run_script"));
    }

    #[test]
    fn test_password_field_heuristic() {
        assert!(looks_like_password("Password"));
        assert!(looks_like_password("Enter your passphrase"));
        assert!(!looks_like_password("Search field"));
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
        let tool = TypeTextTool { ctx: ctx() };
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_type_text_refuses_password_target() {
        let tool = TypeTextTool { ctx: ctx() };
        let err = tool
            .execute(json!({ "text": "hunter2", "target": "Password" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("password"));
    }

    #[tokio::test]
    async fn test_press_keys_requires_keys() {
        let tool = PressKeysTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_open_requires_target() {
        let tool = OpenTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_hotkey_requires_keys() {
        let tool = HotkeyTool;
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_drag_requires_both_targets() {
        let tool = DragTool { ctx: ctx() };
        assert!(tool
            .execute(json!({ "start_target": "file" }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_remember_requires_note() {
        let tool = RememberTool { ctx: ctx() };
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_done_and_fail_need_text() {
        assert!(DoneTool.execute(json!({})).await.is_err());
        assert!(FailTool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn test_scroll_invalid_direction_errors() {
        let tool = ScrollTool { ctx: ctx() };
        assert!(tool.execute(json!({ "direction": "sideways" })).await.is_err());
    }

    #[tokio::test]
    async fn test_browser_eval_disabled_by_default() {
        let tool = crate::browser::BrowserTool::new(ctx(), crate::browser::Action::Eval);
        assert!(tool
            .execute(json!({ "js": "document.title" }))
            .await
            .is_err());
    }
}