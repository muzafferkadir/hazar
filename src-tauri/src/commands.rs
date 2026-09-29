//! Thin Tauri surface over `hazar-engine`. All download logic lives in the
//! engine crate so the CLI, the desktop app and the browser bridge share one
//! implementation.

use std::sync::Arc;

use hazar_engine::{
    channel, default_client, probe, DownloadOptions, Downloader, ProgressEvent, ResourceInfo,
    DEFAULT_CONNECTIONS, DEFAULT_MIN_PART_SIZE, MAX_CONNECTIONS,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

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
    state.cancel(&id)
}
