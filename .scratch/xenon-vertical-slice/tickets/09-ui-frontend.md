# Ticket 09: ui.rs + frontend — Pet window + sprite renderer

## Goal
Implement the Tauri window management (transparent, frameless, always-on-top, click-through) and the vanilla JS + Canvas frontend (5 sprite states, event-driven).

## Deliverables
- `src-tauri/src/ui.rs` with:
  - Window setup: transparent, frameless, always-on-top, click-through toggle via `set_ignore_cursor_events`
  - Tauri commands: `set_click_through(bool)`, `get_pet_state() -> PetState`, `trigger_listen()`
  - Emit PetState changes to frontend via Tauri events
- `src/index.html` — minimal canvas element
- `src/main.js` — Canvas 2D sprite renderer:
  - 5 states: Idle, Listening (waveform), Thinking (pulse), Speaking (lip-sync), Agentic (eyes track)
  - Listens for Tauri events to switch states
  - `requestAnimationFrame` loop at 60fps
- `src/style.css` — transparent background, no margins
- Unit tests: window config values correct; click-through toggle works (mock)

## Dependencies
- Ticket 01 (project structure)
- Ticket 02 (events types — PetState)

## Notes
- No Svelte/Vue/React — vanilla JS only (ADR-0002)
- No build step for frontend
- Click-through is runtime `set_ignore_cursor_events`, not config
- `macos-private-api` not needed (no-op in 2.12.1+)
- Transparent WebView needs `backgroundColor: [0,0,0,0]` CSS
- Known Tauri bug: transparency lost after bundling — test in both dev and bundled
