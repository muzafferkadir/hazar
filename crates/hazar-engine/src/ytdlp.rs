//! Video extraction runs in a bundled, independent yt-dlp process.
use crate::{Error, ProgressEvent, ProgressSender, Result};
use std::{path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

pub fn canonical_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") { return None; }
    let host = url.host_str()?;
    let parts: Vec<_> = url.path_segments()?.collect();
    let id = if host == "youtu.be" { parts.first()?.to_string() }
        else if matches!(host, "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" | "www.youtube-nocookie.com") {
            if url.path() == "/watch" { url.query_pairs().find(|(key, _)| key == "v")?.1.into_owned() }
            else if matches!(parts.first().copied(), Some("embed" | "shorts" | "live")) { parts.get(1)?.to_string() }
            else { return None; }
        } else { return None; };
    if id.len() != 11 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') { return None; }
    Some(format!("https://www.youtube.com/watch?v={id}"))
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BrowserContext {
    pub cookies: Vec<BrowserCookie>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    #[serde(default)] pub scoped_headers: Vec<ScopedHeaders>,
    #[serde(default)] pub proxy: Option<String>,
    #[serde(default)] pub source_address: Option<String>,
    #[serde(default)] pub network_error: Option<String>,
    #[serde(default)] pub impersonate: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScopedHeaders { pub url: String, pub headers: Vec<(String, String)> }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic { pub code: String, pub message: String, pub retry_after: u64 }

pub fn diagnostic(raw: &str) -> Diagnostic {
    let text = raw.to_lowercase();
    let (code, message, retry_after) = if text.contains("proxy") || text.contains("source address") {
        ("network", "Browser proxy ayarı eşlenemedi. yt-dlp proxy ayarını kontrol et.", 0)
    } else if text.contains("429") || text.contains("rate limit") || text.contains("too many requests") || text.contains("try again later") {
        ("rate_limit", "Site rate limit uyguladı. Bir süre bekleyip tekrar dene.", 300)
    } else if text.contains("po token") || text.contains("po_token") || text.contains("pot provider") {
        ("po_token", "YouTube PO token üretilemedi. Provider veya session güncellenmeli.", 60)
    } else if text.contains("sign in") || text.contains("login") || text.contains("log in") || text.contains("cookies") || text.contains("authentication") || text.contains("401") || text.contains("members-only") || text.contains("private video") || text.contains("age-restricted") {
        ("login", "Sitede oturum açıp video sayfasını yenile; ardından tekrar gönder.", 0)
    } else if text.contains("javascript") || text.contains("js runtime") || text.contains("challenge solver") || text.contains("deno") || text.contains("ejs") {
        ("runtime", "JavaScript solver bulunamadı veya güncel değil. Hazar’ı güncelle.", 60)
    } else if text.contains("403") || text.contains("forbidden") || text.contains("cloudflare") {
        ("forbidden", "Site erişimi reddetti (403). Sayfayı yenile; cookie, proxy ve impersonation ayarını kontrol et.", 60)
    } else if text.contains("drm") {
        ("drm", "Bu video DRM korumalı; indirilemez.", 0)
    } else if text.contains("live") || text.contains("playlist") {
        ("scope", "yt-dlp seçeneği şu an tek video için çalışıyor; live/playlist kapsam dışı.", 0)
    } else if text.contains("bulunamadı") || text.contains("no such file") || text.contains("not found") {
        ("dependency", "yt-dlp bağımlılığı bulunamadı. Hazar’ı güncelle.", 0)
    } else if text.contains("unsupported") || text.contains("not supported") || text.contains("desteklenmiyor") {
        ("unsupported", "yt-dlp bu URL’i desteklemiyor.", 0)
    } else if text.contains("timeout") || text.contains("timed out") || text.contains("meşgul") {
        ("timeout", "yt-dlp analiz süresi doldu. Tekrar dene.", 60)
    } else {
        ("extractor", "yt-dlp videoyu çözemedi. Sayfayı yenile; sorun sürerse extractor güncellenmeli.", 60)
    };
    Diagnostic { code: code.into(), message: message.into(), retry_after }
}

fn scoped_context(context: &BrowserContext) -> BrowserContext {
    let mut context = context.clone();
    for scope in &mut context.scoped_headers {
        let valid = url::Url::parse(&scope.url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
        scope.headers.retain(|(name, value)| valid && name.len() <= 128 && value.len() <= 16384
            && !name.contains(['\r', '\n', ':']) && !value.contains(['\r', '\n'])
            && !matches!(name.to_ascii_lowercase().as_str(), "cookie" | "host" | "connection" | "content-length" | "proxy-authorization" | "range")
            && !name.to_ascii_lowercase().starts_with("sec-"));
    }
    context
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BrowserCookie {
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires: u64,
    pub name: String,
    pub value: String,
}

struct ProcessTree(Option<u32>);
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            #[cfg(unix)] {
                let _ = std::process::Command::new("/bin/kill").args(["-KILL", &format!("-{pid}")])
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            }
            #[cfg(windows)] {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"])
                    .creation_flags(0x08000000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            }
        }
    }
}

