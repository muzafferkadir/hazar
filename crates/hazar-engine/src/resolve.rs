//! Media resolution: find the playable stream behind a page.
//!
//! Hard sites hide the real media URL behind iframes, player configs, packed
//! JavaScript, base64 blobs and page-level obfuscation. This module runs a set
//! of independent strategies over a page and returns ranked candidates with the
//! evidence that produced them.
//!
//! What this module deliberately does **not** do:
//! - no DRM circumvention: Widevine/PlayReady/FairPlay and `SAMPLE-AES` are
//!   detected and reported through [`Candidate::drm`], never decrypted;
//! - no signature/cipher solving (YouTube `s=`/`n=` parameters): the strategy
//!   slot exists ([`STRATEGIES`]), but no implementation is shipped.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Result;
use crate::hls::{best_variant, parse_playlist, Playlist};

/// Strategy identifiers, in the order they are attempted.
pub const STRATEGIES: [&str; 10] = [
    "direct",
    "attribute-scan",
    "bare-url",
    "player-config",
    "json-ld",
    "js-unpack",
    "base64-decode",
    "script-fetch",
    "iframe",
    "segment-group",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    File,
    Hls,
    Dash,
}

impl MediaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::File => "file",
            MediaKind::Hls => "hls",
            MediaKind::Dash => "dash",
        }
    }
}

/// A ranked media URL with the evidence that produced it.
///
/// `Serialize` only: `strategy` is a static identifier, so the type is not
/// deserializable by design (the app reads it, never round-trips it).
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub url: String,
    pub kind: MediaKind,
    /// Which strategy produced it.
    pub strategy: &'static str,
    /// 0–100; higher wins.
    pub confidence: u8,
    /// Human-readable reason (kept for the test matrix output).
    pub evidence: String,
    /// Headers that must be sent with this URL (usually `Referer`).
    pub headers: Vec<(String, String)>,
    /// Segment list when only segments are visible (MSE / sniffed traffic).
    pub segments: Option<Vec<String>>,
    /// `Some("widevine")`, `Some("sample-aes")`, … when the stream is protected.
    pub drm: Option<String>,
    pub page_url: Option<String>,
    /// Playlist body, when the candidate itself is a playlist we fetched.
    pub manifest: Option<String>,
}

impl Candidate {
    pub fn new(url: impl Into<String>, kind: MediaKind, strategy: &'static str) -> Self {
        Self {
            url: url.into(),
            kind,
            strategy,
            confidence: 50,
            evidence: String::new(),
            headers: Vec::new(),
            segments: None,
            drm: None,
            page_url: None,
            manifest: None,
        }
    }

    pub fn with(mut self, confidence: u8, evidence: impl Into<String>) -> Self {
        self.confidence = confidence;
        self.evidence = evidence.into();
        self
    }

    pub fn with_referer(mut self, page_url: &str) -> Self {
        self.headers
            .push(("Referer".to_string(), page_url.to_string()));
        self.headers
            .push(("Origin".to_string(), origin_of(page_url)));
        self.page_url = Some(page_url.to_string());
        self
    }
}

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// How deep to follow iframes.
    pub max_depth: usize,
    /// Total pages fetched (the page itself + iframes).
    pub max_pages: usize,
    pub headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
    /// Fetch manifests found on the page to detect DRM and enumerate variants.
    pub inspect_manifests: bool,
    /// Same-origin `<script src>` files fetched and scanned per document.
    pub max_scripts: usize,
    pub timeout: Duration,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_pages: 10,
            headers: Vec::new(),
            user_agent: None,
            inspect_manifests: true,
            max_scripts: 4,
            timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolveReport {
    pub page_url: String,
    pub candidates: Vec<Candidate>,
    pub visited: Vec<String>,
    pub errors: Vec<String>,
}

impl ResolveReport {
    /// Best candidate: highest confidence, manifest kinds first.
    pub fn best(&self) -> Option<&Candidate> {
        self.candidates.iter().max_by_key(|candidate| {
            let kind_bonus = match candidate.kind {
                MediaKind::Hls => 20,
                MediaKind::Dash => 15,
                MediaKind::File => 0,
            };
            // A real episode playlist lists many segments; a preroll lists a few.
            let segment_bonus = candidate
                .segments
                .as_ref()
                .map(|segments| (segments.len() / 3).min(15) as u8)
                .unwrap_or(0);
            candidate.confidence.saturating_add(kind_bonus + segment_bonus)
        })
    }

    pub fn of_kind(&self, kind: MediaKind) -> Option<&Candidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.kind == kind)
            .max_by_key(|candidate| candidate.confidence)
    }
}

