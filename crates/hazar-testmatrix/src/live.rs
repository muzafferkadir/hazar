//! Live site profiles.
//!
//! Two transports, one assertion:
//!
//! * **HTTP** (`evaluate`) — plain resolver: fetch the page, walk iframes and
//!   scripts, rank candidates.
//! * **Browser** (`browser_rows`) — real headless Chromium with the extension
//!   loaded, for sites whose manifest only exists after JavaScript runs.
//!
//! Both assert **detection + first-segment fetch** (first 64 KiB) and never a
//! full download. `--live` (`HAZAR_LIVE=1`) runs the real sites; the same code
//! runs against local site-shaped pages every matrix run (`live-selftest-*`).
//!
//! Gated rows (DRM, signature/cipher delivery) pass by reporting the gate
//! cleanly — the project ships no DRM or cipher circumvention.

use std::time::{Duration, Instant};

use hazar_engine::resolve::{resolve, Candidate, MediaKind, ResolveOptions};
use hazar_engine::{best_variant, parse_playlist, Playlist};
use url::Url;

use crate::browser::BrowserSession;
use crate::{Row, Status};

/// Bytes fetched to prove a stream really plays (first segment only).
const PROBE_BYTES: u64 = 64 * 1024;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(25);
const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    Hls,
    Dash,
    File,
}

impl Expected {
    fn kind(&self) -> MediaKind {
        match self {
            Expected::Hls => MediaKind::Hls,
            Expected::Dash => MediaKind::Dash,
            Expected::File => MediaKind::File,
        }
    }
}

/// What a site's player needs before the stream is reachable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Needs {
    pub referer: bool,
    pub cookie: bool,
    pub iframe: bool,
}

impl Needs {
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.referer {
            parts.push("referer");
        }
        if self.cookie {
            parts.push("cookie");
        }
        if self.iframe {
            parts.push("iframe");
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join("+")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SiteProfile {
    pub name: &'static str,
    /// Absolute URL for real runs; a path for the self-test (joined with the
    /// fixture server base). `HAZAR_LIVE_<NAME>_URL` overrides it at runtime.
    pub url: &'static str,
    /// Player family the site uses (documentation + the strategy we expect).
    pub player: &'static str,
    pub expected: Expected,
    pub needs: Needs,
    /// True when the site tolerates a proxy exit (some block it outright).
    pub proxy: bool,
    /// The manifest only exists after JavaScript runs: needs a real browser.
    pub browser: bool,
    pub note: &'static str,
    /// Delivery is DRM/cipher protected: "detected, download gated" is a PASS.
    pub gated: Option<&'static str>,
    /// For the harness itself: this row PASSES only when it fails with this text.
    pub expect_failure: Option<&'static str>,
}

/// The hard-site list, one row per site.
///
/// The URLs point at entry pages; for a *stream* assertion the live layer needs a
/// media page, which is why every profile accepts `HAZAR_LIVE_<NAME>_URL`.
pub fn profiles() -> Vec<SiteProfile> {
    vec![
        SiteProfile {
            name: "hdfilmcehennemi",
            url: "https://www.hdfilmcehennemi.nl/",
            player: "iframe + player config",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: true,
            },
            browser: true,
            proxy: true,
            note: "giriş sayfası; film sayfası URL'i HAZAR_LIVE_HDFILMCEHENNEMI_URL ile verilir",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "dizipal",
            url: "https://dizipal.com/",
            player: "player config + token'lı segment",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: true,
                iframe: false,
            },
            browser: true,
            proxy: false,
            note: "domain teyidi gerekir (HAZAR_LIVE_DIZIPAL_URL)",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "youtube",
            url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            player: "JS player (DASH + imza)",
            expected: Expected::Dash,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: false,
            proxy: true,
            note: "DASH manifest, imza/n= cipher'ı çözülmez",
            gated: Some("signature/n-parameter cipher: not implemented by design"),
            expect_failure: None,
        },
        SiteProfile {
            name: "dailymotion",
            url: "https://www.dailymotion.com/",
            player: "player config (HLS)",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: true,
            proxy: true,
            note: "video sayfası URL'i HAZAR_LIVE_DAILYMOTION_URL ile verilir",
            gated: None,
            expect_failure: None,
        },
    ]
}