struct CookieJar(PathBuf);
impl Drop for CookieJar {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}
fn command(raw: &str, context: &BrowserContext) -> Result<(tokio::process::Command, CookieJar)> {
    if let Some(reason) = &context.network_error { return Err(Error::Protocol(reason.clone())); }
    if let Some(address) = &context.source_address { address.parse::<std::net::IpAddr>().map_err(|_| Error::Protocol("source address geçersiz".into()))?; }
    if let Some(proxy) = context.proxy.as_deref().filter(|proxy| !proxy.is_empty()) {
        let parsed = url::Url::parse(proxy).map_err(|_| Error::Protocol("proxy URL geçersiz".into()))?;
        if !matches!(parsed.scheme(), "http" | "https" | "socks4" | "socks5" | "socks5h") { return Err(Error::Protocol("proxy protokolü desteklenmiyor".into())); }
    }
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let dir = std::env::temp_dir().join(format!("hazar-cookies-{}-{nonce}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(&dir)?;
    let jar = CookieJar(dir);
    let mut body = String::from("# Netscape HTTP Cookie File\n");
    for cookie in &context.cookies {
        if [&cookie.domain, &cookie.path, &cookie.name, &cookie.value].iter().any(|v| v.contains(['\t', '\r', '\n'])) { continue; }
        body.push_str(&format!("{}{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            if cookie.http_only { "#HttpOnly_" } else { "" }, cookie.domain,
            if cookie.domain.starts_with('.') { "TRUE" } else { "FALSE" }, cookie.path,
            if cookie.secure { "TRUE" } else { "FALSE" }, cookie.expires, cookie.name, cookie.value));
    }
    let file = jar.0.join("cookies.txt");
    #[cfg(windows)] let body = body.replace("\n", "\r\n");
    std::fs::write(&file, body)?;
    let context_file = jar.0.join("context.json");
    let context = scoped_context(context);
    std::fs::write(&context_file, serde_json::to_vec(&context).map_err(|_| Error::Protocol("context JSON".into()))?)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?; }
    let binary = std::env::var_os("HAZAR_YTDLP").unwrap_or_else(|| "yt-dlp".into());
    let runtime = std::env::var_os("HAZAR_DENO").map(PathBuf::from).unwrap_or_else(|| "deno".into());
    let mut command = tokio::process::Command::new(binary);
    command.args(["--ignore-config", "--no-plugin-dirs", "--no-remote-components", "--no-playlist", "--no-cache-dir", "--socket-timeout", "15"])
        .arg("--cookies").arg(file).arg("--js-runtimes").arg(format!("deno:{}", runtime.display()))
        .stdin(std::process::Stdio::null()).kill_on_drop(true);
    for (flag, value) in [("--referer", &context.referer), ("--user-agent", &context.user_agent)] {
        if let Some(value) = value { if !value.contains(['\r', '\n']) { command.arg(flag).arg(value); } }
    }
    command.env("HAZAR_CONTEXT_FILE", context_file).env("XDG_CACHE_HOME", &jar.0)
        .env("DENO_NO_PROMPT", "1").env("DENO_NO_UPDATE_CHECK", "1");
    let plugins = std::env::var_os("HAZAR_YTDLP_PLUGINS").map(PathBuf::from);
    if let Some(plugins) = plugins.filter(|path| path.is_dir()) { command.arg("--plugin-dirs").arg(plugins); }
    else if context.scoped_headers.iter().any(|scope| !scope.headers.is_empty()) { return Err(Error::Unsupported("Header plugin bulunamadı".into())); }
    if let Some(proxy) = &context.proxy { command.arg("--proxy").arg(proxy); }
    if let Some(address) = &context.source_address { command.arg("--source-address").arg(address); }
    if context.impersonate { command.args(["--impersonate", "chrome"]); }
    if canonical_url(raw).is_some() {
        let home = std::env::var_os("HAZAR_POT_HOME").map(PathBuf::from);
        if let Some(home) = home.filter(|home| home.join("src/generate_once.ts").is_file()) {
            #[cfg(windows)] {
                let native = home.join("node_modules/canvas/build/Release/win32-x64");
                let mut paths = vec![native];
                paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
                if let Ok(path) = std::env::join_paths(paths) { command.env("PATH", path); }
            }
            command.arg("--extractor-args").arg(format!("youtubepot-bgutilscript:server_home={}", home.display()));
            command.args(["--extractor-args", "youtube:player_client=mweb"]);
        }
    }
    #[cfg(unix)] command.process_group(0);
    #[cfg(windows)] command.creation_flags(0x08000000);
    Ok((command, jar))
}

