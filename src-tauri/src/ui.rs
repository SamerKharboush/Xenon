use anyhow::Result;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, State, WebviewWindow, Window};

use crate::events::PetState;

pub struct AppState {
    pub current_state: PetState,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            current_state: PetState::Idle,
        }
    }
}

/// Reported by the frontend on startup so rendering can be verified without
/// screen capture (which needs Screen Recording permission).
#[derive(Debug, Serialize, Deserialize)]
pub struct FrontendReady {
    pub width: u32,
    pub height: u32,
    pub opaque_pixels: u32,
}

#[tauri::command]
pub fn frontend_ready(report: FrontendReady) {
    eprintln!(
        "XENON_FRONTEND_READY canvas={}x{} opaque={}",
        report.width, report.height, report.opaque_pixels
    );
    log::info!(
        "frontend ready: canvas {}x{}, {} opaque pixels",
        report.width,
        report.height,
        report.opaque_pixels
    );
    if report.opaque_pixels == 0 {
        log::error!("frontend canvas drew nothing");
    }
}

#[tauri::command]
pub fn set_click_through(window: Window, ignore: bool) -> Result<(), String> {
    window
        .set_ignore_cursor_events(ignore)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_pet_state(state: State<'_, AppState>) -> PetState {
    state.current_state.clone()
}

#[tauri::command]
pub async fn trigger_listen(window: Window) -> Result<(), String> {
    window
        .emit("pet-state", PetState::Listening)
        .map_err(|e| e.to_string())
}

pub fn setup_window(window: &WebviewWindow) -> Result<()> {
    window.set_always_on_top(true)?;
    window.set_decorations(false)?;
    // Idle pet is click-through so it never blocks work underneath.
    window.set_ignore_cursor_events(true)?;
    window.show()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_default() {
        let state = AppState::new();
        assert!(matches!(state.current_state, PetState::Idle));
    }

    /// Ticket 09: window config must stay transparent, frameless, always-on-top.
    #[test]
    fn test_window_config_matches_spec() {
        let raw = include_str!("../tauri.conf.json");
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let win = &value["app"]["windows"][0];

        assert_eq!(win["transparent"], true);
        assert_eq!(win["decorations"], false);
        assert_eq!(win["alwaysOnTop"], true);
        assert_eq!(win["visible"], true);
        assert_eq!(win["backgroundColor"], serde_json::json!([0, 0, 0, 0]));
        assert_eq!(value["app"]["withGlobalTauri"], true);
        assert_eq!(value["build"]["frontendDist"], "../src");
    }
}
