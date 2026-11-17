use anyhow::Result;
use tauri::{Manager, State, Window};
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

#[tauri::command]
pub fn set_click_through(window: Window, ignore: bool) -> Result<(), String> {
    window.set_ignore_cursor_events(ignore).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_pet_state(state: State<'_, AppState>) -> PetState {
    state.current_state.clone()
}

#[tauri::command]
pub async fn trigger_listen(window: Window) -> Result<(), String> {
    window.emit("pet-state", PetState::Listening).map_err(|e| e.to_string())
}

pub fn setup_window(window: &Window) -> Result<()> {
    let _ = window.set_always_on_top(true);
    let _ = window.set_decorations(false);
    let _ = window.set_transparent(true);
    let _ = window.set_ignore_cursor_events(true);

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

    #[test]
    fn test_click_through_command_exists() {
        let _ = set_click_through;
        let _ = get_pet_state;
        let _ = trigger_listen;
    }
}
