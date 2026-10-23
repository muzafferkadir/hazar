//! Loopback API: handshake rules, message round-trip and port fallback.

use futures_util::{SinkExt, StreamExt};
use hazar_localapi::{Inbound, LocalApiConfig, Outbound, EXTENSION_ID, SUBPROTOCOL};
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16, subprotocol: bool) -> Result<Ws, String> {
    connect_from(port, subprotocol, Some(&format!("chrome-extension://{EXTENSION_ID}"))).await
}

async fn connect_from(port: u16, subprotocol: bool, origin: Option<&str>) -> Result<Ws, String> {
    let url = format!("ws://127.0.0.1:{port}/hazar");
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    if subprotocol {
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(SUBPROTOCOL));
    }
    if let Some(origin) = origin {
        request.headers_mut().insert("Origin", HeaderValue::from_str(origin).map_err(|e| e.to_string())?);
    }
    connect_async(request)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn send<T: serde::Serialize>(ws: &mut Ws, value: &T) {
    ws.send(Message::text(serde_json::to_string(value).unwrap()))
        .await
        .unwrap();
}

async fn recv<T: DeserializeOwned>(ws: &mut Ws) -> T {
    let message = ws.next().await.expect("frame").expect("ok frame");
    match message {
        Message::Text(text) => serde_json::from_str(text.as_ref()).expect("json"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_grab_broadcast_and_cancel() {
    let cfg = LocalApiConfig {
        ports: vec![0],
        ..Default::default()
    };
    let (handle, mut inbound) = hazar_localapi::server::start(cfg).await.unwrap();
    assert_eq!(handle.client_count(), 0);

    let mut ws = connect(handle.port, true).await.expect("connect");

    // First frame must be hello.
    send(
        &mut ws,
        &serde_json::json!({
            "type": "hello",
            "protocol": hazar_engine::PROTOCOL_VERSION,
            "client": "chrome",
            "extension_id": "test-ext"
        }),
    )
    .await;

    let hello: Outbound = recv(&mut ws).await;
    let session = match hello {
        Outbound::HelloOk {
            protocol,
            session,
            features,
            settings,
            ..
        } => {
            assert_eq!(protocol, hazar_engine::PROTOCOL_VERSION);
            assert!(features.iter().any(|f| f == "hls"));
            assert_eq!(settings.connections, hazar_engine::DEFAULT_CONNECTIONS as u32);
            session
        }
        other => panic!("expected hello_ok, got {other:?}"),
    };
    assert_eq!(handle.client_count(), 1);

    // Grab with the right session reaches the app side.
    send(
        &mut ws,
        &serde_json::json!({
            "type": "grab",
            "session": session,
            "id": "grab-1",
            "request": {
                "url": "https://cdn.example.com/vod/index.m3u8",
                "kind": "hls",
                "filename": "movie.ts",
                "referer": "https://site.example/watch",
                "cookie": "sid=abc",
                "headers": [["X-Hazar-Req", "abc"]]
            }
        }),
    )
    .await;

    let message = inbound.recv().await.expect("client message");
    match message.message {
        Inbound::Grab(grab) => {
            assert_eq!(grab.id, "grab-1");
            assert_eq!(grab.request.kind, hazar_localapi::GrabKind::Hls);
            assert_eq!(grab.request.cookie.as_deref(), Some("sid=abc"));
            assert_eq!(grab.request.headers.len(), 1);
        }
        other => panic!("expected grab, got {other:?}"),
    }

    // App → extension broadcast.
    handle.broadcast(Outbound::GrabAck {
        id: "grab-1".into(),
        state: "queued".into(),
        message: None,
    });
    let ack: Outbound = recv(&mut ws).await;
    match ack {
        Outbound::GrabAck { id, state, .. } => {
            assert_eq!(id, "grab-1");
            assert_eq!(state, "queued");
        }
        other => panic!("expected grab_ack, got {other:?}"),
    }

    // Wrong session is rejected without reaching the app.
    send(
        &mut ws,
        &serde_json::json!({
            "type": "cancel",
            "session": "not-the-session",
            "id": "grab-1"
        }),
    )
    .await;
    let error: Outbound = recv(&mut ws).await;
    assert!(matches!(error, Outbound::Error { .. }), "got {error:?}");

    // Ping/pong keeps the extension's keepalive honest.
    send(
        &mut ws,
        &serde_json::json!({ "type": "ping", "session": session, "t": 42 }),
    )
    .await;
    let pong: Outbound = recv(&mut ws).await;
    assert!(matches!(pong, Outbound::Pong { t: 42 }), "got {pong:?}");

    ws.close(None).await.ok();
    handle.shutdown();
}

#[tokio::test]
async fn rejects_connections_without_the_subprotocol() {
    let cfg = LocalApiConfig {
        ports: vec![0],
        ..Default::default()
    };
    let (handle, _inbound) = hazar_localapi::server::start(cfg).await.unwrap();
    assert!(connect(handle.port, false).await.is_err());
    handle.shutdown();
}

#[tokio::test]
async fn rejects_protocol_version_mismatch() {
    let cfg = LocalApiConfig {
        ports: vec![0],
        ..Default::default()
    };
    let (handle, mut inbound) = hazar_localapi::server::start(cfg).await.unwrap();
    let mut ws = connect(handle.port, true).await.expect("connect");

    send(
        &mut ws,
        &serde_json::json!({ "type": "hello", "protocol": 99, "client": "firefox" }),
    )
    .await;

    let reply: Outbound = recv(&mut ws).await;
    match reply {
        Outbound::HelloErr { reason } => assert!(reason.contains("protocol mismatch"), "{reason}"),
        other => panic!("expected hello_err, got {other:?}"),
    }
    assert!(inbound.try_recv().is_err(), "no message should reach the app");
    handle.shutdown();
}

#[tokio::test]
async fn falls_back_to_the_next_free_port() {
    let first = LocalApiConfig {
        ports: vec![0],
        ..Default::default()
    };
    let (a, _rx_a) = hazar_localapi::server::start(first.clone()).await.unwrap();

    // Ask for the busy port first, then an ephemeral one.
    let second = LocalApiConfig {
        ports: vec![a.port, 0],
        ..Default::default()
    };
    let (b, _rx_b) = hazar_localapi::server::start(second).await.unwrap();
    assert_ne!(b.port, a.port);
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn refuses_web_page_origin_and_missing_session() {
    let (handle, mut inbound) = hazar_localapi::server::start(LocalApiConfig { ports: vec![0], ..Default::default() }).await.unwrap();
    assert!(connect_from(handle.port, true, Some("https://attacker.example")).await.is_err());
    let mut ws = connect(handle.port, true).await.unwrap();
    send(&mut ws, &serde_json::json!({"type":"hello", "protocol":1, "client":"test"})).await;
    let _: Outbound = recv(&mut ws).await;
    send(&mut ws, &serde_json::json!({"type":"cancel", "id":"victim"})).await;
    assert!(matches!(recv::<Outbound>(&mut ws).await, Outbound::Error { .. }));
    assert!(inbound.try_recv().is_err());
    handle.shutdown();
}

#[tokio::test]
async fn accepts_only_the_hazar_extension_origin() {
    let (handle, _inbound) = hazar_localapi::server::start(LocalApiConfig { ports: vec![0], ..Default::default() }).await.unwrap();
    assert!(connect_from(handle.port, true, None).await.is_err(), "origin-less client accepted");
    assert!(connect_from(handle.port, true, Some("chrome-extension://abcdefghijklmnopabcdefghijklmnop")).await.is_err(), "other extension accepted");
    assert!(connect_from(handle.port, true, Some("moz-extension://8f1c2d3e-0000-4000-8000-000000000000")).await.is_ok(), "firefox extension refused");
    assert!(connect(handle.port, true).await.is_ok(), "hazar extension refused");
    handle.shutdown();
}

#[test]
fn extension_id_matches_the_manifest_key() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../../../extension/manifest.json")).unwrap();
    let key = base64::engine::general_purpose::STANDARD.decode(manifest["key"].as_str().expect("manifest key")).unwrap();
    // Chromium maps the first 16 bytes of sha256(public key) to the letters a..p.
    let id: String = Sha256::digest(key)[..16].iter().flat_map(|byte| [byte >> 4, byte & 0x0f]).map(|nibble| (b'a' + nibble) as char).collect();
    assert_eq!(id, EXTENSION_ID);
}

async fn hello(ws: &mut Ws) -> String {
    send(ws, &serde_json::json!({ "type": "hello", "protocol": hazar_engine::PROTOCOL_VERSION, "client": "test" })).await;
    match recv::<Outbound>(ws).await {
        Outbound::HelloOk { session, .. } => session,
        other => panic!("expected hello_ok, got {other:?}"),
    }
}

#[tokio::test]
async fn finished_and_released_jobs_free_their_owner_entry() {
    let (handle, mut inbound) = hazar_localapi::server::start(LocalApiConfig { ports: vec![0], ..Default::default() }).await.unwrap();
    let mut chrome = connect(handle.port, true).await.unwrap();
    let chrome_session = hello(&mut chrome).await;
    let mut firefox = connect_from(handle.port, true, Some("moz-extension://8f1c2d3e-0000-4000-8000-000000000000")).await.unwrap();
    let firefox_session = hello(&mut firefox).await;
    let grab = |session: &str, id: &str| serde_json::json!({ "type": "grab", "session": session, "id": id, "request": { "url": "https://cdn.example.com/a.bin", "kind": "file" } });

    for id in ["done-job", "removed-job"] {
        send(&mut chrome, &grab(&chrome_session, id)).await;
        assert!(matches!(inbound.recv().await.unwrap().message, Inbound::Grab(_)));
        send(&mut firefox, &grab(&firefox_session, id)).await;
        assert!(matches!(recv::<Outbound>(&mut firefox).await, Outbound::Error { .. }), "{id} must stay owned while active");
    }

    handle.broadcast(Outbound::Finished { id: "done-job".into(), path: "/tmp/a.bin".into(), size: 1, sha256: None, elapsed_ms: 1 });
    assert!(matches!(recv::<Outbound>(&mut chrome).await, Outbound::Finished { .. }), "owner still gets the final message");
    handle.release("removed-job");

    for id in ["done-job", "removed-job"] {
        send(&mut firefox, &grab(&firefox_session, id)).await;
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.recv()).await.unwrap_or_else(|_| panic!("{id} still owned")).unwrap();
        match message.message {
            Inbound::Grab(grab) => assert_eq!(grab.id, id),
            other => panic!("expected grab, got {other:?}"),
        }
    }
    handle.shutdown();
}