/// The same shapes, served locally by the fixture server (HTTP transport).
pub fn selftest_profiles() -> Vec<SiteProfile> {
    vec![
        SiteProfile {
            name: "hdfilmcehennemi",
            url: "/live/hdfilmcehennemi/page.html",
            player: "iframe + player page",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: true,
            },
            browser: false,
            proxy: true,
            note: "player iframe içinde",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "dizipal",
            url: "/live/dizipal/page.html",
            player: "player config + expiring token",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: false,
            proxy: false,
            note: "segment URL'leri süreli token taşıyor",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "youtube",
            url: "/live/youtube/page.html",
            player: "DASH manifest",
            expected: Expected::Dash,
            needs: Needs::default(),
            browser: false,
            proxy: true,
            note: "DASH manifest (indirme yok, sadece tespit)",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "dailymotion",
            url: "/live/dailymotion/page.html",
            player: "player config sources",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: false,
            proxy: true,
            note: "player config",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "gated-detected-dash",
            url: "/live/gated-detected/page.html",
            player: "DASH manifest + gated delivery",
            expected: Expected::Dash,
            needs: Needs::default(),
            browser: false,
            proxy: false,
            note: "aday bulunur, indirme gated sayılır",
            gated: Some("cipher/DRM gated by design"),
            expect_failure: None,
        },
        SiteProfile {
            name: "gated-manifest-built-in-js",
            url: "/live/gated/page.html",
            player: "runtime-built manifest",
            expected: Expected::Hls,
            needs: Needs::default(),
            browser: false,
            proxy: false,
            note: "manifest only assembled at runtime → tespit edilemez",
            gated: Some("manifest is assembled at runtime; reported as gated"),
            expect_failure: None,
        },
    ]
}

/// Self-test shapes that require the browser transport.
pub fn selftest_browser_profiles() -> Vec<SiteProfile> {
    vec![
        SiteProfile {
            name: "js-only-browser-transport",
            url: "/live/js-only/page.html",
            player: "JS-built manifest + XHR",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: true,
            proxy: false,
            note: "manifest URL'i runtime'da parçalardan kuruluyor; düz HTTP göremez",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "dplayer82-shape",
            url: "/live/dplayer82/page.html",
            player: "iframe player + token manifest + MSE segments",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: true,
            },
            browser: true,
            proxy: false,
            note: "gerçek dizipal yakalamasından türetildi (four.dplayer82.site/master.m3u8?v=…)",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "nav-failure-diagnostic",
            url: "http://127.0.0.1:9/nope.html",
            player: "ulaşılamayan hedef",
            expected: Expected::Hls,
            needs: Needs::default(),
            browser: true,
            proxy: false,
            note: "navigasyon hatası teşhisinin kendini test eder",
            gated: None,
            expect_failure: Some("navigation failed"),
        },
        SiteProfile {
            name: "iframe-token-browser-transport",
            url: "/live/browser-iframe/page.html",
            player: "iframe + runtime manifest + token'lı segment",
            expected: Expected::Hls,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: true,
            },
            browser: true,
            proxy: false,
            note: "hdfilmcehennemi/dizipal şekli",
            gated: None,
            expect_failure: None,
        },
        SiteProfile {
            name: "dash-browser-transport",
            url: "/live/browser-dash/page.html",
            player: "runtime DASH manifest",
            expected: Expected::Dash,
            needs: Needs {
                referer: true,
                cookie: false,
                iframe: false,
            },
            browser: true,
            proxy: false,
            note: "DASH manifest runtime'da kuruluyor (DRM'siz)",
            gated: None,
            expect_failure: None,
        },
    ]
}

/// Live rows accept a URL override so a site can be pointed at a real media page
/// without touching code: `HAZAR_LIVE_DAILYMOTION_URL=https://…/video/<id>`.
fn target_url(profile: &SiteProfile, base: &str) -> String {
    let key = format!(
        "HAZAR_LIVE_{}",
        profile
            .name
            .to_uppercase()
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            })
            .collect::<String>()
    );
    // Accept both `HAZAR_LIVE_<SITE>_URL` (documented) and the bare key.
    let raw = [format!("{key}_URL"), key.clone()]
        .into_iter()
        .find_map(|name| {
            std::env::var(&name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| profile.url.to_string());
    if raw.starts_with('/') {
        format!("{base}{raw}")
    } else {
        raw
    }
}

fn user_agent() -> &'static str {
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36"
}

