use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header;
use tokio::io::AsyncWriteExt;

use crate::assemble::assemble;
use crate::error::{Error, Result};
use crate::hash::{digest_matches, sha256_file};
use crate::meta::{DownloadMeta, PartState, WorkDir};
use crate::plan::{PlanOptions, DEFAULT_CONNECTIONS, DEFAULT_MIN_PART_SIZE};
use crate::probe::{probe, ResourceInfo};
use crate::progress::{ProgressEvent, ProgressSender};

const EMIT_INTERVAL: Duration = Duration::from_millis(120);

/// Average-rate limiter shared by every worker of one download.
///
/// Deliberately simple (token bucket by elapsed time): a download that runs
/// ahead of the limit sleeps until it is back on schedule.
#[derive(Debug)]
pub struct RateLimiter {
    bytes_per_sec: u64,
    state: Mutex<(Instant, u64)>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes_per_sec: bytes_per_sec.max(1),
            state: Mutex::new((Instant::now(), 0)),
        })
    }

    /// Account `bytes` transferred and sleep if we are ahead of the limit.
    pub async fn acquire(&self, bytes: u64) {
        let wait = {
            let mut state = self.state.lock().expect("rate limiter");
            let (started, total) = &mut *state;
            *total += bytes;
            let expected = Duration::from_secs_f64(*total as f64 / self.bytes_per_sec as f64);
            expected.saturating_sub(started.elapsed())
        };
        if wait > Duration::from_millis(5) {
            tokio::time::sleep(wait).await;
        }
    }
}
const META_SAVE_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub dest: PathBuf,
    pub connections: usize,
    pub min_part_size: u64,
    pub expected_sha256: Option<String>,
    pub resume: bool,
    pub max_retries: u32,
    pub user_agent: Option<String>,
    /// Extra request headers (Referer, Cookie, Authorization, ...).
    pub headers: Vec<(String, String)>,
    /// Global speed cap in bytes/second (`None` = unlimited).
    pub speed_limit: Option<u64>,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl DownloadOptions {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            dest: dest.into(),
            connections: DEFAULT_CONNECTIONS,
            min_part_size: DEFAULT_MIN_PART_SIZE,
            expected_sha256: None,
            resume: true,
            max_retries: 3,
            user_agent: None,
            headers: Vec::new(),
            speed_limit: None,
            cancel: None,
        }
    }

    /// Cap the whole download (all connections together) to `bytes_per_sec`.
    pub fn speed_limit(mut self, bytes_per_sec: u64) -> Self {
        self.speed_limit = Some(bytes_per_sec.max(1));
        self
    }

    /// Add a request header (replaces an existing one with the same name).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
        self.headers.push((name, value.into()));
        self
    }

    pub fn headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (name, value) in headers {
            self = self.header(name, value);
        }
        self
    }

    pub fn connections(mut self, n: usize) -> Self {
        self.connections = n;
        self
    }

    pub fn min_part_size(mut self, bytes: u64) -> Self {
        self.min_part_size = bytes.max(1);
        self
    }

    pub fn sha256(mut self, hex: impl Into<String>) -> Self {
        self.expected_sha256 = Some(hex.into());
        self
    }

    pub fn resume(mut self, yes: bool) -> Self {
        self.resume = yes;
        self
    }

    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub connections: usize,
    pub resumed: bool,
    pub elapsed: Duration,
    pub info: ResourceInfo,
}

#[derive(Clone)]
pub struct Downloader {
    opts: DownloadOptions,
    client: reqwest::Client,
    progress: Option<ProgressSender>,
    limiter: Option<Arc<RateLimiter>>,
}

/// Shared client settings for probing and downloading.
pub fn default_client(user_agent: Option<&str>) -> Result<reqwest::Client> {
    default_client_with(user_agent, &[])
}

