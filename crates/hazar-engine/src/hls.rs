//! HLS (m3u8) support: playlist parsing (master + media), AES-128 decryption,
//! concurrent segment download with resume, and ordered assembly.
//!
//! DASH (`mpd`) is detected by the extension/app but not downloaded yet — see
//! [`is_dash`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::download::default_client_with;
use crate::error::{Error, Result};
use crate::hash::{digest_matches, sha256_file};
use crate::meta::WorkDir;
use crate::progress::{ProgressEvent, ProgressSender};

type Aes128Cbc = cbc::Decryptor<aes::Aes128>;

const MAX_PLAYLIST_HOPS: usize = 3;
const SEGMENT_RETRIES: u32 = 3;

/// Is this URL/response an HLS playlist?
pub fn is_hls(url: &str, mime: Option<&str>) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        return true;
    }
    matches!(
        media_type(mime).as_deref(),
        Some("application/vnd.apple.mpegurl")
            | Some("application/x-mpegurl")
            | Some("audio/mpegurl")
            | Some("audio/x-mpegurl")
            | Some("video/mpegurl")
            | Some("video/x-mpegurl")
            | Some("application/octet-stream-m3u8")
    )
}

/// Is this URL/response a DASH manifest?
pub fn is_dash(url: &str, mime: Option<&str>) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".mpd") || media_type(mime).as_deref() == Some("application/dash+xml")
}

