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
    #[serde(default)]
    pub track: u8,
    #[serde(default)]
    pub range: Option<(u64, u64)>,
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

/// Static VOD MPD parser. XML namespaces, inheritance and timeline are explicit.
pub fn parse_manifest(body: &str, base: &Url) -> Result<DashPlan> {
    let doc =
        roxmltree::Document::parse(body).map_err(|_| Error::Protocol("invalid MPD XML".into()))?;
    let mpd = doc.root_element();
    if mpd.tag_name().name() != "MPD" {
        return Err(Error::Protocol("response is not an MPD".into()));
    }
    if mpd.attribute("type") == Some("dynamic") {
        return Err(Error::Unsupported("live DASH is not supported".into()));
    }
    if doc
        .descendants()
        .any(|n| n.has_tag_name("ContentProtection"))
    {
        return Err(Error::Unsupported(format!(
            "DRM protected DASH: {}",
            crate::resolve::detect_drm(body).unwrap_or_else(|| "unknown DRM".into())
        )));
    }
    let periods: Vec<_> = mpd
        .children()
        .filter(|n| n.has_tag_name("Period"))
        .collect();
    if periods.len() != 1 {
        return Err(Error::Unsupported("DASH requires one Period".into()));
    }
    let period = periods[0];
    let duration = period
        .attribute("duration")
        .or(mpd.attribute("mediaPresentationDuration"))
        .and_then(iso8601_ms);
    let mut video = None;
    let mut audio = None;
    let mut sets: Vec<_> = period
        .children()
        .filter(|n| n.has_tag_name("AdaptationSet"))
        .collect();
    if sets.is_empty() {
        sets.push(period);
    }
    for set in sets {
        for rep in set.children().filter(|n| n.has_tag_name("Representation")) {
            let mime = rep
                .attribute("mimeType")
                .or(set.attribute("mimeType"))
                .unwrap_or("");
            let content = set.attribute("contentType").unwrap_or("");
            let slot = if mime.starts_with("audio/") || content == "audio" {
                &mut audio
            } else if mime.starts_with("video/") || content == "video" || mime.is_empty() {
                &mut video
            } else {
                continue;
            };
            let bandwidth = rep
                .attribute("bandwidth")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if slot.as_ref().is_none_or(|(old, _, _)| bandwidth > *old) {
                *slot = Some((bandwidth, set, rep));
            }
        }
    }
    let mut segments = Vec::new();
    let mut representation = None;
    for (track, selected) in [(0u8, video), (1u8, audio)] {
        if let Some((_, set, rep)) = selected {
            let id = rep.attribute("id").unwrap_or("0");
            representation.get_or_insert_with(|| id.to_string());
            let mut url = base.clone();
            for node in [mpd, period, set, rep] {
                if let Some(value) = node
                    .children()
                    .find(|n| n.has_tag_name("BaseURL"))
                    .and_then(|n| n.text())
                {
                    url = url
                        .join(value.trim())
                        .map_err(|_| Error::Protocol("invalid DASH BaseURL".into()))?;
                }
            }
            let inherited = |tag: &str| {
                [rep, set, period, mpd]
                    .into_iter()
                    .find_map(|n| n.children().find(|c| c.has_tag_name(tag)))
            };
            let mut push = |reference: &str, init: bool, range: Option<&str>| -> Result<()> {
                let range = range
                    .map(|v| {
                        let (start, end) = v
                            .split_once('-')
                            .ok_or_else(|| Error::Protocol("bad DASH range".into()))?;
                        let start: u64 = start
                            .parse()
                            .map_err(|_| Error::Protocol("bad DASH range".into()))?;
                        let end: u64 = end
                            .parse()
                            .map_err(|_| Error::Protocol("bad DASH range".into()))?;
                        if end < start {
                            return Err(Error::Protocol("bad DASH range".into()));
                        }
                        Ok((start, end))
                    })
                    .transpose()?;
                segments.push(DashSegment {
                    url: url
                        .join(reference)
                        .map_err(|_| Error::Protocol("bad DASH URL".into()))?
                        .to_string(),
                    init,
                    track,
                    range,
                });
                Ok(())
            };
            if let Some(template) = inherited("SegmentTemplate") {
                let get = |key: &str| {
                    [rep, set, period, mpd].into_iter().find_map(|n| {
                        n.children()
                            .find(|c| c.has_tag_name("SegmentTemplate"))
                            .and_then(|c| c.attribute(key))
                    })
                };
                let bandwidth = rep.attribute("bandwidth").unwrap_or("0");
                if let Some(init) = get("initialization") {
                    push(&expand(init, id, bandwidth, 0, 0), true, None)?;
                }
                let media = get("media")
                    .ok_or_else(|| Error::Protocol("DASH template has no media".into()))?;
                let scale = get("timescale")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1);
                if scale == 0 {
                    return Err(Error::Protocol("DASH timescale is zero".into()));
                }
                let mut number = get("startNumber")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1);
                let timeline = template
                    .children()
                    .find(|c| c.has_tag_name("SegmentTimeline"))
                    .or_else(|| {
                        [set, period, mpd].into_iter().find_map(|n| {
                            n.children()
                                .find(|c| c.has_tag_name("SegmentTemplate"))
                                .and_then(|t| {
                                    t.children().find(|c| c.has_tag_name("SegmentTimeline"))
                                })
                        })
                    });
                if let Some(timeline) = timeline {
                    let entries: Vec<_> = timeline
                        .children()
                        .filter(|n| n.has_tag_name("S"))
                        .collect();
                    let mut time = 0u64;
                    for (i, entry) in entries.iter().enumerate() {
                        time = entry
                            .attribute("t")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(time);
                        let d = entry
                            .attribute("d")
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|d| *d > 0)
                            .ok_or_else(|| {
                                Error::Protocol("invalid DASH timeline duration".into())
                            })?;
                        let repeat = entry
                            .attribute("r")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        let count = if repeat >= 0 {
                            repeat as u64 + 1
                        } else if repeat == -1 {
                            let end = entries
                                .get(i + 1)
                                .and_then(|n| n.attribute("t"))
                                .and_then(|s| s.parse::<u64>().ok())
                                .or_else(|| {
                                    duration.map(|ms| (ms / 1000.0 * scale as f64).ceil() as u64)
                                })
                                .ok_or_else(|| {
                                    Error::Unsupported("unbounded DASH timeline".into())
                                })?;
                            end.saturating_sub(time).div_ceil(d)
                        } else {
                            return Err(Error::Protocol("invalid DASH repeat".into()));
                        };
                        if count > 20000 {
                            return Err(Error::Protocol("DASH timeline too large".into()));
                        }
                        for _ in 0..count {
                            push(&expand(media, id, bandwidth, number, time), false, None)?;
                            number += 1;
                            time = time
                                .checked_add(d)
                                .ok_or_else(|| Error::Protocol("DASH timeline overflow".into()))?;
                        }
                    }
                } else {
                    let d = get("duration")
                        .and_then(|s| s.parse::<u64>().ok())
                        .filter(|d| *d > 0)
                        .ok_or_else(|| {
                            Error::Unsupported("DASH template needs duration or timeline".into())
                        })?;
                    let duration = duration
                        .ok_or_else(|| Error::Unsupported("DASH duration unknown".into()))?;
                    let count = (duration / 1000.0 * scale as f64 / d as f64).ceil() as u64;
                    if count > 20000 {
                        return Err(Error::Protocol("DASH too large".into()));
                    }
                    for i in 0..count {
                        push(
                            &expand(media, id, bandwidth, number + i, i * d),
                            false,
                            None,
                        )?;
                    }
                }
            } else if let Some(list) = inherited("SegmentList") {
                if let Some(init) = list.attribute("initialization") {
                    push(init, true, None)?;
                }
                if let Some(init) = list.children().find(|n| n.has_tag_name("Initialization")) {
                    push(
                        init.attribute("sourceURL").unwrap_or(""),
                        true,
                        init.attribute("range"),
                    )?;
                }
                for seg in list.children().filter(|n| n.has_tag_name("SegmentURL")) {
                    push(
                        seg.attribute("media").unwrap_or(""),
                        false,
                        seg.attribute("mediaRange"),
                    )?;
                }
            } else if rep.children().any(|n| n.has_tag_name("BaseURL")) {
                push("", false, None)?;
            } else {
                return Err(Error::Unsupported("unsupported MPD shape".into()));
            }
        }
    }
    if segments.is_empty() || segments.len() > 40000 {
        return Err(Error::Protocol("invalid DASH segment count".into()));
    }
    Ok(DashPlan {
        manifest: base.to_string(),
        representation,
        segments,
    })
}

