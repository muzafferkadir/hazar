mod bridge;
mod commands;

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
            // Browser-extension bridge (loopback WebSocket) + capture queue.
            let capture = bridge::start(app.handle().clone(), hazar_localapi::Settings::default());
            app.manage(capture);

            // Native macOS translucency behind the transparent window.
            #[cfg(target_os = "macos")]
            {
                use tauri_plugin_decorum::WebviewWindowExt;
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                if let Some(win) = app.get_webview_window("main") {
                    let _ = apply_vibrancy(
                        &win,
                        NSVisualEffectMaterial::UnderWindowBackground,
                        None,
                        None,
                    );
                    let _ = win.set_traffic_lights_inset(16.0, 22.0);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::engine_version,
            commands::engine_probe,
            commands::engine_download,
            commands::engine_hls,
            commands::capture_status,
            commands::capture_queue,
            commands::capture_cancel
        ])
        .run(tauri::generate_context!())
        .expect("error while running hazar");
}

/// Re-export for the UI state type.
pub type Capture = Arc<bridge::CaptureState>;
