//! DASH (MPD) support: manifest parsing and segment download.
//!
//! Scope: the shapes real pages actually serve — `SegmentTemplate` with
//! `$Number$`, `SegmentList`, and `SegmentBase` (single file + range). Live
//! (`type="dynamic"`) manifests and DRM-protected representations are detected
//! and refused with a clear error instead of downloading garbage.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::download::default_client_with;
use crate::error::{Error, Result};
use crate::hash::{digest_matches, sha256_file};
use crate::meta::WorkDir;
use crate::progress::{ProgressEvent, ProgressSender};

const SEGMENT_RETRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashSegment {
    pub url: String,
    pub init: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashPlan {
    pub manifest: String,
    pub representation: Option<String>,
    pub segments: Vec<DashSegment>,
}

impl DashPlan {
    pub fn segment_count(&self) -> usize {
        self.segments.iter().filter(|segment| !segment.init).count()
    }
}

/// Parse an MPD into an ordered segment list (init first).
pub fn parse_manifest(body: &str, base: &Url) -> Result<DashPlan> {
    if body.contains("type=\"dynamic\"") {
        return Err(Error::Unsupported(
            "live DASH (type=\"dynamic\") manifests are not supported".into(),
        ));
    }
    if let Some(drm) = crate::resolve::detect_drm(body) {
        return Err(Error::Unsupported(format!("DASH protected by {drm}")));
    }

    let period = elements(body, "Period")
        .into_iter()
        .next()
        .unwrap_or(Element::default());
    let period_duration_ms = attr(&body[..body.len().min(0)], "mediaPresentationDuration")
        .or_else(|| attr(&leading_tag(body, "MPD"), "mediaPresentationDuration"))
        .and_then(|value| iso8601_ms(&value))
        .or_else(|| attr(&period.attrs, "duration").and_then(|value| iso8601_ms(&value)));

    // BaseURL: element text (may be relative), period first then MPD level.
    let base_url: Url = elements(&period.content, "BaseURL")
        .first()
        .map(|element| element.content.trim().to_string())
        .or_else(|| {
            elements(body, "BaseURL")
                .first()
                .map(|element| element.content.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .map(|value| {
            base.join(&value)
                .map_err(|error| Error::Protocol(format!("bad BaseURL {value}: {error}")))
        })
        .transpose()?
        .unwrap_or_else(|| base.clone());

    // Highest-bandwidth representation inside the first period.
    let mut best: Option<(u64, Element)> = None;
    for representation in elements(&period.content, "Representation") {
        let bandwidth = attr(&representation.attrs, "bandwidth")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if best
            .as_ref()
            .map(|(current, _)| bandwidth > *current)
            .unwrap_or(true)
        {
            best = Some((bandwidth, representation));
        }
    }
    let Some((_, representation)) = best else {
        return Err(Error::Protocol("MPD has no Representation".into()));
    };
    let representation_id = attr(&representation.attrs, "id").unwrap_or_else(|| "0".to_string());
    let bandwidth = attr(&representation.attrs, "bandwidth").unwrap_or_else(|| "0".to_string());

    let mut segments = Vec::new();

    // 1) SegmentTemplate
    if let Some(template) = tags(&representation.content, "SegmentTemplate")
        .first()
        .cloned()
    {
        let timescale = attr(&template, "timescale")
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(1.0);
        let duration = attr(&template, "duration").and_then(|value| value.parse::<f64>().ok());
        let start_number = attr(&template, "startNumber")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1);

        if let Some(init) = attr(&template, "initialization") {
            let expanded = expand(&init, &representation_id, &bandwidth, None);
            segments.push(DashSegment {
                url: resolve(&base_url, &expanded).map_err(Error::Protocol)?,
                init: true,
            });
        }

        let media = attr(&template, "media")
            .ok_or_else(|| Error::Protocol("SegmentTemplate without media attribute".into()))?;
        match (duration, period_duration_ms) {
            (Some(duration), Some(period_ms)) => {
                let per_segment_ms = duration / timescale * 1000.0;
                if per_segment_ms <= 0.0 {
                    return Err(Error::Protocol("SegmentTemplate duration is zero".into()));
                }
                let count = (period_ms / per_segment_ms).ceil() as u64;
                if count == 0 || count > 20_000 {
                    return Err(Error::Protocol(format!(
                        "SegmentTemplate implies an implausible segment count ({count})"
                    )));
                }
                for index in 0..count {
                    let number = start_number + index;
                    let expanded = expand(&media, &representation_id, &bandwidth, Some(number));
                    segments.push(DashSegment {
                        url: resolve(&base_url, &expanded).map_err(Error::Protocol)?,
                        init: false,
                    });
                }
            }
            (None, _) => {
                // SegmentTimeline: count the <S> entries.
                let timeline = tags(&template, "SegmentTimeline");
                let entries: usize = timeline
                    .iter()
                    .map(|_| 0)
                    .sum::<usize>()
                    .max(0);
                let _ = entries;
                let sizes = count_entries(&representation.content);
                if sizes == 0 {
                    return Err(Error::Unsupported(
                        "SegmentTemplate without duration or SegmentTimeline".into(),
                    ));
                }
                for index in 0..sizes as u64 {
                    let number = start_number + index;
                    let expanded = expand(&media, &representation_id, &bandwidth, Some(number));
                    segments.push(DashSegment {
                        url: resolve(&base_url, &expanded).map_err(Error::Protocol)?,
                        init: false,
                    });
                }
            }
            (Some(_), None) => {
                return Err(Error::Unsupported(
                    "SegmentTemplate without mediaPresentationDuration/Period duration".into(),
                ))
            }
        }
    }

    // 2) SegmentList
    if segments.is_empty() {
        if let Some(list) = elements(&representation.content, "SegmentList")
            .into_iter()
            .next()
        {
            if let Some(init) = attr(&list.attrs, "initialization") {
                segments.push(DashSegment {
                    url: resolve(&base_url, &init).map_err(Error::Protocol)?,
                    init: true,
                });
            }
            for entry in tags(&list.content, "SegmentURL") {
                if let Some(media) = attr(&entry, "media") {
                    segments.push(DashSegment {
                        url: resolve(&base_url, &media).map_err(Error::Protocol)?,
                        init: false,
                    });
                }
            }
        }
    }

    // 3) A single BaseURL inside the representation → one file.
    if segments.is_empty() {
        if let Some(single) = elements(&representation.content, "BaseURL")
            .first()
            .map(|element| element.content.trim().to_string())
        {
            if !single.is_empty() {
                segments.push(DashSegment {
                    url: resolve(&base_url, &single).map_err(Error::Protocol)?,
                    init: false,
                });
            }
        }
    }

    if segments.is_empty() {
        return Err(Error::Unsupported(
            "unsupported MPD shape (no SegmentTemplate/SegmentList/SegmentBase)".into(),
        ));
    }

    Ok(DashPlan {
        manifest: base.as_str().to_string(),
        representation: Some(representation_id),
        segments,
    })
}

/// Number of `<S ...>` entries inside a `SegmentTimeline` (or 0).
fn count_entries(representation_content: &str) -> usize {
    let Some(timeline) = elements(representation_content, "SegmentTimeline")
        .into_iter()
        .next()
    else {
        return 0;
    };
    tags(&timeline.content, "S").len()
}

/// Attribute text of the first `name` tag (used for MPD-level attributes).
fn leading_tag(body: &str, name: &str) -> String {
    tags(body, name).first().cloned().unwrap_or_default()
}

/// Expand a DASH template ($Number$, $RepresentationID$, $Bandwidth$, $Time$).
fn expand(template: &str, representation_id: &str, bandwidth: &str, number: Option<u64>) -> String {
    let mut out = template
        .replace("$RepresentationID$", representation_id)
        .replace("$Bandwidth$", bandwidth);
    let value = number.unwrap_or(0).to_string();
    out = out.replace("$Number$", &value);
    while let Some(start) = out.find("$Number%") {
        // Closing `$` must be searched *after* the opening token.
        let Some(offset) = out[start + 1..].find('$') else {
            break;
        };
        let end = start + 1 + offset;
        let spec = &out[start + 8..end];
        let width: usize = spec.trim_end_matches('d').trim().parse().unwrap_or(0);
        let formatted = format!("{number:0>width$}", number = number.unwrap_or(0), width = width);
        out = format!("{}{}{}", &out[..start], formatted, &out[end + 1..]);
    }
    out.replace("$Time$", &value)
}

fn resolve(base: &Url, reference: &str) -> std::result::Result<String, String> {
    if let Ok(absolute) = Url::parse(reference) {
        return Ok(absolute.to_string());
    }
    base.join(reference)
        .map(|url| url.to_string())
        .map_err(|error| format!("cannot resolve {reference}: {error}"))
}

#[derive(Debug, Clone, Default)]
struct Element {
    /// Text between `<Name` and `>` (attributes).
    attrs: String,
    /// Text between `>` and `</Name>` (children), empty for self-closing tags.
    content: String,
}

/// Every `name` element with its attributes and inner content.
fn elements(body: &str, name: &str) -> Vec<Element> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(found) = body[index..].find(&open) {
        let start = index + found;
        let Some(gt) = body[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        let raw = &body[start..gt];
        let self_closing = raw.trim_end().ends_with('/');
        let attrs = body[start + open.len()..gt]
            .trim_end()
            .trim_end_matches('/')
            .to_string();
        let content = if self_closing {
            String::new()
        } else {
            match body[gt..].find(&close) {
                Some(offset) => body[gt + 1..gt + offset].to_string(),
                None => String::new(),
            }
        };
        out.push(Element { attrs, content });
        index = gt + 1;
        if index >= body.len() {
            break;
        }
    }
    out
}

/// Attribute text of every `name` tag.
fn tags(body: &str, name: &str) -> Vec<String> {
    elements(body, name)
        .into_iter()
        .map(|element| element.attrs)
        .collect()
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let mut index = 0;
    while let Some(position) = tag[index..].find(&needle) {
        let position = index + position;
        let after = &tag[position + needle.len()..];
        let quote = after.chars().next()?;
        if quote == '"' || quote == '\'' {
            let value: String = after[1..].chars().take_while(|c| *c != quote).collect();
            return Some(value);
        }
        index = position + needle.len();
        if index >= tag.len() {
            break;
        }
    }
    None
}

/// `PT1H2M3.5S` → milliseconds
fn iso8601_ms(value: &str) -> Option<f64> {
    let value = value.trim();
    let body = value.strip_prefix("PT")?;
    let mut total = 0.0f64;
    let mut number = String::new();
    for character in body.chars() {
        match character {
            '0'..='9' | '.' => number.push(character),
            'H' => {
                total += number.parse::<f64>().ok()? * 3600.0;
                number.clear();
            }
            'M' => {
                total += number.parse::<f64>().ok()? * 60.0;
                number.clear();
            }
            'S' => {
                total += number.parse::<f64>().ok()?;
                number.clear();
            }
            _ => return None,
        }
    }
    Some(total * 1000.0)
}

#[derive(Debug, Clone)]
pub struct DashOptions {
    pub manifest: String,
    pub output: PathBuf,
    pub connections: usize,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub expected_sha256: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone)]
pub struct DashOutcome {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub segments: usize,
    pub representation: Option<String>,
    pub elapsed: Duration,
}

/// Download an MPD into a single file (init + media segments, in order).
pub async fn download_dash(
    opts: DashOptions,
    progress: Option<ProgressSender>,
) -> Result<DashOutcome> {
    let started = Instant::now();
    let emit = |event: ProgressEvent| {
        if let Some(tx) = &progress {
            let _ = tx.send(event);
        }
    };

    let client = default_client_with(opts.user_agent.as_deref(), &opts.headers)?;
    let body = fetch_text(&client, &opts.manifest).await?;
    let base = Url::parse(&opts.manifest)
        .map_err(|error| Error::Protocol(format!("bad manifest url: {error}")))?;
    let plan = parse_manifest(&body, &base)?;

    let work = WorkDir::new(&opts.output);
    let plan_path = work.root.join("dash-plan.json");
    let existing: Option<DashPlan> = match tokio::fs::read(&plan_path).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };
    if existing.as_ref().map(|previous| previous.segments != plan.segments).unwrap_or(true) {
        work.reset().await?;
    }
    work.ensure().await?;
    tokio::fs::write(&plan_path, serde_json::to_vec_pretty(&plan)?).await?;

    let total = plan.segments.len();
    let mut ready = Vec::with_capacity(total);
    for index in 0..total {
        ready.push(work.dash_ready(index).await);
    }
    let already = ready.iter().filter(|done| **done).count();

    emit(ProgressEvent::HlsPlanned {
        segments: total,
        variant: plan.representation.clone(),
        resumed: already > 0,
        done: already,
    });

    let done = Arc::new(AtomicU64::new(already as u64));
    let bytes = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(opts.connections.clamp(1, 16)));
    let mut set = tokio::task::JoinSet::new();

    for (index, segment) in plan.segments.iter().enumerate() {
        if ready[index] {
            continue;
        }
        let segment = segment.clone();
        let client = client.clone();
        let work = work.clone();
        let semaphore = semaphore.clone();
        let done = done.clone();
        let bytes = bytes.clone();
        let cancel = opts.cancel.clone();
        let tx = progress.clone();
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore");
            let mut attempt = 0u32;
            loop {
                if cancel.as_ref().map(|flag| flag.load(Ordering::Relaxed)).unwrap_or(false) {
                    return Err(Error::Cancelled);
                }
                match fetch_bytes(&client, &segment.url).await {
                    Ok(data) => {
                        work.dash_write(index, &data).await?;
                        let written =
                            bytes.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
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
                    Err(error) => {
                        attempt += 1;
                        if attempt > SEGMENT_RETRIES || !error.is_retryable() {
                            return Err(error);
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
            Ok(Err(error)) => {
                let hard = matches!(error, Error::Cancelled);
                if fatal.is_none() {
                    fatal = Some(error);
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
    let size = work.dash_assemble(&opts.output, total).await?;

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
    if let Err(error) = work.reset().await {
        eprintln!("hazar: could not remove {}: {error}", work.root.display());
    }

    Ok(DashOutcome {
        path: opts.output,
        size,
        sha256: digest,
        segments: plan.segment_count(),
        representation: plan.representation,
        elapsed,
    })
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        return Err(Error::Status {
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
    if !body.contains("<MPD") {
        return Err(Error::Protocol(format!(
            "manifest is not an MPD document (content-type {content_type:?}, {} bytes)",
            body.len()
        )));
    }
    Ok(body)
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        return Err(Error::Status {
            status: response.status().as_u16(),
            url: url.to_string(),
        });
    }
    let mut data = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        data.extend_from_slice(&chunk?);
    }
    Ok(data)
}

impl WorkDir {
    fn segment_path_named(&self, index: usize) -> PathBuf {
        self.root.join(format!("dash-{index:05}.bin"))
    }

    async fn dash_ready(&self, index: usize) -> bool {
        tokio::fs::metadata(self.segment_path_named(index))
            .await
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    }

    async fn dash_write(&self, index: usize, data: &[u8]) -> Result<()> {
        let final_path = self.segment_path_named(index);
        let partial = final_path.with_extension("partial");
        let mut file = tokio::fs::File::create(&partial).await?;
        file.write_all(data).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, &final_path).await?;
        Ok(())
    }

    async fn dash_assemble(&self, output: &std::path::Path, count: usize) -> Result<u64> {
        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let tmp = output.with_extension("assembling");
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut total = 0u64;
        for index in 0..count {
            let mut input = tokio::fs::File::open(self.segment_path_named(index)).await?;
            total += tokio::io::copy(&mut input, &mut out).await?;
        }
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        tokio::fs::rename(&tmp, output).await?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE_MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT18S" profiles="urn:mpeg:dash:profile:isoff-live:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v0" bandwidth="800000" codecs="avc1.4d401f" width="640" height="360">
        <SegmentTemplate timescale="1000" duration="6000" startNumber="1" initialization="init-360.mp4" media="seg-360-$Number$.m4s"/>
      </Representation>
      <Representation id="v1" bandwidth="2400000" codecs="avc1.64001f" width="1920" height="1080">
        <SegmentTemplate timescale="1000" duration="6000" startNumber="1" initialization="init-1080.mp4" media="seg-1080-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn picks_the_highest_bandwidth_and_numbers_segments() {
        let base = Url::parse("https://cdn.example/vod/manifest.mpd").unwrap();
        let plan = parse_manifest(TEMPLATE_MPD, &base).unwrap();
        assert_eq!(plan.representation.as_deref(), Some("v1"));
        assert_eq!(plan.segments.len(), 4, "init + 3 segments");
        assert!(plan.segments[0].init);
        assert!(plan.segments[0].url.ends_with("init-1080.mp4"));
        assert!(plan.segments[1].url.ends_with("seg-1080-1.m4s"));
        assert!(plan.segments[3].url.ends_with("seg-1080-3.m4s"));
    }

    #[test]
    fn resolves_base_url_and_segment_list() {
        let body = r#"<MPD mediaPresentationDuration="PT4S"><Period>
          <BaseURL>https://cdn.example/vod/</BaseURL>
          <Representation id="a" bandwidth="100">
            <SegmentList initialization="init.mp4">
              <SegmentURL media="s1.m4s"/><SegmentURL media="s2.m4s"/>
            </SegmentList>
          </Representation></Period></MPD>"#;
        let base = Url::parse("https://origin.example/movie/manifest.mpd").unwrap();
        let plan = parse_manifest(body, &base).unwrap();
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(plan.segments[0].url, "https://cdn.example/vod/init.mp4");
        assert_eq!(plan.segments[2].url, "https://cdn.example/vod/s2.m4s");
    }

    #[test]
    fn refuses_live_and_drm_manifests() {
        let base = Url::parse("https://cdn.example/live.mpd").unwrap();
        let live = r#"<MPD type="dynamic"><Period><Representation id="a" bandwidth="1">
            <SegmentTemplate initialization="i.mp4" media="s-$Number$.m4s" duration="2" timescale="1"/>
        </Representation></Period></MPD>"#;
        assert!(matches!(
            parse_manifest(live, &base),
            Err(Error::Unsupported(_))
        ));

        let drm = r#"<MPD mediaPresentationDuration="PT6S"><Period>
          <ContentProtection schemeIdUri="urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED"/>
          <Representation id="a" bandwidth="1"><SegmentList><SegmentURL media="s.m4s"/></SegmentList></Representation>
        </Period></MPD>"#;
        let error = parse_manifest(drm, &base).unwrap_err();
        assert!(error.to_string().contains("widevine"), "{error}");
    }

    #[test]
    fn expands_padded_number_templates() {
        let base = Url::parse("https://cdn.example/m.mpd").unwrap();
        let body = r#"<MPD mediaPresentationDuration="PT4S"><Period>
          <Representation id="v" bandwidth="10">
            <SegmentTemplate duration="2" timescale="1" media="s-$Number%03d$.m4s"/>
          </Representation></Period></MPD>"#;
        let plan = parse_manifest(body, &base).unwrap();
        assert_eq!(plan.segments[0].url, "https://cdn.example/s-001.m4s");
        assert_eq!(plan.segments[1].url, "https://cdn.example/s-002.m4s");
    }
}