fn media_type(mime: Option<&str>) -> Option<String> {
    mime.map(|m| {
        m.split(';')
            .next()
            .unwrap_or(m)
            .trim()
            .to_ascii_lowercase()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub method: String,
    pub uri: Option<String>,
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub url: String,
    pub byterange: Option<(u64, u64)>,
    pub key: Option<Key>,
    pub seq: u64,
    /// `#EXT-X-MAP` init segment (fMP4) — always assembled first.
    pub init: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media {
        segments: Vec<Segment>,
        end_list: bool,
        target_duration: f64,
    },
}

/// Parse a playlist body. `base` is the playlist URL used to resolve relatives.
pub fn parse_playlist(text: &str, base: &Url) -> Result<Playlist> {
    let mut variants: Vec<Variant> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut pending_key: Option<Key> = None;
    let mut pending_range: Option<(u64, u64)> = None;
    let mut pending_variant: Option<(u64, Option<String>)> = None;
    let mut seq: u64 = 0;
    let mut end_list = false;
    let mut target_duration = 0.0f64;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bandwidth = attr(rest, "BANDWIDTH")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            pending_variant = Some((bandwidth, attr(rest, "RESOLUTION")));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().unwrap_or(0.0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = rest.trim().parse().unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attr(rest, "METHOD").unwrap_or_else(|| "NONE".into());
            if method.eq_ignore_ascii_case("NONE") {
                pending_key = None;
            } else {
                if !method.eq_ignore_ascii_case("AES-128") {
                    return Err(Error::Unsupported(format!("HLS key method {method}")));
                }
                pending_key = Some(Key {
                    method,
                    uri: attr(rest, "URI").and_then(|u| resolve(base, &u)),
                    iv: attr(rest, "IV").and_then(|iv| parse_iv(&iv)),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = attr(rest, "URI").and_then(|u| resolve(base, &u)) {
                let byterange = attr(rest, "BYTERANGE").and_then(|v| parse_byterange(&v, None));
                segments.push(Segment {
                    url: uri,
                    byterange,
                    key: pending_key.clone(),
                    seq,
                    init: true,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            // Without an explicit offset the range continues after the previous
            // sub-range of the same resource.
            let previous_end = segments
                .last()
                .and_then(|s| s.byterange)
                .map(|(_, end)| end + 1);
            pending_range = parse_byterange(rest.trim(), previous_end);
            continue;
        }
        if line == "#EXT-X-ENDLIST" {
            end_list = true;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        // A URI line: either a variant (after EXT-X-STREAM-INF) or a segment.
        if let Some((bandwidth, resolution)) = pending_variant.take() {
            if let Some(url) = resolve(base, line) {
                variants.push(Variant {
                    uri: url,
                    bandwidth,
                    resolution,
                });
            }
            continue;
        }

        if let Some(url) = resolve(base, line) {
            segments.push(Segment {
                url,
                byterange: pending_range.take(),
                key: pending_key.clone(),
                seq,
                init: false,
            });
            seq += 1;
        }
    }

    if !variants.is_empty() {
        if variants.len() > 512 {
            return Err(Error::Protocol("playlist has too many variants".into()));
        }
        return Ok(Playlist::Master(variants));
    }

    Ok(Playlist::Media {
        segments,
        end_list,
        target_duration,
    })
}

fn attr(input: &str, name: &str) -> Option<String> {
    let mut key = String::new();
    let mut value = String::new();
    let mut in_quotes = false;
    let mut reading_value = false;
    let mut found = None;

    for ch in input.chars().chain(std::iter::once(',')) {
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if ch == ',' && !in_quotes {
            if reading_value && key.eq_ignore_ascii_case(name) && found.is_none() {
                found = Some(value.clone());
            }
            key.clear();
            value.clear();
            reading_value = false;
            continue;
        }
        if ch == '=' && !reading_value && !in_quotes {
            reading_value = true;
            continue;
        }
        if reading_value {
            value.push(ch);
        } else {
            key.push(ch);
        }
    }
    found
}

fn parse_iv(value: &str) -> Option<[u8; 16]> {
    let hex = value.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn parse_byterange(value: &str, previous_end: Option<u64>) -> Option<(u64, u64)> {
    let (len, offset) = match value.split_once('@') {
        Some((len, offset)) => (len.trim(), Some(offset.trim())),
        None => (value.trim(), None),
    };
    let len: u64 = len.parse().ok()?;
    let start = match offset {
        Some(offset) => offset.parse().ok()?,
        None => previous_end?,
    };
    Some((start, start + len - 1))
}

fn resolve(base: &Url, reference: &str) -> Option<String> {
    if reference.trim().is_empty() {
        return None;
    }
    Url::parse(reference)
        .or_else(|_| base.join(reference))
        .ok()
        .map(|u| u.to_string())
}

/// Pick the highest-bandwidth variant (or the first one).
pub fn best_variant(variants: &[Variant]) -> Option<&Variant> {
    variants
        .iter()
        .take(MAX_PLAYLIST_HOPS.max(1) * 512)
        .max_by_key(|v| v.bandwidth)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsPlan {
    pub manifest: String,
    pub variant: Option<String>,
    pub segments: Vec<Segment>,
}

pub struct HlsOptions {
    /// Playlist URL (fetched, and used to resolve relative URIs).
    pub manifest: String,
    /// Segment URLs captured by the extension when the playlist body itself
    /// never left the page (MSE / fetch). Bypasses playlist fetching.
    pub segments: Option<Vec<String>>,
    /// Page URL used to resolve relative segment URLs.
    pub base_url: Option<String>,
    pub output: PathBuf,
    pub connections: usize,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub expected_sha256: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
pub struct HlsOutcome {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub segments: usize,
    pub variant: Option<String>,
    pub resumed: bool,
    pub elapsed: Duration,
}

/// Download an HLS stream into a single file.
pub async fn download_hls(
    opts: HlsOptions,
    progress: Option<ProgressSender>,
) -> Result<HlsOutcome> {
    let started = Instant::now();
    let emit = |event: ProgressEvent| {
        if let Some(tx) = &progress {
            let _ = tx.send(event);
        }
    };

    let client = default_client_with(opts.user_agent.as_deref(), &opts.headers)?;
    let plan = match &opts.segments {
        Some(urls) if !urls.is_empty() => plan_from_segments(
            urls,
            opts.base_url.as_deref().unwrap_or(&opts.manifest),
        )?,
        _ => resolve_plan(&client, &opts.manifest).await?,
    };

    let work = WorkDir::new(&opts.output);
    let mut resumed = false;
    let existing: Option<HlsPlan> = match tokio::fs::read(work.plan_path()).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };
    if let Some(previous) = &existing {
        if previous.segments == plan.segments && previous.manifest == plan.manifest {
            resumed = true;
        }
    }
    if !resumed {
        work.reset().await?;
    }
    work.ensure().await?;
    tokio::fs::write(work.plan_path(), serde_json::to_vec_pretty(&plan)?).await?;

    let total = plan.segments.len();
    let done_files: Vec<bool> = {
        let mut flags = Vec::with_capacity(total);
        for index in 0..total {
            flags.push(work.segment_ready(index).await);
        }
        flags
    };
    let already = done_files.iter().filter(|d| **d).count();

    emit(ProgressEvent::HlsPlanned {
        segments: total,
        variant: plan.variant.clone(),
        resumed,
        done: already,
    });

    let done = Arc::new(AtomicU64::new(already as u64));
    let bytes = Arc::new(AtomicU64::new(0));
    let keys: Arc<tokio::sync::Mutex<HashMap<String, Vec<u8>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(opts.connections.clamp(1, 16)));

    let mut set = tokio::task::JoinSet::new();
    for (index, segment) in plan.segments.iter().enumerate() {
        if done_files[index] {
            continue;
        }
        let segment = segment.clone();
        let client = client.clone();
        let work = work.clone();
        let keys = keys.clone();
        let semaphore = semaphore.clone();
        let done = done.clone();
        let bytes = bytes.clone();
        let cancel = opts.cancel.clone();
        let tx = progress.clone();
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore");
            let mut attempt = 0u32;
            loop {
                if cancel.as_ref().map(|f| f.load(Ordering::Relaxed)).unwrap_or(false) {
                    return Err(Error::Cancelled);
                }
                match fetch_segment(&client, &segment, &keys, cancel.as_deref()).await {
                    Ok(data) => {
                        work.write_segment(index, &data).await?;
                        let written = bytes.fetch_add(data.len() as u64, Ordering::Relaxed)
                            + data.len() as u64;
                        let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if let Some(tx) = &tx {
                            let _ = tx.send(ProgressEvent::HlsSegment {
                                done: (finished as usize).min(total),
                                total,
                                bytes: written,
                            });
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        attempt += 1;
                        if attempt > SEGMENT_RETRIES || !e.is_retryable() {
                            return Err(e);
                        }
                        tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))).await;
                    }
                }
            }
        });
    }

    let mut fatal: Option<Error> = None;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let hard = matches!(e, Error::Cancelled);
                if fatal.is_none() {
                    fatal = Some(e);
                }
                if hard {
                    set.abort_all();
                    break;
                }
            }
            Err(_) => {
                if fatal.is_none() {
                    fatal = Some(Error::Io(std::io::Error::other("segment task failed")));
                }
            }
        }
    }
    if let Some(error) = fatal {
        return Err(error);
    }

    emit(ProgressEvent::Assembling { parts: total });
    let size = work.assemble(&opts.output, total).await?;

    let digest = if opts.expected_sha256.is_some() {
        emit(ProgressEvent::Verifying);
        Some(sha256_file(&opts.output).await?)
    } else {
        None
    };
    if let (Some(expected), Some(actual)) = (&opts.expected_sha256, &digest) {
        if !digest_matches(expected, actual) {
            return Err(Error::ChecksumMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            });
        }
    }

    let elapsed = started.elapsed();
    emit(ProgressEvent::Finished {
        bytes: size,
        elapsed_ms: elapsed.as_millis() as u64,
        sha256: digest.clone(),
    });
    if let Err(e) = work.reset().await {
        eprintln!("hazar: could not remove {}: {e}", work.root.display());
    }

    Ok(HlsOutcome {
        path: opts.output,
        size,
        sha256: digest,
        segments: total,
        variant: plan.variant,
        resumed,
        elapsed,
    })
}

/// Build a plan from sniffed segment URLs (no playlist available).
pub fn plan_from_segments(urls: &[String], base: &str) -> Result<HlsPlan> {
    let base = Url::parse(base).ok();
    let mut segments = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        let url = match (&base, Url::parse(url)) {
            (_, Ok(absolute)) => absolute.to_string(),
            (Some(base), Err(_)) => base
                .join(url)
                .map(|u| u.to_string())
                .map_err(|e| Error::Protocol(format!("bad segment url {url}: {e}")))?,
            (None, Err(e)) => return Err(Error::Protocol(format!("bad segment url {url}: {e}"))),
        };
        segments.push(Segment {
            url,
            byterange: None,
            key: None,
            seq: index as u64,
            init: false,
        });
    }
    if segments.is_empty() {
        return Err(Error::Protocol("no segments to download".into()));
    }
    Ok(HlsPlan {
        manifest: base.map(|u| u.to_string()).unwrap_or_default(),
        variant: None,
        segments,
    })
}

