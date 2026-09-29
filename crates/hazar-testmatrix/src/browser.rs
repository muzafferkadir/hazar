//! Headless Chrome + the real extension.
//!
//! The offline matrix proves the *engine* and the *resolver*. This layer proves
//! the browser path: a real Chromium loads `extension/` unpacked, the extension
//! connects to the loopback API, intercepts a download and hands it over.
//!
//! Deliberately light: a single headless Chromium with its own throwaway
//! profile, no Node, no Playwright — just CDP over a WebSocket (which we
//! already depend on) and our own fixture server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hazar_engine::resolve::MediaKind;
use hazar_localapi::{Inbound, LocalApiConfig};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::blob_public as blob;
use crate::server::{self, Route};
use crate::{Report, Row, Status};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(25);
const GRAB_TIMEOUT: Duration = Duration::from_secs(30);

/// Find a Chromium-family browser. `HAZAR_CHROME` / `CHROME_PATH` win.
pub fn detect_chrome() -> Option<PathBuf> {
    for key in ["HAZAR_CHROME", "CHROME_PATH"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value);
            if path.exists() {
                return Some(path);
            }
        }
    }
    const CANDIDATES: [&str; 8] = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
    ];
    // Chrome for Testing (Playwright / Puppeteer caches) first: stable Chrome
    // 137+ ignores `--load-extension`, CfT still honours it.
    let home = std::env::var("HOME").unwrap_or_default();
    let cache_roots = [
        format!("{home}/Library/Caches/ms-playwright"),
        format!("{home}/.cache/ms-playwright"),
        format!("{home}/.cache/puppeteer"),
    ];
    for root in cache_roots {
        if let Some(path) = find_in_cache(Path::new(&root), 0) {
            return Some(path);
        }
    }

    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.exists())
}

/// Depth-limited search for a Chromium binary inside a browser cache directory.
fn find_in_cache(dir: &Path, depth: usize) -> Option<PathBuf> {
    const NAMES: [&str; 4] = [
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Chromium.app/Contents/MacOS/Chromium",
        "chrome-linux64/chrome",
        "chrome",
    ];
    for name in NAMES {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    if depth >= 3 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(found) = find_in_cache(&path, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn extension_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extension")
}

/// Chrome derives an unpacked extension's id from sha256 of its absolute path,
/// mapping the first 16 bytes' nibbles to `a..p`. Knowing it lets us attach to
/// **our** service worker instead of whichever extension Chrome starts first.
fn extension_id() -> String {
    use sha2::{Digest, Sha256};
    let path = extension_dir();
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| {
            let high = b'a' + (byte >> 4);
            let low = b'a' + (byte & 0x0f);
            format!("{}{}", high as char, low as char)
        })
        .collect()
}

/// True for our extension's MV3 service worker target.
fn is_hazar_worker(url: &str) -> bool {
    let expected = format!("chrome-extension://{}/src/background.js", extension_id());
    url == expected || url.ends_with("/src/background.js")
}

struct Chrome {
    child: Child,
    profile: PathBuf,
}

impl Chrome {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

async fn launch_chrome(binary: &Path, port: u16, proxy: Option<&str>, headless: bool) -> Result<Chrome, String> {
    let profile = std::env::temp_dir().join(format!(
        "hazar-chrome-{}-{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(&profile).map_err(|error| error.to_string())?;
    let extension = extension_dir();

    let mut command = Command::new(binary);
    if headless {
        command.arg("--headless=new");
    }
    let child = command
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--load-extension={}", extension.display()))
        .arg(format!("--disable-extensions-except={}", extension.display()))
        // Chrome 137+ removed `--load-extension` for stable builds; the removal
        // is behind a feature flag, so opt out of it explicitly.
        .arg("--disable-features=DisableLoadExtensionCommandLineSwitch")
        .args(
            proxy
                .map(|proxy| vec![format!("--proxy-server={proxy}")])
                .unwrap_or_default(),
        )
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--mute-audio",
            "--window-size=800,600",
        ])
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot launch {}: {error}", binary.display()))?;

    Ok(Chrome { child, profile })
}

async fn cdp_websocket(port: u16, client: &reqwest::Client) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
        {
            Ok(response) => {
                if let Ok(bytes) = response.bytes().await {
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        if let Some(url) = value.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                            return Ok(url.to_string());
                        }
                    }
                }
            }
            Err(_) => {}
        }
        if Instant::now() > deadline {
            return Err("chrome devtools endpoint never came up".to_string());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct Cdp {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self, String> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            socket,
            next_id: 1,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.call_in(None, method, params).await
    }

    /// `session` targets a page/worker session created by `Target.attachToTarget`.
    async fn call_in(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut payload = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            payload["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::text(payload.to_string()))
            .await
            .map_err(|error| error.to_string())?;

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("{method}: timed out"));
            }
            let next = tokio::time::timeout(remaining, self.socket.next()).await;
            match next {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: Value = serde_json::from_str(text.as_ref())
                        .map_err(|error| error.to_string())?;
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            return Err(format!("{method}: {error}"));
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(error))) => return Err(format!("{method}: {error}")),
                Ok(None) => return Err(format!("{method}: socket closed")),
                Err(_) => return Err(format!("{method}: timed out")),
            }
        }
    }
}

