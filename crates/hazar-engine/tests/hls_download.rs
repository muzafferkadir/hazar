//! HLS: playlist parsing (pure) and an end-to-end download against a local
//! server that serves a master playlist, an AES-128 encrypted media playlist
//! and segment files.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use hazar_engine::{download_hls, is_hls, parse_playlist, HlsOptions, Playlist, WorkDir};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
const IV: [u8; 16] = [
    0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0, 0xf0, 0x01,
];

fn payload(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut out = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state & 0xff) as u8);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn encrypt(data: &[u8], iv: &[u8; 16]) -> Vec<u8> {
    let enc = Aes128CbcEnc::new_from_slices(&KEY, iv).expect("key/iv");
    enc.encrypt_padded_vec_mut::<Pkcs7>(data)
}

/// Static file server: path → body, `Connection: close`.
async fn serve(routes: HashMap<String, Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let _ = handle(stream, routes).await;
            });
        }
    });
    addr
}

async fn handle(mut stream: TcpStream, routes: Arc<HashMap<String, Vec<u8>>>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let path = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let (status, body): (&str, Vec<u8>) = match routes.get(&path) {
        Some(body) => ("200 OK", body.clone()),
        None => ("404 Not Found", b"not found".to_vec()),
    };
    let content_type = if path.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp2t"
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hazar-hls-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn detects_playlists_by_url_and_mime() {
    assert!(is_hls("https://x/y/index.m3u8?token=1", None));
    assert!(is_hls("https://x/y", Some("application/vnd.apple.mpegurl")));
    assert!(is_hls("https://x/y", Some("application/x-mpegURL; charset=utf-8")));
    assert!(!is_hls("https://x/y.mp4", Some("video/mp4")));
}

#[test]
fn parses_master_playlist_variants() {
    let body = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
        low/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n\
        high/index.m3u8\n";
    let base = Url::parse("https://cdn.example.com/vod/master.m3u8").unwrap();
    match parse_playlist(body, &base).unwrap() {
        Playlist::Master(variants) => {
            assert_eq!(variants.len(), 2);
            assert_eq!(variants[1].uri, "https://cdn.example.com/vod/high/index.m3u8");
            assert_eq!(variants[1].bandwidth, 2_400_000);
            assert_eq!(variants[1].resolution.as_deref(), Some("1280x720"));
        }
        other => panic!("expected master playlist, got {other:?}"),
    }
}

#[test]
fn parses_media_playlist_with_key_map_and_byterange() {
    let body = "#EXTM3U\n\
        #EXT-X-VERSION:7\n\
        #EXT-X-TARGETDURATION:6\n\
        #EXT-X-MEDIA-SEQUENCE:5\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x102030405060708090a0b0c0d0e0f001\n\
        #EXTINF:6.0,\n\
        #EXT-X-BYTERANGE:1000@0\n\
        media.mp4\n\
        #EXTINF:6.0,\n\
        #EXT-X-BYTERANGE:1000\n\
        media.mp4\n\
        #EXTINF:6.0,\n\
        seg3.ts\n\
        #EXT-X-ENDLIST\n";
    let base = Url::parse("https://cdn.example.com/vod/high/index.m3u8").unwrap();
    match parse_playlist(body, &base).unwrap() {
        Playlist::Media {
            segments, end_list, ..
        } => {
            assert!(end_list);
            assert_eq!(segments.len(), 4, "map + 3 segments");
            assert!(segments[0].init);
            assert_eq!(segments[1].byterange, Some((0, 999)));
            assert_eq!(segments[2].byterange, Some((1000, 1999)));
            assert_eq!(segments[2].seq, 6);
            assert_eq!(segments[3].byterange, None);
            let key = segments[1].key.as_ref().expect("key inherited");
            assert_eq!(key.method, "AES-128");
            assert_eq!(key.uri.as_deref(), Some("https://cdn.example.com/vod/high/key.bin"));
            assert_eq!(key.iv, Some(IV));
        }
        other => panic!("expected media playlist, got {other:?}"),
    }
}

#[tokio::test]
async fn downloads_encrypted_hls_matches_hash_and_resumes() {
    let total = 3 * 4096;
    let media = Arc::new(payload(total));
    let segments: Vec<Vec<u8>> = (0..3)
        .map(|i| media[i * 4096..(i + 1) * 4096].to_vec())
        .collect();
    let digest = sha256_hex(&media);

    let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
    routes.insert(
        "/vod/master.m3u8".into(),
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhigh.m3u8\n".to_vec(),
    );
    routes.insert(
        "/vod/low.m3u8".into(),
        b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nlow-0.ts\n#EXT-X-ENDLIST\n".to_vec(),
    );
    routes.insert(
        "/vod/high.m3u8".into(),
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x102030405060708090a0b0c0d0e0f001\n\
             #EXTINF:6,\nseg0.ts\n#EXTINF:6,\nseg1.ts\n#EXTINF:6,\nseg2.ts\n#EXT-X-ENDLIST\n"
        )
        .into_bytes(),
    );
    routes.insert("/vod/key.bin".into(), KEY.to_vec());
    for (i, segment) in segments.iter().enumerate() {
        routes.insert(format!("/vod/seg{i}.ts"), encrypt(segment, &IV));
    }

    let addr = serve(routes).await;
    let manifest = format!("http://{addr}/vod/master.m3u8");
    let dir = tmp_dir("encrypted");
    let output = dir.join("out.ts");

    let outcome = download_hls(
        HlsOptions {
            manifest: manifest.clone(),
            segments: None,
            base_url: None,
            output: output.clone(),
            connections: 3,
            user_agent: None,
            headers: Vec::new(),
            expected_sha256: Some(digest.clone()),
            cancel: None,
        },
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome.segments, 3);
    assert_eq!(
        outcome.variant.as_deref(),
        Some(format!("http://{addr}/vod/high.m3u8").as_str()),
        "must pick the highest bandwidth variant"
    );
    assert!(!outcome.resumed);
    let got = tokio::fs::read(&output).await.unwrap();
    assert_eq!(got, *media, "decrypted output must equal the fixture");
    assert_eq!(outcome.sha256.as_deref(), Some(digest.as_str()));
    assert!(!WorkDir::new(&output).root.exists(), "sidecar removed on success");

    // Second run: wrong checksum keeps the work dir, third run resumes it.
    let wrong = download_hls(
        HlsOptions {
            manifest: manifest.clone(),
            segments: None,
            base_url: None,
            output: output.clone(),
            connections: 3,
            user_agent: None,
            headers: Vec::new(),
            expected_sha256: Some("00".repeat(32)),
            cancel: None,
        },
        None,
    )
    .await;
    assert!(wrong.is_err());
    assert!(
        WorkDir::new(&output).root.exists(),
        "a failed run must keep segments for resume"
    );

    let resumed = download_hls(
        HlsOptions {
            manifest,
            segments: None,
            base_url: None,
            output: output.clone(),
            connections: 3,
            user_agent: None,
            headers: Vec::new(),
            expected_sha256: Some(digest),
            cancel: None,
        },
        None,
    )
    .await
    .unwrap();
    assert!(resumed.resumed, "segments on disk must be reused");
    assert_eq!(tokio::fs::read(&output).await.unwrap(), *media);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn downloads_plain_hls_without_key() {
    let total = 2048;
    let media = Arc::new(payload(total));
    let digest = sha256_hex(&media);

    let mut routes: HashMap<String, Vec<u8>> = HashMap::new();
    routes.insert(
        "/plain/index.m3u8".into(),
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n"
            .to_vec(),
    );
    routes.insert("/plain/a.ts".into(), media[..1024].to_vec());
    routes.insert("/plain/b.ts".into(), media[1024..].to_vec());

    let addr = serve(routes).await;
    let dir = tmp_dir("plain");
    let output = dir.join("out.ts");

    let outcome = download_hls(
        HlsOptions {
            manifest: format!("http://{addr}/plain/index.m3u8"),
            segments: None,
            base_url: None,
            output: output.clone(),
            connections: 2,
            user_agent: None,
            headers: Vec::new(),
            expected_sha256: Some(digest),
            cancel: None,
        },
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome.segments, 2);
    assert_eq!(tokio::fs::read(&output).await.unwrap(), *media);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn sends_custom_headers_to_the_playlist_and_segments() {
    let media = payload(512);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<tokio::sync::Mutex<Vec<String>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_task = seen.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let seen = seen_task.clone();
            let media = media.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                seen.lock().await.push(text.clone());
                let path = text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/");
                let body = if path.ends_with(".m3u8") {
                    b"#EXTM3U\n#EXTINF:4,\nonly.ts\n#EXT-X-ENDLIST\n".to_vec()
                } else {
                    media.clone()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.flush().await;
            });
        }
    });

    let dir = tmp_dir("headers");
    let output = dir.join("out.ts");
    download_hls(
        HlsOptions {
            manifest: format!("http://{addr}/index.m3u8"),
            segments: None,
            base_url: None,
            output,
            connections: 1,
            user_agent: Some("HazarTest/9".into()),
            headers: vec![
                ("Referer".into(), "https://site.example/watch".into()),
                ("Cookie".into(), "sid=abc123".into()),
            ],
            expected_sha256: None,
            cancel: None,
        },
        None,
    )
    .await
    .unwrap();

    let requests = seen.lock().await.clone();
    assert!(requests.len() >= 2, "playlist + segment, got {}", requests.len());
    for request in &requests {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("referer: https://site.example/watch"), "{request}");
        assert!(lower.contains("cookie: sid=abc123"), "{request}");
        assert!(lower.contains("user-agent: hazartest/9"), "{request}");
    }

    std::fs::remove_dir_all(dir).ok();
}
