//! Bridge between the browser extension (loopback API) and the download engine.
//!
//! The extension hands over a `grab` message; this module turns it into an
//! engine job, mirrors progress back over the WebSocket and emits the same
//! state to the UI.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hazar_engine::{DownloadOptions, Downloader, HlsOptions, ProgressEvent};
use hazar_localapi::{
    ClientMessage, GrabKind, GrabRequest, Inbound, LocalApiConfig, Outbound, ServerHandle, Settings,
};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri::{AppHandle, Emitter};

pub const QUEUE_EVENT: &str = "capture-event";
pub const STATUS_EVENT: &str = "capture-status";
const PROGRESS_EMIT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A grab waiting for a free slot / for the schedule window.
struct PendingJob {
    id: String,
    request: GrabRequest,
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

    fn remove(&mut self, id: &str) -> bool {
        let old = self.items.len();
        self.items.retain(|entry| entry.id != id);
        self.index = self.items.iter().enumerate().map(|(i, entry)| (entry.id.clone(), i)).collect();
        self.items.len() != old
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
    /// Jobs waiting for a concurrency slot or for the schedule window.
    pending: Arc<Mutex<VecDeque<PendingJob>>>,
    running: Arc<std::sync::atomic::AtomicUsize>,
    cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    settings: Arc<Mutex<Settings>>,
    last_emit: Arc<Mutex<HashMap<String, Instant>>>,
    requests: Mutex<HashMap<String, GrabRequest>>,
    store: crate::store::Store,
    paused: Mutex<std::collections::HashSet<String>>,
    pump_lock: Mutex<()>,
    removed: Mutex<std::collections::HashSet<String>>,
    context_replies: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Result<hazar_engine::ytdlp::BrowserContext, String>>>>,
}

impl CaptureState {
    pub fn persist(&self) -> Result<(), String> {
        self.store.save(
            &self.settings(),
            &self.queue(),
            &self.requests.lock().expect("requests"),
        )
    }

    pub fn remove_job(&self, app: &AppHandle, id: &str) -> Result<bool, String> {
        let _pump = self.pump_lock.lock().expect("pump");
        self.removed.lock().expect("removed").insert(id.into());
        self.cancel(id);
        self.pending.lock().expect("pending").retain(|job| job.id != id);
        let removed = self.queue.lock().expect("queue").remove(id);
        self.requests.lock().expect("requests").remove(id);
        self.paused.lock().expect("paused").remove(id);
        self.persist()?;
        self.emit_queue(app);
        self.emit_status(app);
        Ok(removed)
    }

    pub fn clear_jobs(&self, app: &AppHandle) -> Result<(), String> {
        for entry in self.queue() { self.remove_job(app, &entry.id)?; }
        Ok(())
    }

    pub fn pause(&self, id: &str) -> bool {
        let _pump_guard = self.pump_lock.lock().expect("pump");
        if self.drop_pending(id) {
            self.update(id, |e| e.state = "paused".into());
            let _ = self.persist();
            return true;
        }
        self.paused.lock().expect("paused").insert(id.into());
        if self.cancel(id) {
            true
        } else {
            self.paused.lock().expect("paused").remove(id);
            false
        }
    }

    pub fn resume(
        state: &Arc<Self>,
        app: &AppHandle,
        id: &str,
        url: Option<String>,
    ) -> Result<(), String> {
        let entry = state
            .queue()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or("İndirme bulunamadı")?;
        if !matches!(
            entry.state.as_str(),
            "paused" | "interrupted" | "failed" | "cancelled" | "needs_refresh"
        ) {
            return Err("Bu indirme zaten aktif veya tamamlandı".into());
        }
        let mut request = state
            .requests
            .lock()
            .expect("requests")
            .get(id)
            .cloned()
            .ok_or("Tarayıcıdan yeniden gönder")?;
        if let Some(url) = url.filter(|s| !s.trim().is_empty()) {
            request.url = url;
        }
        let parsed = url::Url::parse(&request.url).map_err(|_| "Geçersiz link")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("HTTP/HTTPS linki gerekli".into());
        }
        state.paused.lock().expect("paused").remove(id);
        enqueue(state, app, id.into(), request, &entry.source);
        Ok(())
    }

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

