# Ticket 05: grounding.rs — Screenshot → VLM → click

## Goal
Implement the grounding pipeline: screenshot → 512×512 downscale → VLM via Provider → parse JSON coords → enigo click → verify.

## Deliverables
- `src-tauri/src/grounding.rs` with:
  - `ground_and_click(provider: &dyn Provider, screenshot: &[u8]) -> Result<GroundingResult>`
  - Downscale to 512×512 (use `image` crate)
  - Base64-encode screenshot, send to VLM via Provider with grounding prompt
  - Parse JSON response: `{target: String, x: u32, y: u32}`
  - Move mouse + click via enigo
  - Take follow-up screenshot to verify
  - Return `GroundingResult { success: bool, message: String }`
- Unit tests: given a screenshot, downscales to 512×512; given a mock VLM response, parses coords correctly; given malformed response, returns Err

## Dependencies
- Ticket 03 (Provider trait)
- Ticket 04 (enigo via ClickMouseTool, xcap via TakeScreenshotTool)

## Notes
- VLM may be same Provider or different (cloud fallback for heavy vision)
- Grounding prompt is strict JSON-only
- This is the perception→action bridge
