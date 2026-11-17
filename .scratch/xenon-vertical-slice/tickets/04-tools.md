# Ticket 04: tools.rs — Tool trait + implementations

## Goal
Implement the Tool trait and three tool implementations: take_screenshot, click_mouse, open_app.

## Deliverables
- `src-tauri/src/tools.rs` with:
  - `Tool` trait: `name()`, `description()`, `schema() -> serde_json::Value`, `execute(args: serde_json::Value) -> ToolResult`
  - `ToolResult` type: `Ok(serde_json::Value)`, `Err(String)`
  - `TakeScreenshotTool` — uses xcap, saves screenshot, returns path/base64
  - `ClickMouseTool` — uses enigo, takes x/y coords, moves + clicks
  - `OpenAppTool` — uses macOS `open` command
- Unit tests: each tool implements name/description/schema correctly; execute with valid args produces Ok; execute with invalid args produces Err (not panic)

## Dependencies
- Ticket 01 (project structure)
- Ticket 02 (events types — ToolCall)

## Notes
- Each tool is a single trait implementation — adding a new tool is one impl
- xcap requires Screen Recording permission; enigo requires Accessibility
- Tools are the agent capability seam
