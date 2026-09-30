mod bridge;
mod commands;
mod store;

use std::sync::Arc;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            if let Ok(dir) = app.path().resource_dir() {
                let binary = dir.join("media").join(if cfg!(windows) {
                    "ffmpeg.exe"
                } else {
                    "ffmpeg"
                });
                if binary.exists() {
                    std::env::set_var("HAZAR_FFMPEG", binary);
                }
            }
            // Browser-extension bridge (loopback WebSocket) + capture queue.
            let capture = bridge::start(app.handle().clone(), hazar_localapi::Settings::default());
            app.manage(capture);

            // Menu bar / tray icon: keep downloading while the window is closed.
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::TrayIconBuilder;
                let open = MenuItem::with_id(app, "open", "Hazar'ı Aç", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "quit", "Çıkış", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&open, &quit])?;
                if let Some(icon) = app.default_window_icon().cloned() {
                    TrayIconBuilder::new()
                        .icon(icon)
                        .tooltip("Hazar — indirme yöneticisi")
                        .menu(&menu)
                        .show_menu_on_left_click(true)
                        .on_menu_event(|app, event| match event.id().as_ref() {
                            "open" => {
                                if let Some(window) = app.get_webview_window("main") {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                            "quit" => app.exit(0),
                            _ => {}
                        })
                        .build(app)?;
                }
            }

            #[cfg(target_os = "macos")]
            {
                use tauri_plugin_decorum::WebviewWindowExt;
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                if let Some(window) = app.get_webview_window("main") {
                    apply_vibrancy(&window, NSVisualEffectMaterial::UnderWindowBackground, None, None)?;
                    window.set_traffic_lights_inset(16.0, 22.0)?;
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::engine_version,
            commands::capture_status,
            commands::capture_queue,
            commands::capture_cancel,
            commands::capture_cancel_all,
            commands::capture_settings_get,
            commands::capture_settings_set,
            commands::queue_add,
            commands::queue_pause,
            commands::queue_resume,
            commands::extension_path,
            commands::extension_reveal,
            commands::extension_export
        ])
        .run(tauri::generate_context!())
        .expect("error while running hazar");
}

/// Re-export for the UI state type.
pub type Capture = Arc<bridge::CaptureState>;