async fn resolve_plan(client: &reqwest::Client, manifest: &str) -> Result<HlsPlan> {
    let mut current = manifest.to_string();
    let mut variant = None;

    for _ in 0..MAX_PLAYLIST_HOPS {
        let base = Url::parse(&current)
            .map_err(|e| Error::Protocol(format!("bad playlist url {current}: {e}")))?;
        let text = fetch_text(client, &current).await?;
        match parse_playlist(&text, &base)? {
            Playlist::Master(variants) => {
                let picked = best_variant(&variants)
                    .ok_or_else(|| Error::Protocol("master playlist has no variants".into()))?;
                variant = Some(picked.uri.clone());
                current = picked.uri.clone();
            }
            Playlist::Media { segments, .. } => {
                if segments.is_empty() {
                    return Err(Error::Protocol("media playlist has no segments".into()));
                }
                return Ok(HlsPlan {
                    manifest: manifest.to_string(),
                    variant,
                    segments,
                });
            }
        }
    }
    Err(Error::Protocol("playlist nesting too deep".into()))
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: url.to_string(),
        });
    }
    Ok(resp.text().await?)
}

async fn fetch_segment(
    client: &reqwest::Client,
    segment: &Segment,
    keys: &Arc<tokio::sync::Mutex<HashMap<String, Vec<u8>>>>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    let mut request = client.get(&segment.url);
    if let Some((start, end)) = segment.byterange {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    }
    let resp = request.send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: segment.url.clone(),
        });
    }

    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.map(|f| f.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(Error::Cancelled);
        }
        data.extend_from_slice(&chunk?);
    }

    match &segment.key {
        Some(key) if key.method.eq_ignore_ascii_case("AES-128") => {
            let uri = key
                .uri
                .as_ref()
                .ok_or_else(|| Error::Protocol("AES-128 key without URI".into()))?;
            let key_bytes = {
                let mut cache = keys.lock().await;
                match cache.get(uri) {
                    Some(bytes) => bytes.clone(),
                    None => {
                        let bytes = fetch_key(client, uri).await?;
                        cache.insert(uri.clone(), bytes.clone());
                        bytes
                    }
                }
            };
            decrypt_aes128_cbc(&key_bytes, &iv_for(key, segment.seq), &data)
        }
        _ => Ok(data),
    }
}

