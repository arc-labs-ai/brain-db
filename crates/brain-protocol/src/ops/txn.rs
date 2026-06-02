//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT requests.

use crate::envelope::request::WireUuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnBeginRequest {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnCommitRequest {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnAbortRequest {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
}

// ============================================================
// Response payloads
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnBeginResponse {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
    pub started_at_unix_nanos: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnCommitResponse {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
    pub committed_at_unix_nanos: u64,
    pub operations_applied: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnAbortResponse {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
    pub operations_discarded: u32,
}
