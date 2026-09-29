//! Loopback API: handshake rules, message round-trip and port fallback.

use futures_util::{SinkExt, StreamExt};
use hazar_localapi::{Inbound, LocalApiConfig, Outbound, SUBPROTOCOL};
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16, subprotocol: bool) -> Result<Ws, String> {
    let url = format!("ws://127.0.0.1:{port}/hazar");
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    if subprotocol {
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(SUBPROTOCOL));
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
