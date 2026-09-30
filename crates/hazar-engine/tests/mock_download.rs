//! Engine behaviour against a hand-rolled HTTP server, so the tests can lie
//! about ranges, drop connections and report ETags on demand.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hazar_engine::{
    plan_parts, probe, DownloadMeta, DownloadOptions, Downloader, PlanOptions, WorkDir,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ETAG: &str = "\"hazar-test\"";
const FILE_NAME: &str = "fixture.bin";

#[derive(Clone)]
struct Config {
    ignore_range: bool,
    declare_ranges: bool,
    wrong_range: bool,
    /// Truncate the body of the first N body responses (dropped connections).
    truncate_bodies: usize,
}

struct Server {
    addr: SocketAddr,
    bytes: Arc<Vec<u8>>,
    digest: String,
    ranges: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }

    fn range_requests(&self) -> Vec<String> {
        self.ranges.lock().unwrap().clone()
    }

    fn matches(&self, path: &std::path::Path) -> bool {
        std::fs::read(path).map(|got| got == *self.bytes).unwrap_or(false)
    }
}

/// Deterministic fixture + its sha256.
fn payload(size: usize) -> (Arc<Vec<u8>>, String) {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut out = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state & 0xff) as u8);
    }
    let mut hasher = Sha256::new();
    hasher.update(&out);
    let digest = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    (Arc::new(out), digest)
}

async fn start(size: usize, cfg: Config) -> Server {
    let (bytes, digest) = payload(size);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(AtomicUsize::new(0));

    let server_bytes = bytes.clone();
    let server_ranges = ranges.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let cfg = cfg.clone();
            let ranges = server_ranges.clone();
            let bodies = bodies.clone();
            let bytes = server_bytes.clone();
            tokio::spawn(async move {
                let _ = handle(stream, bytes, cfg, ranges, bodies).await;
            });
        }
    });

    Server {
        addr,
        bytes,
        digest,
        ranges,
    }
}

async fn handle(
    mut stream: TcpStream,
    payload: Arc<Vec<u8>>,
    cfg: Config,
    ranges: Arc<Mutex<Vec<String>>>,
    bodies: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 32 * 1024 {
            return Ok(());
        }
    }

    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.split("\r\n");
    let method = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or("GET")
        .to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let total = payload.len() as u64;
    let accept = if cfg.declare_ranges { "bytes" } else { "none" };
    let common = format!(
        "Accept-Ranges: {accept}\r\nETag: {ETAG}\r\nContent-Disposition: attachment; filename=\"{FILE_NAME}\"\r\nConnection: close\r\n"
    );

    if method == "HEAD" {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nContent-Type: application/octet-stream\r\n{common}\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        return Ok(());
    }

    let range = headers.get("range").cloned();
    if let Some(range) = &range {
        ranges.lock().unwrap().push(range.clone());
    }

    let use_range = range.is_some() && cfg.declare_ranges && !cfg.ignore_range;
    let (status, extra, start, end) = if use_range {
        let (start, end) = parse_range(range.as_deref().unwrap_or("bytes=0-"), total);
        (
            206,
            format!("Content-Range: bytes {}-{end}/{total}\r\n", if cfg.wrong_range { start + 1 } else { start }),
            start,
            end,
        )
    } else {
        (200, String::new(), 0, total.saturating_sub(1))
    };

    let body: &[u8] = if total == 0 {
        &[]
    } else {
        &payload[start as usize..=(end as usize).min(payload.len() - 1)]
    };

    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n{extra}{common}\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;

    if bodies.fetch_add(1, Ordering::SeqCst) < cfg.truncate_bodies {
        let half = (body.len() / 2).max(1);
        stream.write_all(&body[..half]).await?;
        stream.flush().await?;
        return Ok(());
    }

    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

