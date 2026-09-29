use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use hazar_engine::{
    channel, download_hls, probe, DownloadOptions, Downloader, HlsOptions, ProgressEvent,
    ResourceInfo, DEFAULT_CONNECTIONS,
};

#[derive(Parser)]
#[command(
    name = "hazar",
    version = hazar_engine::VERSION,
    about = "Hazar — multi-connection download engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL.
    Get {
        url: String,
        /// Where to save it (default: server-provided file name).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Parallel connections.
        #[arg(short = 'n', long, default_value_t = DEFAULT_CONNECTIONS)]
        connections: usize,
        /// Verify the result against this SHA-256.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
        /// Smallest part size in MiB (guards tiny files).
        #[arg(long, default_value_t = 1, value_name = "MIB")]
        min_part_mib: u64,
        /// Ignore any existing part state and start over.
        #[arg(long)]
        no_resume: bool,
        /// Override the User-Agent header.
        #[arg(long)]
        user_agent: Option<String>,
        /// Referer header (some CDNs require the page URL).
        #[arg(long)]
        referer: Option<String>,
        /// Cookie header, e.g. `sid=abc; theme=dark`.
        #[arg(long)]
        cookie: Option<String>,
        /// Global speed cap in MB/s (0 = unlimited).
        #[arg(long, value_name = "MBPS")]
        speed_limit: Option<f64>,
    },
    /// Download an HLS (m3u8) stream into one file.
    Hls {
        url: String,
        /// Output file (default: playlist name with .ts).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Parallel segment downloads.
        #[arg(short = 'n', long, default_value_t = DEFAULT_CONNECTIONS)]
        connections: usize,
        /// Verify the result against this SHA-256.
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
        #[arg(long)]
        user_agent: Option<String>,
        #[arg(long)]
        referer: Option<String>,
        #[arg(long)]
        cookie: Option<String>,
    },
    /// Download a DASH (mpd) manifest into one file.
    Dash {
        url: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(short = 'n', long, default_value_t = DEFAULT_CONNECTIONS)]
        connections: usize,
        #[arg(long, value_name = "HEX")]
        sha256: Option<String>,
        #[arg(long)]
        user_agent: Option<String>,
        #[arg(long)]
        referer: Option<String>,
        #[arg(long)]
        cookie: Option<String>,
    },
    /// Print what the server reports for a URL.
    Probe { url: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("hazar: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(async {
        match cli.command {
            Command::Dash {
                url,
                out,
                connections,
                sha256,
                user_agent,
                referer,
                cookie,
            } => {
                dash_cmd(HlsArgs {
                    url,
                    out,
                    connections,
                    sha256,
                    user_agent,
                    referer,
                    cookie,
                })
                .await
            }
            Command::Probe { url } => probe_cmd(&url).await,
            Command::Hls {
                url,
                out,
                connections,
                sha256,
                user_agent,
                referer,
                cookie,
            } => {
                hls_cmd(HlsArgs {
                    url,
                    out,
                    connections,
                    sha256,
                    user_agent,
                    referer,
                    cookie,
                })
                .await
            }
            Command::Get {
                url,
                out,
                connections,
                sha256,
                min_part_mib,
                no_resume,
                user_agent,
                referer,
                cookie,
                speed_limit,
            } => {
                get_cmd(GetArgs {
                    url,
                    out,
                    connections,
                    sha256,
                    min_part_mib,
                    no_resume,
                    user_agent,
                    referer,
                    cookie,
                    speed_limit,
                })
                .await
            }
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hazar: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn dash_cmd(args: HlsArgs) -> Result<(), String> {
    let dest = args.out.clone().unwrap_or_else(|| {
        let mut path = default_hls_name(&args.url);
        path.set_extension("mp4");
        path
    });
    println!("{} → {} (dash)", args.url, dest.display());

    let cancel = Arc::new(AtomicBool::new(false));
    spawn_cancel_watcher(cancel.clone());

    let options = hazar_engine::DashOptions {
        manifest: args.url,
        output: dest,
        connections: args.connections,
        user_agent: args.user_agent,
        headers: extra_headers(args.referer, args.cookie),
        expected_sha256: args.sha256,
        cancel: Some(cancel),
    };

    let (tx, mut rx) = channel();
    let mut renderer = Renderer::new();
    let handle = tokio::spawn(async move { hazar_engine::download_dash(options, Some(tx)).await });
    while let Some(event) = rx.recv().await {
        renderer.on(&event);
    }
    let outcome = handle
        .await
        .map_err(|error| format!("dash task failed: {error}"))?
        .map_err(|error| error.to_string())?;

    renderer.finish(
        &outcome.path,
        outcome.size,
        outcome.elapsed,
        outcome.sha256.as_deref(),
    );
    println!(
        "segments: {} | representation: {}",
        outcome.segments,
        outcome.representation.as_deref().unwrap_or("-")
    );
    Ok(())
}

async fn probe_cmd(url: &str) -> Result<(), String> {
    let client = hazar_engine::default_client(None).map_err(|e| e.to_string())?;
    let info = probe(&client, url).await.map_err(|e| e.to_string())?;
    print_info(&info);
    Ok(())
}

fn print_info(info: &ResourceInfo) {
    let size = info
        .len
        .map(human_bytes)
        .unwrap_or_else(|| "unknown".to_string());
    println!("url              {}", info.requested_url);
    println!("final url        {}", info.final_url);
    println!("size             {size}");
    println!(
        "range requests   {}",
        if info.accept_ranges { "yes" } else { "no" }
    );
    println!("content type     {}", info.content_type.as_deref().unwrap_or("-"));
    println!("etag             {}", info.etag.as_deref().unwrap_or("-"));
    println!("last modified    {}", info.last_modified.as_deref().unwrap_or("-"));
    println!("file name        {}", info.filename_hint.as_deref().unwrap_or("-"));
}

struct GetArgs {
    url: String,
    out: Option<PathBuf>,
    connections: usize,
    sha256: Option<String>,
    min_part_mib: u64,
    no_resume: bool,
    user_agent: Option<String>,
    referer: Option<String>,
    cookie: Option<String>,
    speed_limit: Option<f64>,
}

struct HlsArgs {
    url: String,
    out: Option<PathBuf>,
    connections: usize,
    sha256: Option<String>,
    user_agent: Option<String>,
    referer: Option<String>,
    cookie: Option<String>,
}

fn extra_headers(referer: Option<String>, cookie: Option<String>) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let Some(referer) = referer {
        headers.push(("Referer".to_string(), referer));
    }
    if let Some(cookie) = cookie {
        headers.push(("Cookie".to_string(), cookie));
    }
    headers
}

fn default_hls_name(url: &str) -> PathBuf {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("stream");
    let stem = last.split('.').next().unwrap_or("stream");
    PathBuf::from(format!("{stem}.ts"))
}

/// Ctrl-C sets the engine's cancel flag; part/segment state stays on disk.
fn spawn_cancel_watcher(cancel: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.store(true, Ordering::Relaxed);
            eprintln!("\ninterrupted — state kept, run the same command to resume");
        }
    });
}

