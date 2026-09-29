use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("metadata error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unexpected status {status} for {url}")]
    Status { status: u16, url: String },

    #[error("server does not support range requests")]
    RangeUnsupported,

    #[error("server ignored the Range header")]
    RangeIgnored,

    #[error("connection closed early: expected {expected} bytes, got {got}")]
    ShortBody { expected: u64, got: u64 },

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("download cancelled")]
    Cancelled,

    #[error("cannot resume: {0}")]
    ResumeMismatch(String),

    #[error("invalid metadata {path}: {reason}")]
    InvalidMeta { path: PathBuf, reason: String },

    #[error("size is unknown and the server does not support ranges")]
    UnknownSize,

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("protocol error: {0}")]
    Protocol(String),
}

impl Error {
    /// Transient failures worth another attempt at the part level.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(e) => {
                // Decode/body errors are how a truncated response shows up.
                e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
            }
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Error::ShortBody { .. } => true,
            Error::Status { status, .. } => (500..600).contains(status) || *status == 429,
            Error::RangeIgnored => true,
            _ => false,
        }
    }
}
