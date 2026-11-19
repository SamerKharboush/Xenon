mod agent;
mod audio;
mod audio_dsp;
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
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tokio::sync::RwLock;

use audio::AudioPipeline;
use runtime::Runtime;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let window = app.get_webview_window("main").unwrap();
            ui::setup_window(&window)?;

            let cfg = config::XenonConfig::load();
            log::info!(
                "Xenon starting: pet_name={}, provider={}, model={}, listen_mode={}",
                cfg.pet_name,
                cfg.provider.provider_url,
                cfg.provider.model_name,
                cfg.listen_mode,
            );

            // Shared config: owned by the Runtime, read by commands, written
            // by `update_config` so settings apply live.
            let config: ui::ArcRwLockConfig = Arc::new(RwLock::new(cfg));

            let mut runtime = Runtime::new(config.clone());
            runtime.attach_app(app.handle().clone());
            runtime.start();

            // Real Audio Pipeline: cpal capture -> VAD -> STT, plus the
            // global-hotkey force-listen. Runs on its own dedicated thread.
            let audio_pipeline = Arc::new(AudioPipeline::new(
                runtime.audio_sender(),
                app.handle().clone(),
                config.clone(),
            ));

            // Cmd+Shift+Space force-listen (barge-in from Speaking).
            let hotkey_pipeline = audio_pipeline.clone();
            if let Err(e) =
                app.global_shortcut()
                    .on_shortcut("Cmd+Shift+Space", move |_app, _sc, event| {
                        use tauri_plugin_global_shortcut::ShortcutState;
                        if event.state == ShortcutState::Pressed {
                            hotkey_pipeline.force_listen();
                        }
                    })
            {
                log::error!("global shortcut register failed: {e:#}");
            }
            app.manage(audio_pipeline);

            let audio_tx = runtime.audio_sender();
            app.manage(ui::AppState::new(audio_tx));
            app.manage(config.clone());

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