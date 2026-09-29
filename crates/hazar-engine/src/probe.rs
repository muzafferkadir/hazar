use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What a URL told us before we start moving bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub requested_url: String,
    pub final_url: String,
    pub len: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub filename_hint: Option<String>,
}

impl ResourceInfo {
    pub fn supports_segments(&self) -> bool {
        self.accept_ranges && self.len.unwrap_or(0) > 0
    }
}

/// HEAD first, then a `GET Range: bytes=0-0` to confirm range support.
/// Some CDNs advertise no `Accept-Ranges` on HEAD but honour ranges on GET.
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<ResourceInfo> {
    let head_info = head(client, url).await.ok();
    if let Some(info) = &head_info {
        if info.accept_ranges && info.len.is_some() {
            return Ok(info.clone());
        }
    }

    match ranged_get(client, url).await {
        Ok(mut ranged) => {
            if let Some(head) = head_info {
                ranged.len = ranged.len.or(head.len);
                if ranged.etag.is_none() {
                    ranged.etag = head.etag;
                }
                if ranged.last_modified.is_none() {
                    ranged.last_modified = head.last_modified;
                }
                if ranged.content_disposition.is_none() {
                    ranged.content_disposition = head.content_disposition;
                }
                if ranged.filename_hint.is_none() {
                    ranged.filename_hint = head.filename_hint;
                }
            }
            Ok(ranged)
        }
        Err(e) => head_info.ok_or(e),
    }
}

async fn head(client: &reqwest::Client, url: &str) -> Result<ResourceInfo> {
    let resp = client.head(url).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: url.to_string(),
        });
    }
    Ok(from_response(resp, url))
}

async fn ranged_get(client: &reqwest::Client, url: &str) -> Result<ResourceInfo> {
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            url: url.to_string(),
        });
    }
    Ok(from_response(resp, url))
}

fn from_response(resp: reqwest::Response, requested_url: &str) -> ResourceInfo {
    let status = resp.status();
    let headers = resp.headers().clone();
    let final_url = resp.url().to_string();

    let header = |name: reqwest::header::HeaderName| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
    };

    // Total size: 206 gives the real total in Content-Range, 200 in Content-Length.
    let mut len = header(reqwest::header::CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
    if let Some(cr) = header(reqwest::header::CONTENT_RANGE) {
        if let Some((_, total)) = cr.rsplit_once('/') {
            if let Ok(total) = total.trim().parse::<u64>() {
                len = Some(total);
            }
        }
    }

    let accept_ranges = status == reqwest::StatusCode::PARTIAL_CONTENT
        || header(reqwest::header::ACCEPT_RANGES)
            .map(|v| v.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);

    let content_disposition = header(reqwest::header::CONTENT_DISPOSITION);
    let filename_hint = content_disposition
        .as_deref()
        .and_then(parse_disposition_filename)
        .or_else(|| filename_from_url(&final_url));

    ResourceInfo {
        requested_url: requested_url.to_string(),
        final_url,
        len,
        accept_ranges,
        etag: header(reqwest::header::ETAG),
        last_modified: header(reqwest::header::LAST_MODIFIED),
        content_type: header(reqwest::header::CONTENT_TYPE),
        content_disposition,
        filename_hint,
    }
}

/// `attachment; filename="a.zip"` / `filename*=UTF-8''a.zip`
pub fn parse_disposition_filename(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    for key in ["filename*=", "filename="] {
        let Some(pos) = lower.find(key) else { continue };
        let rest = &value[pos + key.len()..];
        let raw = rest.split(';').next().unwrap_or("").trim();
        let raw = raw.trim_matches('"').trim_matches('\'').trim();
        let raw = match raw.split_once("''") {
            Some((_, val)) => val,
            None => raw,
        };
        let decoded = percent_decode(raw);
        if !decoded.is_empty() {
            return Some(sanitize(&decoded));
        }
    }
    None
}

fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|s| !s.is_empty())?;
    Some(sanitize(&percent_decode(last)))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed
    }
}