    /// Unique job id for app-initiated downloads.
    pub fn new_job_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        format!("{nanos:x}{:x}", std::process::id())
    }

    /// Remove a waiting job (used when the UI cancels a queued item).
    pub fn drop_pending(&self, id: &str) -> bool {
        let mut pending = self.pending.lock().expect("pending");
        match pending.iter().position(|job| job.id == id) {
            Some(position) => { let removed = pending.remove(position).is_some(); self.update(id, |e| e.state = "cancelled".into()); removed },
            None => false,
        }
    }

    /// Push the current settings to every connected extension.
    pub fn publish_settings(&self, settings: &Settings) {
        self.broadcast(Outbound::Settings {
            settings: settings.clone(),
        });
    }

    /// Drop every waiting and running job.
    pub fn cancel_all(&self) -> usize {
        let dropped = {
            let mut pending = self.pending.lock().expect("pending");
            let count = pending.len();
            pending.clear();
            count
        };
        let running = {
            let flags = self.cancels.lock().expect("cancel lock");
            for flag in flags.values() {
                flag.store(true, Ordering::Relaxed);
            }
            flags.len()
        };
        for entry in self.queue() {
            if entry.state == "queued" || entry.state == "scheduled" || entry.state == "downloading"
            {
                self.update(&entry.id, |entry| entry.state = "cancelled".into());
            }
        }
        dropped + running
    }

    pub fn set_settings(&self, settings: Settings) -> Result<(), String> {
        *self.settings.lock().expect("settings lock") = settings;
        self.persist()
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
        if let Err(error) = self.persist() {
            eprintln!("hazar: state save failed: {error}");
        }
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
                enqueue(state, app, grab.id, grab.request, "extension");
            }
            Inbound::Extract(grab) => {
                let state = state.clone();
                tauri::async_runtime::spawn(async move {
                    let context = extractor_context(&grab.request);
                    let result = match tokio::time::timeout(Duration::from_secs(58), hazar_engine::ytdlp::probe(&grab.request.url, &context)).await {
                        Ok(result) => result,
                        Err(_) => Err(hazar_engine::Error::Protocol("yt-dlp analiz timeout".into())),
                    };
                    let (title, height, error) = match result {
                        Ok(probe) => (probe.as_ref().map(|p| p.title.clone()), probe.and_then(|p| p.height), None),
                        Err(error) => (None, None, Some(hazar_engine::ytdlp::diagnostic(&error.to_string()))),
                    };
                    state.broadcast(Outbound::Extracted { id: grab.id, title, height, error });
                });
            }
            Inbound::Context(reply) => {
                if let Some(sender) = state.context_replies.lock().expect("context replies").remove(&reply.id) {
                    let _ = sender.send(reply.context.ok_or_else(|| reply.error.unwrap_or_else(|| "Browser session bulunamadı".into())));
                }
            }
            Inbound::Cancel(cancel) => {
                let removed = state.drop_pending(&cancel.id);
                if removed {
                    state.update(&cancel.id, |entry| entry.state = "cancelled".into());
                    state.emit_queue(app);
                }
                if state.cancel(&cancel.id) {
                    state.update(&cancel.id, |entry| entry.state = "cancelling".into());
                    state.emit_queue(app);
                }
            }
            Inbound::Bytes(bytes) => {
                on_bytes(state, app, bytes);
            }
            Inbound::CaptureFailed(failed) => {
                let reason = redact_error(&failed.reason.chars().take(512).collect::<String>());
                state.cancel(&failed.id);
                state.update(&failed.id, |entry| { entry.state = "failed".into(); entry.error = Some(reason.clone()); });
                state.broadcast(Outbound::Failed { id: failed.id, reason });
                state.emit_queue(app);
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

/// Bir işi kuyruğa alır: boş slot ve (açıksa) zamanlama penceresi varsa hemen başlar.
pub(crate) fn enqueue(
    state: &Arc<CaptureState>,
    app: &AppHandle,
    id: String,
    request: GrabRequest,
    source: &str,
) {
    let mut request = request;
    let youtube = hazar_engine::ytdlp::canonical_url(&request.url);
    if youtube.is_some() || request.extractor.as_deref() == Some("ytdlp") {
        if let Some(url) = &youtube { request.url = url.clone(); }
        request.kind = GrabKind::File;
        let filename = request.filename.get_or_insert_with(|| "Video".into());
        if !filename.to_lowercase().ends_with(".mp4") { filename.push_str(".mp4"); }
    }
    if state.requests.lock().expect("requests").contains_key(&id)
        && state.queue().iter().any(|e| {
            e.id == id && matches!(e.state.as_str(), "queued" | "scheduled" | "downloading")
        })
    {
        return;
    }
    let settings = state.settings();
    let existing = state.queue().into_iter().find(|e| e.id == id).and_then(|e| e.path).map(PathBuf::from);
    let mut dest = existing.unwrap_or_else(|| resolve_dest(&request, &settings));
    if (youtube.is_some() || request.extractor.as_deref() == Some("ytdlp")) && !dest.extension().is_some_and(|ext| ext == "mp4") {
        dest = PathBuf::from(format!("{}.mp4", dest.display()));
    }
    let original = dest.clone();
    let mut suffix = 1;
    while state.queue().iter().any(|e| e.id != id && e.path.as_deref() == Some(dest.to_string_lossy().as_ref())) {
        let stem = original.file_stem().unwrap_or_default().to_string_lossy();
        let ext = original.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        dest = original.with_file_name(format!("{stem} ({suffix}){ext}")); suffix += 1;
    }
    request.save_dir = dest.parent().map(|p| p.to_string_lossy().into_owned());
    request.filename = dest.file_name().map(|p| p.to_string_lossy().into_owned());
    state
        .requests
        .lock()
        .expect("requests")
        .insert(id.clone(), request.clone());
    let scheduled = !schedule_allows(&settings);
    let kind = match request.kind {
        GrabKind::File => "file",
        GrabKind::Hls => "hls",
        GrabKind::Dash => "dash",
    }
    .to_string();
    let filename = dest
        .file_name()
        .map(|name| name.to_string_lossy().to_string());

    state
        .pending
        .lock()
        .expect("pending")
        .push_back(PendingJob {
            id: id.clone(),
            request: request.clone(),
        });
    let ack_id = id.clone();
    state.queue.lock().expect("queue lock").upsert(QueueEntry {
        id,
        url: request.url.clone(),
        kind,
        state: if scheduled { "scheduled" } else { "queued" }.into(),
        source: source.to_string(),
        written: 0,
        total: 0,
        filename,
        path: Some(dest.to_string_lossy().into_owned()),
        error: None,
    });
    state.broadcast(Outbound::GrabAck { id: ack_id, state: "queued".into(), message: None });
    state.emit_queue(app);
    pump(state, app);
}

/// Boş slot olduğu ve zamanlama izin verdiği sürece bekleyen işleri başlatır.
pub(crate) fn pump(state: &Arc<CaptureState>, app: &AppHandle) {
    let _pump_guard = state.pump_lock.lock().expect("pump");
    loop {
        let settings = state.settings();
        if !schedule_allows(&settings) {
            return;
        }
        let max = settings.max_concurrent_downloads.max(1) as usize;
        if state.running.load(std::sync::atomic::Ordering::Relaxed) >= max {
            return;
        }
        let next = state.pending.lock().expect("pending").pop_front();
        let Some(job) = next else {
            return;
        };

        state
            .running
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.update(&job.id, |entry| {
            entry.state = "downloading".into();
            if entry.url.is_empty() {
                entry.url = job.request.url.clone();
            }
        });
        state.emit_queue(app);

        state.cancels.lock().expect("cancel lock").insert(job.id.clone(), Arc::new(AtomicBool::new(false)));
        let state = state.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            run_grab(app.clone(), state.clone(), job.id, job.request).await;
            state
                .running
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            pump(&state, &app);
        });
    }
}

