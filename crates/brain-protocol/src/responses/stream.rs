//! Stream-control responses: CANCEL_STREAM_ACK, CLIENT_PONG, SERVER_PING.

use rkyv::{Archive, Deserialize, Serialize};

/// Spec §08 §12.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct CancelStreamAck {
    pub target_stream_id: u32,
    pub cancelled_at_unix_nanos: u64,
}

/// Spec §08 §13.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct PongResponse {
    pub client_timestamp_unix_nanos: u64,
    pub server_timestamp_unix_nanos: u64,
}

/// Spec §08 §14 — server-initiated keepalive (despite "Request" in the
/// spec name, this is a server→client frame).
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ServerPingResponse {
    pub server_timestamp_unix_nanos: u64,
}
