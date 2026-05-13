//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT requests.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

/// Spec §07/9.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnBeginRequest {
    pub txn_id: WireUuid,
    pub timeout_seconds: u32,
}

/// Spec §07/10.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnCommitRequest {
    pub txn_id: WireUuid,
}

/// Spec §07/11.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TxnAbortRequest {
    pub txn_id: WireUuid,
}
