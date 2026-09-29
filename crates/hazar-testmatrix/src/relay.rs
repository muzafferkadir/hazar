//! Minimal local HTTP proxy that relays to an upstream proxy **with
//! credentials**.
//!
//! Chromium has no way to authenticate against a proxy from the command line
//! (`--proxy-server` cannot carry `user:pass@`), so live browser rows point
//! Chrome at this relay and the relay talks to the real proxy.
//!
//! Deliberately small: `CONNECT` for TLS plus absolute-form plain HTTP, one
//! task per client connection, bytes spliced both ways.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const MAX_HEAD: usize = 64 * 1024;

pub struct Relay {
    pub port: u16,
    shutdown: watch::Sender<bool>,
}

impl Relay {
    pub fn address(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

#[derive(Debug, Clone)]
struct Upstream {
    host: String,
    port: u16,
    /// Pre-encoded `Basic …` value.
    authorization: String,
}

impl Upstream {
    fn parse(url: &str) -> Result<Self, String> {
        let trimmed = url.trim();
        let without_scheme = trimmed
            .strip_prefix("http://")
            .ok_or_else(|| format!("relay only supports http upstream proxies, got {trimmed}"))?;
        let (credentials, hostport) = match without_scheme.rsplit_once('@') {
            Some((credentials, hostport)) => (Some(credentials), hostport),
            None => (None, without_scheme),
        };
        let authorization = match credentials {
            Some(credentials) => format!("Basic {}", base64(credentials.as_bytes())),
            None => String::new(),
        };
        let (host, port) = hostport
            .rsplit_once(':')
            .ok_or_else(|| format!("upstream proxy has no port: {hostport}"))?;
        Ok(Self {
            host: host.to_string(),
            port: port.parse().map_err(|_| "bad upstream port".to_string())?,
            authorization,
        })
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Start a relay in front of `upstream` (`http://user:pass@host:port`).
pub async fn start(upstream: &str) -> Result<Arc<Relay>, String> {
    let upstream = Upstream::parse(upstream)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let (shutdown, mut shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((client, _)) => {
                            let upstream = upstream.clone();
                            tokio::spawn(async move {
                                if let Err(error) = handle(client, upstream).await {
                                    let _ = error;
                                }
                            });
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    Ok(Arc::new(Relay { port, shutdown }))
}

async fn handle(mut client: TcpStream, upstream: Upstream) -> std::io::Result<()> {
    let head = read_head(&mut client).await?;
    if head.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut upstream_conn = TcpStream::connect(upstream.address()).await?;

    if method.eq_ignore_ascii_case("CONNECT") {
        let mut request = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: keep-alive\r\n"
        );
        if !upstream.authorization.is_empty() {
            request.push_str(&format!(
                "Proxy-Authorization: {}\r\n",
                upstream.authorization
            ));
        }
        request.push_str("\r\n");
        upstream_conn.write_all(request.as_bytes()).await?;

        let response = read_head(&mut upstream_conn).await?;
        let response_text = String::from_utf8_lossy(&response).to_string();
        let ok = response_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .map(|code| code == "200")
            .unwrap_or(false);
        if !ok {
            client.write_all(&response).await?;
            return Ok(());
        }
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream_conn).await;
        return Ok(());
    }

    // Absolute-form plain HTTP: rewrite to origin-form and forward.
    let origin_form = match target.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(index) => rest[index..].to_string(),
            None => "/".to_string(),
        },
        None => target.clone(),
    };
    let mut request = format!("{method} {origin_form} HTTP/1.1\r\n");
    let mut host_header = None;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-authorization:") || lower.starts_with("proxy-connection:") {
            continue;
        }
        if lower.starts_with("host:") {
            host_header = Some(line.to_string());
        }
        request.push_str(line);
        request.push_str("\r\n");
    }
    if host_header.is_none() {
        if let Some(host) = target
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
        {
            request.push_str(&format!("Host: {host}\r\n"));
        }
    }
    if !upstream.authorization.is_empty() {
        request.push_str(&format!(
            "Proxy-Authorization: {}\r\n",
            upstream.authorization
        ));
    }
    request.push_str("Proxy-Connection: keep-alive\r\n\r\n");

    upstream_conn.write_all(request.as_bytes()).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream_conn).await;
    Ok(())
}

async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(buffer);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(buffer);
        }
        if buffer.len() > MAX_HEAD {
            return Ok(buffer);
        }
    }
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let buffer = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((buffer >> 18) & 63) as usize] as char);
        out.push(TABLE[((buffer >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((buffer >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(buffer & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[allow(dead_code)]
async fn splice<A: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin>(
    a: &mut A,
    b: &mut B,
) {
    let _ = tokio::io::copy_bidirectional(a, b).await;
}
