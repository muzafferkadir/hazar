//! Test helper: runs the loopback API on an ephemeral port and echoes what it
//! receives, so `extension/test/bridge.test.cjs` can drive it like the browser
//! extension would.
//!
//! Prints:
//!   PORT <port>
//!   READY
//!   GRAB <json request>
//!   OTHER <kind>
//!   DONE

use std::time::Duration;

use hazar_localapi::{Inbound, LocalApiConfig, Outbound};

#[tokio::main]
async fn main() {
    let cfg = LocalApiConfig {
        ports: vec![0],
        ..Default::default()
    };
    let (server, mut inbound) = hazar_localapi::server::start(cfg).await.expect("start server");
    println!("PORT {}", server.port);
    println!("READY");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, inbound.recv()).await {
            Ok(Some(message)) => match message.message {
                Inbound::Grab(grab) => {
                    let request = serde_json::to_string(&grab.request).expect("json");
                    println!("GRAB {} {}", grab.id, request);
                    server.broadcast(Outbound::GrabAck {
                        id: grab.id,
                        state: "queued".into(),
                        message: None,
                    });
                }
                Inbound::Cancel(cancel) => println!("CANCEL {}", cancel.id),
                Inbound::Media(media) => println!("MEDIA {}", media.items.len()),
                Inbound::Ping(_) => {}
                Inbound::Hello(_) => {}
            },
            Ok(None) => break,
            Err(_) => break,
        }
    }

    server.shutdown();
    println!("DONE");
}