/// Fetch a page (with its iframes) and rank every media URL we can find.
pub async fn resolve(
    client: &reqwest::Client,
    page_url: &str,
    opts: &ResolveOptions,
) -> ResolveReport {
    let mut report = ResolveReport {
        page_url: page_url.to_string(),
        candidates: Vec::new(),
        visited: Vec::new(),
        errors: Vec::new(),
    };

    // Strategy: direct — the URL itself may already be the media.
    if let Some(kind) = classify_url(page_url) {
        let mut candidate = Candidate::new(page_url, kind, "direct");
        candidate.confidence = if kind == MediaKind::File { 70 } else { 95 };
        candidate.evidence = "url looks like media/manifest".to_string();
        report.candidates.push(candidate);
    }

    let mut queue: Vec<(String, usize)> = vec![(page_url.to_string(), 0)];
    let mut seen: HashSet<String> = HashSet::new();

    while let Some((url, depth)) = queue.pop() {
        if report.visited.len() >= opts.max_pages || !seen.insert(url.clone()) {
            continue;
        }
        report.visited.push(url.clone());

        match fetch_text(client, &url, opts).await {
            Ok((_content_type, body)) => {
                scan_body(&url, &body, &mut report);

                // Same-origin player scripts: the real URL is often only inside them.
                for source in find_script_srcs(&body).into_iter().take(opts.max_scripts) {
                    let Some(script_url) = absolute(&url, &source) else {
                        continue;
                    };
                    if !same_origin(&url, &script_url) || !seen.insert(script_url.clone()) {
                        continue;
                    }
                    match fetch_text(client, &script_url, opts).await {
                        Ok((_, script_body)) => {
                            let before = report.candidates.len();
                            scan_body(&script_url, &script_body, &mut report);
                            for candidate in report.candidates[before..].iter_mut() {
                                if candidate.strategy == "attribute-scan"
                                    || candidate.strategy == "bare-url"
                                {
                                    candidate.strategy = "script-fetch";
                                }
                            }
                            report.visited.push(script_url);
                        }
                        Err(e) => report.errors.push(format!("{script_url}: {e}")),
                    }
                }

                if depth < opts.max_depth {
                    for iframe in find_iframes(&body) {
                        if let Some(target) = absolute(&url, &iframe) {
                            if !seen.contains(&target) {
                                queue.push((target, depth + 1));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // A page fetch can fail for plenty of legitimate reasons; keep going.
                if let Some(kind) = classify_url(&url) {
                    let mut candidate = Candidate::new(url.clone(), kind, "direct");
                    candidate.confidence = 80;
                    candidate.evidence = format!("fetch failed ({e}) but the url is media-like");
                    report.candidates.push(candidate);
                } else {
                    report.errors.push(format!("{url}: {e}"));
                }
            }
        }
    }

    if opts.inspect_manifests {
        inspect_manifests(client, opts, &mut report).await;
    }

    dedupe_and_rank(&mut report);
    report
}

async fn inspect_manifests(
    client: &reqwest::Client,
    opts: &ResolveOptions,
    report: &mut ResolveReport,
) {
    let urls: Vec<String> = report
        .candidates
        .iter()
        .filter(|candidate| candidate.kind != MediaKind::File && candidate.manifest.is_none())
        .map(|candidate| candidate.url.clone())
        .collect();

    for url in urls {
        let Ok((_, body)) = fetch_text(client, &url, opts).await else {
            continue;
        };
        let drm = detect_drm(&body);
        match classify_url(&url) {
            Some(MediaKind::Hls) => {
                if let Ok(base) = Url::parse(&url) {
                    if let Ok(Playlist::Master(variants)) = parse_playlist(&body, &base) {
                        if let Some(picked) = best_variant(&variants) {
                            let mut candidate = Candidate::new(
                                picked.uri.clone(),
                                MediaKind::Hls,
                                "manifest-enum",
                            );
                            candidate.confidence = 90;
                            candidate.evidence = format!(
                                "best variant BANDWIDTH={} of {} variant(s)",
                                picked.bandwidth,
                                variants.len()
                            );
                            candidate.drm = drm.clone();
                            candidate.manifest = Some(body.clone());
                            report.candidates.push(candidate);
                        }
                    }
                }
            }
            Some(MediaKind::Dash) => {}
            _ => {}
        }
        for candidate in report
            .candidates
            .iter_mut()
            .filter(|candidate| candidate.url == url)
        {
            candidate.drm = drm.clone();
            candidate.manifest = Some(body.clone());
        }
    }
}

/// Run every text strategy over one document body.
pub fn scan_body(document_url: &str, body: &str, report: &mut ResolveReport) {
    let decoded = decode_escapes(body);

    // attribute scan: src/href/data-*/content + "file":"…" style JSON keys
    for (url, evidence) in find_urls_in_text(&decoded) {
        let Some(absolute_url) = absolute(document_url, &url) else {
            continue;
        };
        if is_ad_url(&absolute_url) {
            continue;
        }
        let media_element = evidence == "media-element src";
        let kind = match classify_url(&absolute_url) {
            Some(kind) => kind,
            None if media_element => MediaKind::File,
            None => continue,
        };
        let strategy = match evidence.as_str() {
            "json-ld" => "json-ld",
            evidence if evidence.starts_with("player") => "player-config",
            _ => "attribute-scan",
        };
        let mut candidate = Candidate::new(absolute_url, kind, strategy);
        // Player configuration and JSON-LD name the stream explicitly, so they
        // outrank a plain attribute or a bare URL.
        let explicit = matches!(strategy, "player-config" | "json-ld");
        candidate.confidence = match (explicit, media_element, kind) {
            (true, _, _) => 88,
            (false, true, MediaKind::File) => 60,
            (false, true, _) => 85,
            (false, false, MediaKind::File) => 55,
            (false, false, _) => 80,
        };
        candidate.evidence = evidence;
        report.candidates.push(candidate.with_referer(document_url));
    }

    // bare urls: `"…/index.m3u8"` in inline scripts, JSON blobs, data attributes
    for url in find_bare_urls(&decoded) {
        let Some(absolute_url) = absolute(document_url, &url) else {
            continue;
        };
        if is_ad_url(&absolute_url) {
            continue;
        }
        let Some(kind) = classify_url(&absolute_url) else {
            continue;
        };
        let mut candidate = Candidate::new(absolute_url, kind, "bare-url");
        candidate.confidence = if kind == MediaKind::File { 45 } else { 70 };
        candidate.evidence = "bare media url in document text".to_string();
        report.candidates.push(candidate.with_referer(document_url));
    }

    // packed javascript
    if let Some(unpacked) = unpack_packed_js(&decoded) {
        collect_from_text(&unpacked, document_url, "js-unpack", 75, "unpacked eval-packed javascript", report);
    }

    // base64 blobs
    for decoded_text in decode_base64_candidates(&decoded) {
        collect_from_text(
            &decoded_text,
            document_url,
            "base64-decode",
            70,
            "base64 blob decoded to a media url",
            report,
        );
    }

    // segment grouping (MSE: no manifest, only segment requests)
    let segments = find_segment_urls(&decoded, document_url);
    for group in group_segments(&segments) {
        if group.len() < 3 {
            continue;
        }
        let mut candidate = Candidate::new(group[0].clone(), MediaKind::Hls, "segment-group");
        candidate.confidence = 60;
        candidate.evidence = format!(
            "{} segments share a directory, no manifest visible",
            group.len()
        );
        candidate.segments = Some(group);
        report.candidates.push(candidate.with_referer(document_url));
    }
}

/// Scan an arbitrary text blob with both the attribute and the bare-url scanners.
fn collect_from_text(
    text: &str,
    document_url: &str,
    strategy: &'static str,
    confidence: u8,
    evidence: &str,
    report: &mut ResolveReport,
) {
    let mut urls: Vec<String> = find_urls_in_text(text)
        .into_iter()
        .map(|(url, _)| url)
        .collect();
    urls.extend(find_bare_urls(text));

    for url in urls {
        let Some(absolute_url) = absolute(document_url, &url) else {
            continue;
        };
        if is_ad_url(&absolute_url) {
            continue;
        }
        let Some(kind) = classify_url(&absolute_url) else {
            continue;
        };
        let mut candidate = Candidate::new(absolute_url, kind, strategy);
        candidate.confidence = confidence;
        candidate.evidence = evidence.to_string();
        report.candidates.push(candidate.with_referer(document_url));
    }
}

/// Media URLs that appear as plain strings (not behind an attribute).
pub fn find_bare_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    let bytes = text.as_bytes();
    while index < bytes.len() {
        let starts_here = bytes[index] == b'/' && bytes.get(index.wrapping_sub(1)).map(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b':' || *b == b'/' || *b == b'\\').unwrap_or(false) == false
            || (bytes[index] == b'h' && text[index..].starts_with("http"));
        if !starts_here {
            index += 1;
            continue;
        }
        let tail = &text[index..];
        let end = tail
            .find(|character: char| {
                character == '"'
                    || character == '\''
                    || character == '<'
                    || character == '>'
                    || character == ' '
                    || character == '\\'
                    || character == '\n'
                    || character == '\r'
                    || character == ')'
                    || character == ','
            })
            .unwrap_or(tail.len());
        let candidate = &tail[..end];
        if candidate.len() > 6 && classify_url(candidate).is_some() {
            out.push(candidate.to_string());
        }
        let step = if end == 0 {
            text[index..].chars().next().map(char::len_utf8).unwrap_or(1)
        } else {
            end
        };
        index += step;
    }
    out.sort();
    out.dedup();
    out
}

/// `<script src="…">` targets (the player code often lives in its own file).
pub fn find_script_srcs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(position) = rest.find("<script") {
        let tag = &rest[position..];
        let tag_end = tag.find('>').map(|offset| position + offset).unwrap_or(rest.len());
        let tag_text = &rest[position..tag_end.min(rest.len())];
        if let Some(source_position) = tag_text.find("src=") {
            let after = &tag_text[source_position + 4..];
            let quote = after.chars().next().unwrap_or('"');
            if quote == '"' || quote == '\'' {
                let value: String = after[1..].chars().take_while(|c| *c != quote).collect();
                if value.len() > 3 {
                    out.push(value);
                }
            }
        }
        rest = &rest[tag_end.min(rest.len())..];
        if rest.len() < 8 {
            break;
        }
    }
    out
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    opts: &ResolveOptions,
) -> Result<(String, String)> {
    let mut request = client.get(url);
    for (name, value) in &opts.headers {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(opts.timeout, request.send())
        .await
        .map_err(|_| crate::Error::Protocol(format!("resolve timeout for {url}")))??;
    if !response.status().is_success() {
        return Err(crate::Error::Status {
            status: response.status().as_u16(),
            url: url.to_string(),
        });
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await?;
    Ok((content_type, body))
}

// ---------------------------------------------------------------------------
// pure helpers (unit tested)
// ---------------------------------------------------------------------------

/// Classify a URL by its path/query, ignoring the response.
/// Ad / preroll / tracker URL heuristics.
///
/// A page usually carries the real stream *and* an ad stream; the ad must be
/// dropped before a candidate is even created.
pub fn is_ad_url(url: &str) -> bool {
    const PATTERNS: [&str; 16] = [
        "/ads/", "/ad/", "/adv/", "adserver", "adservice", "doubleclick",
        "googlesyndication", "adsystem", "popads", "propellerads", "taboola",
        "outbrain", "preroll", "pre-roll", "vast", "/banner",
    ];
    let lower = url.to_ascii_lowercase();
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

pub fn classify_url(url: &str) -> Option<MediaKind> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        return Some(MediaKind::Hls);
    }
    if path.ends_with(".mpd") {
        return Some(MediaKind::Dash);
    }
    const MEDIA_EXT: [&str; 32] = [
        ".mp4", ".m4v", ".mkv", ".webm", ".mov", ".avi", ".flv", ".ts", ".m4s", ".mp3", ".m4a",
        ".aac", ".zip", ".pdf", ".bin", ".iso", ".img", ".rar", ".7z", ".tar", ".gz", ".xz",
        ".dmg", ".pkg", ".exe", ".msi", ".apk", ".deb", ".rpm", ".epub", ".torrent", ".srt",
    ];
    if MEDIA_EXT.iter().any(|extension| path.ends_with(extension)) {
        return Some(MediaKind::File);
    }
    None
}

pub fn is_hls_manifest(url: &str) -> bool {
    classify_url(url) == Some(MediaKind::Hls)
}

/// `http://a/b/c` → `http://a`
pub fn origin_of(url: &str) -> String {
    match Url::parse(url) {
        Ok(parsed) => format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or(""),
            parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default()
        ),
        Err(_) => url.to_string(),
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default(),
        _ => false,
    }
}

fn absolute(base: &str, reference: &str) -> Option<String> {
    let reference = reference.trim();
    if reference.is_empty() || reference.starts_with("data:") || reference.starts_with("blob:") {
        return None;
    }
    if let Ok(parsed) = Url::parse(reference) {
        if parsed.scheme() == "http" || parsed.scheme() == "https" {
            return Some(parsed.to_string());
        }
        return None;
    }
    Url::parse(base)
        .ok()
        .and_then(|base| base.join(reference).ok())
        .map(|url| url.to_string())
}

/// Decode `\xNN`, `\uNNNN`, `\/`, `\n`.
///
/// Character-driven on purpose: a backslash may be followed by a multi-byte
/// character (Turkish text, arrows, emoji) and byte-index arithmetic used to
/// slice through the middle of one.
pub fn decode_escapes(input: &str) -> String {
    fn hex_digits(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        count: usize,
    ) -> Option<u32> {
        let mut probe = chars.clone();
        let mut value = 0u32;
        for _ in 0..count {
            let (_, character) = probe.next()?;
            let digit = character.to_digit(16)?;
            value = value * 16 + digit;
        }
        *chars = probe;
        Some(value)
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((_, character)) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }

        match chars.peek().map(|(_, next)| *next) {
            Some('x') => {
                let mut probe = chars.clone();
                probe.next();
                match hex_digits(&mut probe, 2) {
                    Some(value) => {
                        out.push(value as u8 as char);
                        chars = probe;
                    }
                    None => out.push('\\'),
                }
            }
            Some('u') => {
                let mut probe = chars.clone();
                probe.next();
                match hex_digits(&mut probe, 4) {
                    Some(value) => {
                        out.push(char::from_u32(value).unwrap_or('\\'));
                        chars = probe;
                    }
                    None => out.push('\\'),
                }
            }
            Some('/') => {
                chars.next();
                out.push('/');
            }
            Some('n') => {
                chars.next();
                out.push('\n');
            }
            Some(other) => {
                // Keep the escape verbatim; never split the following character.
                out.push('\\');
                out.push(other);
                chars.next();
            }
            None => out.push('\\'),
        }
    }

    out
}

/// Extract media-looking URLs together with the evidence that found them.
pub fn find_urls_in_text(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let patterns: [(&str, &str); 21] = [
        ("src=\"", "attribute src"),
        ("src='", "attribute src"),
        ("href=\"", "attribute href"),
        ("data-src=\"", "attribute data-src"),
        ("data-file=\"", "attribute data-file"),
        ("data-video=\"", "attribute data-video"),
        ("data-url=\"", "attribute data-url"),
        ("content=\"", "meta content"),
        ("\"file\":\"", "player-config file"),
        ("\"file\": \"", "player-config file"),
        ("file:\"", "player-config file"),
        ("file: \"", "player-config file"),
        ("'file':'", "player-config file"),
        ("\"src\":\"", "player-config src"),
        ("src:\"", "player-config src"),
        ("\"url\":\"", "player-config url"),
        ("url:\"", "player-config url"),
        ("\"hls\":\"", "player-config hls"),
        ("\"contentUrl\":\"", "json-ld contentUrl"),
        ("contentUrl\":\"", "json-ld contentUrl"),
        ("file: '", "player-config file"),
    ];

    for (needle, evidence) in patterns {
        let mut rest = text;
        while let Some(position) = rest.find(needle) {
            let before = &rest[..position];
            let after = &rest[position + needle.len()..];
            let terminator = if needle.ends_with('\'') { '\'' } else { '"' };
            let value: String = after.chars().take_while(|c| *c != terminator).collect();

            // A `src` inside <video>/<audio>/<source>/<embed> is media even when
            // the URL carries no recognisable extension.
            let evidence = if evidence == "attribute src" || evidence == "attribute data-src" {
                match nearby_tag(before) {
                    Some(tag)
                        if matches!(
                            tag.as_str(),
                            "video" | "audio" | "source" | "embed" | "object" | "track"
                        ) =>
                    {
                        "media-element src"
                    }
                    _ => evidence,
                }
            } else {
                evidence
            };

            rest = &after[value.len().min(after.len())..];
            if value.len() > 4 && (value.contains("://") || value.starts_with('/')) {
                out.push((value, evidence.to_string()));
            }
            if rest.len() < needle.len() {
                break;
            }
        }
    }
    out
}

/// Tag name of the element that a `src` attribute belongs to.
fn nearby_tag(prefix: &str) -> Option<String> {
    let mut start = prefix.len().saturating_sub(240);
    while start < prefix.len() && !prefix.is_char_boundary(start) {
        start += 1;
    }
    let window = &prefix[start..];
    let start = window.rfind('<')?;
    let tag: String = window[start + 1..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .collect();
    (!tag.is_empty()).then(|| tag.to_ascii_lowercase())
}

/// Segment URLs (absolute or page-relative) found anywhere in a document.
pub fn find_segment_urls(text: &str, base: &str) -> Vec<String> {
    let mut out: Vec<String> = find_bare_urls(text)
        .into_iter()
        .filter(|url| is_segment_path(url))
        .map(|url| absolute(base, &url).unwrap_or(url))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn is_segment_path(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    [".ts", ".m4s", ".m4a", ".aac", ".cmfv", ".cmfa"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

/// Bucket segment URLs by host + directory + extension, ordered by trailing number.
pub fn group_segments(urls: &[String]) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for url in urls {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let directory = path.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
        let extension = path.rsplit_once('.').map(|(_, tail)| tail).unwrap_or("");
        buckets
            .entry(format!("{directory}|{extension}"))
            .or_default()
            .push(url.clone());
    }
    let mut groups: Vec<Vec<String>> = buckets
        .into_values()
        .map(|mut group| {
            group.sort_by_key(|url| segment_index(url));
            group.dedup();
            group
        })
        .collect();
    groups.sort_by(|a, b| b.len().cmp(&a.len()));
    groups
}

fn segment_index(url: &str) -> u64 {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let mut last = 0u64;
    let mut current: Option<u64> = None;
    for character in path.chars() {
        if let Some(digit) = character.to_digit(10) {
            current = Some(current.unwrap_or(0) * 10 + digit as u64);
        } else if let Some(value) = current.take() {
            last = value;
        }
    }
    current.unwrap_or(last)
}

pub fn find_iframes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (url, evidence) in find_urls_in_text(text) {
        if evidence == "attribute src" && !url.ends_with(".m3u8") && !url.ends_with(".mpd") {
            if text.contains("<iframe") || text.contains("allowfullscreen") || url.contains("embed")
            {
                out.push(url);
            }
        }
    }
    out
}

/// DRM / protection markers. Detection only — never a decryption attempt.
pub fn detect_drm(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("widevine") || lower.contains("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed") {
        return Some("widevine".to_string());
    }
    if lower.contains("9a04f079-9840-4286-ab92-e65be0885f95") {
        return Some("playready".to_string());
    }
    if lower.contains("94ce86fb-07ff-4f43-adb8-93d2fa968ca2") {
        return Some("fairplay".to_string());
    }
    if lower.contains("playready") {
        return Some("playready".to_string());
    }
    if lower.contains("fairplay") {
        return Some("fairplay".to_string());
    }
    if lower.contains("method=sample-aes") || lower.contains("sample-aes-cenc") {
        return Some("sample-aes".to_string());
    }
    if lower.contains("license_type=com.widevine.alpha") {
        return Some("widevine".to_string());
    }
    None
}

/// Dean Edwards style packer:
/// `eval(function(p,a,c,k,e,…){…}('payload',base,count,'words'.split('|'),0,{}))`
/// Returns `None` when the block is not a packer we understand.
pub fn unpack_packed_js(text: &str) -> Option<String> {
    let start = text.find("eval(function(")?;
    let slice = &text[start..];

    // Argument list starts right after the last `}(` of the packer body.
    let mut positions = Vec::new();
    let mut index = 0;
    while let Some(offset) = slice[index..].find("}(") {
        index += offset + 2;
        positions.push(index);
    }

    for position in positions.into_iter().rev() {
        if let Some(unpacked) = unpack_at(&slice[position..]) {
            return Some(unpacked);
        }
    }
    None
}

fn unpack_at(arguments: &str) -> Option<String> {
    let quote = *arguments.as_bytes().first()? as char;
    if quote != '\'' && quote != '"' {
        return None;
    }

    let payload_end = arguments[1..].find(quote)? + 1;
    let payload = &arguments[1..payload_end];
    let rest = &arguments[payload_end + 1..];

    // `base` and `count`
    let mut numbers = Vec::new();
    for part in rest.split(',') {
        let digits: String = part
            .trim()
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            if numbers.len() >= 2 {
                break;
            }
            continue;
        }
        numbers.push(digits.parse::<u32>().ok()?);
        if numbers.len() == 2 {
            break;
        }
    }
    if numbers.len() < 2 {
        return None;
    }
    let base = numbers[0];
    let count = numbers[1] as usize;
    if base < 2 || count == 0 {
        return None;
    }

    // Dictionary: the next quoted string, up to `.split(`
    let dictionary_start = rest.find(quote)? + 1;
    let region = &rest[dictionary_start..];
    let dictionary_end = region.find(".split(").unwrap_or(region.len());
    let dictionary = region[..dictionary_end].trim_end_matches(quote);
    let words: Vec<&str> = dictionary.split('|').collect();
    if words.len() < count {
        return None;
    }

    let mut decoded = payload.to_string();
    for (index, word) in words.iter().enumerate().take(count) {
        if word.is_empty() {
            continue;
        }
        decoded = replace_word(&decoded, &encode_base(index as u32, base), word);
    }
    Some(decoded)
}

fn encode_base(mut value: u32, base: u32) -> String {
    let mut out = String::new();
    loop {
        let digit = (value % base) as u8;
        let character = if digit > 35 {
            char::from(digit + 29)
        } else {
            char::from_digit(digit as u32, 36).unwrap_or('0')
        };
        out.insert(0, character);
        value /= base;
        if value == 0 {
            break;
        }
    }
    out
}

/// Replace `\b<token>\b` occurrences.
fn replace_word(input: &str, token: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let token_bytes = token.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if index + token_bytes.len() <= bytes.len()
            && &bytes[index..index + token_bytes.len()] == token_bytes
        {
            let before = index
                .checked_sub(1)
                .map(|i| bytes[i])
                .map(|b| b.is_ascii_alphanumeric() || b == b'_')
                .unwrap_or(false);
            let after = bytes
                .get(index + token_bytes.len())
                .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .unwrap_or(false);
            if !before && !after {
                out.push_str(replacement);
                index += token_bytes.len();
                continue;
            }
        }
        let character = input[index..].chars().next().unwrap_or('?');
        out.push(character);
        index += character.len_utf8();
    }
    out
}

/// Decode base64-ish blobs and keep the ones that look like text with URLs.
pub fn decode_base64_candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '+' || character == '/' || character == '=' {
            current.push(character);
        } else {
            if current.len() >= 24 {
                if let Some(decoded) = base64_decode(&current) {
                    if decoded.contains("http") || decoded.contains(".m3u8") || decoded.contains(".mpd")
                    {
                        out.push(decoded);
                    }
                }
            }
            current.clear();
        }
    }
    out
}

