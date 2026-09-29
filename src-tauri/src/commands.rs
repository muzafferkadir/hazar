//! Thin Tauri surface over `hazar-engine`. All download logic lives in the
//! engine crate so the CLI, the desktop app and the browser bridge share one
//! implementation.

use std::sync::Arc;

use hazar_engine::{
    channel, default_client, probe, DownloadOptions, Downloader, ProgressEvent, ResourceInfo,
    DEFAULT_CONNECTIONS, DEFAULT_MIN_PART_SIZE, MAX_CONNECTIONS,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::bridge::{CaptureState, QueueEntry, StatusDto, PROGRESS_EVENT};

#[tauri::command]
pub fn engine_version() -> String {
    hazar_engine::VERSION.to_string()
}

/// What the server reports before we commit to a download.
#[tauri::command]
pub async fn engine_probe(url: String) -> Result<ResourceInfo, String> {
    let client = default_client(None).map_err(|e| e.to_string())?;
    probe(&client, &url).await.map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeDto {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub connections: usize,
    pub resumed: bool,
    pub elapsed_ms: u64,
}

/// Start a download from the app UI; progress is streamed on [`PROGRESS_EVENT`].
#[tauri::command]
pub async fn engine_download(
    app: AppHandle,
    url: String,
    dest: String,
    connections: Option<usize>,
    sha256: Option<String>,
    speed_limit_mbps: Option<f64>,
) -> Result<OutcomeDto, String> {
    let (tx, mut rx) = channel();
    let emitter = app.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let _ = emitter.emit(PROGRESS_EVENT, &event as &ProgressEvent);
        }
    });

    let mut opts = DownloadOptions::new(url, dest)
        .connections(connections.unwrap_or(DEFAULT_CONNECTIONS).clamp(1, MAX_CONNECTIONS))
        .min_part_size(DEFAULT_MIN_PART_SIZE);
    if let Some(sha) = sha256.filter(|s| !s.trim().is_empty()) {
        opts = opts.sha256(sha);
    }
    if let Some(mbps) = speed_limit_mbps.filter(|value| *value > 0.0) {
        opts = opts.speed_limit((mbps * 1024.0 * 1024.0) as u64);
    }

    let downloader = Downloader::new(opts)
        .map_err(|e| e.to_string())?
        .with_progress(tx);

    let outcome = downloader.run().await.map_err(|e| e.to_string())?;
    Ok(OutcomeDto {
        path: outcome.path.display().to_string(),
        size: outcome.size,
        sha256: outcome.sha256,
        connections: outcome.connections,
        resumed: outcome.resumed,
        elapsed_ms: outcome.elapsed.as_millis() as u64,
    })
}

/// Download an HLS stream from the UI.
#[tauri::command]
pub async fn engine_hls(
    url: String,
    dest: String,
    connections: Option<usize>,
) -> Result<OutcomeDto, String> {
    let opts = hazar_engine::HlsOptions {
        manifest: url,
        segments: None,
        base_url: None,
        output: std::path::PathBuf::from(dest),
        connections: connections.unwrap_or(DEFAULT_CONNECTIONS).clamp(1, MAX_CONNECTIONS),
        user_agent: None,
        headers: Vec::new(),
        expected_sha256: None,
        cancel: None,
    };
    let outcome = hazar_engine::download_hls(opts, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(OutcomeDto {
        path: outcome.path.display().to_string(),
        size: outcome.size,
        sha256: outcome.sha256,
        connections: outcome.segments,
        resumed: outcome.resumed,
        elapsed_ms: outcome.elapsed.as_millis() as u64,
    })
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
    state.cancel(&id) || state.inner().drop_pending(&id)
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
) -> hazar_localapi::Settings {
    state.set_settings(settings.clone());
    state.inner().publish_settings(&settings);
    state.settings()
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
        save_dir: std::path::Path::new(&dest)
            .parent()
            .map(|parent| parent.display().to_string()),
        connections: connections.map(|value| value as u32),
        expected_sha256: sha256.filter(|value| !value.trim().is_empty()),
        speed_limit_bps,
    };
    let id = format!("app-{}", crate::bridge::CaptureState::new_job_id());
    crate::bridge::enqueue(state.inner(), &app, id.clone(), request, "app");
    id
}
