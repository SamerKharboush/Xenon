mod agent;
mod audio;
mod config;
mod events;
mod grounding;
mod provider;
mod runtime;
mod tools;
mod tts;
mod ui;

use std::sync::Arc;
use tauri::Manager;
use tokio::sync::RwLock;

use runtime::Runtime;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .setup(|app| {
            let window = app.get_webview_window("main").unwrap();
            ui::setup_window(&window)?;

            let cfg = config::XenonConfig::load();
            log::info!(
                "Xenon starting: pet_name={}, provider={}, model={}",
                cfg.pet_name,
                cfg.provider.provider_url,
                cfg.provider.model_name
            );

            // Shared config: owned by the Runtime, read by commands, written
            // by `update_config` so settings apply live.
            let config: ui::ArcRwLockConfig = Arc::new(RwLock::new(cfg));

            let mut runtime = Runtime::new(config.clone());
            runtime.attach_app(app.handle().clone());
            runtime.start();

            let audio_tx = runtime.audio_sender();
            app.manage(ui::AppState::new(audio_tx));
            app.manage(config);

            // Keep the runtime alive for the lifetime of the app.
            app.manage(runtime);

            // `xenon --debug-open-settings` opens the settings window at
            // startup; used to verify it renders without needing a real click.
            if std::env::args().any(|a| a == "--debug-open-settings") {
                if let Some(w) = app.get_webview_window("settings") {
                    let _ = w.show();
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui::frontend_ready,
            ui::set_click_through,
            ui::get_pet_state,
            ui::trigger_listen,
            ui::pet_tap,
            ui::say,
            ui::get_config,
            ui::update_config,
            ui::open_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    Ok(())
}