/// Proxy URL from the environment (`HAZAR_PROXY`, then the usual `*_proxy` vars).
///
/// Live-site rows often need one (geo blocks, bot walls). Loopback is always
/// bypassed so the fixtures, the bridge and the local API keep working.
pub fn proxy_from_env() -> Option<String> {
    for key in [
        "HAZAR_PROXY",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Same, with default request headers applied to every request (cookie, referer, ...).
pub fn default_client_with(
    user_agent: Option<&str>,
    headers: &[(String, String)],
) -> Result<reqwest::Client> {
    let proxy = proxy_from_env();
    default_client_full(user_agent, headers, proxy.as_deref())
}

/// Fully explicit client construction (headers + optional proxy).
pub fn default_client_full(
    user_agent: Option<&str>,
    headers: &[(String, String)],
    proxy: Option<&str>,
) -> Result<reqwest::Client> {
    let ua = user_agent
        .map(|ua| ua.to_string())
        .unwrap_or_else(|| format!("hazar/{}", env!("CARGO_PKG_VERSION")));

    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        match (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                map.insert(name, value);
            }
            _ => eprintln!("hazar: skipping malformed header {name:?}"),
        }
    }

    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(180))
        .pool_max_idle_per_host(DEFAULT_CONNECTIONS + 4)
        .default_headers(map)
        .user_agent(ua)
        .gzip(false).brotli(false);

    if let Some(proxy) = proxy {
        let parsed = reqwest::Proxy::all(proxy)
            .map_err(|error| crate::Error::Protocol(format!("bad proxy {proxy:?}: {error}")))?
            .no_proxy(reqwest::NoProxy::from_string("127.0.0.1,localhost,::1"));
        builder = builder.proxy(parsed);
    }

    Ok(builder.build()?)
}

