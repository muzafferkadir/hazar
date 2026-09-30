//! Thin Tauri surface over `hazar-engine`. All download logic lives in the
//! engine crate so the CLI, the desktop app and the browser bridge share one
//! implementation.

use std::sync::Arc;

use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::bridge::{CaptureState, QueueEntry, StatusDto};

#[tauri::command]
pub fn engine_version() -> String {
    hazar_engine::VERSION.to_string()
}

/// Tarayıcı eklentisinin bulunduğu klasör (bundle resource ya da dev checkout).
fn extension_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let candidates: Vec<std::path::PathBuf> = {
        let mut list = Vec::new();
        if let Ok(resource_dir) = app.path().resource_dir() {
            list.push(resource_dir.join("extension"));
            // Tauri üst dizinden gelen resource'ları `_up_/` altına koyar.
            list.push(resource_dir.join("_up_").join("extension"));
        }
        list.push(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../extension")
                .to_path_buf(),
        );
        list
    };

    candidates
        .into_iter()
        .find(|path| path.join("manifest.json").exists())
        .ok_or_else(|| "extension klasörü bulunamadı".to_string())
}

/// Eklenti klasörünün tam yolu (UI'da gösterilir).
#[tauri::command]
pub fn extension_path(app: AppHandle) -> Result<String, String> {
    extension_dir(&app).map(|path| path.display().to_string())
}

/// Eklenti klasörünü kullanıcının seçtiği dizine kopyalar ("indir" akışı).
///
/// macOS'ta yol metnini elle kopyalamak/pratik değil; kullanıcı bir klasör seçer,
/// biz eklentiyi oraya `Hazar-extension/` olarak yazarız ve Finder'da açarız.
#[tauri::command]
pub fn extension_export(app: AppHandle, dest_dir: String) -> Result<String, String> {
    let source = extension_dir(&app)?;
    let target = std::path::Path::new(&dest_dir).join("Hazar-extension");
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|error| format!("eski kopya silinemedi: {error}"))?;
    }
    copy_dir(&source, &target)?;
    let _ = app.opener().reveal_item_in_dir(&target);
    Ok(target.display().to_string())
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        // test klasörü ve editör artıkları kopyalanmasın.
        if name == "test" {
            continue;
        }
        let target = to.join(&name);
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Eklenti klasörünü Finder/Explorer'da açar (kullanıcı "load unpacked" yapacak).
#[tauri::command]
pub fn extension_reveal(app: AppHandle) -> Result<String, String> {
    let dir = extension_dir(&app)?;
    app.opener()
        .reveal_item_in_dir(&dir)
        .map_err(|error| format!("klasör açılamadı: {error}"))?;
    Ok(dir.display().to_string())
}

/// Capture/bridge status: where the extension should connect, and what it sent.
#[tauri::command]
pub fn capture_status(state: State<'_, Arc<CaptureState>>) -> StatusDto {
    state.status()
}

#[tauri::command]
pub fn capture_queue(state: State<'_, Arc<CaptureState>>) -> Vec<QueueEntry> {
    state.queue()
}

#[tauri::command]
pub fn capture_cancel(state: State<'_, Arc<CaptureState>>, id: String) -> bool {
    let changed = state.cancel(&id) || state.inner().drop_pending(&id);
    let _ = state.persist();
    changed
}

#[tauri::command]
pub fn capture_cancel_all(state: State<'_, Arc<CaptureState>>) -> usize {
    state.cancel_all()
}

#[tauri::command]
pub fn capture_settings_get(state: State<'_, Arc<CaptureState>>) -> hazar_localapi::Settings {
    state.settings()
}

#[tauri::command]
pub fn capture_settings_set(
    state: State<'_, Arc<CaptureState>>,
    settings: hazar_localapi::Settings,
) -> Result<hazar_localapi::Settings, String> {
    state.set_settings(settings.clone())?;
    state.inner().publish_settings(&settings);
    Ok(state.settings())
}

/// Uygulamadan (form) kuyruğa indirme ekler; capture ile aynı yolu kullanır.
#[tauri::command]
pub fn queue_add(
    app: AppHandle,
    state: State<'_, Arc<CaptureState>>,
    url: String,
    dest: String,
    connections: Option<usize>,
    sha256: Option<String>,
    speed_limit_mbps: Option<f64>,
) -> String {
    let kind = if hazar_engine::is_hls(&url, None) {
        hazar_localapi::GrabKind::Hls
    } else if hazar_engine::is_dash(&url, None) {
        hazar_localapi::GrabKind::Dash
    } else {
        hazar_localapi::GrabKind::File
    };
    let speed_limit_bps = speed_limit_mbps
        .filter(|value| *value > 0.0)
        .map(|value| (value * 1024.0 * 1024.0) as u64);
    let request = hazar_localapi::GrabRequest {
        url: url.clone(),
        extractor: None,
        browser_cookies: Vec::new(),
        browser_context: None,
        kind,
        filename: std::path::Path::new(&dest)
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
        mime: None,
        size: None,
        method: Some("GET".to_string()),
        referer: None,
        user_agent: None,
        cookie: None,
        headers: Vec::new(),
        segments: None,
        manifest: None,
        page_url: None,
        tab_id: None,
        frame_url: None,
        save_dir: std::path::Path::new(&dest)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.display().to_string()),
        connections: connections.map(|value| value as u32),
        expected_sha256: sha256.filter(|value| !value.trim().is_empty()),
        speed_limit_bps,
    };
    let id = format!("app-{}", crate::bridge::CaptureState::new_job_id());
    crate::bridge::enqueue(state.inner(), &app, id.clone(), request, "app");
    id
}

#[tauri::command]
pub fn queue_pause(state: State<'_, Arc<CaptureState>>, id: String) -> bool {
    state.pause(&id)
}

#[tauri::command]
pub fn queue_resume(
    app: AppHandle,
    state: State<'_, Arc<CaptureState>>,
    id: String,
    url: Option<String>,
) -> Result<(), String> {
    CaptureState::resume(state.inner(), &app, &id, url)
}

#[tauri::command]
pub fn queue_remove(app: AppHandle, state: State<'_, Arc<CaptureState>>, id: String) -> Result<bool, String> {
    state.remove_job(&app, &id)
}

#[tauri::command]
pub fn queue_clear(app: AppHandle, state: State<'_, Arc<CaptureState>>) -> Result<(), String> {
    state.clear_jobs(&app)
}