fn parse_range(header: &str, total: u64) -> (u64, u64) {
    let spec = header.trim().trim_start_matches("bytes=");
    let (start, end) = spec.split_once('-').unwrap_or((spec, ""));
    let start: u64 = start.trim().parse().unwrap_or(0);
    let end: u64 = if end.trim().is_empty() {
        total - 1
    } else {
        end.trim().parse().unwrap_or(total - 1)
    };
    (start, end.min(total - 1))
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hazar-test-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn probe_reports_size_ranges_and_name() {
    let server = start(4096, base_config()).await;
    let client = hazar_engine::default_client(None).unwrap();
    let info = probe(&client, &server.url()).await.unwrap();

    assert_eq!(info.len, Some(4096));
    assert!(info.accept_ranges);
    assert_eq!(info.etag.as_deref(), Some(ETAG));
    assert_eq!(info.filename_hint.as_deref(), Some(FILE_NAME));
}

#[tokio::test]
async fn segmented_download_is_byte_exact() {
    let size = 6 * 1024 * 1024;
    let server = start(size, base_config()).await;
    let dir = tmp_dir("segmented");
    let dest = dir.join("out.bin");

    let outcome = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(8)
            .min_part_size(512 * 1024)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert_eq!(outcome.size, size as u64);
    assert!(!outcome.resumed);
    assert_eq!(outcome.sha256.as_deref(), Some(server.digest.as_str()));
    assert!(server.matches(&dest), "downloaded bytes must match the fixture");
    assert!(
        server.range_requests().len() >= 8,
        "expected one range request per part, got {:?}",
        server.range_requests()
    );
    assert!(
        !WorkDir::new(&dest).root.exists(),
        "sidecar directory must be cleaned up after success"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn checksum_mismatch_fails_the_download() {
    let size = 1024 * 1024;
    let server = start(size, base_config()).await;
    let dir = tmp_dir("checksum");
    let dest = dir.join("out.bin");

    let error = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(2)
            .sha256("00".repeat(32)),
    )
    .unwrap()
    .run()
    .await
    .expect_err("bogus checksum must fail");

    assert!(matches!(error, hazar_engine::Error::ChecksumMismatch { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn resumes_from_an_existing_part_state() {
    let size = 4 * 1024 * 1024;
    let server = start(size, base_config()).await;
    let url = server.url();
    let dir = tmp_dir("resume");
    let dest = dir.join("out.bin");

    let client = hazar_engine::default_client(None).unwrap();
    let info = probe(&client, &url).await.unwrap();
    let total = info.len.unwrap();

    // Pretend a previous run wrote 100 KiB of part 0 and nothing else.
    let work = WorkDir::new(&dest);
    work.ensure().await.unwrap();
    let parts = plan_parts(
        total,
        PlanOptions {
            connections: 4,
            min_part_size: 1024 * 1024,
        },
    );
    assert_eq!(parts.len(), 4, "fixture must produce four parts");
    let mut meta = DownloadMeta::new(&url, &info, total, 4, parts);
    meta.parts[0].written = 100 * 1024;
    tokio::fs::write(work.part_path(0), &server.bytes[..100 * 1024])
        .await
        .unwrap();
    work.save_meta(&meta).await.unwrap();

    let outcome = Downloader::new(
        DownloadOptions::new(&url, &dest)
            .connections(4)
            .min_part_size(1024 * 1024)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert!(outcome.resumed, "second run must resume the previous state");
    assert!(server.matches(&dest));
    let ranges = server.range_requests();
    assert!(
        ranges.iter().any(|r| r.starts_with("bytes=102400-")),
        "part 0 must continue at the recorded offset, got {ranges:?}"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn restarts_when_the_remote_file_changed() {
    let size = 1024 * 1024;
    let server = start(size, base_config()).await;
    let url = server.url();
    let dir = tmp_dir("stale");
    let dest = dir.join("out.bin");

    let client = hazar_engine::default_client(None).unwrap();
    let info = probe(&client, &url).await.unwrap();
    let total = info.len.unwrap();

    // Meta from a different ETag must be discarded, not resumed.
    let work = WorkDir::new(&dest);
    work.ensure().await.unwrap();
    let parts = plan_parts(total, PlanOptions::default());
    let mut meta = DownloadMeta::new(&url, &info, total, 2, parts);
    meta.etag = Some("\"older-version\"".to_string());
    meta.parts[0].written = 4096;
    tokio::fs::write(work.part_path(0), &server.bytes[..4096])
        .await
        .unwrap();
    work.save_meta(&meta).await.unwrap();

    let outcome = Downloader::new(
        DownloadOptions::new(&url, &dest)
            .connections(2)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert!(!outcome.resumed, "stale ETag must force a fresh download");
    assert!(server.matches(&dest));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn falls_back_to_a_single_connection_when_range_is_ignored() {
    let size = 4 * 1024 * 1024;
    let server = start(
        size,
        Config {
            ignore_range: true,
            ..base_config()
        },
    )
    .await;
    let dir = tmp_dir("rangeignored");
    let dest = dir.join("out.bin");

    let outcome = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(4)
            .min_part_size(1024 * 1024)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert_eq!(outcome.size, size as u64);
    assert!(server.matches(&dest));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn single_connection_when_server_advertises_no_ranges() {
    let size = 1024 * 1024;
    let server = start(
        size,
        Config {
            declare_ranges: false,
            wrong_range: false,
            ..base_config()
        },
    )
    .await;
    let dir = tmp_dir("noranges");
    let dest = dir.join("out.bin");

    let outcome = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(8)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert_eq!(outcome.connections, 1);
    // Only the probe's confirmation `Range: bytes=0-0` request is expected.
    assert_eq!(
        server.range_requests().len(),
        1,
        "single connection mode must not open per-part ranges, got {:?}",
        server.range_requests()
    );
    assert!(server.matches(&dest));

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn retries_after_a_dropped_connection() {
    let size = 4 * 1024 * 1024;
    let server = start(
        size,
        Config {
            truncate_bodies: 2,
            ..base_config()
        },
    )
    .await;
    let dir = tmp_dir("retry");
    let dest = dir.join("out.bin");

    let outcome = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(4)
            .min_part_size(1024 * 1024)
            .max_retries(5)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();

    assert_eq!(outcome.size, size as u64);
    assert!(server.matches(&dest));

    std::fs::remove_dir_all(dir).ok();
}

fn base_config() -> Config {
    Config {
        ignore_range: false,
        declare_ranges: true,
            wrong_range: false,
        truncate_bodies: 0,
    }
}

#[tokio::test]
async fn speed_limit_slows_the_download_down() {
    let size = 256 * 1024;
    let server = start(size, base_config()).await;
    let dir = tmp_dir("ratelimit");
    let dest = dir.join("out.bin");

    let started = std::time::Instant::now();
    let outcome = Downloader::new(
        DownloadOptions::new(server.url(), &dest)
            .connections(4)
            .min_part_size(64 * 1024)
            .speed_limit(128 * 1024)
            .sha256(server.digest.clone()),
    )
    .unwrap()
    .run()
    .await
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome.size, size as u64);
    assert!(server.matches(&dest));
    assert!(
        elapsed >= std::time::Duration::from_millis(1200),
        "256 KiB at 128 KiB/s cannot finish in {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the limiter must not stall the download: {elapsed:?}"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn rejects_wrong_content_range_and_preserves_existing_output() {
    let server = start(2 * 1024 * 1024, Config { declare_ranges: true, ignore_range: false, truncate_bodies: 0, wrong_range: true }).await;
    let dir = tmp_dir("wrong-range"); let dest = dir.join("file.bin");
    std::fs::write(&dest, b"keep me").unwrap();
    let error = Downloader::new(DownloadOptions::new(server.url(), &dest)).unwrap().run().await.unwrap_err();
    assert!(error.to_string().contains("Content-Range"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn failed_single_stream_preserves_existing_output() {
    let server = start(1024 * 1024, Config { declare_ranges: false, ignore_range: false, truncate_bodies: 100, wrong_range: false }).await;
    let dir = tmp_dir("single-preserve"); let dest = dir.join("file.bin");
    std::fs::write(&dest, b"keep me").unwrap();
    assert!(Downloader::new(DownloadOptions::new(server.url(), &dest)).unwrap().run().await.is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
    std::fs::remove_dir_all(dir).ok();
}
