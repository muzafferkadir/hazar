use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{Hello, Inbound, Outbound, Settings, SUBPROTOCOL};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not bind any port in {0:?}")]
    Bind(Vec<u16>),
    #[error("client did not complete the handshake")]
    Handshake,
    #[error("protocol mismatch: extension speaks {client}, app speaks {app}")]
    Version { client: u32, app: u32 },
}

pub type Result<T, E = ApiError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct LocalApiConfig {
    pub host: String,
    pub ports: Vec<u16>,
    pub app_name: String,
    pub app_version: String,
    pub protocol: u32,
    pub features: Vec<String>,
    pub settings: Settings,
    pub handshake_timeout: Duration,
}

impl Default for LocalApiConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            ports: (8722..=8730).collect(),
            app_name: "Hazar".to_string(),
            app_version: hazar_engine::VERSION.to_string(),
            protocol: hazar_engine::PROTOCOL_VERSION,
            features: vec![
                "grab".into(),
                "cancel".into(),
                "progress".into(),
                "hls".into(),
                "media_candidates".into(),
                "dash".into(),
                "bytes_ack".into(),
                "hls_audio_bytes".into(),
                "youtube".into(),
                "ytdlp".into(),
                "ytdlp_context".into(),
            ],
            settings: Settings::default(),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// One decoded message from a connected extension.
#[derive(Debug, Clone)]
pub struct ClientMessage {
    pub client_id: u64,
    pub peer: SocketAddr,
    pub session: String,
    pub message: Inbound,
}

pub struct ServerHandle {
    pub addr: SocketAddr,
    pub port: u16,
    outbound: broadcast::Sender<Outbound>,
    clients: Arc<AtomicU64>,
    shutdown: watch::Sender<bool>,
}

impl ServerHandle {
    /// Send a message to every connected client.
    pub fn broadcast(&self, message: Outbound) {
        if self.outbound.receiver_count() > 0 {
            let _ = self.outbound.send(message);
        }
    }

