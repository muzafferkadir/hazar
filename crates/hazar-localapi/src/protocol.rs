//! Wire protocol between the Hazar app and its browser extension.
//!
//! Deliberately **named JSON**, not IDM's positional arrays: every message
//! carries a `type` tag, and the payload schema is versioned by
//! [`hazar_engine::PROTOCOL_VERSION`] exchanged in [`Hello`] / [`Outbound::HelloOk`].

use serde::{Deserialize, Serialize};

/// Sub-protocol token the extension must ask for in the WebSocket handshake.
pub const SUBPROTOCOL: &str = "hazar.v1";

/// Extension → app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inbound {
    Hello(Hello),
    Grab(Grab),
    Cancel(Cancel),
    Media(MediaCandidates),
    Ping(Ping),
}

impl Inbound {
    pub fn session(&self) -> Option<&str> {
        match self {
            Inbound::Hello(_) => None,
            Inbound::Grab(m) => m.session.as_deref(),
            Inbound::Cancel(m) => m.session.as_deref(),
            Inbound::Media(m) => m.session.as_deref(),
            Inbound::Ping(m) => m.session.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub client: String,
    #[serde(default)]
    pub extension_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grab {
    #[serde(default)]
    pub session: Option<String>,
    pub id: String,
    pub request: GrabRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cancel {
    #[serde(default)]
    pub session: Option<String>,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaCandidates {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub tab_id: i64,
    pub items: Vec<MediaCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub t: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrabKind {
    File,
    Hls,
    Dash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrabRequest {
    pub url: String,
    pub kind: GrabKind,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// `name=value; ...` — built by the extension from `cookies.getAll`.
    #[serde(default)]
    pub cookie: Option<String>,
    /// Extra request headers, `[[name, value], ...]`.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Sniffed segment URLs, used when the playlist itself was not captured.
    #[serde(default)]
    pub segments: Option<Vec<String>>,
    /// Playlist body, when the extension already has it.
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub tab_id: Option<i64>,
    #[serde(default)]
    pub save_dir: Option<String>,
    /// Uygulamadan gelen işler için ek parametreler (extension bunları yok sayar).
    #[serde(default)]
    pub connections: Option<u32>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub speed_limit_bps: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaCandidate {
    pub url: String,
    pub kind: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub label: Option<String>,
}

/// App → extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    HelloOk {
        protocol: u32,
        app: String,
        version: String,
        session: String,
        features: Vec<String>,
        settings: Settings,
    },
    HelloErr {
        reason: String,
    },
    GrabAck {
        id: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Progress {
        id: String,
        phase: String,
        written: u64,
        total: u64,
        connections: u32,
        speed_bps: u64,
    },
    Finished {
        id: String,
        path: String,
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        elapsed_ms: u64,
    },
    Failed {
        id: String,
        reason: String,
    },
    Queue {
        items: Vec<QueueItem>,
    },
    /// Updated settings, pushed after the user changes them in the app.
    Settings {
        settings: Settings,
    },
    Pong {
        t: u64,
    },
    Error {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub download_dir: Option<String>,
    pub connections: u32,
    pub max_concurrent_downloads: u32,
    pub capture_enabled: bool,
    pub min_size_bytes: u64,
    pub excluded_hosts: Vec<String>,
    /// Gece indirme penceresi (yerel saat, "HH:MM"); kapalıysa hep indirir.
    #[serde(default)]
    pub schedule_enabled: bool,
    #[serde(default = "default_schedule_from")]
    pub schedule_from: String,
    #[serde(default = "default_schedule_to")]
    pub schedule_to: String,
}

fn default_schedule_from() -> String {
    "02:00".to_string()
}

fn default_schedule_to() -> String {
    "08:00".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: None,
            connections: hazar_engine::DEFAULT_CONNECTIONS as u32,
            max_concurrent_downloads: 3,
            capture_enabled: true,
            min_size_bytes: 512 * 1024,
            excluded_hosts: Vec::new(),
            schedule_enabled: false,
            schedule_from: default_schedule_from(),
            schedule_to: default_schedule_to(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub url: String,
    pub state: String,
    pub written: u64,
    pub total: u64,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}