/// Read the extension's own debug state out of its service worker.
async fn extension_debug(port: u16, client: &reqwest::Client) -> Option<String> {
    let websocket = cdp_websocket(port, client).await.ok()?;
    let mut cdp = Cdp::connect(&websocket).await.ok()?;
    let targets = cdp.call("Target.getTargets", json!({})).await.ok()?;
    let list = targets.get("targetInfos").and_then(Value::as_array)?;
    let worker = list.iter().find(|target| {
        target
            .get("url")
            .and_then(Value::as_str)
            .map(|url| url.starts_with("chrome-extension://") && is_hazar_worker(url))
            .unwrap_or(false)
    })?;
    let target_id = worker.get("targetId").and_then(Value::as_str)?;
    let attached = cdp
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await
        .ok()?;
    let session = attached.get("sessionId").and_then(Value::as_str)?;
    let evaluated = cdp
        .call_in(
            Some(session),
            "Runtime.evaluate",
            json!({
                "expression": "JSON.stringify(typeof __hazarDebug === 'function' ? __hazarDebug() : {error:'no debug hook'})",
                "returnByValue": true,
            }),
        )
        .await
        .ok()?;
    evaluated
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Browser-layer rows. Never fails the matrix because of a missing browser:
/// an environment without a usable Chromium reports SKIP with the reason.
pub async fn run_rows() -> Vec<Row> {
    let started = Instant::now();
    let mut rows: Vec<Row> = Vec::new();

    let Some(binary) = detect_chrome() else {
        rows.push(skip_row(
            "browser-extension-handshake",
            "no chromium found (set HAZAR_CHROME)",
        ));
        rows.push(skip_row("browser-download-takeover", "no chromium found"));
        return rows;
    };

    let payload = blob(77, 640 * 1024);
    let fixture = server::serve(vec![
        Route::file("/browser/clip.mp4", payload.clone()).tweak(|route| {
            route.headers.push((
                "Content-Disposition".to_string(),
                "attachment; filename=\"clip.mp4\"".to_string(),
            ));
        }),
        Route::html(
            "/browser/page.html",
            "<html><body><a href=\"/browser/clip.mp4\">download</a></body></html>".to_string(),
        ),
    ])
    .await
    .expect("fixture server");

    // The extension's default port list is 8722-8730, so bind inside it.
    let api_config = LocalApiConfig {
        ports: (8722..=8730).collect(),
        ..Default::default()
    };
    let (api, mut inbound) = match hazar_localapi::server::start(api_config).await {
        Ok(pair) => pair,
        Err(error) => {
            rows.push(skip_row(
                "browser-extension-handshake",
                &format!("loopback API unavailable: {error}"),
            ));
            rows.push(skip_row("browser-download-takeover", "loopback API unavailable"));
            return rows;
        }
    };

    let Some(port) = free_port() else {
        rows.push(skip_row("browser-extension-handshake", "no free port"));
        return rows;
    };

    let mut chrome = match launch_chrome(&binary, port, None, true).await {
        Ok(chrome) => chrome,
        Err(error) => {
            rows.push(skip_row("browser-extension-handshake", &error));
            rows.push(skip_row("browser-download-takeover", &error));
            api.shutdown();
            return rows;
        }
    };

    let client = match hazar_engine::default_client(None) {
        Ok(client) => client,
        Err(error) => {
            rows.push(skip_row("browser-extension-handshake", &error.to_string()));
            chrome.kill();
            api.shutdown();
            return rows;
        }
    };

    // 1) handshake: the extension must find us and complete `hello`
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut connected = false;
    while Instant::now() < handshake_deadline {
        if api.client_count() > 0 {
            connected = true;
            break;
        }
        // Drain anything that arrives while we wait.
        if let Ok(Some(_message)) = tokio::time::timeout(Duration::from_millis(200), inbound.recv())
            .await
        {
            if api.client_count() > 0 {
                connected = true;
                break;
            }
        }
    }

    if !connected {
        rows.push(skip_row(
            "browser-extension-handshake",
            "chromium started but the extension never connected (Chrome 137+ needs Chrome for Testing, or set HAZAR_CHROME)",
        ));
        rows.push(skip_row(
            "browser-download-takeover",
            "extension not connected",
        ));
        chrome.kill();
        api.shutdown();
        return rows;
    }
    rows.push(Row {
        name: "browser-extension-handshake".to_string(),
        status: Status::Pass,
        kind: "bridge".to_string(),
        strategy: "websocket".to_string(),
        found: Vec::new(),
        detail: format!("extension connected on 127.0.0.1:{}", api.port),
        ms: started.elapsed().as_millis(),
    });

    // 2) drive Chrome to the download URL and wait for the extension to hand it over
    let page_url = fixture.url("/browser/page.html");
    let attachment_url = fixture.url("/browser/clip.mp4");

    let navigation = async {
        let websocket = cdp_websocket(port, &client).await?;
        let mut cdp = Cdp::connect(&websocket).await?;

        let target = cdp
            .call("Target.createTarget", json!({ "url": page_url }))
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session.is_empty() {
            return Err("could not attach to the page target".to_string());
        }

        // Let the page settle, then click the link like a user would.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let click = cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({
                    "expression": "(function(){var a=document.querySelector('a');if(!a)return 'no-link';a.click();return 'clicked';})()",
                    "returnByValue": true,
                }),
            )
            .await?;
        eprintln!(
            "browser: {}",
            click
                .get("result")
                .and_then(|result| result.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        );

        // Fallback: navigate straight to the attachment (direct-link case).
        cdp.call("Target.createTarget", json!({ "url": attachment_url }))
            .await?;
        Ok::<String, String>(target_id)
    };

    let grab = async {
        let deadline = Instant::now() + GRAB_TIMEOUT;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, inbound.recv()).await {
                Ok(Some(message)) => {
                    if let Inbound::Grab(grab) = message.message {
                        // Ack like the app does, so the extension keeps the
                        // download instead of resuming it in the browser.
                        api.broadcast(hazar_localapi::Outbound::GrabAck {
                            id: grab.id.clone(),
                            state: "downloading".to_string(),
                            message: None,
                        });
                        eprintln!("browser: grab from extension → {}", grab.request.url);
                        return Some(grab);
                    }
                    eprintln!("browser: ignoring inbound message");
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        None
    };

    let (navigation_result, grab_result) = tokio::join!(navigation, grab);
    if let Err(error) = navigation_result {
        // Keep going: the grab may still have arrived from a previous attempt.
        eprintln!("browser: cdp navigation issue: {error}");
    }

    match grab_result {
        Some(grab) => {
            let kind = grab.request.kind;
            let url = grab.request.url.clone();
            let mut headers = grab.request.headers.clone();
            if let Some(cookie) = grab.request.cookie.clone() {
                headers.push(("Cookie".to_string(), cookie));
            }
            let dest = std::env::temp_dir().join(format!("hazar-browser-{}.mp4", std::process::id()));
            let result = match kind {
                hazar_localapi::GrabKind::File => {
                    let options = hazar_engine::DownloadOptions::new(url.clone(), dest.clone())
                        .connections(4)
                        .headers(headers);
                    match hazar_engine::Downloader::new(options) {
                        Ok(downloader) => downloader.run().await.map(|_| ()).map_err(|e| e.to_string()),
                        Err(error) => Err(error.to_string()),
                    }
                }
                other => Err(format!("unexpected grab kind for this row: {other:?}")),
            };
            let status = match (&result, std::fs::read(&dest)) {
                (Ok(()), Ok(bytes)) if bytes == payload => Status::Pass,
                (Ok(()), Ok(bytes)) => {
                    rows.push(Row {
                        name: "browser-download-takeover".to_string(),
                        status: Status::Fail,
                        kind: "file".to_string(),
                        strategy: "extension".to_string(),
                        found: Vec::new(),
                        detail: format!(
                            "byte mismatch: got {} expected {}",
                            bytes.len(),
                            payload.len()
                        ),
                        ms: started.elapsed().as_millis(),
                    });
                    chrome.kill();
                    api.shutdown();
                    let _ = std::fs::remove_file(&dest);
                    return rows;
                }
                (Err(error), _) => {
                    rows.push(Row {
                        name: "browser-download-takeover".to_string(),
                        status: Status::Fail,
                        kind: "file".to_string(),
                        strategy: "extension".to_string(),
                        found: Vec::new(),
                        detail: error.clone(),
                        ms: started.elapsed().as_millis(),
                    });
                    chrome.kill();
                    api.shutdown();
                    return rows;
                }
                _ => Status::Fail,
            };
            rows.push(Row {
                name: "browser-download-takeover".to_string(),
                status,
                kind: "file".to_string(),
                strategy: "extension".to_string(),
                found: Vec::new(),
                detail: format!("grab → download → {} bytes match", payload.len()),
                ms: started.elapsed().as_millis(),
            });
            let _ = std::fs::remove_file(&dest);
        }
        None => {
            let hits = fixture.hits("/browser/clip.mp4");
            let debug = extension_debug(port, &client)
                .await
                .unwrap_or_else(|| "extension debug unavailable".to_string());
            eprintln!(
                "browser: fixture hits for clip.mp4 = {hits}, app clients = {}",
                api.client_count()
            );
            eprintln!("browser: extension state = {debug}");
            rows.push(Row {
                name: "browser-download-takeover".to_string(),
                status: Status::Fail,
                kind: "file".to_string(),
                strategy: "extension".to_string(),
                found: Vec::new(),
                detail: format!(
                    "no grab within the timeout (fixture requests: {hits})",
                ),
                ms: started.elapsed().as_millis(),
            })
        }
    }

    chrome.kill();
    api.shutdown();
    rows
}