    pub fn client_count(&self) -> u64 {
        self.clients.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Bind the loopback API and start serving.
pub async fn start(
    cfg: LocalApiConfig,
) -> Result<(ServerHandle, mpsc::UnboundedReceiver<ClientMessage>)> {
    let attempts: Vec<u16> = if cfg.ports.is_empty() {
        vec![0]
    } else {
        cfg.ports.clone()
    };

    let mut listener = None;
    for port in &attempts {
        match TcpListener::bind((cfg.host.as_str(), *port)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => {
                eprintln!("hazar-localapi: port {port} unavailable ({e})");
            }
        }
    }
    let listener = listener.ok_or(ApiError::Bind(attempts))?;
    let addr = listener.local_addr()?;

    let (outbound, _) = broadcast::channel::<Outbound>(512);
    let (tx, rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let clients = Arc::new(AtomicU64::new(0));
    let next_id = Arc::new(AtomicU64::new(1));

    let handle = ServerHandle {
        addr,
        port: addr.port(),
        outbound: outbound.clone(),
        clients: clients.clone(),
        shutdown: shutdown_tx,
    };

    let owners = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let accept_cfg = cfg.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let id = next_id.fetch_add(1, Ordering::Relaxed);
                            let cfg = accept_cfg.clone();
                            let outbound = outbound.clone();
                            let tx = tx.clone();
                            let clients = clients.clone();
                            let owners = owners.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer, id, cfg, outbound, tx, clients.clone(), owners).await {
                                    eprintln!("hazar-localapi: client {id} ({peer}) disconnected: {e}");
                                }
                            });
                        }
                        Err(e) => eprintln!("hazar-localapi: accept failed: {e}"),
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    Ok((handle, rx))
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    client_id: u64,
    cfg: LocalApiConfig,
    outbound: broadcast::Sender<Outbound>,
    inbound_tx: mpsc::UnboundedSender<ClientMessage>,
    clients: Arc<AtomicU64>,
    owners: Arc<Mutex<HashMap<String, String>>>,
) -> Result<()> {
    let origin = Arc::new(Mutex::new(String::new()));
    let callback = |req: &Request,
                    mut resp: Response|
     -> std::result::Result<Response, ErrorResponse> {
        if let Some(origin) = req.headers().get("origin") {
            let origin = origin.to_str().unwrap_or("");
            if !(origin.starts_with("chrome-extension://")
                || origin.starts_with("moz-extension://"))
            {
                let mut error = ErrorResponse::new(Some("extension origin required".into()));
                *error.status_mut() = StatusCode::FORBIDDEN;
                return Err(error);
            }
        }
        *origin.lock().expect("origin") = req.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let offered = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !offered.split(',').any(|p| p.trim() == SUBPROTOCOL) {
            let mut err = ErrorResponse::new(Some(format!("subprotocol {SUBPROTOCOL} required")));
            *err.status_mut() = StatusCode::BAD_REQUEST;
            return Err(err);
        }
        resp.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        Ok(resp)
    };

    let config = WebSocketConfig { max_message_size: Some(32 * 1024 * 1024), max_frame_size: Some(32 * 1024 * 1024), ..Default::default() };
    let ws = accept_hdr_async_with_config(stream, callback, Some(config)).await?;
    let (mut sink, mut source) = ws.split();
    clients.fetch_add(1, Ordering::Relaxed);
    let guard = ClientGuard {
        clients: clients.clone(),
    };

    // --- handshake: first frame must be `hello` ---
    let first = tokio::time::timeout(cfg.handshake_timeout, source.next()).await;
    let hello = match first {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Inbound>(&text) {
            Ok(Inbound::Hello(hello)) => hello,
            Ok(_) => {
                let _ = send(
                    &mut sink,
                    &Outbound::HelloErr {
                        reason: "first message must be hello".into(),
                    },
                )
                .await;
                return Ok(());
            }
            Err(e) => {
                let _ = send(
                    &mut sink,
                    &Outbound::HelloErr {
                        reason: format!("bad hello json: {e}"),
                    },
                )
                .await;
                return Ok(());
            }
        },
        _ => {
            let _ = send(
                &mut sink,
                &Outbound::HelloErr {
                    reason: "handshake timeout".into(),
                },
            )
            .await;
            return Ok(());
        }
    };

    if hello.protocol != cfg.protocol {
        let _ = send(
            &mut sink,
            &Outbound::HelloErr {
                reason: format!(
                    "protocol mismatch: extension speaks {}, app speaks {}",
                    hello.protocol, cfg.protocol
                ),
            },
        )
        .await;
        drop(guard);
        return Ok(());
    }

    let identity = { let origin = origin.lock().expect("origin"); if origin.is_empty() { format!("native-{client_id}") } else { origin.clone() } };
    let session = session_token(client_id)?;
    send(
        &mut sink,
        &Outbound::HelloOk {
            protocol: cfg.protocol,
            app: cfg.app_name.clone(),
            version: cfg.app_version.clone(),
            session: session.clone(),
            features: cfg.features.clone(),
            settings: cfg.settings.clone(),
        },
    )
    .await?;

    eprintln!(
        "hazar-localapi: {} ({}) connected from {peer} — session {}",
        hello.client,
        hello.extension_id.as_deref().unwrap_or("-"),
        &session[..8]
    );

    let mut rx = outbound.subscribe();
    let mut idle = tokio::time::interval(Duration::from_secs(30));
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            incoming = source.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Inbound>(&text) {
                            Ok(message) => {
                                if message.session() != Some(session.as_str()) {
                                    let _ = send(&mut sink, &Outbound::Error { reason: "session mismatch".into() }).await;
                                    continue;
                                }
                                if let Inbound::Grab(grab) | Inbound::Extract(grab) = &message {
                                    let parsed = url::Url::parse(&grab.request.url);
                                    if !parsed.is_ok_and(|url| matches!(url.scheme(), "http" | "https")) || grab.request.method.as_deref().is_some_and(|m| m != "GET") {
                                        let _ = send(&mut sink, &Outbound::Error { reason: "only HTTP/HTTPS GET downloads are supported".into() }).await; continue;
                                    }
                                }
                                let job_id = match &message { Inbound::Grab(m) | Inbound::Extract(m) => Some(&m.id), Inbound::Context(m) => Some(&m.id), Inbound::Cancel(m) => Some(&m.id), Inbound::Bytes(m) => Some(&m.stream_id), Inbound::CaptureFailed(m) => Some(&m.id), _ => None };
                                let authorized = if let Some(id) = job_id {
                                    let mut owners = owners.lock().expect("owners");
                                    if id.is_empty() || id.len() > 128 || owners.len() >= 10000 { false }
                                    else if matches!(message, Inbound::Context(_) | Inbound::Cancel(_) | Inbound::CaptureFailed(_)) { owners.get(id) == Some(&identity) }
                                    else { owners.entry(id.clone()).or_insert_with(|| identity.clone()) == &identity }
                                } else { true };
                                if !authorized { let _ = send(&mut sink, &Outbound::Error { reason: "job belongs to another client".into() }).await; continue; }
                                if let Inbound::Ping(ping) = &message {
                                    let _ = send(&mut sink, &Outbound::Pong { t: ping.t.unwrap_or(0) }).await;
                                }
                                let _ = inbound_tx.send(ClientMessage {
                                    client_id,
                                    peer,
                                    session: session.clone(),
                                    message,
                                });
                            }
                            Err(e) => {
                                let _ = send(&mut sink, &Outbound::Error { reason: format!("bad message: {e}") }).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        eprintln!("hazar-localapi: client {client_id} stream error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Ok(message) => {
                        let id = match &message {
                            Outbound::RefreshContext { id, .. } | Outbound::Extracted { id, .. } | Outbound::GrabAck { id, .. } | Outbound::Progress { id, .. } | Outbound::Finished { id, .. } | Outbound::Failed { id, .. } => Some(id),
                            Outbound::BytesAck { stream_id, .. } => Some(stream_id),
                            _ => None
                        };
                        let deliver = id.map(|id| owners.lock().expect("owners").get(id) == Some(&identity)).unwrap_or(true);
                        if deliver {
                            send(&mut sink, &message).await?;
                            if let Outbound::Extracted { id, .. } = &message { owners.lock().expect("owners").remove(id); }
                        }
                    },
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        eprintln!("hazar-localapi: client {client_id} lagged, dropped {skipped} messages");
                    }
                    Err(_) => break,
                }
            }
            _ = idle.tick() => {}
        }
    }

    drop(guard);
    eprintln!("hazar-localapi: client {client_id} closed");
    Ok(())
}

async fn send(
    sink: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
    message: &Outbound,
) -> Result<()> {
    let json = serde_json::to_string(message)?;
    sink.send(Message::text(json)).await?;
    Ok(())
}

fn session_token(_client_id: u64) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| ApiError::Io(std::io::Error::other(e.to_string())))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct ClientGuard {
    clients: Arc<AtomicU64>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Helper for the app: is a handshake-complete extension connected?
pub fn has_client(handle: &ServerHandle) -> bool {
    handle.client_count() > 0
}

/// Re-export so callers do not need to depend on `Hello` directly.
pub fn hello_example() -> Inbound {
    Inbound::Hello(Hello {
        protocol: hazar_engine::PROTOCOL_VERSION,
        client: "chrome".into(),
        extension_id: None,
        version: None,
    })
}