async fn hls_cmd(args: HlsArgs) -> Result<(), String> {
    let dest = args.out.clone().unwrap_or_else(|| default_hls_name(&args.url));
    println!("{} → {} (hls)", args.url, dest.display());

    let cancel = Arc::new(AtomicBool::new(false));
    spawn_cancel_watcher(cancel.clone());

    let opts = HlsOptions {
        manifest: args.url,
        segments: None,
        base_url: None,
        output: dest,
        connections: args.connections,
        user_agent: args.user_agent,
        headers: extra_headers(args.referer, args.cookie),
        expected_sha256: args.sha256,
        cancel: Some(cancel),
    };

    let (tx, mut rx) = channel();
    let mut renderer = Renderer::new();
    let handle = tokio::spawn(async move { download_hls(opts, Some(tx)).await });
    while let Some(event) = rx.recv().await {
        renderer.on(&event);
    }
    let outcome = handle
        .await
        .map_err(|e| format!("hls task failed: {e}"))?
        .map_err(|e| e.to_string())?;

    renderer.finish(
        &outcome.path,
        outcome.size,
        outcome.elapsed,
        outcome.sha256.as_deref(),
    );
    println!(
        "segments: {} | variant: {}",
        outcome.segments,
        outcome.variant.as_deref().unwrap_or("-")
    );
    Ok(())
}