fn skip_row(name: &str, reason: &str) -> Row {
    Row {
        name: name.to_string(),
        status: Status::Skip,
        kind: "browser".to_string(),
        strategy: "-".to_string(),
        found: Vec::new(),
        detail: reason.to_string(),
        ms: 0,
    }
}

/// Unused today, kept so the browser layer can reuse the fixture server.
pub fn media_kind_name(kind: MediaKind) -> &'static str {
    kind.as_str()
}

/// Silence an unused-import warning when the browser layer is trimmed.
pub fn _touch(_: &Report) {}

// ---------------------------------------------------------------------------
// Reusable headless-browser session (used by the live layer)
// ---------------------------------------------------------------------------

/// Ad/tracker URL patterns handed to Chromium's network stack.
const AD_BLOCK_PATTERNS: [&str; 14] = [
    "*doubleclick.net*", "*googlesyndication.com*", "*googleadservices.com*",
    "*adsystem.com*", "*adservice.google*", "*popads.net*", "*propellerads.com*",
    "*taboola.com*", "*outbrain.com*", "*adserver*", "*/ads/*", "*/preroll/*",
    "*analytics.google.com*", "*googletagmanager.com*",
];

/// A media candidate the extension sniffed inside a real browser.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtensionCandidate {
    pub url: String,
    pub kind: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, rename = "isManifest")]
    pub is_manifest: bool,
    #[serde(default, rename = "pageUrl")]
    pub page_url: Option<String>,
    #[serde(default, rename = "frameUrl")]
    pub frame_url: Option<String>,
    #[serde(default)]
    pub segments: Option<Vec<String>>,
}