/// Zamanlama kapalıysa hep açık; açıksa "from-to" penceresi (gece yarısını sarabilir).
fn schedule_allows(settings: &Settings) -> bool {
    if !settings.schedule_enabled {
        return true;
    }
    let minutes = |value: &str| -> Option<u32> {
        let (hours, minutes) = value.split_once(':')?;
        Some(hours.trim().parse::<u32>().ok()? * 60 + minutes.trim().parse::<u32>().ok()?)
    };
    let (Some(from), Some(to)) = (
        minutes(&settings.schedule_from),
        minutes(&settings.schedule_to),
    ) else {
        return true;
    };
    let now = {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0) as i64
            + local_offset_seconds();
        ((seconds.rem_euclid(86_400)) / 60) as u32
    };
    if from <= to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

/// UTC→yerel fark (saniye). Tarih/saat kütüphanesi eklemeden pratik çözüm.
fn local_offset_seconds() -> i64 {
    // `date +%z` çıktısını kullan (her platformda var).
    let output = std::process::Command::new("date").arg("+%z").output();
    let Ok(output) = output else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.len() < 5 {
        return 0;
    }
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let hours: i64 = text[1..3].parse().unwrap_or(0);
    let minutes: i64 = text[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

/// Start the loopback API and wire it to the engine.
pub fn start(app: AppHandle, settings: Settings) -> Arc<CaptureState> {
    let store = crate::store::Store::new(app.path().app_data_dir().expect("app data"));
    let saved = store.load();
    let settings = saved
        .as_ref()
        .map(|s| s.settings.clone())
        .unwrap_or(settings);
    let mut queue = Queue::default();
    let mut requests = HashMap::new();
    if let Some(saved) = saved {
        for mut entry in saved.entries {
            if matches!(
                entry.state.as_str(),
                "downloading" | "queued" | "scheduled" | "cancelling" | "assembling"
            ) {
                entry.state = "interrupted".into();
            }
            if (entry.kind == "hls" || hazar_engine::ytdlp::canonical_url(&entry.url).is_some()) && entry.state == "done" {
                if let Some(path) = &entry.path {
                    use std::io::Read;
                    if let Ok(mut file) = std::fs::File::open(path) {
                        let mut prefix = [0u8; 512];
                        if let Ok(n) = file.read(&mut prefix) {
                            if hazar_engine::hls::validate_media_body(&prefix[..n]).is_err() {
                                entry.state = "failed".into();
                                entry.error = Some("Eski download video yerine HTML/playlist kaydetmiş; yeniden indir".into());
                            }
                        }
                    }
                }
            }
            queue.upsert(entry);
        }
        requests = saved.requests;
    }
    let state = Arc::new(CaptureState {
        port: Arc::new(Mutex::new(None)),
        server: Arc::new(Mutex::new(None)),
        queue: Arc::new(Mutex::new(queue)),
        pending: Arc::new(Mutex::new(VecDeque::new())),
        running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        cancels: Arc::new(Mutex::new(HashMap::new())),
        settings: Arc::new(Mutex::new(settings.clone())),
        last_emit: Arc::new(Mutex::new(HashMap::new())),
        requests: Mutex::new(requests),
        store,
        paused: Mutex::new(std::collections::HashSet::new()),
        pump_lock: Mutex::new(()),
        removed: Mutex::new(std::collections::HashSet::new()),
        context_replies: Mutex::new(HashMap::new()),
    });

    // Zamanlanmış işleri pencere açıldığında başlat.
    {
        let tick_state = state.clone();
        let tick_app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                pump(&tick_state, &tick_app);
            }
        });
    }

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
    let dest = state
        .queue()
        .into_iter()
        .find(|e| e.id == id)
        .and_then(|e| e.path)
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_dest(&request, &settings));
    let source = state
        .queue()
        .into_iter()
        .find(|e| e.id == id)
        .map(|e| e.source)
        .unwrap_or_else(|| "extension".into());
    let kind = match request.kind {
        GrabKind::File => "file",
        GrabKind::Hls => "hls",
        GrabKind::Dash => "dash",
    };

    {
    let removed = state.removed.lock().expect("removed");
    if removed.contains(&id) { return; }
    state.queue.lock().expect("queue lock").upsert(QueueEntry {
        id: id.clone(),
        url: request.url.clone(),
        kind: kind.to_string(),
        state: "downloading".into(),
        source,
        written: 0,
        total: request.size.unwrap_or(0),
        filename: dest.file_name().map(|n| n.to_string_lossy().to_string()),
        path: Some(dest.to_string_lossy().into_owned()),
        error: None,
    });
    }
    state.emit_queue(&app);

    let cancel = state.cancels.lock().expect("cancel lock").get(&id).cloned().unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

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

    // Eklenti Referer/Cookie'yi ayrı alanlarda gönderiyor; bunlar header'a
    // çevrilmezse app oynatıcı oturumu olmadan indirir ve CDN 403 döner
    // (popup'ta "master-3.ts failed · 403" kaydının sebebi buydu).
    let headers = capture_headers(
        &request.headers,
        request.referer.as_deref(),
        request.cookie.as_deref(),
    );
    let user_agent = request.user_agent.clone();
    let connections = request
        .connections
        .map(|value| value as usize)
        .unwrap_or(settings.connections as usize)
        .max(1);
    // Teşhis: CDN 403'lerinde hangi URL'e hangi başlıklarla gidildiğini göster.
    // Token/sorgu değerleri log'a yazılmaz (yalnızca path + header isimleri).
    eprintln!(
        "hazar: grab {id} kind={kind} url={url_label} segments={segments} referer={referer} cookie={cookie} headers=[{names}]",
        id = id,
        kind = kind,
        url_label = url_label(&request.url),
        segments = request.segments.as_ref().map(|list| list.len()).unwrap_or(0),
        referer = request
            .referer
            .as_deref()
            .map(url_label)
            .unwrap_or_else(|| "-".to_string()),
        cookie = if request.cookie.as_deref().is_some_and(|c| !c.trim().is_empty()) {
            "var"
        } else {
            "yok"
        },
        names = headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let result: GrabResult = match request.kind {
        GrabKind::File if request.extractor.as_deref() == Some("ytdlp") || hazar_engine::ytdlp::canonical_url(&request.url).is_some() => {
            let started = Instant::now();
            let context = if let Some(tab_id) = request.tab_id.filter(|id| *id >= 0) {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                state.context_replies.lock().expect("context replies").insert(id.clone(), sender);
                state.broadcast(Outbound::RefreshContext { id: id.clone(), url: request.url.clone(), page_url: request.page_url.clone(), tab_id });
                let refreshed = tokio::time::timeout(Duration::from_secs(8), receiver).await;
                state.context_replies.lock().expect("context replies").remove(&id);
                match refreshed {
                    Ok(Ok(context)) => context,
                    _ => Err("Browser session yenilenemedi. Video sayfasını açıp extension’dan tekrar gönder.".into()),
                }
            } else { Ok(extractor_context(&request)) };
            match context {
                Ok(context) => hazar_engine::ytdlp::download(&request.url, &dest, cancel.clone(), request.expected_sha256.as_deref(), request.speed_limit_bps, &context, tx).await
                .map(|(size, digest)| (dest.clone(), size, digest, started.elapsed().as_millis() as u64))
                .map_err(|error| { if matches!(error, hazar_engine::Error::Cancelled) { error.to_string() } else { let issue = hazar_engine::ytdlp::diagnostic(&error.to_string()); format!("[{}] {}", issue.code, issue.message) } }),
                Err(reason) => Err(reason),
            }
        }
        GrabKind::File => {
            let mut opts = DownloadOptions::new(&request.url, &dest)
                .connections(connections)
                .cancel_flag(cancel.clone())
                .headers(headers.clone());
            if let Some(sha) = request.expected_sha256.clone() {
                opts = opts.sha256(sha);
            }
            if let Some(limit) = request.speed_limit_bps {
                opts = opts.speed_limit(limit);
            }
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
            // Eklenti playlist'i tarayıcıda zaten çektiyse segment listesini kullan:
            // token'lar tek kullanımlık olabiliyor, app manifest'i tekrar isteyince 404 alıyor.
            let segments = request.segments.clone().filter(|list| !list.is_empty());
            let opts = HlsOptions {
                manifest: request.url.clone(),
                segments,
                base_url: request
                    .frame_url
                    .clone()
                    .or_else(|| request.page_url.clone()),
                output: dest.clone(),
                connections,
                user_agent,
                headers,
                expected_sha256: request.expected_sha256.clone(),
                cancel: Some(cancel.clone()),
            };
            hazar_engine::hls::download_hls_captured(opts, request.manifest.as_deref(), Some(tx))
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
        GrabKind::Dash => {
            let opts = hazar_engine::DashOptions {
                manifest: request.url.clone(),
                output: dest.clone(),
                connections,
                user_agent,
                headers,
                expected_sha256: request.expected_sha256.clone(),
                cancel: Some(cancel.clone()),
            };
            hazar_engine::dash::download_dash_captured(opts, request.manifest.as_deref(), Some(tx))
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
            let reason = redact_error(&reason);
            // Nedeni log'a da yaz (URL zaten engine hatasında var, sorgu değil).
            eprintln!("hazar: grab {id} failed: {reason}");
            state.update(&id, |entry| {
                entry.state = if reason.contains("cancelled") {
                    if state.paused.lock().expect("paused").remove(&id) {
                        "paused".into()
                    } else {
                        "cancelled".into()
                    }
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

/// Tarayıcıdan gelen segment gövdesini diske yazar; son parçada birleştirir.
fn on_bytes(state: &Arc<CaptureState>, app: &AppHandle, bytes: hazar_localapi::Bytes) {
    if state.removed.lock().expect("removed").contains(&bytes.stream_id) {
        state.broadcast(Outbound::Failed { id: bytes.stream_id, reason: "Download listeden silindi".into() });
        return;
    }
    if state.queue().iter().any(|entry| entry.id == bytes.stream_id && matches!(entry.state.as_str(), "done" | "assembling")) {
        state.broadcast(Outbound::BytesAck { stream_id: bytes.stream_id, index: bytes.index });
        return;
    }
    if bytes.stream_id.is_empty()
        || bytes.stream_id.len() > 128
        || !bytes
            .stream_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || bytes.total == 0
        || bytes.total > 20000
        || bytes.audio_start.is_some_and(|start| start == 0 || start >= bytes.total)
        || bytes.index >= bytes.total
        || bytes.data_b64.len() > 24 * 1024 * 1024
    {
        state.broadcast(Outbound::Failed {
            id: bytes.stream_id,
            reason: "invalid capture chunk".into(),
        });
        return;
    }
    let cancel = state.cancels.lock().expect("cancels")
        .entry(bytes.stream_id.clone()).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone();
    if cancel.load(Ordering::Relaxed) {
        state.broadcast(Outbound::Failed { id: bytes.stream_id, reason: "Download durduruldu".into() });
        return;
    }
    let settings = state.settings();
    let dir = settings
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    let name = bytes
        .filename
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{}.ts", bytes.stream_id));
    let name = sanitize(&name);
    let stream_dir = Path::new(&dir).join(format!(".{}.capture", bytes.stream_id));

    if let Err(error) = std::fs::create_dir_all(&stream_dir) {
        eprintln!("hazar: capture dir failed: {error}");
        return;
    }

    let descriptor = stream_dir.join("capture.json");
    let descriptor_value = serde_json::json!({ "total": bytes.total, "name": name, "audio_start": bytes.audio_start });
    if let Ok(raw) = std::fs::read(&descriptor) {
        if serde_json::from_slice::<serde_json::Value>(&raw).ok().as_ref() != Some(&descriptor_value) {
            state.broadcast(Outbound::Failed { id: bytes.stream_id, reason: "capture plan changed".into() }); return;
        }
    } else if std::fs::write(&descriptor, descriptor_value.to_string()).is_err() { return; }

    let Some(data) = base64_decode(&bytes.data_b64) else {
        eprintln!("hazar: bad base64 chunk for {}", bytes.stream_id);
        return;
    };
    if let Err(error) = hazar_engine::hls::validate_media_body(&data) {
        state.broadcast(Outbound::Failed { id: bytes.stream_id.clone(), reason: error.to_string() });
        state.update(&bytes.stream_id, |entry| { entry.state = "failed".into(); entry.error = Some(error.to_string()); });
        state.emit_queue(app);
        return;
    }
    let part = stream_dir.join(format!("part-{:05}.bin", bytes.index));
    let duplicate = part.exists();
    if duplicate && std::fs::read(&part).ok().as_deref() != Some(data.as_slice()) {
        state.broadcast(Outbound::Failed { id: bytes.stream_id, reason: "capture chunk changed".into() }); return;
    }
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let tmp = part.with_extension("partial");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&data)?; file.sync_all()?; drop(file);
        if !duplicate { std::fs::rename(tmp, &part)?; } else { std::fs::remove_file(tmp)?; }
        Ok(())
    };
    if let Err(error) = write() {
        eprintln!("hazar: capture write failed: {error}");
        return;
    }

    state.broadcast(Outbound::BytesAck {
        stream_id: bytes.stream_id.clone(),
        index: bytes.index,
    });
    // Kuyruk girdisi (ilk parçada oluştur).
    if !state.queue().iter().any(|e| e.id == bytes.stream_id) {
        state.queue.lock().expect("queue lock").upsert(QueueEntry {
            id: bytes.stream_id.clone(),
            url: bytes.url.clone().unwrap_or_default(),
            kind: "hls".to_string(),
            state: "downloading".into(),
            source: "browser-capture".into(),
            written: 0,
            total: 0,
            filename: Some(name.clone()),
            path: None,
            error: None,
        });
    }

    let written = state
        .queue
        .lock()
        .expect("queue lock")
        .snapshot()
        .iter()
        .find(|entry| entry.id == bytes.stream_id)
        .map(|entry| entry.written)
        .unwrap_or(0)
        + if duplicate { 0 } else { data.len() as u64 };
    state.update(&bytes.stream_id, |entry| {
        entry.written = written; entry.state = "downloading".into(); entry.error = None;
    });
    state.broadcast(Outbound::Progress {
        id: bytes.stream_id.clone(),
        phase: "browser-capture".to_string(),
        written,
        total: 0,
        connections: 1,
        speed_bps: 0,
    });
    state.emit_queue(app);

    // Son parça geldiyse sırayla birleştir.
    let done = (0..bytes.total).all(|i| stream_dir.join(format!("part-{i:05}.bin")).exists());
    if !done {
        return;
    }

    let mut out_path = state.queue().iter().find(|e| e.id == bytes.stream_id)
        .and_then(|e| e.path.as_ref()).map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&dir).join(&name));
    let mut counter = 1;
    while out_path.exists() && counter < 1000 {
        out_path = Path::new(&dir).join(format!("{}-{counter}.ts", name.trim_end_matches(".ts")));
        counter += 1;
    }
    // Only one finalizer may assemble/mux a stream, even after a repeated ACK.
    let marker = stream_dir.join("assembling.lock");
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&marker).is_err() {
        return;
    }
    let state = Arc::clone(state);
    let app = app.clone();
    // Keep the bridge responsive while FFmpeg muxes the captured tracks.
    state.update(&bytes.stream_id, |entry| entry.state = "assembling".into());
    state.emit_queue(&app);
    tokio::spawn(async move {
        let result: Result<u64, String> = async {
            let assembling = stream_dir.join("video.ts");
            let video_end = bytes.audio_start.unwrap_or(bytes.total);
            assemble_capture_parts(&stream_dir, 0, video_end, &assembling, &cancel).await?;
            if let Some(audio_start) = bytes.audio_start {
                let audio = stream_dir.join("audio.ts");
                assemble_capture_parts(&stream_dir, audio_start, bytes.total, &audio, &cancel).await?;
                hazar_engine::media::mux(&assembling, &audio, &out_path, Some(cancel.clone()))
                    .await.map_err(|error| error.to_string())?;
            } else {
                if cancel.load(Ordering::Relaxed) { return Err("Download durduruldu".into()); }
                tokio::fs::rename(&assembling, &out_path).await.map_err(|e| e.to_string())?;
            }
            let size = tokio::fs::metadata(&out_path).await.map_err(|e| e.to_string())?.len();
            let _ = tokio::fs::remove_dir_all(&stream_dir).await;
            Ok(size)
        }.await;
        match result {
            Ok(size) => {
                let path = out_path.display().to_string();
                state.update(&bytes.stream_id, |entry| {
                    entry.state = "done".into();
                    entry.written = size;
                    entry.total = size;
                    entry.path = Some(path.clone());
                    entry.error = None;
                });
                state.broadcast(Outbound::Finished {
                    id: bytes.stream_id.clone(), path, size,
                    sha256: None, elapsed_ms: 0,
                });
            }
            Err(reason) => {
                let _ = tokio::fs::remove_file(&marker).await;
                state.update(&bytes.stream_id, |entry| {
                    entry.state = "failed".into(); entry.error = Some(reason.clone());
                });
                state.broadcast(Outbound::Failed { id: bytes.stream_id.clone(), reason });
            }
        }
        state.cancels.lock().expect("cancels").remove(&bytes.stream_id);
        state.emit_queue(&app);
        state.emit_status(&app);
    });
}

async fn assemble_capture_parts(dir: &Path, start: u32, end: u32, output: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut out = tokio::fs::File::create(output).await.map_err(|e| e.to_string())?;
    for index in start..end {
        if cancel.load(Ordering::Relaxed) { return Err("Download durduruldu".into()); }
        let mut part = tokio::fs::File::open(dir.join(format!("part-{index:05}.bin")))
            .await.map_err(|e| e.to_string())?;
        tokio::io::copy(&mut part, &mut out).await.map_err(|e| e.to_string())?;
    }
    out.sync_all().await.map_err(|e| e.to_string())
}

/// Küçük, bağımlılıksız base64 çözücü (segment gövdeleri için).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut lookup = [255u8; 256];
    for (index, byte) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        lookup[*byte as usize] = index as u8;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in input.bytes() {
        if byte == b'=' || byte == 10 || byte == 13 {
            continue;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
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
    let last = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download");
    let name = sanitize(last);
    if name.ends_with(".mpd") {
        return name.replace(".mpd", ".mp4");
    }
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

fn redact_error(message: &str) -> String {
    message.split_whitespace().map(|word| {
        if word.contains("http://") || word.contains("https://") {
            word.split(['?', '#']).next().unwrap_or(word).to_string()
        } else { word.to_string() }
    }).collect::<Vec<_>>().join(" ")
}

/// Log için URL etiketi: host + path, sorgu/token atılır.
fn url_label(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let trimmed = without_query
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    trimmed.chars().take(120).collect()
}

/// Engine'e verilecek başlıkları hazırlar: eklentinin yakaladığı başlıklar +
/// ayrı alanlarda gelen Referer/Cookie. Var olan başlık tekrar eklenmez
/// (büyük/küçük harf yok sayılır).
fn capture_headers(
    headers: &[(String, String)],
    referer: Option<&str>,
    cookie: Option<&str>,
) -> Vec<(String, String)> {
    let has = |list: &[(String, String)], name: &str| {
        list.iter().any(|(key, _)| key.eq_ignore_ascii_case(name))
    };

    let mut out: Vec<(String, String)> = headers.to_vec();
    if let Some(referer) = referer.filter(|value| !value.trim().is_empty()) {
        if !has(&out, "referer") {
            out.push(("Referer".to_string(), referer.to_string()));
        }
    }
    if let Some(cookie) = cookie.filter(|value| !value.trim().is_empty()) {
        if !has(&out, "cookie") {
            out.push(("Cookie".to_string(), cookie.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_headers_adds_referer_and_cookie() {
        let out = capture_headers(&[], Some("https://player.example/iframe"), Some("a=1"));
        assert!(out
            .iter()
            .any(|(key, value)| key == "Referer" && value == "https://player.example/iframe"));
        assert!(out
            .iter()
            .any(|(key, value)| key == "Cookie" && value == "a=1"));
    }

    #[test]
    fn capture_headers_keeps_captured_headers_and_does_not_duplicate() {
        let existing = vec![
            ("referer".to_string(), "https://kept.example".to_string()),
            ("Cookie".to_string(), "kept=1".to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ];
        let out = capture_headers(&existing, Some("https://other.example"), Some("other=2"));
        assert!(out
            .iter()
            .any(|(key, value)| key == "accept" && value == "*/*"));
        assert_eq!(
            out.iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("referer"))
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("cookie"))
                .count(),
            1
        );
        assert!(out
            .iter()
            .any(|(key, value)| key == "referer" && value == "https://kept.example"));
    }

    #[test]
    fn url_label_strips_query_and_fragment() {
        assert_eq!(
            url_label("https://four.dplayer82.site/hls/x/seg-1.ts?token=SECRET#frag"),
            "four.dplayer82.site/hls/x/seg-1.ts"
        );
        assert!(!url_label("https://a.example/b?t=1").contains('1'));
    }

    #[test]
    fn capture_headers_skips_empty_values() {
        let out = capture_headers(&[], Some("   "), Some(""));
        assert!(out.is_empty(), "boş değerler eklenmemeli: {out:?}");
    }
}

fn extractor_context(request: &GrabRequest) -> hazar_engine::ytdlp::BrowserContext {
    request.browser_context.clone().unwrap_or_else(|| hazar_engine::ytdlp::BrowserContext {
        cookies: request.browser_cookies.clone(), referer: request.referer.clone(), user_agent: request.user_agent.clone(),
        ..Default::default()
    })
}
