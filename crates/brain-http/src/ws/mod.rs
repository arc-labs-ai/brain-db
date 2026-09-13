//! WebSocket server (RFC 6455).
//!
//! Ships server-side ([`accept`]) plus the matching client
//! ([`connect`] / [`ConnectBuilder`]). The HTTP/1.1
//! `Upgrade: websocket` handshake is handled inside this module; the
//! frame protocol below the upgrade is driven by
//! `tokio-tungstenite` (~3 kLOC of audited RFC 6455 conformance).
//!
//! ## What we own
//!
//! - `Sec-WebSocket-Accept` derivation (`base64(sha1(key + GUID))`).
//! - Request validation (method, version, required headers).
//! - The `101 Switching Protocols` response builder.
//! - The bridge from hyper's [`hyper::upgrade::Upgraded`] (which
//!   uses hyper's `Read`/`Write` traits) to a
//!   [`tokio_tungstenite::WebSocketStream`] (which speaks Tokio's
//!   I/O via [`hyper_util::rt::TokioIo`]).
//!
//! ## What tokio-tungstenite owns
//!
//! - Frame parsing + masking (RFC 6455 §5).
//! - Control frames: ping/pong auto-reply, oversized-control rejection.
//! - Close handshake state machine.
//! - Per-message size limits (default 64 MiB).
//!
//! ## Re-exports
//!
//! Handlers get [`Message`], [`WsError`], [`CloseFrame`], and
//! [`WebSocketStream`] from this module so they don't take a direct
//! dep on `tungstenite`.

mod accept_key;
mod client;
mod server;
mod upgrade;

pub use accept_key::derive as derive_accept_key;
pub use client::{connect, ConnectBuilder, Connected};
pub use server::{accept, OnUpgrade};

// Re-exports so consumers don't pull tungstenite directly.
pub use tokio_tungstenite::tungstenite::protocol::CloseFrame;
pub use tokio_tungstenite::tungstenite::Error as WsError;
pub use tokio_tungstenite::tungstenite::Message;
pub use tokio_tungstenite::WebSocketStream;