fn expand(template: &str, id: &str, bandwidth: &str, number: u64, time: u64) -> String {
    let mut out = template
        .replace("$RepresentationID$", id)
        .replace("$Bandwidth$", bandwidth)
        .replace("$Time$", &time.to_string())
        .replace("$Number$", &number.to_string());
    while let Some(start) = out.find("$Number%") {
        let Some(offset) = out[start + 1..].find('$') else {
            break;
        };
        let end = start + 1 + offset;
        let width = out[start + 8..end]
            .trim_end_matches('d')
            .parse::<usize>()
            .unwrap_or(0)
            .min(32);
        out.replace_range(start..=end, &format!("{number:0width$}"));
    }
    out.replace("$$", "$")
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
    download_dash_captured(opts, None, progress).await
}

pub async fn download_dash_captured(
    opts: DashOptions,
    captured: Option<&str>,
    progress: Option<ProgressSender>,
) -> Result<DashOutcome> {
    let started = Instant::now();
    let emit = |event: ProgressEvent| {
        if let Some(tx) = &progress {
            let _ = tx.send(event);
        }
    };

    let client = default_client_with(opts.user_agent.as_deref(), &opts.headers)?;
    let body = match captured {
        Some(body) => body.to_string(),
        None => fetch_text(&client, &opts.manifest).await?,
    };
    let base = Url::parse(&opts.manifest)
        .map_err(|error| Error::Protocol(format!("bad manifest url: {error}")))?;
    let plan = parse_manifest(&body, &base)?;

    let work = WorkDir::new(&opts.output);
    let plan_path = work.root.join("dash-plan.json");
    let existing: Option<DashPlan> = match tokio::fs::read(&plan_path).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };
    if existing
        .as_ref()
        .map(|previous| previous.segments != plan.segments)
        .unwrap_or(true)
    {
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
                if cancel
                    .as_ref()
                    .map(|flag| flag.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    return Err(Error::Cancelled);
                }
                match fetch_bytes(&client, &segment.url, segment.range).await {
                    Ok(data) => {
                        work.dash_write(index, &data).await?;
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

    let result_path = work.root.join(format!("result.{}", opts.output.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
    emit(ProgressEvent::Assembling { parts: total });
    let has_audio = plan.segments.iter().any(|s| s.track == 1);
    let size = if has_audio && plan.segments.iter().any(|s| s.track == 0) {
        let video = work.root.join("video.mp4");
        let audio = work.root.join("audio.mp4");
        work.dash_assemble_track(&video, &plan, 0).await?;
        work.dash_assemble_track(&audio, &plan, 1).await?;
        crate::media::mux(&video, &audio, &result_path, opts.cancel.clone()).await?;
        tokio::fs::metadata(&result_path).await?.len()
    } else {
        work.dash_assemble(&result_path, total).await?
    };

    let digest = if opts.expected_sha256.is_some() {
        emit(ProgressEvent::Verifying);
        Some(sha256_file(&result_path).await?)
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

    if opts
        .cancel
        .as_ref()
        .is_some_and(|c| c.load(Ordering::Relaxed))
    {
        return Err(Error::Cancelled);
    }
    tokio::fs::rename(&result_path, &opts.output).await?;
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

async fn fetch_bytes(
    client: &reqwest::Client,
    url: &str,
    range: Option<(u64, u64)>,
) -> Result<Vec<u8>> {
    let mut request = client.get(url);
    if let Some((start, end)) = range {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    }
    let response = request.send().await?;
    if let Some((start, end)) = range {
        let expected = format!("bytes {start}-{end}/");
        if response.status().as_u16() != 206
            || !response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|s| s.to_str().ok())
                .is_some_and(|s| s.starts_with(&expected))
        {
            return Err(Error::Protocol("DASH byte range mismatch".into()));
        }
    }
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

    async fn dash_assemble_track(
        &self,
        output: &std::path::Path,
        plan: &DashPlan,
        track: u8,
    ) -> Result<()> {
        let mut out = tokio::fs::File::create(output).await?;
        for (index, segment) in plan
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.track == track)
        {
            let _ = segment;
            let mut part = tokio::fs::File::open(self.segment_path_named(index)).await?;
            tokio::io::copy(&mut part, &mut out).await?;
        }
        out.sync_all().await?;
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
    fn inherited_timeline_preserves_time_and_separates_audio() {
        let body = r#"<MPD mediaPresentationDuration="PT6S"><Period>
        <AdaptationSet mimeType="video/mp4"><SegmentTemplate timescale="1" initialization="v-init.mp4" media="v-$Time$.m4s"><SegmentTimeline><S t="100" d="2" r="2"/></SegmentTimeline></SegmentTemplate><Representation id="v" bandwidth="1000"/></AdaptationSet>
        <AdaptationSet mimeType="audio/mp4"><SegmentTemplate duration="2" media="a-$Number$.m4s"/><Representation id="a" bandwidth="100"/></AdaptationSet>
        </Period></MPD>"#;
        let plan = parse_manifest(body, &Url::parse("https://cdn.example/m.mpd").unwrap()).unwrap();
        let video: Vec<_> = plan.segments.iter().filter(|s| s.track == 0 && !s.init).map(|s| s.url.clone()).collect();
        assert_eq!(video, vec!["https://cdn.example/v-100.m4s", "https://cdn.example/v-102.m4s", "https://cdn.example/v-104.m4s"]);
        assert_eq!(plan.segments.iter().filter(|s| s.track == 1).count(), 3);
    }

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