fn guest_context(context: &BrowserContext) -> BrowserContext {
    let mut guest = context.clone();
    guest.cookies.retain(|cookie| matches!(cookie.name.as_str(), "VISITOR_INFO1_LIVE" | "VISITOR_PRIVACY_METADATA" | "PREF" | "CONSENT" | "SOCS" | "YSC"));
    guest.scoped_headers.clear();
    guest
}

/// Analiz sonucu: başlık + indirilecek en yüksek video yüksekliği (ör. 1080).
#[derive(Debug, Clone)]
pub struct Probe { pub title: String, pub height: Option<u32>, pub heights: Vec<u32> }

/// Analizin metadata'sı download'da tekrar kullanılır; yt-dlp aynı videoyu iki kez analiz etmez.
const INFO_TTL: Duration = Duration::from_secs(10 * 60);

fn info_path(raw: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let key = canonical_url(raw).unwrap_or_else(|| raw.to_string());
    std::env::temp_dir().join("hazar-ytdlp").join(format!("{:x}.info.json", Sha256::digest(key.as_bytes())))
}

fn fresh_info(raw: &str) -> Option<PathBuf> {
    let path = info_path(raw);
    let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    (age < INFO_TTL).then_some(path)
}

pub async fn probe(raw: &str, context: &BrowserContext) -> Result<Option<Probe>> {
    if canonical_url(raw).is_none() { return probe_once(raw, context).await; }
    let guest = guest_context(context);
    let result = probe_once(raw, &guest).await;
    if result.as_ref().err().is_some_and(|error| diagnostic(&error.to_string()).code == "login") && guest.cookies.len() < context.cookies.len() {
        return probe_once(raw, context).await;
    }
    result
}