/// Headless Chromium + the unpacked extension, driven over CDP.
///
/// Hard sites build their manifest inside JavaScript, so the plain HTTP
/// resolver cannot see it. This session runs the page for real and asks the
/// extension what it sniffed.
pub struct BrowserSession {
    chrome: Chrome,
    cdp: Cdp,
    worker: Option<String>,
    page: Option<String>,
    /// Keeps the local proxy relay alive while Chrome runs (authenticated
    /// upstream proxies cannot be handed to Chrome directly).
    relay: Option<std::sync::Arc<crate::relay::Relay>>,
    /// Last page interaction result (`played` / `clicked` / `consent` / …).
    last_nudge: String,
    /// Escalating nudge step: consent → server → click into the frame → play.
    nudge_step: u32,
}

impl BrowserSession {
    pub async fn launch() -> Result<Self, String> {
        Self::launch_with(true).await
    }

    /// `use_proxy = false` ignores `HAZAR_PROXY` (sites that block proxy exits).
    pub async fn launch_configured(headless: bool, use_proxy: bool) -> Result<Self, String> {
        if use_proxy {
            return Self::launch_with(headless).await;
        }
        let saved = std::env::var("HAZAR_PROXY").ok();
        // Scope the removal to this launch only.
        std::env::remove_var("HAZAR_PROXY");
        let result = Self::launch_with(headless).await;
        if let Some(value) = saved {
            std::env::set_var("HAZAR_PROXY", value);
        }
        result
    }

