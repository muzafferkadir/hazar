//! Hazar download engine.
//!
//! Multi-connection segmented HTTP(S) downloads with resume, dynamic part
//! planning, assembly, checksum verification and HLS (m3u8) support.
//!
//! ```no_run
//! use hazar_engine::{DownloadOptions, Downloader};
//!
//! # async fn run() -> Result<(), hazar_engine::Error> {
//! let opts = DownloadOptions::new("https://example.com/big.iso", "/tmp/big.iso")
//!     .connections(8);
//! let outcome = Downloader::new(opts)?.run().await?;
//! println!("{} bytes in {:?}", outcome.size, outcome.elapsed);
//! # Ok(())
//! # }
//! ```

pub mod assemble;
pub mod dash;
pub mod download;
pub mod error;
pub mod hash;
pub mod hls;
pub mod media;
pub mod meta;
pub mod plan;
pub mod probe;
pub mod progress;
pub mod resolve;

pub use dash::{download_dash, DashOptions, DashOutcome, DashPlan};
pub use download::{
    default_client, default_client_full, default_client_with, proxy_from_env, DownloadOptions,
    Downloader, Outcome, RateLimiter,
};
pub use error::{Error, Result};
pub use hls::{
    best_variant, download_hls, is_dash, is_hls, parse_playlist, plan_from_segments, HlsOptions,
    HlsOutcome, HlsPlan, Key, Playlist, Segment, Variant,
};
pub use meta::{DownloadMeta, PartState, WorkDir};
pub use plan::{
    plan_parts, PlanOptions, DEFAULT_CONNECTIONS, DEFAULT_MIN_PART_SIZE, MAX_CONNECTIONS,
};
pub use probe::{probe, ResourceInfo};
pub use progress::{channel, ProgressEvent, ProgressSender};
pub use resolve::{
    classify_url, resolve, Candidate, MediaKind, ResolveOptions, ResolveReport, STRATEGIES,
};

/// Version string shared by CLI, app and extension handshakes.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire protocol version spoken with the browser extension.
pub const PROTOCOL_VERSION: u32 = 1;