async fn get_cmd(args: GetArgs) -> Result<(), String> {
    let probe_client = hazar_engine::default_client(args.user_agent.as_deref())
        .map_err(|e| e.to_string())?;
    let info = probe(&probe_client, &args.url)
        .await
        .map_err(|e| e.to_string())?;

    let dest = args
        .out
        .clone()
        .or_else(|| info.filename_hint.clone().map(PathBuf::from))
        .ok_or_else(|| "could not guess a file name, pass --out".to_string())?;

    if let Some(size) = info.len {
        println!("{} → {} ({})", args.url, dest.display(), human_bytes(size));
        if info.accept_ranges {
            println!(
                "range requests supported, up to {} connections",
                args.connections
            );
        } else {
            println!("range requests unsupported, falling back to a single connection");
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    spawn_cancel_watcher(cancel.clone());

    let mut opts = DownloadOptions::new(&args.url, &dest)
        .connections(args.connections)
        .min_part_size(args.min_part_mib.max(1) * 1024 * 1024)
        .resume(!args.no_resume)
        .cancel_flag(cancel)
        .headers(extra_headers(args.referer.clone(), args.cookie.clone()));
    if let Some(mbps) = args.speed_limit.filter(|value| *value > 0.0) {
        opts = opts.speed_limit((mbps * 1024.0 * 1024.0) as u64);
    }
    if let Some(sha) = args.sha256.clone() {
        opts = opts.sha256(sha);
    }
    if let Some(ua) = args.user_agent.clone() {
        opts = opts.user_agent(ua);
    }

    let (tx, mut rx) = channel();
    let downloader = Downloader::new(opts)
        .map_err(|e| e.to_string())?
        .with_progress(tx);

    let mut renderer = Renderer::new();
    let handle = tokio::spawn(async move { downloader.run().await });
    while let Some(event) = rx.recv().await {
        renderer.on(&event);
    }
    let outcome = handle
        .await
        .map_err(|e| format!("download task failed: {e}"))?
        .map_err(|e| e.to_string())?;

    renderer.finish(&outcome.path, outcome.size, outcome.elapsed, outcome.sha256.as_deref());
    Ok(())
}

struct Renderer {
    total: u64,
    connections: usize,
    last_bytes: u64,
    last_time: Instant,
    speed: f64,
    last_line: usize,
    live: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            total: 0,
            connections: 0,
            last_bytes: 0,
            last_time: Instant::now(),
            speed: 0.0,
            last_line: 0,
            live: false,
        }
    }

    fn on(&mut self, event: &ProgressEvent) {
        match event {
            ProgressEvent::Probing { .. } => {}
            ProgressEvent::Planned {
                size,
                connections,
                resumed,
                bytes_done,
            } => {
                self.total = *size;
                self.connections = *connections;
                if *resumed && *bytes_done > 0 {
                    println!(
                        "resuming: {} of {} already on disk, {connections} parts",
                        human_bytes(*bytes_done),
                        human_bytes(*size)
                    );
                } else {
                    println!(
                        "starting: {} in {connections} parts, {}",
                        human_bytes(*size),
                        human_bytes(*size / (*connections).max(1) as u64)
                    );
                }
            }
            ProgressEvent::FalldownSingle { reason } => {
                println!("single connection mode: {reason}");
            }
            ProgressEvent::HlsPlanned {
                segments,
                variant,
                resumed,
                done,
            } => {
                self.clear_line();
                println!(
                    "hls: {segments} segments{}",
                    variant
                        .as_deref()
                        .map(|v| format!(" · {v}"))
                        .unwrap_or_default()
                );
                if *resumed && *done > 0 {
                    println!("resuming: {done}/{segments} segments already on disk");
                }
            }
            ProgressEvent::HlsSegment { done, total, bytes } => {
                self.clear_line();
                println!(
                    "  segments {:>4}/{}  {}",
                    done,
                    total,
                    human_bytes(*bytes)
                );
            }
            ProgressEvent::Retrying {
                index,
                attempt,
                reason,
            } => {
                self.clear_line();
                println!("part {index}: retry {attempt} ({reason})");
            }
            ProgressEvent::PartProgress {
                total_written,
                total_size,
                ..
            } => {
                self.total = (*total_size).max(self.total);
                self.draw(*total_written);
            }
            ProgressEvent::Assembling { parts } => {
                self.clear_line();
                println!("assembling {parts} parts");
            }
            ProgressEvent::Verifying => {
                self.clear_line();
                println!("verifying sha256");
            }
            ProgressEvent::Finished { .. } | ProgressEvent::Failed { .. } => {}
        }
    }

    fn draw(&mut self, written: u64) {
        let elapsed = self.last_time.elapsed();
        if elapsed >= Duration::from_millis(250) {
            let delta = written.saturating_sub(self.last_bytes) as f64;
            let instant = delta / elapsed.as_secs_f64().max(0.001);
            self.speed = if self.speed == 0.0 {
                instant
            } else {
                self.speed * 0.7 + instant * 0.3
            };
            self.last_bytes = written;
            self.last_time = Instant::now();
        }

        let ratio = if self.total > 0 {
            (written as f64 / self.total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let width = 28usize;
        let filled = (ratio * width as f64).round() as usize;
        let bar = format!("{}{}", "#".repeat(filled), "-".repeat(width - filled));
        let eta = if self.speed > 1.0 && self.total > written {
            let secs = (self.total - written) as f64 / self.speed;
            Some(Duration::from_secs_f64(secs))
        } else {
            None
        };

        let line = format!(
            "  [{bar}] {:>5.1}%  {:>10}/s  ETA {}  {}",
            ratio * 100.0,
            human_bytes(self.speed.max(0.0) as u64),
            eta.map(human_duration).unwrap_or_else(|| "--:--".to_string()),
            human_bytes(written),
        );
        self.last_line = line.chars().count();
        self.live = true;
        print!("\r{line}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn clear_line(&mut self) {
        if self.live {
            print!("\r{}\r", " ".repeat(self.last_line));
            use std::io::Write;
            let _ = std::io::stdout().flush();
            self.live = false;
        }
    }

    fn finish(
        &mut self,
        path: &std::path::Path,
        size: u64,
        elapsed: Duration,
        sha256: Option<&str>,
    ) {
        self.clear_line();
        let speed = size as f64 / elapsed.as_secs_f64().max(0.001);
        println!(
            "done: {} → {} in {} ({}/s)",
            human_bytes(size),
            path.display(),
            human_duration(elapsed),
            human_bytes(speed as u64)
        );
        if let Some(sha) = sha256 {
            println!("sha256: {sha}");
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}