    /// `headless = false` opens a visible window (used by capture mode, where the
    /// user clicks through consent/ad/player steps by hand).
    pub async fn launch_with(headless: bool) -> Result<Self, String> {
        let binary = detect_chrome().ok_or_else(|| "no chromium found (set HAZAR_CHROME)".to_string())?;
        let port = free_port().ok_or_else(|| "no free port".to_string())?;

        // Chromium cannot authenticate to a proxy from the command line, so an
        // authenticated upstream gets a credentials-injecting local relay.
        let upstream = hazar_engine::proxy_from_env();
        let (chrome_proxy, relay) = match upstream.as_deref() {
            Some(url) if url.contains('@') => {
                let relay = crate::relay::start(url).await?;
                (Some(relay.address()), Some(relay))
            }
            Some(url) => (Some(url.to_string()), None),
            None => (None, None),
        };

        let chrome = launch_chrome(&binary, port, chrome_proxy.as_deref(), headless).await?;
        let client = hazar_engine::default_client(None).map_err(|error| error.to_string())?;
        let websocket = cdp_websocket(port, &client).await?;
        let cdp = Cdp::connect(&websocket).await?;
        let mut session = Self {
            chrome,
            cdp,
            worker: None,
            page: None,
            relay,
            last_nudge: String::new(),
            nudge_step: 0,
        };

        // Warm the extension's service worker *before* any navigation.
        //
        // MV3 only delivers events to listeners registered during the worker's
        // last run. On a fresh profile Chrome may not start the worker at launch,
        // so the first page's requests would be dropped — simulate what a real
        // browser session already has: a running worker. Listening to the event
        // stream needs the listener set to exist, hence the warm-up here.
        let _ = session
            .cdp
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match session
                .evaluate_worker("typeof __hazarDebug === 'function'")
                .await
            {
                Ok(value) if value.as_bool() == Some(true) => break,
                _ if Instant::now() > deadline => break,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
        Ok(session)
    }

    pub fn kill(&mut self) {
        self.chrome.kill();
        self.relay = None;
    }

    /// Attach to the extension's service worker once.
    async fn worker_session(&mut self) -> Result<String, String> {
        if let Some(session) = &self.worker {
            return Ok(session.clone());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let targets = self.cdp.call("Target.getTargets", json!({})).await?;
            let list = targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let worker = list.iter().find(|target| {
                target
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|url| url.starts_with("chrome-extension://") && is_hazar_worker(url))
                    .unwrap_or(false)
            });
            if let Some(target) = worker {
                let target_id = target.get("targetId").and_then(Value::as_str).unwrap_or_default();
                let attached = self
                    .cdp
                    .call(
                        "Target.attachToTarget",
                        json!({ "targetId": target_id, "flatten": true }),
                    )
                    .await?;
                let session = attached
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "extension worker attach failed".to_string())?
                    .to_string();
                self.worker = Some(session.clone());
                return Ok(session);
            }
            if Instant::now() > deadline {
                return Err("extension service worker never appeared".to_string());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn evaluate_worker(&mut self, expression: &str) -> Result<Value, String> {
        let session = self.worker_session().await?;
        let evaluated = self
            .cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        Ok(evaluated
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Everything the extension sniffed so far, across all tabs.
    pub async fn candidates(&mut self) -> Result<Vec<ExtensionCandidate>, String> {
        self.candidates_matching(None).await
    }

    /// Only the candidates sniffed for the tab that shows `page_url`.
    ///
    /// Rows share one browser session, so a global read leaks traffic from the
    /// previous row (a red live row once "saw" the previous site's requests).
    pub async fn candidates_for(&mut self, page_url: &str) -> Result<Vec<ExtensionCandidate>, String> {
        self.candidates_matching(Some(page_url)).await
    }

    async fn candidates_matching(
        &mut self,
        page_url: Option<&str>,
    ) -> Result<Vec<ExtensionCandidate>, String> {
        let argument = match page_url {
            Some(url) => serde_json::to_string(url).map_err(|error| error.to_string())?,
            None => "undefined".to_string(),
        };
        let expression = format!(
            "(async () => JSON.stringify(typeof __hazarCandidates === 'function' ? await __hazarCandidates({argument}) : []))()"
        );
        let value = self.evaluate_worker(&expression).await?;
        let text = value.as_str().unwrap_or("[]");
        serde_json::from_str::<Vec<ExtensionCandidate>>(text).map_err(|error| error.to_string())
    }

    /// Navigate a fresh tab to `url` and keep it as the page context.
    pub async fn open(&mut self, url: &str) -> Result<(), String> {
        let target = self
            .cdp
            .call("Target.createTarget", json!({ "url": url }))
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| "page attach failed".to_string())?
            .to_string();
        // Block ad/tracker hosts so the player mounts faster and the extension
        // never mistakes an ad stream for the real one.
        let _ = self
            .cdp
            .call_in(Some(&session), "Network.enable", json!({}))
            .await;
        let _ = self
            .cdp
            .call_in(
                Some(&session),
                "Network.setBlockedURLs",
                json!({ "urls": AD_BLOCK_PATTERNS.map(String::from) }),
            )
            .await;

        self.page = Some(session);
        Ok(())
    }

    /// What the current page thinks it is (used for failure diagnostics).
    pub async fn page_state(&mut self) -> String {
        let Some(session) = self.page.clone() else {
            return "no page session".to_string();
        };
        let expression = r#"JSON.stringify({
            href: location.href,
            ready: document.readyState,
            title: document.title,
            body: (document.body ? document.body.innerHTML.length : -1),
            videos: document.querySelectorAll('video,audio,source').length
        })"#;
        match self
            .cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
            )
            .await
        {
            Ok(value) => value
                .get("result")
                .and_then(|result| result.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("page state unavailable")
                .to_string(),
            Err(error) => format!("page state error: {error}"),
        }
    }

    /// Poll the extension until a candidate of `kind` shows up.
    ///
    /// A manifest candidate appears the moment its response headers arrive, but
    /// its segment list is only attached after the extension has fetched the
    /// playlist. Prefer a candidate that already carries segments, with a short
    /// grace period so a playlist-less stream still reports something.
    pub async fn wait_for_candidate(
        &mut self,
        kind: &str,
        timeout: Duration,
        page_url: &str,
    ) -> Result<ExtensionCandidate, String> {
        const GRACE: Duration = Duration::from_secs(3);
        let deadline = Instant::now() + timeout;
        let mut seen = 0usize;
        let mut best: Option<(ExtensionCandidate, Instant)> = None;

        let mut iteration = 0usize;
        loop {
            iteration += 1;
            // Players only request the stream once playback starts (and after
            // consent banners are dismissed), so poke the page periodically.
            if iteration == 1 || iteration % 10 == 0 {
                self.nudge_player().await;
            }
            let mut candidates = match self.candidates_for(page_url).await {
                Ok(candidates) => candidates,
                Err(error) => return Err(format!("extension candidates unavailable: {error}")),
            };
            // Some players open the stream in a popup/new tab after the server
            // click; look there too when this tab has nothing of the wanted kind.
            if !candidates.iter().any(|candidate| candidate.kind == kind) {
                if let Ok(all) = self.candidates().await {
                    candidates.extend(
                        all.into_iter()
                            .filter(|candidate| candidate.kind == kind),
                    );
                }
            }
            seen = seen.max(candidates.len());

            for candidate in candidates.iter().filter(|candidate| candidate.kind == kind) {
                let has_segments = candidate
                    .segments
                    .as_ref()
                    .map(|segments| !segments.is_empty())
                    .unwrap_or(false);
                if has_segments || kind == "dash" {
                    return Ok(candidate.clone());
                }
                if best.is_none() {
                    best = Some((candidate.clone(), Instant::now()));
                }
            }

            if let Some((candidate, first_seen)) = &best {
                if first_seen.elapsed() >= GRACE {
                    return Ok(candidate.clone());
                }
            }

            if Instant::now() > deadline {
                if let Some((candidate, _)) = best {
                    return Ok(candidate);
                }
                let debug = self
                    .evaluate_worker(
                        "JSON.stringify(typeof __hazarDebug === 'function' ? __hazarDebug() : {})",
                    )
                    .await
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "no debug".to_string());
                let summary = serde_json::from_str::<Value>(&debug)
                    .ok()
                    .map(|value| {
                        let tail: Vec<String> = value
                            .get("log")
                            .and_then(Value::as_array)
                            .map(|lines| {
                                lines
                                    .iter()
                                    .rev()
                                    .take(3)
                                    .map(|line| line.as_str().unwrap_or("").to_string())
                                    .collect()
                            })
                            .unwrap_or_default();
                        format!(
                            "lib={} nudge={:?} counters={} urls={} log={}",
                            value.get("libLoaded").unwrap_or(&Value::Null),
                            self.last_nudge,
                            value.get("counters").unwrap_or(&Value::Null),
                            value.get("recentUrls").unwrap_or(&Value::Null),
                            tail.join(" | ")
                        )
                    })
                    .unwrap_or_else(|| debug.chars().take(300).collect());
                return Err(format!(
                    "no {kind} candidate from the browser after {}s ({} candidate(s) sniffed) · {}",
                    timeout.as_secs(),
                    seen,
                    summary
                ));
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Fetch the first 64 KiB of a segment **inside the page**, so the site's
    /// own cookies and referer apply. Cross-origin CDNs fall back to `no-cors`,
    /// where only "the request succeeded" can be asserted.
    /// Session of the cross-origin frame that owns `frame_url`, when Chrome exposes
    /// it as its own target (site isolation). Probing there sends the player's own
    /// Referer, which player CDNs require.
    async fn frame_session(&mut self, frame_url: &str) -> Option<String> {
        let host = url::Url::parse(frame_url).ok()?.host_str()?.to_string();
        let targets = self.cdp.call("Target.getTargets", json!({})).await.ok()?;
        let list = targets.get("targetInfos")?.as_array()?.to_vec();
        let found = list.iter().find(|target| {
            target.get("type").and_then(Value::as_str) == Some("iframe")
                && target
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|url| url.contains(&host))
                    .unwrap_or(false)
        })?;
        let target_id = found.get("targetId")?.as_str()?;
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
            .ok()?;
        attached
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub async fn probe_first_segment(
        &mut self,
        url: &str,
        frame_url: Option<&str>,
    ) -> Result<String, String> {
        let session = match frame_url {
            Some(frame) => self.frame_session(frame).await.or_else(|| self.page.clone()),
            None => self.page.clone(),
        }
        .ok_or_else(|| "no page open".to_string())?;
        let escaped = serde_json::to_string(url).map_err(|error| error.to_string())?;
        let expression = format!(
            r#"(async () => {{
                const url = {escaped};
                const common = {{headers: {{Range: "bytes=0-65535"}}, credentials: "include"}};
                try {{
                    const r = await fetch(url, common);
                    const b = await r.arrayBuffer();
                    return JSON.stringify({{status: r.status, bytes: b.byteLength}});
                }} catch (error) {{
                    try {{
                        const r = await fetch(url, {{mode: "no-cors", credentials: "include"}});
                        return JSON.stringify({{status: r.status || 0, opaque: true, type: r.type}});
                    }} catch (inner) {{
                        return JSON.stringify({{error: String(inner)}});
                    }}
                }}
            }})()"#
        );

        let evaluated = self
            .cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            )
            .await?;
        let raw = evaluated
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let parsed: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;

