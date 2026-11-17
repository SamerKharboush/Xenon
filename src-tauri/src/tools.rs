use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::process::Command;

use crate::events::{ToolCall, ToolResult};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<ToolResult>;
}

pub struct TakeScreenshotTool {
    output_dir: String,
}

impl TakeScreenshotTool {
    pub fn new(output_dir: String) -> Self {
        Self { output_dir }
    }
}

#[async_trait]
impl Tool for TakeScreenshotTool {

impl ClickMouseTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ClickMouseTool {
    fn name(&self) -> &str {
        "click_mouse"
    }

    fn description(&self) -> &str {
        "Move the mouse to specified coordinates and click. Use this to interact with UI elements."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "x": {
                    "type": "integer",
                    "description": "X coordinate in screen pixels"
                },
                "y": {
                    "type": "integer",
                    "description": "Y coordinate in screen pixels"
                },
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button to click",
                    "default": "left"
                }
            },
            "required": ["x", "y"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let x = args["x"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing or invalid 'x' coordinate"))? as i32;
        let y = args["y"]
            .as_i64()
            .ok_or_else(|| anyhow!("Missing or invalid 'y' coordinate"))? as i32;
        let button = args["button"].as_str().unwrap_or("left");

        let mut enigo = enigo::Enigo::new(&enigo::Settings::default())?;

        enigo.move_mouse(x, y, enigo::Coordinate::Abs)?;

        match button {
            "left" => enigo.button(enigo::Button::Left, enigo::ButtonFunction::Click)?,
            "right" => enigo.button(enigo::Button::Right, enigo::ButtonFunction::Click)?,
            "middle" => enigo.button(enigo::Button::Middle, enigo::ButtonFunction::Click)?,
            _ => return Err(anyhow!("Invalid button: {}", button)),
        }

        Ok(ToolResult {
            call_id: String::new(),
            result: serde_json::json!({
                "clicked": true,
                "x": x,
                "y": y,
                "button": button
            }),
            is_error: false,
            error_message: None,
        })
    }
}

pub struct OpenAppTool;

impl OpenAppTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for OpenAppTool {
    fn name(&self) -> &str {
        "open_app"
    }

    fn description(&self) -> &str {
        "Open a macOS application by name. Use this to launch apps like Spotify, Safari, etc."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "app_name": {
                    "type": "string",
                    "description": "Name of the application to open (e.g., 'Spotify', 'Safari', 'Notes')"
                }
            },
            "required": ["app_name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let app_name = args["app_name"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing or invalid 'app_name'"))?;

        let output = Command::new("open")
            .arg("-a")
            .arg(app_name)
            .output()
            .map_err(|e| anyhow!("Failed to execute 'open' command: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Failed to open app '{}': {}", app_name, stderr));
        }

        Ok(ToolResult {
            call_id: String::new(),
            result: serde_json::json!({
                "opened": true,
                "app_name": app_name
            }),
            is_error: false,
            error_message: None,
        })
    }
}

pub fn create_default_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(TakeScreenshotTool::new("/tmp".to_string())),
        Box::new(ClickMouseTool::new()),
        Box::new(OpenAppTool::new()),
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

    #[test]
    fn test_take_screenshot_tool_schema() {
        let tool = TakeScreenshotTool::new("/tmp".to_string());
        assert_eq!(tool.name(), "take_screenshot");
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }

    #[test]
    fn test_click_mouse_tool_schema() {
        let tool = ClickMouseTool::new();
        assert_eq!(tool.name(), "click_mouse");
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema["properties"]["x"].is_object());
        assert!(schema["properties"]["y"].is_object());
    }

    #[test]
    fn test_open_app_tool_schema() {
        let tool = OpenAppTool::new();
        assert_eq!(tool.name(), "open_app");
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema["properties"]["app_name"].is_object());
    }

    #[tokio::test]
    async fn test_click_mouse_invalid_args() {
        let tool = ClickMouseTool::new();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_open_app_missing_name() {
        let tool = OpenAppTool::new();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
