//! Hazar capture test matrix.
//!
//! Every row is a deterministic local scenario: the resolver must find the
//! stream, the engine must download it, and the bytes must match. Red rows are
//! capability gaps; the runner exits non-zero so the goal loop keeps going.
//!
//! `--browser` adds the headless-Chrome + extension layer (see `browser.rs`) and
//! `--live` adds the real-site profiles (detection only).

pub mod browser;
pub mod fixtures;
pub mod live;
pub mod relay;
pub mod server;

use std::path::{Path, PathBuf};
use std::time::Instant;

use hazar_engine::resolve::{resolve, Candidate, MediaKind, ResolveOptions};
use hazar_engine::{download_hls, DownloadOptions, Downloader, HlsOptions};

use fixtures::{Case, Expectation};

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Only run rows whose name contains this substring.
    pub filter: Option<String>,
    /// Add the headless-Chrome + extension rows.
    pub browser: bool,
    /// Add the live site profiles (needs HAZAR_LIVE=1).
    pub live: bool,
    /// Keep downloaded files for inspection.
    pub keep_files: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub status: Status,
    pub kind: String,
    /// Strategy that produced the winning candidate ("-" when not applicable).
    pub strategy: String,
    /// Every strategy that produced a candidate for this fixture, best first.
    pub found: Vec<String>,
    pub detail: String,
    pub ms: u128,
}

#[derive(Debug, Default)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Pass)
            .count()
    }

    pub fn failed(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Fail)
            .count()
    }

    pub fn skipped(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == Status::Skip)
            .count()
    }

    pub fn is_green(&self) -> bool {
        self.failed() == 0
    }

    pub fn print(&self) {
        let width = self
            .rows
            .iter()
            .map(|row| row.name.len())
            .max()
            .unwrap_or(20)
            .max(20);
        println!(
            "\n{:<width$}  {:<4}  {:<10}  {:<16}  {}",
            "fixture", "st", "kind", "strategy", "detail"
        );
        println!("{}", "-".repeat(width + 46));
        for row in &self.rows {
            println!(
                "{:<width$}  {:<4}  {:<10}  {:<16}  {} ({} ms)",
                row.name,
                row.status.label(),
                row.kind,
                row.strategy,
                row.detail,
                row.ms
            );
        }
        println!(
            "\n{} rows: {} passed, {} failed, {} skipped",
            self.rows.len(),
            self.passed(),
            self.failed(),
            self.skipped()
        );

        // Strategy score table: how often each strategy produced a candidate and
        // how often it produced the winning one.
        let mut mentions: Vec<(&str, usize, usize)> = hazar_engine::resolve::STRATEGIES
            .iter()
            .map(|strategy| {
                let seen = self
                    .rows
                    .iter()
                    .filter(|row| row.found.iter().any(|found| found == strategy))
                    .count();
                let wins = self
                    .rows
                    .iter()
                    .filter(|row| row.strategy == *strategy)
                    .count();
                (*strategy, seen, wins)
            })
            .collect();
        mentions.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));

        println!("\nstrategy            fixtures  wins");
        println!("{}", "-".repeat(38));
        for (strategy, seen, wins) in mentions {
            if seen == 0 && wins == 0 {
                continue;
            }
            println!("{strategy:<18}  {seen:>7}  {wins:>4}");
        }
    }

    /// Fixture × strategy coverage matrix (which strategy saw which fixture).
    pub fn print_coverage(&self) {
        let strategies = hazar_engine::resolve::STRATEGIES;
        let width = self
            .rows
            .iter()
            .map(|row| row.name.len())
            .max()
            .unwrap_or(20)
            .max(20);

        println!("\ncoverage matrix (◆ = winning strategy, · = candidate seen)");
        print!("{:<width$} ", "fixture");
        for strategy in strategies {
            let code: String = strategy.chars().take(4).collect();
            print!("{code:>4}");
        }
        println!();
        println!("{}", "-".repeat(width + 4 * strategies.len() + 1));
        for row in &self.rows {
            print!("{:<width$} ", row.name);
            for strategy in strategies {
                let mark = if row.strategy == *strategy {
                    "◆"
                } else if row.found.iter().any(|found| found == strategy) {
                    "·"
                } else {
                    " "
                };
                print!("{:>4}", mark);
            }
            println!();
        }
    }

    pub fn to_json(&self) -> String {
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                format!(
                    "  {{\"name\":\"{}\",\"status\":\"{}\",\"kind\":\"{}\",\"strategy\":\"{}\",\"found\":[{}],\"detail\":\"{}\",\"ms\":{}}}",
                    row.name,
                    row.status.label().to_lowercase(),
                    row.kind,
                    row.strategy,
                    row.found
                        .iter()
                        .map(|found| format!("\"{found}\""))
                        .collect::<Vec<String>>()
                        .join(","),
                    row.detail.replace('"', "'"),
                    row.ms
                )
            })
            .collect();
        format!("{{\"rows\":[\n{}\n]}}", rows.join(",\n"))
    }
}

