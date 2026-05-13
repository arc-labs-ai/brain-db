//! LINK / UNLINK requests.

use rkyv::{Archive, Deserialize, Serialize};

use super::types::EdgeKindWire;
use crate::request::{WireMemoryId, WireUuid};

/// Spec §07/6 — `LINK_REQ` body. Creates an edge between two memories.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct LinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `[0, 1]` for most kinds; `[-1, 1]` for `Contradicts` (spec §09/07 §2).
    pub weight: f32,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}

/// Spec §07/6 — `UNLINK_REQ` body. Removes an edge identified by the
/// `(source, kind, target)` triple.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnlinkRequest {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub request_id: WireUuid,
    pub txn_id: Option<WireUuid>,
}
