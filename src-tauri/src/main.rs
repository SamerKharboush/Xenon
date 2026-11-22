mod agent;
mod audio;
mod audio_dsp;
mod browser;
mod config;
mod cursor;
mod events;
mod grounding;
mod headless;
mod hub;
mod permissions;
mod presets;
mod provider;
mod runtime;
mod screenshot;
mod scripts;
mod tools;
mod tts;
mod ui;
mod updater;
mod voice;

use std::sync::Arc;
use tauri::{Emitter, Manager, Runtime as TauriRuntime};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tokio::sync::RwLock;

use audio::AudioPipeline;
use runtime::Runtime;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Loaded here (not only in setup) so the Performance toggles can pick the
    // tokio worker count before any async runtime exists.
    let cfg = config::XenonConfig::load();

    // Tauri's default async runtime starts one tokio worker per core (24 on a
    // maxed Mac Studio), each with its own stack and TLS. The app's async work
    // is two idle loops plus a handful of commands, so a small fixed pool is
    // enough. `lazy_audio = false` restores the default (per-core) pool; the
    // runtime is created before the app, so that toggle applies after restart.
    // The runtime must outlive the app, hence the binding.
    let rt = if cfg.lazy_audio {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
    } else {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    };
    tauri::async_runtime::set(rt.handle().clone());

    // Headless smoke-test entry: `xenon --agent-task "<task>" [--profile <name>]`
    // runs one agent turn and exits 0/1 without starting the pet UI.
    if let Some(code) = headless::run_from_args(&cfg) {
        std::process::exit(code);
    }

    // `xenon --permissions` prints permission status and exits. Must run BEFORE
    // the Tauri builder so no window/tray flashes.
    if std::env::args().any(|a| a == "--permissions") {
        let perms = permissions::get_permissions();
        println!("Accessibility: {}", if perms.accessibility.granted { "✓ granted" } else { "✗ denied" });
        println!("Screen Recording: {}", if perms.screen_recording.granted { "✓ granted" } else { "✗ denied" });
        println!("Input Monitoring: {}", if perms.input_monitoring.state == "not-needed" { "— not needed" } else if perms.input_monitoring.granted { "✓ granted" } else { "✗ denied" });
        println!("Microphone: {}", if perms.microphone.granted { "✓ granted" } else { "✗ not determined" });
        std::process::exit(0);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(move |app| {
            let window = app.get_webview_window("main").unwrap();
            ui::setup_window(&window)?;

            log::info!(
                "Xenon starting: pet_name={}, provider={}, model={}, listen_mode={}",
                cfg.pet_name,
                cfg.provider.provider_url,
                cfg.provider.model_name,
                cfg.listen_mode,
            );

            // Shared config: owned by the Runtime, read by commands, written
            // by `update_config` so settings apply live.
            let lazy_audio = cfg.lazy_audio;
            let dismissed = !cfg.onboarding_dismissed_for_version.is_empty()
                && cfg.onboarding_dismissed_for_version == env!("CARGO_PKG_VERSION");
            let config: ui::ArcRwLockConfig = Arc::new(RwLock::new(cfg));

            let mut runtime = Runtime::new(config.clone());
            runtime.attach_app(app.handle().clone());
            runtime.start();

            // Keep the runtime alive for the lifetime of the app. Manage it
            // before the shortcuts so `try_state::<Runtime>()` works inside
            // the abort hotkey handler.
            app.manage(runtime);

            // Performance toggle off: load the neural voice model now.
            if !lazy_audio {
                if let Some(rt) = app.try_state::<Runtime>() {
                    rt.preload_tts();
                }
            }

            // Real Audio Pipeline: cpal capture -> VAD -> STT, plus the
            // global-hotkey force-listen. Runs on its own dedicated thread.
            let audio_pipeline = Arc::new(AudioPipeline::new(
                // We need audio_sender before the Runtime is managed; grab it
                // from the audio pipeline's sender directly.
                app.try_state::<Runtime>()
                    .expect("runtime must be managed")
                    .audio_sender(),
                app.handle().clone(),
                config.clone(),
                app.try_state::<Runtime>()
                    .expect("runtime must be managed")
                    .speaking_flag(),
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

            // Cmd+Shift+Escape: abort the running agent turn and stop TTS.
            if let Err(e) =
                app.global_shortcut()
                    .on_shortcut("Cmd+Shift+Escape", move |app, _sc, event| {
                        use tauri_plugin_global_shortcut::ShortcutState;
                        if event.state == ShortcutState::Pressed {
                            if let Some(rt) = app.try_state::<Runtime>() {
                                rt.abort();
                            }
                        }
                    })
            {
                log::error!("global shortcut register failed (abort hotkey): {e:#}");
            }

            app.manage(audio_pipeline);

            let audio_tx = app
                .try_state::<Runtime>()
                .expect("runtime must be managed")
                .audio_sender();
            app.manage(ui::AppState::new(audio_tx));
            app.manage(config.clone());

            // In-app update check (GitHub Releases). Managed state + a single
            // setup call that starts the 24 h scheduler.
            let updater_state = std::sync::Arc::new(updater::UpdaterState::default());
            app.manage(updater_state.clone());
            updater::setup(app.handle(), config.clone(), updater_state);

            // macOS menu-bar tray (skipTaskbar means the Dock is hidden).
            setup_tray(app)?;

            // `xenon --debug-open-settings` opens the settings window at
            // startup; used to verify it renders without needing a real click.
            if std::env::args().any(|a| a == "--debug-open-settings") {
                let _ = ui::show_settings(app.handle());
            }

            // If required permissions are missing AND the user hasn't dismissed
            // onboarding for this version, show the onboarding window.
            // Input Monitoring is NOT required (cursor via NSEvent::mouseLocation).
            let perms = permissions::get_permissions();
            if (!perms.accessibility.granted || !perms.screen_recording.granted) && !dismissed {
                let _ = ui::show_onboarding(app.handle());
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui::frontend_ready,
            ui::start_window_drag,
            ui::set_click_through,
            ui::get_pet_state,
            ui::trigger_listen,
            ui::pet_tap,
            ui::say,
            ui::get_config,
            ui::update_config,
            ui::open_settings,
            ui::dismiss_onboarding,
            ui::stop_speaking,
            permissions::get_permissions,
            permissions::request_permission,
            permissions::open_permission_settings,
            updater::get_update_status,
            updater::check_now,
            updater::skip_update,
            updater::install_update,
            hub::provider_presets,
            hub::detect_local_providers,
            hub::list_provider_models,
            hub::test_provider_profile,
            hub::open_support_link,
            voice::start_listening,
            voice::list_voices,
            voice::import_voice_folder,
            voice::preview_voice,
            voice::record_clone,
            voice::open_typed_message,
            voice::set_typing,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, _event| {});

    Ok(())
}

fn setup_tray(app: &tauri::App) -> anyhow::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show_item = MenuItem::with_id(app, "show", "Show Pet", true, None::<&str>)?;
    let talk_item = MenuItem::with_id(app, "talk", "Talk", true, None::<&str>)?;
    let type_item = MenuItem::with_id(app, "type", "Type a message…", true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let support_item = MenuItem::with_id(app, "support", "Support Xenon ♥", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &talk_item,
            &type_item,
            &settings_item,
            &support_item,
            &sep,
            &quit_item,
        ],
    )?;

    // Hand the tray menu + a detached update item to the updater. It appends
    // the item ("Update to vX.Y.Z…") only when a newer release is found, so the
    // menu stays clean until then.
    if let Some(state) = app.try_state::<std::sync::Arc<updater::UpdaterState>>() {
        let update_item =
            MenuItem::with_id(app, "update", "Update available", false, None::<&str>)?;
        *state.menu.blocking_write() = Some(menu.clone());
        *state.update_item.blocking_write() = Some(update_item);
    }

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Xenon")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .icon_as_template(true);

    if let Ok(icon) = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")) {
        builder = builder.icon(icon);
    }

    builder
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => toggle_pet_visibility(app, true),
            "talk" => {
                // Same path as Cmd+Shift+Space: force-listen / barge-in.
                if let Some(pipeline) = app.try_state::<Arc<AudioPipeline>>() {
                    pipeline.force_listen();
                } else {
                    log::warn!("tray Talk: audio pipeline not available");
                }
            }
            "type" => {
                let _ = voice::open_typed_message(app.app_handle().clone());
            }
            "settings" => {
                let _ = ui::open_settings(app.app_handle().clone());
            }
            "support" => {
                // Open Settings scrolled to the Support section.
                let win = app
                    .get_webview_window("settings")
                    .or_else(|| ui::show_settings(app.app_handle()).ok().and_then(|_| app.get_webview_window("settings")));
                if let Some(win) = win {
                    let _ = win.show();
                    let _ = win.set_focus();
                    let win2 = win.clone();
                    std::thread::spawn(move || {
                        // Give a freshly-created window time to render.
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let _ = win2.eval("document.getElementById('support-github')?.scrollIntoView({behavior:'smooth',block:'center'})");
                    });
                }
            }
            "update" => {
                // Same path as Settings → Update: download, verify, open DMG.
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) =
                        app.try_state::<std::sync::Arc<updater::UpdaterState>>()
                    {
                        let st = state.inner().clone();
                        if let Err(e) = updater::install_update_impl(&app, &st).await {
                            log::warn!("tray update failed: {e}");
                            let _ = app.emit("xenon-notice", format!("Update failed: {e}"));
                        }
                    }
                });
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_pet_visibility(tray.app_handle(), false);
            }
        })
        .build(app)?;

    Ok(())
}

fn toggle_pet_visibility<R: TauriRuntime>(app: &tauri::AppHandle<R>, show: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let visible = window.is_visible().unwrap_or(true);
        if show && !visible {
            let _ = window.show();
            let _ = window.set_focus();
        } else if !show && visible {
            let _ = window.hide();
        } else if !show && !visible {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}