/// Lowercase hex SHA-256, used to let the engine verify every matrix row.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn work_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hazar-matrix-{}-{}", tag, std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Run the offline fixture matrix.
pub async fn run(opts: &RunOptions) -> Report {
    let built = fixtures::build();
    let mut routes = built.routes.clone();
    routes.extend(fixtures::live_like_routes());
    let server = server::serve(routes)
        .await
        .expect("fixture server must bind");
    let client = hazar_engine::default_client(None).expect("http client");
    let root = work_root("offline");

    let mut report = Report::default();

    for case in &built.cases {
        if let Some(filter) = &opts.filter {
            if !case.name.contains(filter.as_str()) {
                continue;
            }
        }

        let page_url = server.url(&case.page_path);
        let resolve_options = ResolveOptions {
            headers: case.headers.clone(),
            ..Default::default()
        };

        let started = Instant::now();
        let resolved = resolve(&client, &page_url, &resolve_options).await;
        if std::env::var("HAZAR_DEBUG").is_ok() {
            eprintln!("\n== {} ({} candidates)", case.name, resolved.candidates.len());
            for candidate in &resolved.candidates {
                eprintln!(
                    "   {:<16} {:>3}  {}  {}",
                    candidate.strategy,
                    candidate.confidence,
                    candidate.kind.as_str(),
                    candidate.url
                );
            }
        }
        let row = evaluate(case, &resolved, &root).await;
        let mut row = row;
        row.ms = started.elapsed().as_millis();
        if row.status != Status::Pass && !resolved.errors.is_empty() {
            row.detail = format!("{} | resolver errors: {}", row.detail, resolved.errors.join("; "));
        }
        report.rows.push(row);
    }

    // Prove the live-layer logic locally before the user runs it against the
    // network: same profiles, same assertions, fixture-served pages.
    let mut selftest = live::run_selftest_rows(&server.base_url()).await;
    if let Some(filter) = &opts.filter {
        selftest.retain(|row| row.name.contains(filter.as_str()));
    }
    report.rows.extend(selftest);

    if opts.browser {
        report.rows.extend(browser::run_rows().await);
        report
            .rows
            .extend(live::run_selftest_browser_rows(&server.base_url()).await);
    }
    if opts.live {
        report.rows.extend(live::run_rows().await);
    }

    // Row-name filter applies to every layer, so a single row can be iterated on.
    if let Some(filter) = &opts.filter {
        report.rows.retain(|row| row.name.contains(filter.as_str()));
    }

    if !opts.keep_files {
        let _ = std::fs::remove_dir_all(&root);
    }
    report
}