pub async fn download(url: &str, height: Option<u32>, output: &Path, cancel: Arc<AtomicBool>, expected_sha256: Option<&str>, speed_limit: Option<u64>, context: &BrowserContext, progress: ProgressSender) -> Result<(u64, Option<String>)> {
    if cancel.load(Ordering::Relaxed) { return Err(Error::Cancelled); }
    if let Some(info) = fresh_info(url) {
        match download_once(url, height, Some(&info), output, cancel.clone(), expected_sha256, speed_limit, context, progress.clone()).await {
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(_) => { let _ = std::fs::remove_file(&info); }
            ok => return ok,
        }
    }
    if canonical_url(url).is_none() { return download_once(url, height, None, output, cancel, expected_sha256, speed_limit, context, progress).await; }
    let guest = guest_context(context);
    let result = download_once(url, height, None, output, cancel.clone(), expected_sha256, speed_limit, &guest, progress.clone()).await;
    if result.as_ref().err().is_some_and(|error| diagnostic(&error.to_string()).code == "login") && guest.cookies.len() < context.cookies.len() && !cancel.load(Ordering::Relaxed) {
        return download_once(url, height, None, output, cancel, expected_sha256, speed_limit, context, progress).await;
    }
    result
}

/// Seçilen yükseklik önce birebir aranır (1440/2160 için vp9 dahil), yoksa en yakın alt kalite.
fn format_selector(height: Option<u32>) -> String {
    const DEFAULT: &str = "bv*[ext=mp4][vcodec^=avc1]+ba[ext=m4a]/b[ext=mp4][vcodec^=avc1]/bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b";
    match height {
        None => DEFAULT.into(),
        Some(h) => {
            let c = format!("[height<={h}]");
            format!("bv*[height={h}][ext=mp4]+ba[ext=m4a]/bv*[height={h}]+ba/b[height={h}]/\
                bv*{c}[ext=mp4][vcodec^=avc1]+ba[ext=m4a]/b{c}[ext=mp4][vcodec^=avc1]/bv*{c}[ext=mp4]+ba[ext=m4a]/b{c}[ext=mp4]/bv*{c}+ba/b{c}")
        }
    }
}

#[cfg(test)]
mod format_tests {
    #[test]
    fn selector_caps_every_fallback() {
        assert!(!super::format_selector(None).contains("height"));
        let s = super::format_selector(Some(720));
        assert!(s.starts_with("bv*[height=720][ext=mp4]"));
        for alt in s.split('/') { assert!(alt.contains("height=720") || alt.contains("height<=720"), "{alt}"); }
    }
}

async fn probe_once(raw: &str, context: &BrowserContext) -> Result<Option<Probe>> {
    static PROBES: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let semaphore = PROBES.get_or_init(|| tokio::sync::Semaphore::new(2));
    let _permit = tokio::time::timeout(Duration::from_secs(5), semaphore.acquire()).await
        .map_err(|_| Error::Protocol("yt-dlp analiz meşgul".into()))?
        .map_err(|_| Error::Protocol("yt-dlp analiz kapalı".into()))?;
    let url = url::Url::parse(raw).map_err(|_| Error::Unsupported("Geçersiz video URL".into()))?;
    if !matches!(url.scheme(), "http" | "https") { return Err(Error::Unsupported("HTTP/HTTPS gerekli".into())); }
    let (mut command, _jar) = command(raw, context)?;
    command.args(["--skip-download", "--dump-single-json", "--playlist-end", "1", "--retries", "0", "--"]).arg(raw);
    command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|_| Error::Unsupported("yt-dlp bulunamadı".into()))?;
    let mut tree = ProcessTree(child.id());
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::try_join!(
            async { let mut data = Vec::new(); stdout.take(32 * 1024 * 1024).read_to_end(&mut data).await?; Ok::<_, std::io::Error>(data) },
            async { let mut data = Vec::new(); stderr.take(128 * 1024).read_to_end(&mut data).await?; Ok::<_, std::io::Error>(data) },
            child.wait(),
        )
    }).await;
    let (stdout, stderr, status) = match result {
        Ok(result) => result?,
        Err(_) => { stop(&mut child).await; tree.0 = None; return Err(Error::Protocol("yt-dlp analiz timeout".into())); }
    };
    tree.0 = None;
    if !status.success() { return Err(Error::Protocol(String::from_utf8_lossy(&stderr).chars().rev().take(8192).collect::<String>().chars().rev().collect())); }
    let info: serde_json::Value = serde_json::from_slice(&stdout).map_err(|_| Error::Protocol("yt-dlp metadata geçersiz".into()))?;
    if info["has_drm"].as_bool() == Some(true) { return Err(Error::Unsupported("DRM".into())); }
    if info["_type"].as_str().is_some_and(|kind| kind != "video") || info["is_live"].as_bool() == Some(true) { return Err(Error::Unsupported("live/playlist".into())); }
    let video = info["formats"].as_array().is_some_and(|formats| formats.iter().any(|f| f["vcodec"].as_str().is_some_and(|v| v != "none")))
        || info["vcodec"].as_str().is_some_and(|v| v != "none");
    if !video { return Ok(None); }
    let path = info_path(raw);
    if let Some(dir) = path.parent() { let _ = tokio::fs::create_dir_all(dir).await; }
    let _ = tokio::fs::write(&path, &stdout).await;
    // download --format ile aynı öncelik: önce mp4/avc1, yoksa herhangi bir video.
    let heights = |avc: bool| info["formats"].as_array().into_iter().flatten()
        .filter(|f| f["vcodec"].as_str().is_some_and(|v| v != "none" && (!avc || v.starts_with("avc1"))))
        .filter_map(|f| f["height"].as_u64()).max();
    let height = heights(true).or_else(|| heights(false)).or_else(|| info["height"].as_u64()).map(|h| h as u32);
    let mut all: Vec<u32> = info["formats"].as_array().into_iter().flatten()
        .filter(|f| f["vcodec"].as_str().is_some_and(|v| v != "none"))
        .filter_map(|f| f["height"].as_u64()).filter(|h| *h >= 144).map(|h| h as u32).collect();
    all.sort_unstable_by(|a, b| b.cmp(a)); all.dedup();
    Ok(Some(Probe { title: info["title"].as_str().unwrap_or("Video").chars().take(200).collect(), height, heights: all }))
}

