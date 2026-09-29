//! Bridge between the browser extension (loopback API) and the download engine.
//!
//! The extension hands over a `grab` message; this module turns it into an
//! engine job, mirrors progress back over the WebSocket and emits the same
//! state to the UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hazar_engine::{download_hls, DownloadOptions, Downloader, HlsOptions, ProgressEvent};
use hazar_localapi::{
    ClientMessage, GrabKind, GrabRequest, Inbound, LocalApiConfig, Outbound, ServerHandle, Settings,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

pub const QUEUE_EVENT: &str = "capture-event";
pub const STATUS_EVENT: &str = "capture-status";
pub const PROGRESS_EVENT: &str = "download-progress";
const PROGRESS_EMIT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntry {
    pub id: String,
    pub url: String,
    pub kind: String,
    pub state: String,
    pub source: String,
    pub written: u64,
    pub total: u64,
    pub filename: Option<String>,
    pub path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub listening: bool,
    pub port: Option<u16>,
    pub extension_clients: u64,
    pub captured: usize,
    pub download_dir: String,
}

#[derive(Default)]
struct Queue {
    items: Vec<QueueEntry>,
    index: HashMap<String, usize>,
}

impl Queue {
    fn upsert(&mut self, entry: QueueEntry) {
        match self.index.get(&entry.id) {
            Some(position) => self.items[*position] = entry,
            None => {
                self.index.insert(entry.id.clone(), self.items.len());
                self.items.push(entry);
            }
        }
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut QueueEntry> {
        let position = *self.index.get(id)?;
        self.items.get_mut(position)
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn snapshot(&self) -> Vec<QueueEntry> {
        self.items.clone()
    }
}

pub struct CaptureState {
    port: Arc<Mutex<Option<u16>>>,
    server: Arc<Mutex<Option<ServerHandle>>>,
    queue: Arc<Mutex<Queue>>,
    cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    settings: Arc<Mutex<Settings>>,
    last_emit: Arc<Mutex<HashMap<String, Instant>>>,
}

impl CaptureState {
    pub fn port(&self) -> Option<u16> {
        *self.port.lock().expect("port lock")
    }

    pub fn extension_clients(&self) -> u64 {
        self.server
            .lock()
            .expect("server lock")
            .as_ref()
            .map(|s| s.client_count())
            .unwrap_or(0)
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().expect("settings lock").clone()
    }

    pub fn queue(&self) -> Vec<QueueEntry> {
        self.queue.lock().expect("queue lock").snapshot()
    }

    pub fn status(&self) -> StatusDto {
        StatusDto {
            listening: self.port().is_some(),
            port: self.port(),
            extension_clients: self.extension_clients(),
            captured: self.queue.lock().expect("queue lock").len(),
            download_dir: self
                .settings()
                .download_dir
                .unwrap_or_else(default_download_dir),
        }
    }

    /// Cancel an in-flight capture (returns false when it already finished).
    pub fn cancel(&self, id: &str) -> bool {
        let flags = self.cancels.lock().expect("cancel lock");
        match flags.get(id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    fn broadcast(&self, message: Outbound) {
        if let Some(server) = self.server.lock().expect("server lock").as_ref() {
            server.broadcast(message);
        }
    }

    fn emit_status(&self, app: &AppHandle) {
        let _ = app.emit(STATUS_EVENT, self.status());
    }

    fn emit_queue(&self, app: &AppHandle) {
        let _ = app.emit(QUEUE_EVENT, self.queue());
    }

    fn update<F: FnOnce(&mut QueueEntry)>(&self, id: &str, change: F) {
        if let Some(entry) = self.queue.lock().expect("queue lock").get_mut(id) {
            change(entry);
        }
    }

    fn should_emit(&self, id: &str) -> bool {
        let mut last = self.last_emit.lock().expect("emit lock");
        match last.get(id) {
            Some(at) if at.elapsed() < PROGRESS_EMIT => false,
            _ => {
                last.insert(id.to_string(), Instant::now());
                true
            }
        }
    }

    fn on_inbound(state: &Arc<CaptureState>, app: &AppHandle, message: ClientMessage) {
        match message.message {
            Inbound::Hello(_) => {}
            Inbound::Grab(grab) => {
                let state = state.clone();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    run_grab(app, state, grab.id, grab.request).await;
                });
            }
            Inbound::Cancel(cancel) => {
                if state.cancel(&cancel.id) {
                    state.update(&cancel.id, |entry| entry.state = "cancelling".into());
                    state.emit_queue(app);
                }
            }
            Inbound::Media(_) | Inbound::Ping(_) => {}
        }
    }

    fn on_progress(&self, app: &AppHandle, id: &str, event: &ProgressEvent) {
        let (phase, written, total) = match event {
            ProgressEvent::Probing { .. } => ("probing", 0, 0),
            ProgressEvent::Planned {
                size, bytes_done, ..
            } => ("download", *bytes_done, *size),
            ProgressEvent::PartProgress {
                total_written,
                total_size,
                ..
            } => ("download", *total_written, *total_size),
            ProgressEvent::HlsPlanned { .. } => ("hls", 0, 0),
            ProgressEvent::HlsSegment { bytes, .. } => ("hls", *bytes, 0),
            ProgressEvent::Retrying { .. } => ("retrying", 0, 0),
            ProgressEvent::FalldownSingle { .. } => ("download", 0, 0),
            ProgressEvent::Assembling { .. } => ("assembling", 0, 0),
            ProgressEvent::Verifying => ("verifying", 0, 0),
            ProgressEvent::Finished { bytes, .. } => ("done", *bytes, *bytes),
            ProgressEvent::Failed { .. } => ("failed", 0, 0),
        };

        if total > 0 || written > 0 {
            self.update(id, |entry| {
                if total > 0 {
                    entry.total = total;
                }
                entry.written = written;
            });
        }

        self.broadcast(Outbound::Progress {
            id: id.to_string(),
            phase: phase.to_string(),
            written,
            total,
            connections: self.settings().connections,
            speed_bps: 0,
        });

        if self.should_emit(id) {
            self.emit_queue(app);
        }
    }
}

/// Start the loopback API and wire it to the engine.
pub fn start(app: AppHandle, settings: Settings) -> Arc<CaptureState> {
    let state = Arc::new(CaptureState {
        port: Arc::new(Mutex::new(None)),
        server: Arc::new(Mutex::new(None)),
        queue: Arc::new(Mutex::new(Queue::default())),
        cancels: Arc::new(Mutex::new(HashMap::new())),
        settings: Arc::new(Mutex::new(settings.clone())),
        last_emit: Arc::new(Mutex::new(HashMap::new())),
    });

    let run_state = state.clone();
    let run_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let cfg = LocalApiConfig {
            settings,
            app_version: hazar_engine::VERSION.to_string(),
            ..Default::default()
        };
        match hazar_localapi::server::start(cfg).await {
            Ok((server, mut inbound)) => {
                *run_state.port.lock().expect("port lock") = Some(server.port);
                *run_state.server.lock().expect("server lock") = Some(server);
                run_state.emit_status(&run_app);
                eprintln!(
                    "hazar: capture API listening on 127.0.0.1:{}",
                    run_state.port().unwrap_or(0)
                );
                while let Some(message) = inbound.recv().await {
                    CaptureState::on_inbound(&run_state, &run_app, message);
                    run_state.emit_status(&run_app);
                }
                eprintln!("hazar: capture API stopped");
            }
            Err(e) => eprintln!("hazar: capture API failed to start: {e}"),
        }
    });

    state
}

type GrabResult = Result<(PathBuf, u64, Option<String>, u64), String>;

async fn run_grab(app: AppHandle, state: Arc<CaptureState>, id: String, request: GrabRequest) {
    let settings = state.settings();
    let dest = resolve_dest(&request, &settings);
    let kind = match request.kind {
        GrabKind::File => "file",
        GrabKind::Hls => "hls",
        GrabKind::Dash => "dash",
    };

    state.queue.lock().expect("queue lock").upsert(QueueEntry {
        id: id.clone(),
        url: request.url.clone(),
        kind: kind.to_string(),
        state: "downloading".into(),
        source: "extension".into(),
        written: 0,
        total: request.size.unwrap_or(0),
        filename: dest.file_name().map(|n| n.to_string_lossy().to_string()),
        path: None,
        error: None,
    });
    state.emit_queue(&app);

    let cancel = Arc::new(AtomicBool::new(false));
    state
        .cancels
        .lock()
        .expect("cancel lock")
        .insert(id.clone(), cancel.clone());

    state.broadcast(Outbound::GrabAck {
        id: id.clone(),
        state: "downloading".into(),
        message: None,
    });

    let (tx, mut rx) = hazar_engine::channel();
    let pump = {
        let app = app.clone();
        let state = state.clone();
        let id = id.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = rx.recv().await {
                state.on_progress(&app, &id, &event);
            }
        })
    };

    let headers = request.headers.clone();
    let user_agent = request.user_agent.clone();
    let result: GrabResult = match request.kind {
        GrabKind::File => {
            let mut opts = DownloadOptions::new(&request.url, &dest)
                .connections(settings.connections as usize)
                .cancel_flag(cancel.clone())
                .headers(headers.clone());
            if let Some(ua) = user_agent.clone() {
                opts = opts.user_agent(ua);
            }
            match Downloader::new(opts) {
                Ok(downloader) => downloader
                    .with_progress(tx)
                    .run()
                    .await
                    .map(|outcome| {
                        (
                            outcome.path,
                            outcome.size,
                            outcome.sha256,
                            outcome.elapsed.as_millis() as u64,
                        )
                    })
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        GrabKind::Hls => {
            let opts = HlsOptions {
                manifest: request.url.clone(),
                segments: request.segments.clone(),
                base_url: request.page_url.clone(),
                output: dest.clone(),
                connections: settings.connections as usize,
                user_agent,
                headers,
                expected_sha256: None,
                cancel: Some(cancel.clone()),
            };
            download_hls(opts, Some(tx))
                .await
                .map(|outcome| {
                    (
                        outcome.path,
                        outcome.size,
                        outcome.sha256,
                        outcome.elapsed.as_millis() as u64,
                    )
                })
                .map_err(|e| e.to_string())
        }
        GrabKind::Dash => Err("DASH (mpd) indirme henüz yok — yakalama kaydedildi".to_string()),
    };

    let _ = pump.await;
    state.cancels.lock().expect("cancel lock").remove(&id);
    state.last_emit.lock().expect("emit lock").remove(&id);

    match result {
        Ok((path, size, sha256, elapsed_ms)) => {
            let path = path.display().to_string();
            state.update(&id, |entry| {
                entry.state = "done".into();
                entry.written = size;
                entry.total = size;
                entry.path = Some(path.clone());
            });
            state.broadcast(Outbound::Finished {
                id: id.clone(),
                path,
                size,
                sha256,
                elapsed_ms,
            });
        }
        Err(reason) => {
            state.update(&id, |entry| {
                entry.state = if reason.contains("cancelled") {
                    "cancelled".into()
                } else {
                    "failed".into()
                };
                entry.error = Some(reason.clone());
            });
            state.broadcast(Outbound::Failed {
                id: id.clone(),
                reason,
            });
        }
    }

    state.emit_queue(&app);
    state.emit_status(&app);
}

fn resolve_dest(request: &GrabRequest, settings: &Settings) -> PathBuf {
    let dir = request
        .save_dir
        .clone()
        .or_else(|| settings.download_dir.clone())
        .unwrap_or_else(default_download_dir);
    let name = request
        .filename
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| name_from_url(&request.url));
    let mut candidate = Path::new(&dir).join(sanitize(&name));
    let mut counter = 1;
    while candidate.exists() && counter < 1000 {
        let stem = candidate
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".into());
        let extension = candidate
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        candidate = Path::new(&dir).join(format!("{stem} ({counter}){extension}"));
        counter += 1;
    }
    candidate
}

pub fn default_download_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join("Downloads").display().to_string()
}

fn name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
    let name = sanitize(last);
    if name.ends_with(".m3u8") {
        return name.replace(".m3u8", ".ts");
    }
    if Path::new(&name).extension().is_none() {
        format!("{name}.bin")
    } else {
        name
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        format!("download-{}", now_secs())
    } else {
        trimmed
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