/// Fetch only the first segment of the detected stream: enough to prove it
/// plays, never enough to be a download.
async fn first_segment_probe(
    client: &reqwest::Client,
    candidate: &Candidate,
    timeout: Duration,
) -> Result<String, String> {
    if candidate.kind == MediaKind::Dash {
        return Ok("first segment: skipped (DASH)".to_string());
    }

    // HLS: resolve the playlist down to its first media segment.
    let url = if candidate.kind == MediaKind::Hls {
        let base = Url::parse(&candidate.url).map_err(|error| error.to_string())?;
        let body = match &candidate.manifest {
            Some(body) => body.clone(),
            None => fetch_text(client, &candidate.url, &candidate.headers, timeout).await?,
        };
        let mut playlist = parse_playlist(&body, &base).map_err(|error| error.to_string())?;
        if let Playlist::Master(variants) = &playlist {
            let picked = best_variant(variants).ok_or("master playlist has no variants")?;
            let variant_url = picked.uri.clone();
            let variant_body = fetch_text(client, &variant_url, &candidate.headers, timeout).await?;
            let variant_base = Url::parse(&variant_url).map_err(|error| error.to_string())?;
            playlist =
                parse_playlist(&variant_body, &variant_base).map_err(|error| error.to_string())?;
        }
        match playlist {
            Playlist::Media { segments, .. } => segments
                .iter()
                .find(|segment| !segment.init)
                .or_else(|| segments.first())
                .map(|segment| segment.url.clone())
                .ok_or("playlist has no segments")?,
            Playlist::Master(_) => return Err("playlist nesting too deep".to_string()),
        }
    } else {
        candidate.url.clone()
    };

    let mut request = client
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", PROBE_BYTES - 1));
    for (name, value) in &candidate.headers {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| format!("first segment timeout: {url}"))?
        .map_err(|error| error.to_string())?;

    if !(response.status().is_success() || response.status() == reqwest::StatusCode::PARTIAL_CONTENT)
    {
        return Err(format!(
            "first segment status {} for {url}",
            response.status().as_u16()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| error.to_string())?
        .len();
    if bytes == 0 {
        return Err(format!("first segment was empty: {url}"));
    }
    Ok(format!("first segment: {bytes} bytes"))
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    timeout: Duration,
) -> Result<String, String> {
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| format!("timeout: {url}"))?
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {} for {url}", response.status().as_u16()));
    }
    response.text().await.map_err(|error| error.to_string())
}

/// HTTP-transport evaluation.
async fn evaluate(
    client: &reqwest::Client,
    profiles: Vec<SiteProfile>,
    base: &str,
    row_prefix: &str,
) -> Vec<Row> {
    let mut rows = Vec::new();

    for profile in profiles {
        let started = Instant::now();
        let target = target_url(&profile, base);
        let options = ResolveOptions {
            timeout: RESOLVE_TIMEOUT,
            max_scripts: 4,
            ..Default::default()
        };
        let meta = format!(
            "player={} needs={}",
            profile.player,
            profile.needs.describe()
        );

        let report = resolve(client, &target, &options).await;
        let wanted = profile.expected.kind();
        let mut found: Vec<String> = Vec::new();
        for candidate in &report.candidates {
            if !found.iter().any(|seen| seen == candidate.strategy) {
                found.push(candidate.strategy.to_string());
            }
        }

        let base_row = Row {
            name: format!("{row_prefix}{}", profile.name),
            status: Status::Fail,
            kind: wanted.as_str().to_string(),
            strategy: "-".to_string(),
            found,
            detail: String::new(),
            ms: 0,
        };

        let picked = report
            .candidates
            .iter()
            .find(|candidate| candidate.kind == wanted)
            .cloned();

        let row = match picked {
            Some(candidate) => {
                if let Some(text) = profile.expect_failure {
                    Row {
                        detail: format!(
                            "{meta} · expected a failure ({text}) but detected {}",
                            candidate.url
                        ),
                        ..Row {
                            strategy: candidate.strategy.to_string(),
                            ..base_row
                        }
                    }
                } else if let Some(reason) = profile.gated {
                    Row {
                        status: Status::Pass,
                        kind: candidate.kind.as_str().to_string(),
                        strategy: candidate.strategy.to_string(),
                        detail: format!("{meta} · detected, download gated: {reason}"),
                        ms: started.elapsed().as_millis(),
                        ..base_row
                    }
                } else {
                    match first_segment_probe(client, &candidate, RESOLVE_TIMEOUT).await {
                        Ok(probe) => Row {
                            status: Status::Pass,
                            kind: candidate.kind.as_str().to_string(),
                            strategy: candidate.strategy.to_string(),
                            detail: match candidate.drm.as_deref() {
                                Some(drm) => format!("{meta} · detected, {drm} — download gated"),
                                None => format!("{meta} · {probe}"),
                            },
                            ms: started.elapsed().as_millis(),
                            ..base_row
                        },
                        Err(error) => Row {
                            kind: candidate.kind.as_str().to_string(),
                            strategy: candidate.strategy.to_string(),
                            detail: format!("{meta} · detected but first segment failed: {error}"),
                            ms: started.elapsed().as_millis(),
                            ..base_row
                        },
                    }
                }
            }
            None => {
                if let Some(text) = profile.expect_failure {
                    Row {
                        status: Status::Pass,
                        detail: format!("{meta} · expected failure observed (no candidate): {text}"),
                        ms: started.elapsed().as_millis(),
                        ..base_row
                    }
                } else if let Some(reason) = profile.gated {
                    Row {
                        status: Status::Pass,
                        kind: "gated".to_string(),
                        detail: format!("{meta} · {} — {reason}", profile.note),
                        ms: started.elapsed().as_millis(),
                        ..base_row
                    }
                } else {
                    let wall = if report
                        .errors
                        .iter()
                        .any(|error| error.contains("451"))
                    {
                        " · signals=[legally unavailable (451)]"
                    } else if report.errors.iter().any(|error| error.contains("403")) {
                        " · signals=[blocked (403)]"
                    } else {
                        ""
                    };
                    Row {
                        detail: format!(
                            "{meta} · no {} candidate ({} documents, {} errors){wall}: {}",
                            wanted.as_str(),
                            report.visited.len(),
                            report.errors.len(),
                            profile.note
                        ),
                        ms: started.elapsed().as_millis(),
                        ..base_row
                    }
                }
            }
        };
        rows.push(row);
    }

    rows
}

