//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT responses.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// Spec §08 §9.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnBeginResponse {
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
    pub started_at_unix_nanos: u64,
}

/// Spec §08 §10.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnCommitResponse {
    pub txn_id: WireUuid,
    pub committed_at_unix_nanos: u64,
    pub operations_applied: u32,
}

/// Spec §08 §11.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnAbortResponse {
    pub txn_id: WireUuid,
    pub operations_discarded: u32,
}