async fn evaluate(
    case: &Case,
    resolved: &hazar_engine::ResolveReport,
    root: &Path,
) -> Row {
    let base = Row {
        name: case.name.to_string(),
        status: Status::Fail,
        kind: match case.expectation {
            Expectation::Download { kind } | Expectation::DetectOnly { kind, .. } => {
                kind.as_str().to_string()
            }
            Expectation::Refused { .. } => "refused".to_string(),
        },
        strategy: "-".to_string(),
        found: Vec::new(),
        detail: String::new(),
        ms: 0,
    };

    let mut found: Vec<String> = Vec::new();
    for candidate in &resolved.candidates {
        if !found.iter().any(|seen| seen == candidate.strategy) {
            found.push(candidate.strategy.to_string());
        }
    }

    let Some(candidate) = resolved.best() else {
        return Row {
            strategy: "-".to_string(),
            found,
            detail: format!(
                "no candidate (visited {} document(s))",
                resolved.visited.len()
            ),
            ..base
        };
    };

    let mut row = Row {
        strategy: candidate.strategy.to_string(),
        found,
        ..base
    };

    match case.expectation {
        Expectation::DetectOnly { kind, drm } => {
            if candidate.kind != kind {
                row.detail = format!("detected {} but expected {}", candidate.kind.as_str(), kind.as_str());
                return row;
            }
            if let Some(expected_drm) = drm {
                if candidate.drm.as_deref() != Some(expected_drm) {
                    row.detail = format!(
                        "drm was {:?}, expected {expected_drm}",
                        candidate.drm
                    );
                    return row;
                }
            }
            row.status = Status::Pass;
            row.detail = match candidate.drm.as_deref() {
                Some(drm) => format!("detected {} ({drm}), download gated", candidate.kind.as_str()),
                None => format!("detected {}, download gated", candidate.kind.as_str()),
            };
        }
        Expectation::Refused { contains } => match download(candidate, case, root).await {
            Err(error) if error.contains(contains) => {
                row.status = Status::Pass;
                row.detail = format!("refused cleanly: {error}");
            }
            Err(error) => row.detail = format!("wrong error: {error}"),
            Ok(_) => row.detail = "download unexpectedly succeeded".to_string(),
        },
        Expectation::Download { kind } => {
            let candidates: Vec<&Candidate> = resolved
                .candidates
                .iter()
                .filter(|candidate| candidate.kind == kind)
                .collect();
            if candidates.is_empty() {
                row.detail = format!(
                    "no {} candidate (best was {} {})",
                    kind.as_str(),
                    candidate.kind.as_str(),
                    candidate.url
                );
                return row;
            }

            // Real pages list several mirrors; walk them in confidence order.
            let mut tried = 0usize;
            let mut last_error = String::new();
            for candidate in candidates {
                tried += 1;
                match download(candidate, case, root).await {
                    Ok(path) => match std::fs::read(&path) {
                        Ok(bytes) if bytes == case.expected_bytes => {
                            row.status = Status::Pass;
                            row.strategy = candidate.strategy.to_string();
                            row.detail = format!(
                                "{} bytes + sha256 verified ({} candidate{})",
                                bytes.len(),
                                tried,
                                if tried == 1 { "" } else { "s tried" }
                            );
                            return row;
                        }
                        Ok(bytes) => {
                            last_error = format!(
                                "byte mismatch: got {} expected {} [{}]",
                                bytes.len(),
                                case.expected_bytes.len(),
                                candidate.url
                            )
                        }
                        Err(error) => last_error = format!("cannot read output: {error}"),
                    },
                    Err(error) => {
                        last_error = format!("{error} [{}]", candidate.url);
                    }
                }
            }

            row.detail = if last_error.is_empty() {
                "no candidate could be downloaded".to_string()
            } else {
                last_error
            };
        }
    }

    row
}

async fn download(candidate: &Candidate, case: &Case, root: &Path) -> Result<PathBuf, String> {
    let dest = root.join(format!("{}.out", case.name));
    let mut headers = candidate.headers.clone();
    headers.extend(case.headers.clone());
    // Rows that expect output hand the digest to the engine, so the matrix
    // exercises the engine's own checksum verification (not just a byte diff).
    let digest = (!case.expected_bytes.is_empty()).then(|| sha256_hex(&case.expected_bytes));

    match candidate.kind {
        MediaKind::Hls => {
            let options = HlsOptions {
                manifest: candidate.url.clone(),
                segments: candidate.segments.clone(),
                base_url: candidate.page_url.clone(),
                output: dest,
                connections: 4,
                user_agent: None,
                headers,
                expected_sha256: digest,
                cancel: None,
            };
            download_hls(options, None)
                .await
                .map(|outcome| outcome.path)
                .map_err(|error| error.to_string())
        }
        MediaKind::File => {
            let mut options = DownloadOptions::new(candidate.url.clone(), dest)
                .connections(4)
                .min_part_size(16 * 1024)
                .headers(headers);
            if let Some(digest) = digest {
                options = options.sha256(digest);
            }
            let downloader = Downloader::new(options).map_err(|error| error.to_string())?;
            downloader
                .run()
                .await
                .map(|outcome| outcome.path)
                .map_err(|error| error.to_string())
        }
        MediaKind::Dash => Err("DASH download not implemented yet".to_string()),
    }
}