impl Downloader {
    pub fn new(opts: DownloadOptions) -> Result<Self> {
        let client = default_client_with(opts.user_agent.as_deref(), &opts.headers)?;
        let limiter = opts.speed_limit.map(RateLimiter::new);
        Ok(Self {
            opts,
            client,
            progress: None,
            limiter,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn with_client(opts: DownloadOptions, client: reqwest::Client) -> Self {
        let limiter = opts.speed_limit.map(RateLimiter::new);
        Self {
            opts,
            client,
            progress: None,
            limiter,
        }
    }

    pub fn with_progress(mut self, tx: ProgressSender) -> Self {
        self.progress = Some(tx);
        self
    }

    pub fn options(&self) -> &DownloadOptions {
        &self.opts
    }

    /// Probe, plan, download (resuming when possible), assemble, verify.
    pub async fn run(&self) -> Result<Outcome> {
        let started = Instant::now();
        self.emit(ProgressEvent::Probing {
            url: self.opts.url.clone(),
        });

        let info = self.probe_with_retry().await?;
        let Some(size) = info.len else {
            self.emit(ProgressEvent::FalldownSingle {
                reason: "server did not report a size".into(),
            });
            return self.single_stream(&info, started).await;
        };

        if !info.supports_segments() {
            self.emit(ProgressEvent::FalldownSingle {
                reason: "range requests unsupported".into(),
            });
            return self.single_stream(&info, started).await;
        }

        self.segmented(info, size, started).await
    }

    /// Probing can hit a rate limit (429) or a flaky edge; retry before giving up.
    async fn probe_with_retry(&self) -> Result<ResourceInfo> {
        let mut attempt = 0u32;
        loop {
            match probe(&self.client, &self.opts.url).await {
                Ok(info) => return Ok(info),
                Err(e) => {
                    if !e.is_retryable() || attempt >= self.opts.max_retries {
                        return Err(e);
                    }
                    attempt += 1;
                    self.emit(ProgressEvent::Retrying {
                        index: 0,
                        attempt,
                        reason: format!("probe: {e}"),
                    });
                    tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
                }
            }
        }
    }

    async fn segmented(&self, info: ResourceInfo, size: u64, started: Instant) -> Result<Outcome> {
        let work = WorkDir::new(&self.opts.dest);
        let plan_opts = PlanOptions {
            connections: self.opts.connections,
            min_part_size: self.opts.min_part_size,
        };

        let existing = if self.opts.resume {
            work.load_meta().await?
        } else {
            None
        };
        let resumed = existing
            .as_ref()
            .map(|m| m.matches(&info, size))
            .unwrap_or(false);

        if !resumed {
            work.reset().await?;
        }

        let meta = if resumed {
            let mut meta = existing.expect("resumed implies meta");
            for part in meta.parts.iter_mut() {
                let actual = work.part_len(part.index).await?;
                if actual > part.length() {
                    work.truncate_part(part.index).await?;
                    part.written = 0;
                } else {
                    part.written = actual;
                }
            }
            meta
        } else {
            DownloadMeta::new(
                &self.opts.url,
                &info,
                size,
                self.opts.connections,
                crate::plan::plan_work(size, plan_opts),
            )
        };

        self.emit(ProgressEvent::Planned {
            size,
            connections: self.opts.connections.clamp(1, crate::plan::MAX_CONNECTIONS).min(meta.parts.len()),
            resumed,
            bytes_done: meta.bytes_done(),
        });

        let shared = Arc::new(Mutex::new(meta));
        // Bind the snapshot first: holding a MutexGuard across an await would
        // make the download future non-Send.
        let initial = shared.lock().expect("meta lock").clone();
        work.save_meta(&initial).await?;

        let stop = Arc::new(AtomicBool::new(false));
        let saver = {
            let work = work.clone();
            let shared = shared.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(META_SAVE_INTERVAL).await;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let snapshot = shared.lock().expect("meta lock").clone();
                    if work.save_meta(&snapshot).await.is_err() {
                        break;
                    }
                }
            })
        };

        let parts = shared.lock().expect("meta lock").parts.clone();
        let jobs = Arc::new(Mutex::new(std::collections::VecDeque::from(parts)));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..self.opts.connections.clamp(1, crate::plan::MAX_CONNECTIONS) {
            let this = self.clone();
            let work = work.clone();
            let shared = shared.clone();
            let jobs = jobs.clone();
            set.spawn(async move {
                loop {
                    let part = jobs.lock().expect("jobs").pop_front();
                    let Some(part) = part else {
                        return Ok(());
                    };
                    if this.cancelled() {
                        return Err(Error::Cancelled);
                    }
                    this.part_worker(part, work.clone(), shared.clone()).await?;
                }
            });
        }

        let mut fatal: Option<Error> = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let hard = matches!(e, Error::RangeIgnored | Error::Cancelled);
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
                        fatal = Some(Error::Io(std::io::Error::other("worker task failed")));
                    }
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
        saver.abort();
        let _ = saver.await;

        if let Some(e) = fatal {
            let snapshot = shared.lock().expect("meta lock").clone();
            let _ = work.save_meta(&snapshot).await;
            return match e {
                Error::RangeIgnored => {
                    self.emit(ProgressEvent::FalldownSingle {
                        reason: "server ignored the Range header".into(),
                    });
                    work.reset().await?;
                    self.single_stream(&info, started).await
                }
                Error::Cancelled => Err(e),
                other => {
                    self.emit(ProgressEvent::Failed {
                        reason: other.to_string(),
                    });
                    Err(other)
                }
            };
        }

        let final_meta = shared.lock().expect("meta lock").clone();
        work.save_meta(&final_meta).await?;

        self.emit(ProgressEvent::Assembling {
            parts: final_meta.parts.len(),
        });
        assemble(
            &work,
            &self.opts.dest,
            &final_meta,
            self.opts.expected_sha256.as_deref(),
        )
        .await?;

