//! Fixture HTTP server: one process serves every scenario.
//!
//! Behaviours are per-route flags instead of bespoke servers, so a new hard
//! case (rate limits, dropped connections, gated files, expiring tokens) is a
//! few lines in `fixtures.rs`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug)]
pub struct Route {
    pub path: String,
    pub status: u16,
    pub content_type: String,
    pub body: Arc<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    pub redirect_to: Option<String>,
    /// Honour `Range` requests (206 + Content-Range).
    pub ranges: bool,
    /// Require this `name=value` cookie, else 403.
    pub require_cookie: Option<String>,
    /// Require a `Referer` header, else 403.
    pub require_referer: bool,
    /// Require this query substring, else 403 (expiring-token simulation).
    pub require_query: Option<String>,
    /// Answer the first N GETs with `rate_limit_status` + `Retry-After: 1`.
    pub rate_limit_first: usize,
    /// Status used while the rate limit is active (429 or 503).
    pub rate_limit_status: u16,
    /// Truncate the body of the first N GETs (dropped connection).
    pub truncate_first: usize,
}

impl Route {
    pub fn new(path: impl Into<String>, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            status: 200,
            content_type: content_type.to_string(),
            body: Arc::new(body),
            headers: Vec::new(),
            redirect_to: None,
            ranges: true,
            require_cookie: None,
            require_referer: false,
            require_query: None,
            rate_limit_first: 0,
            rate_limit_status: 429,
            truncate_first: 0,
        }
    }

    pub fn file(path: impl Into<String>, body: Vec<u8>) -> Self {
        Self::new(path, "application/octet-stream", body)
    }

    pub fn html(path: impl Into<String>, body: String) -> Self {
        Self::new(path, "text/html; charset=utf-8", body.into_bytes())
    }

    pub fn playlist(path: impl Into<String>, body: String) -> Self {
        Self::new(path, "application/vnd.apple.mpegurl", body.into_bytes())
    }

    pub fn dash(path: impl Into<String>, body: String) -> Self {
        Self::new(path, "application/dash+xml", body.into_bytes())
    }

    pub fn redirect(path: impl Into<String>, to: impl Into<String>) -> Self {
        let mut route = Self::new(path, "text/plain", Vec::new());
        route.status = 302;
        route.redirect_to = Some(to.into());
        route
    }

    pub fn tweak(mut self, change: impl FnOnce(&mut Self)) -> Self {
        change(&mut self);
        self
    }
}

pub struct FixtureServer {
    pub addr: SocketAddr,
    counters: Arc<Mutex<HashMap<String, usize>>>,
}

impl FixtureServer {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn hits(&self, path: &str) -> usize {
        self.counters
            .lock()
            .expect("counters")
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

pub async fn serve(routes: Vec<Route>) -> std::io::Result<FixtureServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let table: Arc<HashMap<String, Route>> = Arc::new(
        routes
            .into_iter()
            .map(|route| (route.path.clone(), route))
            .collect(),
    );
    let counters = Arc::new(Mutex::new(HashMap::new()));
    let live = Arc::new(AtomicUsize::new(0));

    let task_table = table.clone();
    let task_counters = counters.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            live.fetch_add(1, Ordering::Relaxed);
            let table = task_table.clone();
            let counters = task_counters.clone();
            tokio::spawn(async move {
                let _ = handle(stream, table, counters).await;
            });
        }
    });

    Ok(FixtureServer { addr, counters })
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(None);
        }
    }

    let text = String::from_utf8_lossy(&buffer).to_string();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
    }))
}

async fn handle(
    mut stream: TcpStream,
    table: Arc<HashMap<String, Route>>,
    counters: Arc<Mutex<HashMap<String, usize>>>,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };

    let Some(route) = table.get(&request.path) else {
        let body = b"not found".to_vec();
        write_response(&mut stream, 404, "text/plain", &body, &[], None, true).await?;
        return Ok(());
    };

    let is_get = request.method == "GET";
    let hits = {
        let mut counters = counters.lock().expect("counters");
        let entry = counters.entry(request.path.clone()).or_insert(0);
        if is_get {
            *entry += 1;
        }
        *entry
    };

    // Expiring token / cookie / referer gates.
    if let Some(required) = &route.require_query {
        if !request.query.contains(required.as_str()) {
            let body = b"expired token".to_vec();
            write_response(&mut stream, 403, "text/plain", &body, &[], None, true).await?;
            return Ok(());
        }
    }
    if let Some(required) = &route.require_cookie {
        let cookie = request.headers.get("cookie").cloned().unwrap_or_default();
        if !cookie.contains(required.as_str()) {
            let body = b"login required".to_vec();
            write_response(&mut stream, 403, "text/plain", &body, &[], None, true).await?;
            return Ok(());
        }
    }
    if route.require_referer && !request.headers.contains_key("referer") {
        let body = b"hotlink protection".to_vec();
        write_response(&mut stream, 403, "text/plain", &body, &[], None, true).await?;
        return Ok(());
    }

    if is_get && route.rate_limit_first > 0 && hits <= route.rate_limit_first {
        let (status, body) = if route.rate_limit_status == 503 {
            (503, b"service unavailable".to_vec())
        } else {
            (429, b"slow down".to_vec())
        };
        write_response(
            &mut stream,
            status,
            "text/plain",
            &body,
            &[("Retry-After".to_string(), "1".to_string())],
            None,
            true,
        )
        .await?;
        return Ok(());
    }

    if let Some(target) = &route.redirect_to {
        write_response(
            &mut stream,
            route.status,
            "text/plain",
            &[],
            &[("Location".to_string(), target.clone())],
            None,
            true,
        )
        .await?;
        return Ok(());
    }

    // Range handling
    let range = request.headers.get("range").cloned();
    let (start, end) = match range.as_deref().and_then(|value| parse_range(value, route.body.len() as u64)) {
        Some((start, end)) if route.ranges && request.method == "GET" => (Some(start), Some(end)),
        _ => (None, None),
    };

    let (status, extra, body) = match (start, end) {
        (Some(start), Some(end)) => {
            let slice = route.body[start as usize..=(end as usize).min(route.body.len().saturating_sub(1))]
                .to_vec();
            (
                206,
                vec![(
                    "Content-Range".to_string(),
                    format!("bytes {start}-{end}/{}", route.body.len()),
                )],
                slice,
            )
        }
        _ => (route.status, Vec::new(), route.body.as_ref().clone()),
    };

    let mut headers = route.headers.clone();
    headers.extend(extra);
    headers.push((
        "Accept-Ranges".to_string(),
        if route.ranges { "bytes" } else { "none" }.to_string(),
    ));

    if is_get && route.truncate_first > 0 && hits <= route.truncate_first {
        write_truncated(&mut stream, status, &route.content_type, &body, &headers).await?;
        return Ok(());
    }

    let send_body = request.method != "HEAD";
    write_response(
        &mut stream,
        status,
        &route.content_type,
        if send_body { &body } else { &[] },
        &headers,
        Some(body.len()),
        true,
    )
    .await?;
    Ok(())
}

fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.trim().trim_start_matches("bytes=");
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = if end.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        end.trim().parse().ok()?
    };
    Some((start, end.min(total.saturating_sub(1))))
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    headers: &[(String, String)],
    content_length: Option<usize>,
    close: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        reason(status),
        content_length.unwrap_or(body.len())
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    Ok(())
}

/// Declare a full Content-Length but only send half, then drop the socket.
async fn write_truncated(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        reason(status),
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    let half = (body.len() / 2).max(1);
    stream.write_all(&body[..half]).await?;
    stream.flush().await?;
    Ok(())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
