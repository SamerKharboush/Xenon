use anyhow::Result;
use async_channel::Sender;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State, WebviewWindow, Window};
use tokio::sync::RwLock;

use crate::config::XenonConfig;
use crate::events::{AudioEvent, PetState, Utterance};

pub struct AppState {
    pub current_state: PetState,
    pub audio_tx: Sender<AudioEvent>,
}

impl AppState {
    pub fn new(audio_tx: Sender<AudioEvent>) -> Self {
        Self {
            current_state: PetState::Idle,
            audio_tx,
        }
    }
}

/// Reported by the frontend on startup so rendering can be verified without
/// screen capture (which needs Screen Recording permission).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendReady {
    pub width: u32,
    pub height: u32,
    pub opaque_pixels: u32,
}

#[tauri::command]
pub fn frontend_ready(report: FrontendReady) {
    log::info!(
        "Frontend ready: canvas {}x{}, {} pixels drawn",
        report.width,
        report.height,
        report.opaque_pixels
    );
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

/// User tapped the pet. Greet via the agent loop so the reply is spoken.
#[tauri::command]
pub fn pet_tap(state: State<'_, AppState>) -> Result<(), String> {
    state
        .audio_tx
        .try_send(AudioEvent::Greet)
        .map_err(|e| e.to_string())
}

/// Typed input path (used by the HUD and tests): behaves like a spoken line.
#[tauri::command]
pub fn say(state: State<'_, AppState>, text: String) -> Result<(), String> {
    state
        .audio_tx
        .try_send(AudioEvent::Speech(Utterance { text, confidence: 1.0 }))
        .map_err(|e| e.to_string())
}

/// Barge-in / mute: stop any TTS playback immediately. Callable from the
/// frontend (renderer.js "mute voice" and pet-tap mute paths).
#[tauri::command]
pub fn stop_speaking(runtime: State<'_, crate::runtime::Runtime>) {
    runtime.stop_speaking();
}

/// Manually set the pet to Listening (placeholder for wake-word integration).
#[tauri::command]
pub fn trigger_listen(window: Window) -> Result<(), String> {
    window
        .emit("pet-state", PetState::Listening)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_config(
    config: State<'_, ArcRwLockConfig>,
) -> Result<XenonConfig, String> {
    let mut guard = config.write().await;
    guard.migrate();
    Ok(guard.clone())
}

/// Persist *new_config* and apply it to the shared config so the next
/// agent turn picks up the new provider / model / prompt live. The pet
/// renderer gets the new Performance toggles via a `pet-config` event.
#[tauri::command]
pub async fn update_config(
    app: tauri::AppHandle,
    config: State<'_, ArcRwLockConfig>,
    new_config: XenonConfig,
) -> Result<(), String> {
    new_config.save().map_err(|e| e.to_string())?;
    let mut guard = config.write().await;
    *guard = new_config;
    let _ = app.emit("pet-config", &*guard);
    Ok(())
}

/// Settings window, created on demand.
///
/// Keeping it out of `tauri.conf.json` means no second WebContent process
/// (and its ~20 MB) is spawned until the user actually opens Settings.
/// Closing the window destroys it; the next open rebuilds it.
pub fn show_settings<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("Xenon Settings")
    .inner_size(650.0, 750.0)
    .resizable(true)
    .decorations(true)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Onboarding window, created on demand for the same reason as Settings.
pub fn show_onboarding<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("onboarding") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        app,
        "onboarding",
        tauri::WebviewUrl::App("onboarding.html".into()),
    )
    .title("Xenon — Permission Setup")
    .inner_size(520.0, 440.0)
    .resizable(false)
    .decorations(true)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    show_settings(&app)
}

/// Keeps the window click-through only while the cursor is away from the pet.
///
/// A whole-window `set_ignore_cursor_events(true)` at idle (the old behaviour)
/// meant the webview received *no* mouse events at all: no hover, no click, no
/// double-click, so the settings window was unreachable. Hit-testing the cursor
/// in Rust keeps the "click-through when idle" promise while the pet itself
/// stays interactive.
///
/// Uses CGEventGetLocation, which needs no Accessibility grant (unlike enigo).
fn spawn_hit_test(window: WebviewWindow) {
    use core_graphics::event::{CGEvent, CGEventType};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    std::thread::spawn(move || {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).unwrap();

        let mut ignore: Option<bool> = None;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(40));

            let event = match CGEvent::new(source.clone()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            event.set_type(CGEventType::Null);
            let cursor = event.location();

            let (wx, wy) = match window.outer_position() {
                Ok(p) => (p.x as f64, p.y as f64),
                Err(_) => continue,
            };
            let size = match window.inner_size() {
                Ok(s) => (s.width as f64, s.height as f64),
                Err(_) => continue,
            };

            let cx = wx + size.0 / 2.0;
            // Pet is anchored bottom-center; its feet sit ~30px above the
            // window bottom. Center hit-test on that lower region.
            let cy = wy + size.1 - 100.0;
            let dx = cursor.x - cx;
            let dy = cursor.y - cy;

            // Pet body is ~120px tall / ~120px wide when drawn at 240px sprite.
            let hit_radius = 90.0_f64.max(size.0.min(size.1) / 2.0);
            let over_pet = (dx * dx + dy * dy) <= hit_radius * hit_radius;

            let should_ignore = !over_pet;
            if ignore != Some(should_ignore) {
                if let Err(e) = window.set_ignore_cursor_events(should_ignore) {
                    log::warn!("hit-test: set_ignore_cursor_events failed: {e}");
                    continue;
                }
                log::info!(
                    "hit-test: cursor=({:.0},{:.0}) pet=({cx},{cy}) r={hit_radius:.0} -> click_through={should_ignore}",
                    cursor.x, cursor.y
                );
                ignore = Some(should_ignore);
            }
        }
    });
}

pub fn setup_window(window: &WebviewWindow) -> Result<()> {
    window.set_always_on_top(true)?;
    window.set_decorations(false)?;
    // Start interactive; spawn_hit_test flips to click-through only while the
    // cursor is off the pet.
    window.set_ignore_cursor_events(false)?;
    window.show()?;
    spawn_hit_test(window.clone());
    Ok(())
}

/// Type alias so Tauri commands can reference the shared config state
/// without a long generic parameter.
pub type ArcRwLockConfig = std::sync::Arc<RwLock<XenonConfig>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_state_default() {
        let (tx, _rx) = async_channel::bounded(4);
        let state = AppState::new(tx);
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

        // Task 5: enlarged to 320x300 so the pet + speech bubble fit above.
        let width = win["width"].as_u64().expect("width");
        let height = win["height"].as_u64().expect("height");
        assert!(width >= 320, "window too narrow for pet + bubble: {width}");
        assert!(height >= 280, "window too short for pet + bubble: {height}");
    }
}