async fn fetch_key(client: &reqwest::Client, uri: &str) -> Result<Vec<u8>> {
    let resp = client.get(uri).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: uri.to_string(),
        });
    }
    let bytes = resp.bytes().await?.to_vec();
    if bytes.len() != 16 {
        return Err(Error::Protocol(format!(
            "AES-128 key must be 16 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Explicit IV, else the media sequence number as a 16-byte big-endian value.
fn iv_for(key: &Key, seq: u64) -> [u8; 16] {
    if let Some(iv) = key.iv {
        return iv;
    }
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn decrypt_aes128_cbc(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.len() % 16 != 0 {
        return Err(Error::Protocol(format!(
            "encrypted segment length {} is not a multiple of 16",
            data.len()
        )));
    }
    let decryptor = Aes128Cbc::new_from_slices(key, iv)
        .map_err(|e| Error::Protocol(format!("aes key/iv: {e}")))?;
    // Most streams pad with PKCS#7; some pack the payload exactly.
    match decryptor.clone().decrypt_padded_vec_mut::<Pkcs7>(data) {
        Ok(plain) => Ok(plain),
        Err(_) => {
            use aes::cipher::block_padding::NoPadding;
            decryptor
                .decrypt_padded_vec_mut::<NoPadding>(data)
                .map_err(|e| Error::Protocol(format!("aes decrypt: {e}")))
        }
    }
}

impl WorkDir {
    fn plan_path(&self) -> PathBuf {
        self.root.join("hls-plan.json")
    }

    fn segment_path(&self, index: usize) -> PathBuf {
        self.root.join(format!("seg-{index:05}.bin"))
    }

    async fn segment_ready(&self, index: usize) -> bool {
        tokio::fs::metadata(self.segment_path(index))
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }

    /// Write to `.partial` first so a crash never leaves a half segment that
    /// resume would treat as complete.
    async fn write_segment(&self, index: usize, data: &[u8]) -> Result<()> {
        let final_path = self.segment_path(index);
        let partial = final_path.with_extension("partial");
        let mut file = tokio::fs::File::create(&partial).await?;
        file.write_all(data).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, &final_path).await?;
        Ok(())
    }

    async fn assemble(&self, output: &std::path::Path, count: usize) -> Result<u64> {
        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let tmp = output.with_extension("assembling");
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut total = 0u64;
        for index in 0..count {
            let mut input = tokio::fs::File::open(self.segment_path(index)).await?;
            total += tokio::io::copy(&mut input, &mut out).await?;
        }
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        tokio::fs::rename(&tmp, output).await?;
        Ok(total)
    }
}
