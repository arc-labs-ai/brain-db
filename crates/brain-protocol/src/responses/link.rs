//! LINK / UNLINK responses.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::{EdgeKindWire, WireMemoryId};

/// Spec §09/07 §3 — `LINK_RESP` body.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct LinkResponse {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    pub weight: f32,
    pub created_at_unix_nanos: u64,
    /// `true` if this edge already existed (LINK is overwriting weight),
    /// `false` if newly created.
    pub already_existed: bool,
}

/// Spec §09/07 §5 — `UNLINK_RESP` body.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnlinkResponse {
    pub source: WireMemoryId,
    pub target: WireMemoryId,
    pub kind: EdgeKindWire,
    /// `true` if the edge existed and was removed; `false` if it
    /// didn't exist (UNLINK is idempotent — non-existent = no-op).
    pub removed: bool,
}