        let outcome = self
            .finish(
                &self.opts.dest,
                info,
                size,
                started,
                resumed,
                self.opts.connections.clamp(1, crate::plan::MAX_CONNECTIONS).min(final_meta.parts.len()),
            )
            .await?;
        // The file is on disk and verified; a leftover sidecar is not fatal.
        if let Err(e) = work.reset().await {
            eprintln!("hazar: could not remove {}: {e}", work.root.display());
        }
        Ok(outcome)
    }

    async fn part_worker(
        &self,
        part: PartState,
        work: WorkDir,
        shared: Arc<Mutex<DownloadMeta>>,
    ) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            let result = tokio::select! {
                result = self.try_part(part.index, &work, &shared) => result,
                _ = self.wait_cancel() => Err(Error::Cancelled),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let can_retry = e.is_retryable() && attempt < self.opts.max_retries;
                    if !can_retry {
                        return Err(e);
                    }
                    attempt += 1;
                    self.emit(ProgressEvent::Retrying {
                        index: part.index,
                        attempt,
                        reason: e.to_string(),
                    });
                    tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))).await;
                }
            }
        }
    }

    async fn wait_cancel(&self) {
        loop { if self.cancelled() { return; } tokio::time::sleep(Duration::from_millis(100)).await; }
    }

    async fn try_part(
        &self,
        index: u32,
        work: &WorkDir,
        shared: &Arc<Mutex<DownloadMeta>>,
    ) -> Result<()> {
        let (start, end, written, total, length) = {
            let meta = shared.lock().expect("meta lock");
            let part = &meta.parts[index as usize];
            (part.start, part.end, part.written, meta.size, part.length())
        };
        if written >= length {
            return Ok(());
        }

        let from = start + written;
        let mut request = self
            .client
            .get(&self.opts.url)
            .header(header::RANGE, format!("bytes={from}-{end}"));

        let validator = {
            let meta = shared.lock().expect("meta lock");
            meta.etag
                .clone()
                .filter(|v| !v.starts_with("W/"))
                .or_else(|| meta.last_modified.clone())
        };
        if let Some(validator) = validator {
            request = request.header(header::IF_RANGE, validator);
        }

        let resp = request.send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let expected = format!("bytes={from}-{end}/{total}");
            let actual = resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .replace("bytes ", "bytes=");
            if actual != expected {
                return Err(Error::Protocol(
                    "server returned an unexpected Content-Range".into(),
                ));
            }
        } else if status == reqwest::StatusCode::OK {
            // Server answered with the whole file; only valid when this part is all of it.
            if !(start == 0 && length == total && written == 0) {
                return Err(Error::RangeIgnored);
            }
        } else {
            return Err(Error::Status {
                status: status.as_u16(),
                url: self.opts.url.clone(),
            });
        }

        let mut file = work.open_part_append(index).await?;
        let mut got = written;
        let mut last_emit = Instant::now();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = tokio::select! { chunk = stream.next() => chunk, _ = self.wait_cancel() => return Err(Error::Cancelled) } {
            if self.cancelled() {
                file.flush().await?;
                self.save_snapshot(shared, work).await;
                return Err(Error::Cancelled);
            }
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    // Persist what we have so the retry continues from here.
                    let _ = file.flush().await;
                    drop(file);
                    self.save_snapshot(shared, work).await;
                    return Err(e.into());
                }
            };
            file.write_all(&chunk).await?;
            got += chunk.len() as u64;
            if let Some(limiter) = &self.limiter {
                limiter.acquire(chunk.len() as u64).await;
            }

            {
                let mut meta = shared.lock().expect("meta lock");
                if let Some(part) = meta.parts.get_mut(index as usize) {
                    part.written = got;
                }
            }

            if last_emit.elapsed() >= EMIT_INTERVAL {
                last_emit = Instant::now();
                let total_written = shared.lock().expect("meta lock").bytes_done();
                self.emit(ProgressEvent::PartProgress {
                    index,
                    part_written: got,
                    part_length: length,
                    total_written,
                    total_size: total,
                });
            }
        }

        file.flush().await?;
        drop(file);

        if got != length {
            return Err(Error::ShortBody {
                expected: length,
                got,
            });
        }
        self.save_snapshot(shared, work).await;
        Ok(())
    }

    /// Stream the file with a single connection (no ranges available).
    async fn single_stream(&self, info: &ResourceInfo, started: Instant) -> Result<Outcome> {
        let mut attempt = 0;
        loop {
            match self.single_attempt(info, started).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) if error.is_retryable() && attempt < self.opts.max_retries => {
                    attempt += 1;
                    self.emit(ProgressEvent::Retrying { index: 0, attempt, reason: error.to_string() });
                    tokio::select! { _ = tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))) => {}, _ = self.wait_cancel() => return Err(Error::Cancelled) }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn single_attempt(&self, info: &ResourceInfo, started: Instant) -> Result<Outcome> {
        let resp = tokio::select! { result = self.client.get(&self.opts.url).send() => result?, _ = self.wait_cancel() => return Err(Error::Cancelled) };
        if !resp.status().is_success() {
            return Err(Error::Status {
                status: resp.status().as_u16(),
                url: self.opts.url.clone(),
            });
        }
        let total = resp.content_length().or(info.len).unwrap_or(0);

        if let Some(parent) = self.opts.dest.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let staging = PathBuf::from(format!("{}.partial", self.opts.dest.display()));
        let mut file = tokio::fs::File::create(&staging).await?;
        let mut written = 0u64;
        let mut last_emit = Instant::now();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = tokio::select! { chunk = stream.next() => chunk, _ = self.wait_cancel() => return Err(Error::Cancelled) } {
            if self.cancelled() {
                file.flush().await?;
                return Err(Error::Cancelled);
            }
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            if let Some(limiter) = &self.limiter {
                limiter.acquire(chunk.len() as u64).await;
            }
            if last_emit.elapsed() >= EMIT_INTERVAL {
                last_emit = Instant::now();
                self.emit(ProgressEvent::PartProgress {
                    index: 0,
                    part_written: written,
                    part_length: total,
                    total_written: written,
                    total_size: total,
                });
            }
        }
        file.flush().await?;
        drop(file);

        if total > 0 && written != total {
            return Err(Error::ShortBody {
                expected: total,
                got: written,
            });
        }

        if let Some(expected) = &self.opts.expected_sha256 {
            let actual = sha256_file(&staging).await?;
            if !digest_matches(expected, &actual) {
                return Err(Error::ChecksumMismatch {
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        tokio::fs::rename(&staging, &self.opts.dest).await?;
        self.finish(&self.opts.dest, info.clone(), written, started, false, 1)
            .await
    }

    async fn finish(
        &self,
        path: &Path,
        info: ResourceInfo,
        size: u64,
        started: Instant,
        resumed: bool,
        connections: usize,
    ) -> Result<Outcome> {
        let digest = if self.opts.expected_sha256.is_some() {
            self.emit(ProgressEvent::Verifying);
            Some(sha256_file(path).await?)
        } else {
            None
        };

        if let (Some(expected), Some(actual)) = (&self.opts.expected_sha256, &digest) {
            if !digest_matches(expected, actual) {
                let error = Error::ChecksumMismatch {
                    expected: expected.clone(),
                    actual: actual.clone(),
                };
                self.emit(ProgressEvent::Failed {
                    reason: error.to_string(),
                });
                return Err(error);
            }
        }

        let elapsed = started.elapsed();
        self.emit(ProgressEvent::Finished {
            bytes: size,
            elapsed_ms: elapsed.as_millis() as u64,
            sha256: digest.clone(),
        });

        Ok(Outcome {
            path: path.to_path_buf(),
            size,
            sha256: digest,
            connections,
            resumed,
            elapsed,
            info,
        })
    }

    async fn save_snapshot(&self, shared: &Arc<Mutex<DownloadMeta>>, work: &WorkDir) {
        let snapshot = shared.lock().expect("meta lock").clone();
        let _ = work.save_meta(&snapshot).await;
    }

    fn cancelled(&self) -> bool {
        self.opts
            .cancel
            .as_ref()
            .map(|flag| flag.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_default_user_agent() {
        let client = default_client(None).expect("client");
        drop(client);
    }

    #[test]
    fn options_builder_clamps_min_part_size() {
        let opts = DownloadOptions::new("https://x/y", "/tmp/y")
            .connections(8)
            .min_part_size(0);
        assert_eq!(opts.min_part_size, 1);
    }
}
