use serde::{Deserialize, Serialize};

/// Engine progress, streamed to the CLI today and to the UI/extension later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressEvent {
    Probing {
        url: String,
    },
    Planned {
        size: u64,
        connections: usize,
        resumed: bool,
        bytes_done: u64,
    },
    /// HLS: playlist resolved (master → variant) and the segment plan is known.
    HlsPlanned {
        segments: usize,
        variant: Option<String>,
        resumed: bool,
        done: usize,
    },
    /// HLS: one segment finished (decrypted and renamed into place).
    HlsSegment {
        done: usize,
        total: usize,
        bytes: u64,
    },
    PartProgress {
        index: u32,
        part_written: u64,
        part_length: u64,
        total_written: u64,
        total_size: u64,
    },
    Retrying {
        index: u32,
        attempt: u32,
        reason: String,
    },
    FalldownSingle {
        reason: String,
    },
    Assembling {
        parts: usize,
    },
    Verifying,
    Finished {
        bytes: u64,
        elapsed_ms: u64,
        sha256: Option<String>,
    },
    Failed {
        reason: String,
    },
}

/// Anything that wants progress can drain this channel.
pub type ProgressSender = tokio::sync::mpsc::UnboundedSender<ProgressEvent>;

pub fn channel() -> (ProgressSender, tokio::sync::mpsc::UnboundedReceiver<ProgressEvent>) {
    tokio::sync::mpsc::unbounded_channel()
}
