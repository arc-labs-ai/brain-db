//! Stream-control requests: CANCEL_STREAM, PING, CLIENT_PONG, BYE.

use crate::shared::primitives::CancellationReason;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CancelStreamRequest {
    pub target_stream_id: u32,
    pub reason: CancellationReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PingRequest {
    pub client_timestamp_unix_nanos: u64,
}

/// — `CLIENT_PONG` (despite "Response" in the spec name, it's
/// a client→server frame replying to `SERVER_PING`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientPongRequest {
    pub server_timestamp_unix_nanos: u64,
    pub client_timestamp_unix_nanos: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ByeRequest {
    pub reason: Option<String>,
}

// ============================================================
// Response payloads
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CancelStreamAck {
    pub target_stream_id: u32,
    pub cancelled_at_unix_nanos: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PongResponse {
    pub client_timestamp_unix_nanos: u64,
    pub server_timestamp_unix_nanos: u64,
}

/// — server-initiated keepalive (despite "Request" in the
/// spec name, this is a server→client frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerPingResponse {
    pub server_timestamp_unix_nanos: u64,
}