/// Minimal standard-alphabet base64 decoder (no dependencies).
pub fn base64_decode(input: &str) -> Option<String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (index, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| *byte != b'=' && *byte != b'\n' && *byte != b'\r')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in cleaned {
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    String::from_utf8(out).ok()
}

fn dedupe_and_rank(report: &mut ResolveReport) {
    let mut seen: HashSet<String> = HashSet::new();
    report
        .candidates
        .retain(|candidate| seen.insert(format!("{}|{}", candidate.url, candidate.strategy)));
    report.candidates.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| a.url.cmp(&b.url))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survives_multibyte_text_around_escapes_and_attributes() {
        // A backslash followed by a multi-byte character used to panic.
        let text = "dosya: C:\\→\\x2fmp4/index.m3u8 müzik → çek";
        let decoded = decode_escapes(text);
        assert!(decoded.contains("index.m3u8"), "{decoded}");

        // More than the 240-byte look-back window of Turkish text before src=".
        let padding = "çok uzun türkçe metin → ".repeat(30);
        let html = format!("{padding}<video src=\"/a/index.m3u8\"></video>");
        let found = find_urls_in_text(&html);
        assert!(
            found.iter().any(|(url, _)| url.ends_with("index.m3u8")),
            "{found:?}"
        );
        assert!(
            find_bare_urls(&html)
                .iter()
                .any(|url| url.ends_with("index.m3u8"))
        );
    }

    #[test]
    fn skips_ad_urls_and_prefers_real_playlists() {
        assert!(is_ad_url("https://cdn.example/preroll/ads/x.m3u8"));
        assert!(is_ad_url("https://adsystem.example/live/master.m3u8"));
        assert!(!is_ad_url("https://cdn.example/vod/high/index.m3u8"));

        let html = r#"<html><body>
            <script>jwplayer("p").setup({sources:[{file:"/vod/high/preroll/ads/ad.m3u8"}]});</script>
            <video src="/vod/high/index.m3u8"></video></body></html>"#;
        let mut report = ResolveReport {
            page_url: "https://site.example/watch".into(),
            candidates: Vec::new(),
            visited: Vec::new(),
            errors: Vec::new(),
        };
        scan_body("https://site.example/watch", html, &mut report);
        assert!(
            report.candidates.iter().all(|candidate| !candidate.url.contains("/ads/")),
            "ad candidate leaked: {:?}",
            report.candidates
        );
        assert!(report
            .candidates
            .iter()
            .any(|candidate| candidate.url.ends_with("index.m3u8")));
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(classify_url("https://x/a/index.m3u8?t=1"), Some(MediaKind::Hls));
        assert_eq!(classify_url("https://x/a/manifest.mpd"), Some(MediaKind::Dash));
        assert_eq!(classify_url("https://x/a/movie.mp4"), Some(MediaKind::File));
        assert_eq!(classify_url("https://x/a/index.html"), None);
    }

    #[test]
    fn decodes_escape_sequences() {
        assert_eq!(decode_escapes(r"https:\/\/x\/a.m3u8"), "https://x/a.m3u8");
        assert_eq!(decode_escapes(r"\x68\x74\x74\x70"), "http");
        assert_eq!(decode_escapes(r"\u0068\u0074"), "ht");
    }

    #[test]
    fn finds_urls_in_various_shapes() {
        let html = r#"<video src="/v/movie.mp4"></video>
            <meta property="og:video" content="https://cdn.example/hls/index.m3u8" />
            <script>var config = {"file":"https:\/\/cdn.example\/packed\/master.m3u8"};</script>"#;
        let urls = find_urls_in_text(&decode_escapes(html));
        let found: Vec<&str> = urls.iter().map(|(url, _)| url.as_str()).collect();
        assert!(found.contains(&"/v/movie.mp4"));
        assert!(found.iter().any(|url| url.ends_with("master.m3u8")), "{found:?}");
    }

    #[test]
    fn unpacks_edwards_packed_javascript() {
        let packed = "eval(function(p,a,c,k,e,d){e=function(c){return c};return p}(\\'var u=\"http://cdn.example/a/index.m3u8\"\\',2,2,\\'x|y\\'.split(\\'|\\'),0,{}))";
        assert!(unpack_packed_js(packed).is_none(), "non-standard packer must not panic");

        // A realistic packer: '0' and '1' are the dictionary words.
        let packed = r#"eval(function(p,a,c,k,e,d){e=function(c){return c.toString(36)};if(!''.replace(/^/,String)){while(c--){d[e(c)]=k[c]||e(c)}k=[function(e){return d[e]}];e=function(){return'\\w+'};c=1};while(c--){if(k[c]){p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c])}}return p}('0="1://cdn.example/a/index.m3u8";',2,2,'url|http'.split('|'),0,{}))"#;
        let unpacked = unpack_packed_js(packed).expect("should unpack");
        assert!(unpacked.contains("http://cdn.example/a/index.m3u8"), "{unpacked}");
    }

    #[test]
    fn decodes_base64_blobs() {
        let encoded = "aHR0cHM6Ly9jZG4uZXhhbXBsZS5jb20vaGxzL2luZGV4Lm0zdTg=";
        let decoded = decode_base64_candidates(&format!("<script>var u=\"atob('{encoded}')\"</script>"));
        assert!(decoded.iter().any(|text| text.contains("index.m3u8")), "{decoded:?}");
    }

    #[test]
    fn detects_protection_markers() {
        assert_eq!(detect_drm("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\""), Some("sample-aes".to_string()));
        assert_eq!(
            detect_drm("<ContentProtection schemeIdUri=\"urn:uuid:EDEF8BA9\">widevine</ContentProtection>"),
            Some("widevine".to_string())
        );
        assert_eq!(detect_drm("#EXT-X-KEY:METHOD=AES-128"), None);
    }

    #[test]
    fn groups_and_orders_segments() {
        let urls: Vec<String> = vec![
            "https://cdn/hls/seg10.ts".into(),
            "https://cdn/hls/seg2.ts".into(),
            "https://cdn/hls/seg1.ts".into(),
            "https://cdn/other/seg1.m4s".into(),
        ];
        let groups = group_segments(&urls);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].len() >= groups[1].len());
        let ts = groups.iter().find(|group| group[0].ends_with(".ts")).unwrap();
        assert!(ts[0].ends_with("seg1.ts"));
        assert!(ts[2].ends_with("seg10.ts"));
    }
}

#[cfg(test)]
mod matrix_learning_tests {
    use super::*;

    #[test]
    fn player_config_beats_media_element() {
        let html = r#"<html><body><video><source src="/multi-source/720p.mp4"></video>
        <script>jwplayer("p").setup({sources:[{file:"/multi-source/1080p.mp4"}]});</script></body></html>"#;
        let found = find_urls_in_text(html);
        let hits: Vec<String> = found
            .iter()
            .map(|(url, evidence)| format!("{url} [{evidence}]"))
            .collect();
        assert!(
            hits.iter().any(|hit| hit.contains("1080p.mp4") && hit.contains("player-config")),
            "player config url not extracted: {hits:?}"
        );
        assert!(
            hits.iter().any(|hit| hit.contains("720p.mp4") && hit.contains("media-element")),
            "media element url not tagged: {hits:?}"
        );
    }
}
