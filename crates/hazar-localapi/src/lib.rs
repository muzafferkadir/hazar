//! Loopback API that lets the Hazar browser extension hand downloads to the app.
//!
//! - WebSocket only, bound to `127.0.0.1`, port 8722 (falls through to 8730).
//! - Sub-protocol handshake: [`protocol::SUBPROTOCOL`] must be requested.
//! - First message must be [`protocol::Inbound::Hello`]; a per-connection
//!   session token comes back in [`protocol::Outbound::HelloOk`].

pub mod protocol;
pub mod server;

pub use protocol::{
    Grab, GrabKind, GrabRequest, Hello, Inbound, MediaCandidate, MediaCandidates, Outbound,
    QueueItem, Settings, SUBPROTOCOL,
};
pub use server::{ApiError, ClientMessage, LocalApiConfig, ServerHandle};