/// Self-test rows: identical code path, local pages (HTTP transport).
pub async fn run_selftest_rows(base: &str) -> Vec<Row> {
    let client = match hazar_engine::default_client(Some(user_agent())) {
        Ok(client) => client,
        Err(error) => {
            return vec![Row {
                name: "live-selftest-client".to_string(),
                status: Status::Fail,
                kind: "live".to_string(),
                strategy: "-".to_string(),
                found: Vec::new(),
                detail: error.to_string(),
                ms: 0,
            }]
        }
    };
    evaluate(&client, selftest_profiles(), base, "live-selftest-").await
}

/// Browser-transport self-test rows.
pub async fn run_selftest_browser_rows(base: &str) -> Vec<Row> {
    browser_rows(selftest_browser_profiles(), base, "live-selftest-browser-").await
}

/// Browser-transport rows: run the page for real and ask the extension what it
/// sniffed. Missing Chromium is a SKIP, never a failure.
async fn browser_rows(profiles: Vec<SiteProfile>, base: &str, row_prefix: &str) -> Vec<Row> {
    if profiles.is_empty() {
        return Vec::new();
    }

    // Sites differ on proxies: dizipal/Cloudflare blocks residential exits, others
    // need one. Run each group in its own browser session.
    let mut with_proxy = Vec::new();
    let mut direct = Vec::new();
    for profile in profiles {
        if profile.proxy {
            with_proxy.push(profile);
        } else {
            direct.push(profile);
        }
    }

    let mut rows = Vec::new();
    rows.extend(run_browser_group(with_proxy, base, row_prefix, true).await);
    rows.extend(run_browser_group(direct, base, row_prefix, false).await);
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

async fn run_browser_group(
    profiles: Vec<SiteProfile>,
    base: &str,
    row_prefix: &str,
    use_proxy: bool,
) -> Vec<Row> {
    if profiles.is_empty() {
        return Vec::new();
    }

    let mut session = match BrowserSession::launch_configured(true, use_proxy).await {
        Ok(session) => session,
        Err(reason) => {
            return profiles
                .into_iter()
                .map(|profile| Row {
                    name: format!("{row_prefix}{}", profile.name),
                    status: Status::Skip,
                    kind: "browser".to_string(),
                    strategy: "browser".to_string(),
                    found: Vec::new(),
                    detail: format!("browser transport unavailable: {reason}"),
                    ms: 0,
                })
                .collect()
        }
    };

    let mut rows = Vec::new();
    for profile in profiles {
        rows.push(browser_row(&mut session, profile, base, row_prefix).await);
    }
    session.kill();
    rows
}

async fn browser_row(
    session: &mut BrowserSession,
    profile: SiteProfile,
    base: &str,
    row_prefix: &str,
) -> Row {
    let started = Instant::now();
    let target = target_url(&profile, base);
    let wanted = profile.expected.kind().as_str().to_string();
    let meta = format!(
        "player={} needs={} transport=browser",
        profile.player,
        profile.needs.describe()
    );

    let base_row = Row {
        name: format!("{row_prefix}{}", profile.name),
        status: Status::Fail,
        kind: wanted.clone(),
        strategy: "extension-sniff".to_string(),
        found: Vec::new(),
        detail: String::new(),
        ms: 0,
    };

    if let Err(error) = session.open(&target).await {
        return Row {
            detail: format!("{meta} · open failed: {error}"),
            ms: started.elapsed().as_millis(),
            ..base_row
        };
    }

    // Fail fast when the navigation itself died instead of waiting 25s.
    session.wait_for_navigation(Duration::from_secs(8)).await;
    let diagnosis = session.diagnose().await;
    if diagnosis.error_page {
        let signals = diagnosis.signals().join(", ");
        let expected = profile.expect_failure;
        return Row {
            status: if expected.is_some_and(|text| signals.contains(text)) {
                Status::Pass
            } else {
                Status::Fail
            },
            detail: format!(
                "{meta} · expected failure observed: {signals} · {}",
                diagnosis.describe()
            ),
            ms: started.elapsed().as_millis(),
            ..base_row
        };
    }

    let candidate = match session
        .wait_for_candidate(&wanted, CANDIDATE_TIMEOUT, &target)
        .await
    {
        Ok(candidate) => candidate,
        Err(error) => {
            let page = session.diagnose().await;
            let signals = page.signals().join(", ");
            let (status, detail) = match (profile.expect_failure, profile.gated) {
                (Some(text), _) => (
                    Status::Pass,
                    format!("{meta} · expected failure observed: {text} · {error}"),
                ),
                (None, Some(reason)) => (Status::Pass, format!("{meta} · {error} — {reason}")),
                (None, None) => (
                    Status::Fail,
                    format!("{meta} · {error} · signals=[{signals}] · {}", page.describe()),
                ),
            };
            return Row {
                status,
                detail,
                ms: started.elapsed().as_millis(),
                ..base_row
            };
        }
    };

    if let Some(text) = profile.expect_failure {
        return Row {
            detail: format!(
                "{meta} · expected a failure but detected {} ({text})",
                candidate.url
            ),
            ms: started.elapsed().as_millis(),
            ..base_row
        };
    }

    if let Some(reason) = profile.gated {
        return Row {
            status: Status::Pass,
            kind: candidate.kind.clone(),
            detail: format!("{meta} · detected, download gated: {reason}"),
            ms: started.elapsed().as_millis(),
            ..base_row
        };
    }

    // Probe a real *segment* (the extension gives us the sniffed segment list);
    // falling back to the manifest only when nothing else is known.
    let page_url_hint = Some(target.clone());
    let probe_url = candidate
        .segments
        .as_ref()
        .and_then(|segments| segments.first())
        .cloned()
        .unwrap_or_else(|| candidate.url.clone());
    let probe = if candidate.kind == "dash" {
        Ok("first segment: skipped (DASH)".to_string())
    } else {
        session
            .probe_first_segment(&probe_url, candidate.frame_url.as_deref().or(page_url_hint.as_deref()))
            .await
    };

    match probe {
        Ok(detail) => Row {
            status: Status::Pass,
            kind: candidate.kind.clone(),
            detail: format!("{meta} · {detail}"),
            ms: started.elapsed().as_millis(),
            ..base_row
        },
        Err(error) => Row {
            detail: format!("{meta} · detected {} but {error}", candidate.url),
            ms: started.elapsed().as_millis(),
            ..base_row
        },
    }
}

/// Real site rows. Without `HAZAR_LIVE=1` every profile reports SKIP.
pub async fn run_rows() -> Vec<Row> {
    let live = std::env::var("HAZAR_LIVE")
        .map(|value| value == "1")
        .unwrap_or(false);

    if !live {
        return profiles()
            .into_iter()
            .map(|profile| Row {
                name: format!("live-{}", profile.name),
                status: Status::Skip,
                kind: "live".to_string(),
                strategy: "-".to_string(),
                found: Vec::new(),
                detail: "set HAZAR_LIVE=1 to probe live sites (detection + first segment only)"
                    .to_string(),
                ms: 0,
            })
            .collect();
    }

    let client = match hazar_engine::default_client(Some(user_agent())) {
        Ok(client) => client,
        Err(error) => {
            return vec![Row {
                name: "live-client".to_string(),
                status: Status::Skip,
                kind: "live".to_string(),
                strategy: "-".to_string(),
                found: Vec::new(),
                detail: error.to_string(),
                ms: 0,
            }]
        }
    };

    let mut http = Vec::new();
    let mut browser = Vec::new();
    for profile in profiles() {
        if profile.browser {
            browser.push(profile);
        } else {
            http.push(profile);
        }
    }

    let mut rows = evaluate(&client, http, "", "live-").await;
    rows.extend(browser_rows(browser, "", "live-").await);
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}
