//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT requests.

use crate::envelope::request::WireUuid;
use crate::ops::memory::ActAs;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TxnBeginRequest {
    #[serde(with = "serde_bytes")]
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
    /// Effective identity every write buffered in this transaction commits
    /// as, on behalf of the authenticated connection principal. Delegation
    /// is fixed at begin and applies to the whole txn: `TXN_COMMIT` carries
    /// no `act_as` of its own — the commit runs under whatever identity the
    /// begin established. `None` (the common case, and omitted on the wire)
    /// means the txn commits as the connection's own key-bound identity.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
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