        if let Some(error) = parsed.get("error").and_then(Value::as_str) {
            return Err(format!("first segment failed in browser: {error}"));
        }
        if parsed.get("opaque").and_then(Value::as_bool).unwrap_or(false) {
            return Ok("first segment: fetched (opaque, cross-origin CDN)".to_string());
        }
        let status = parsed.get("status").and_then(Value::as_u64).unwrap_or(0);
        let bytes = parsed.get("bytes").and_then(Value::as_u64).unwrap_or(0);
        if bytes == 0 {
            return Err(format!("first segment returned 0 bytes (status {status})"));
        }
        let name = url.rsplit('/').next().unwrap_or(url);
        Ok(format!("first segment: {name} {bytes} bytes (status {status})"))
    }
}

/// What the page and the extension together say about a failed row.
#[derive(Debug, Clone, Default)]
pub struct PageDiagnosis {
    pub href: String,
    pub title: String,
    pub ready: String,
    pub videos: i64,
    pub iframes: i64,
    pub frame_srcs: Vec<String>,
    pub error_page: bool,
    /// Cloudflare (or similar) challenge scripts were requested.
    pub challenge: bool,
    pub counters: String,
}

impl PageDiagnosis {
    /// Short, human-readable signals for the matrix table.
    pub fn signals(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.error_page {
            out.push("navigation failed (dns/tls/blocked)");
        }
        if self.challenge {
            out.push("bot wall (challenge script seen)");
        }
        if self.title.to_ascii_lowercase().contains("404") {
            out.push("target url is 404");
        }
        if !self.error_page && self.videos == 0 && self.iframes > 0 && !self.challenge {
            out.push("player lives in an iframe (click into it?)");
        }
        if !self.error_page && self.videos == 0 && self.iframes == 0 && !self.challenge {
            out.push("no player on the page (home page? wrong target)");
        }
        out
    }

    pub fn describe(&self) -> String {
        let signals = self.signals();
        format!(
            "href={} title={:?} videos={} iframes={}{} {}",
            self.href,
            self.title.chars().take(60).collect::<String>(),
            self.videos,
            self.iframes,
            self.frame_srcs
                .first()
                .map(|src| format!(" first_frame={}", src.chars().take(80).collect::<String>()))
                .unwrap_or_default(),
            if signals.is_empty() {
                String::new()
            } else {
                format!("signals={}", signals.join(", "))
            }
        )
    }
}