#[allow(clippy::too_many_arguments)]
async fn download_once(url: &str, height: Option<u32>, info: Option<&Path>, output: &Path, cancel: Arc<AtomicBool>, expected_sha256: Option<&str>, speed_limit: Option<u64>, context: &BrowserContext, progress: ProgressSender) -> Result<(u64, Option<String>)> {
    let parsed = url::Url::parse(url).map_err(|_| Error::Unsupported("Video linki geçersiz".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") { return Err(Error::Unsupported("HTTP/HTTPS gerekli".into())); }
    let ffmpeg = std::env::var_os("HAZAR_FFMPEG").map(PathBuf::from).unwrap_or_else(|| "ffmpeg".into());
    use sha2::{Digest, Sha256};
    let key = format!("{:x}", Sha256::digest(url.as_bytes()));
    let work = output.with_extension(format!("ytdlp-{}{}-parts", &key[..16], height.map(|h| format!("-{h}p")).unwrap_or_default()));
    tokio::fs::create_dir_all(&work).await?;
    let (mut command, _jar) = command(url, context)?;
    command.args(["--no-simulate", "--newline", "--progress",
        "--progress-template", r#"download:HAZAR:{"downloaded_bytes":%(progress.downloaded_bytes|0)s,"total_bytes":%(progress.total_bytes,progress.total_bytes_estimate|0)s,"filename":%(progress.filename|unknown)j}"#, "--format",
        ])
        .arg(format_selector(height))
        .args(["--merge-output-format", "mp4", "--remux-video", "mp4", "--socket-timeout", "20", "--retries", "5",
        "--fragment-retries", "5", "--concurrent-fragments", "4", "--ffmpeg-location"])
        .arg(&ffmpeg)
        .arg("--output").arg(format!("{}.%(ext)s", work.join("video").display().to_string().replace('%', "%%")))
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    if let Some(limit) = speed_limit.filter(|value| *value > 0) { command.arg("--limit-rate").arg(limit.to_string()); }
    match info { Some(info) => { command.arg("--load-info-json").arg(info); } None => { command.arg("--").arg(url); } }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().map_err(|_| Error::Unsupported("yt-dlp downloader bulunamadı; Hazar'ı güncelle".into()))?;
    let mut tree = ProcessTree(child.id());
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let stderr = child.stderr.take().expect("stderr");
    let errors = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut last = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("ERROR:") || line.starts_with("WARNING:") { last.push_str(&line); last.push('\n'); if last.len() > 8192 { last = last.chars().rev().take(4096).collect::<String>().chars().rev().collect(); } }
        }
        last
    });
    let started = Instant::now();
    let mut tracks = std::collections::HashMap::<String, (u64, u64)>::new();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                if let Some(json) = line.strip_prefix("HAZAR:") {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
                        let written = value["downloaded_bytes"].as_u64().unwrap_or(0);
                        let total = value["total_bytes"].as_u64().or_else(|| value["total_bytes_estimate"].as_u64()).unwrap_or(0);
                        let filename = value["filename"].as_str().unwrap_or("video").to_string();
                        let track = tracks.entry(filename).or_default();
                        track.0 = track.0.max(written); track.1 = track.1.max(total);
                        let total_written = tracks.values().map(|track| track.0).sum();
                        let total_size = tracks.values().map(|track| track.1).sum();
                        let _ = progress.send(ProgressEvent::PartProgress { index: 0, part_written: written, part_length: total, total_written, total_size });
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancel.load(Ordering::Relaxed) { stop(&mut child).await; tree.0 = None; errors.abort(); return Err(Error::Cancelled); }
            }
        }
    }
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancel.load(Ordering::Relaxed) { stop(&mut child).await; tree.0 = None; errors.abort(); return Err(Error::Cancelled); }
            }
        }
    };
    tree.0 = None;
    if !status.success() {
        let reason = errors.await.unwrap_or_default();
        return Err(Error::Protocol(if reason.is_empty() { "yt-dlp download başarısız; HTML video olarak kaydedilmedi".into() } else { reason }));
    }
    let _ = errors.await;
    let downloaded = work.join("video.mp4");
    let _ = progress.send(ProgressEvent::Assembling { parts: 2 });
    // Require video; YouTube also requires audio. Silent videos on other sites are valid.
    let mut check = tokio::process::Command::new(ffmpeg);
    check.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-protocol_whitelist", "file,pipe", "-i"])
        .arg(&downloaded).args(["-map", "0:v:0", "-map", if canonical_url(url).is_some() { "0:a:0" } else { "0:a:0?" }, "-t", "0", "-c", "copy", "-f", "null", "-"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    check.creation_flags(0x08000000);
    if !tokio::time::timeout(Duration::from_secs(30), check.status()).await
        .map_err(|_| Error::Protocol("yt-dlp track kontrolü timeout".into()))??.success() {
        return Err(Error::Protocol("yt-dlp output'unda video/audio track yok; tamamlandı sayılmadı".into()));
    }
    let digest = if let Some(expected) = expected_sha256 {
        let _ = progress.send(ProgressEvent::Verifying);
        let actual = crate::hash::sha256_file(&downloaded).await?;
        if !actual.eq_ignore_ascii_case(expected.trim()) { return Err(Error::ChecksumMismatch { expected: expected.into(), actual }); }
        Some(actual)
    } else { None };
    if cancel.load(Ordering::Relaxed) { return Err(Error::Cancelled); }
    let size = tokio::fs::metadata(&downloaded).await?.len();
    if size == 0 { return Err(Error::Protocol("yt-dlp output boş".into())); }
    tokio::fs::rename(&downloaded, output).await?;
    let _ = tokio::fs::remove_dir_all(work).await;
    let _ = progress.send(ProgressEvent::Finished { bytes: size, elapsed_ms: started.elapsed().as_millis() as u64, sha256: digest.clone() });
    Ok((size, digest))
}

async fn stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(unix)] {
            let _ = tokio::process::Command::new("/bin/kill").args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().await;
        }
        #[cfg(windows)] {
            let _ = tokio::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(0x08000000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().await;
        }
    }
    let _ = child.kill().await;
}
