mod agent;
mod audio;
mod events;
mod grounding;
mod provider;
mod runtime;
mod tools;
mod tts;
mod ui;

use anyhow::Result;
use std::sync::Arc;
use tauri::Manager;
use runtime::Runtime;

fn main() -> Result<()> {
    env_logger::init();

    let runtime = Arc::new(Runtime::new()?);

    let runtime_clone = runtime.clone();
    tokio::spawn(async move {
        if let Err(e) = runtime_clone.start().await {
            log::error!("Runtime failed: {}", e);
        }
    });

    tauri::Builder::default()
        .setup(|app| {
            let window = app.get_webview_window("main").unwrap();

            ui::setup_window(&window)?;

            app.manage(ui::AppState::new());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui::set_click_through,
            ui::get_pet_state,
            ui::trigger_listen,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    Ok(())
}