impl BrowserSession {
    /// Read the page state plus what the extension saw, in one shot.
    pub async fn diagnose(&mut self) -> PageDiagnosis {
        let mut diagnosis = PageDiagnosis::default();

        if let Some(session) = self.page.clone() {
            let expression = r#"JSON.stringify({
                href: location.href,
                title: document.title,
                ready: document.readyState,
                videos: document.querySelectorAll('video,audio,source').length,
                iframes: document.querySelectorAll('iframe').length,
                frames: Array.from(document.querySelectorAll('iframe')).slice(0, 3).map(function (f) { return f.src || '(no src)'; })
            })"#;
            if let Ok(value) = self
                .cdp
                .call_in(
                    Some(&session),
                    "Runtime.evaluate",
                    json!({ "expression": expression, "returnByValue": true }),
                )
                .await
            {
                let raw = value
                    .get("result")
                    .and_then(|result| result.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                    diagnosis.href = parsed
                        .get("href")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    diagnosis.title = parsed
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    diagnosis.ready = parsed
                        .get("ready")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    diagnosis.videos = parsed.get("videos").and_then(Value::as_i64).unwrap_or(0);
                    diagnosis.iframes = parsed.get("iframes").and_then(Value::as_i64).unwrap_or(0);
                    diagnosis.frame_srcs = parsed
                        .get("frames")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                }
            }
        }

        if let Ok(value) = self
            .evaluate_worker(
                "JSON.stringify(typeof __hazarDebug === 'function' ? __hazarDebug() : {})",
            )
            .await
        {
            if let Some(raw) = value.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                    diagnosis.counters = parsed
                        .get("counters")
                        .map(|counters| counters.to_string())
                        .unwrap_or_default();
                    let urls: Vec<String> = parsed
                        .get("recentUrls")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    diagnosis.challenge = urls
                        .iter()
                        .any(|url| url.contains("challenge-platform") || url.contains("cdn-cgi/challenge"));
                }
            }
        }

        diagnosis.error_page = diagnosis.href.starts_with("chrome-error://");
        diagnosis
    }

    /// Wait until the page finishes a navigation or fails fast.
    pub async fn wait_for_navigation(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let diagnosis = self.diagnose().await;
            if diagnosis.error_page || diagnosis.ready == "complete" {
                return;
            }
            if Instant::now() > deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl BrowserSession {
    /// Dismiss a consent banner and start playback, like a user would.
    ///
    /// Without this, JS players never request the stream in headless mode (the
    /// extension then has nothing to sniff, even on a real media page). A
    /// programmatic `.click()` does **not** count as user activation, so when no
    /// player element is found a real CDP mouse event is dispatched instead.
    /// Iframe URLs currently on the page (the player usually lives in one).
    async fn player_frames(&mut self) -> Vec<String> {
        let Some(session) = self.page.clone() else {
            return Vec::new();
        };
        let expression = "JSON.stringify(Array.from(document.querySelectorAll('iframe')).map(function (f) { return f.src || ''; }).filter(Boolean))";
        self.cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("result")
                    .and_then(|result| result.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    /// Start playback *inside* the player iframe (its own video element / play
    /// button), which is where real players expect the user gesture.
    async fn nudge_inside_frames(&mut self) -> Option<String> {
        let frames = self.player_frames().await;
        for frame in frames {
            let Some(session) = self.frame_session(&frame).await else {
                continue;
            };
            let expression = r#"(function () {
                try {
                    const video = document.querySelector('video');
                    if (video) {
                        video.muted = true;
                        const played = video.play();
                        if (played && typeof played.catch === 'function') played.catch(() => {});
                        if (!video.paused) return JSON.stringify({ action: 'playing' });
                    }
                    for (const selector of [
                        'button[aria-label*="play" i]', '.play-button', '.vjs-big-play-button',
                        '[class*="playButton" i]', '[class*="play" i]'
                    ]) {
                        const button = document.querySelector(selector);
                        if (button) { button.click(); return JSON.stringify({ action: 'clicked' }); }
                    }
                    const box = (document.body || document.documentElement).getBoundingClientRect();
                    return JSON.stringify({
                        action: 'click',
                        x: Math.round(box.width / 2),
                        y: Math.round(box.height / 2)
                    });
                } catch (error) {
                    return JSON.stringify({ action: 'error', detail: String(error).slice(0, 60) });
                }
            })()"#;

            let parsed = self
                .cdp
                .call_in(
                    Some(&session),
                    "Runtime.evaluate",
                    json!({ "expression": expression, "returnByValue": true }),
                )
                .await
                .ok()
                .and_then(|value| {
                    value
                        .get("result")
                        .and_then(|result| result.get("value"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            let Some(parsed) = parsed else {
                continue;
            };
            let action = parsed
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match action {
                "playing" => return Some("frame:playing".to_string()),
                "clicked" => return Some("frame:clicked-play".to_string()),
                "click" => {
                    let x = parsed.get("x").and_then(Value::as_f64).unwrap_or(200.0);
                    let y = parsed.get("y").and_then(Value::as_f64).unwrap_or(150.0);
                    for kind in ["mousePressed", "mouseReleased"] {
                        let _ = self
                            .cdp
                            .call_in(
                                Some(&session),
                                "Input.dispatchMouseEvent",
                                json!({
                                    "type": kind, "x": x, "y": y, "button": "left",
                                    "clickCount": 1, "pointerType": "mouse"
                                }),
                            )
                            .await;
                    }
                    return Some(format!("frame:click@{x:.0},{y:.0}"));
                }
                _ => continue,
            }
        }
        None
    }

    pub async fn nudge_player(&mut self) -> String {
        let Some(session) = self.page.clone() else {
            return "no page".to_string();
        };

        self.nudge_step = self.nudge_step.saturating_add(1);
        // Escalate the way a user does: dismiss consent, pick a player server,
        // then click *into* the player frame and start playback.
        let mode = match self.nudge_step {
            1 => "server",
            2 => "frame",
            _ => "play",
        };

        let expression = format!(
            r#"(function (mode) {{
            try {{
                for (const selector of [
                    'button[aria-label*="accept" i]', 'button[aria-label*="consent" i]',
                    'button[class*="consent" i]', 'button[id*="consent" i]',
                    '[aria-label*="kabul" i]', '.fc-cta-consent'
                ]) {{
                    const button = document.querySelector(selector);
                    if (button) {{ button.click(); return JSON.stringify({{ action: 'consent' }}); }}
                }}

                if (mode === 'server') {{
                    const hint = /izle|player|sunucu|alternatif|altyaz|hd\b|watch|play/i;
                    for (const element of document.querySelectorAll('a, button, [role="button"], [onclick]')) {{
                        const label = (element.textContent || '').trim();
                        if (label && label.length < 32 && hint.test(label)) {{
                            element.click();
                            return JSON.stringify({{ action: 'server-click', label: label.slice(0, 30) }});
                        }}
                    }}
                }}

                const video = document.querySelector('video');
                if (video && mode === 'play') {{
                    video.muted = true;
                    const played = video.play();
                    if (played && typeof played.catch === 'function') played.catch(() => {{}});
                    if (!video.paused) return JSON.stringify({{ action: 'playing' }});
                }}

                const frame = document.querySelector('iframe');
                if (frame) {{
                    const box = frame.getBoundingClientRect();
                    return JSON.stringify({{
                        action: 'frame-click',
                        x: Math.round(box.left + box.width / 2),
                        y: Math.round(box.top + Math.min(box.height / 2, 200)),
                        label: (frame.src || '').slice(0, 40)
                    }});
                }}

                const player = document.querySelector(
                    '[class*="player" i], [id*="player" i], video-js, .video-js, main'
                );
                const rect = player ? player.getBoundingClientRect() : {{
                    left: 0, top: 0, width: window.innerWidth, height: window.innerHeight
                }};
                return JSON.stringify({{
                    action: 'nothing',
                    x: Math.round(rect.left + rect.width / 2),
                    y: Math.round(rect.top + Math.min(rect.height / 2, 260)),
                    target: player ? player.tagName : 'viewport'
                }});
            }} catch (error) {{
                return JSON.stringify({{ action: 'error', detail: String(error).slice(0, 60) }});
            }}
        }})({mode:?})"#
        );

        // Playback usually starts inside the player iframe, not on the top page.
        if mode == "play" {
            if let Some(result) = self.nudge_inside_frames().await {
                self.last_nudge = format!("step{}:{result}", self.nudge_step);
                return self.last_nudge.clone();
            }
        }

        let parsed = self
            .cdp
            .call_in(
                Some(&session),
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("result")
                    .and_then(|result| result.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());

        let Some(parsed) = parsed else {
            return "unavailable".to_string();
        };
        let action = parsed
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let result = match action.as_str() {
            "nothing" | "frame-click" => {
                let x = parsed.get("x").and_then(Value::as_f64).unwrap_or(400.0);
                let y = parsed.get("y").and_then(Value::as_f64).unwrap_or(300.0);
                let mut clicked = false;
                for kind in ["mousePressed", "mouseReleased"] {
                    clicked |= self
                        .cdp
                        .call_in(
                            Some(&session),
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1,
                                "pointerType": "mouse"
                            }),
                        )
                        .await
                        .is_ok();
                }
                if !clicked {
                    format!("{action} (dispatch failed)")
                } else if action == "frame-click" {
                    format!("frame-click@{x:.0},{y:.0}")
                } else {
                    format!("mouse-click@{x:.0},{y:.0}")
                }
            }
            "server-click" => parsed
                .get("label")
                .and_then(Value::as_str)
                .map(|label| format!("server-click:{label}"))
                .unwrap_or_else(|| "server-click".to_string()),
            other => other.to_string(),
        };

        self.last_nudge = format!("step{}:{result}", self.nudge_step);
        self.last_nudge.clone()
    }
}

/// Interactive capture: a visible browser the user drives, while we log what the
/// extension sniffs. Afterwards the log is converted into a deterministic fixture.
pub async fn capture(url: &str, seconds: u64) -> Result<(), String> {
    let mut session = BrowserSession::launch_with(false).await?;
    eprintln!("capture: navigated to {url} — reklamı geç, videoyu başlat, sonra pencereyi kapat");
    let _ = session.open(url).await?;

    let log_path = "/tmp/hazar-capture.jsonl";
    let _ = std::fs::write(log_path, "");
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut last_signature = String::new();

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let candidates = session.candidates_for(url).await.unwrap_or_default();
        let diagnosis = session.diagnose().await;
        let signature = format!("{}|{}|{}", candidates.len(), diagnosis.videos, diagnosis.iframes);
        if signature != last_signature {
            last_signature = signature;
            eprintln!(
                "capture: {} candidate(s) videos={} iframes={} href={}",
                candidates.len(),
                diagnosis.videos,
                diagnosis.iframes,
                diagnosis.href
            );
        }
        for candidate in &candidates {
            let line = format!(
                "{{\"at\":\"{}\",\"kind\":\"{}\",\"isManifest\":{},\"segments\":{},\"frame\":\"{}\",\"url\":\"{}\"}}",
                diagnosis.href,
                candidate.kind,
                candidate.is_manifest,
                candidate.segments.as_ref().map(|s| s.len()).unwrap_or(0),
                candidate.frame_url.as_deref().unwrap_or(""),
                candidate.url
            );
            if !std::fs::read_to_string(log_path).unwrap_or_default().contains(&line) {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
                    let _ = writeln!(file, "{line}");
                }
                eprintln!(
                    "capture: 🎯 {} [{}] segments={} {}",
                    candidate.kind,
                    if candidate.is_manifest { "manifest" } else { "media" },
                    candidate.segments.as_ref().map(|s| s.len()).unwrap_or(0),
                    candidate.url
                );
            }
        }
        if session.page_state().await.contains("no page session") {
            break;
        }
    }

    eprintln!("capture: bitti — log: {log_path}");
    session.kill();
    Ok(())
}
