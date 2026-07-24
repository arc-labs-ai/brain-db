//! SPACE_CREATE / SPACE_LIST / SPACE_DELETE — space registry ops.
//!
//! Non-admin, scoped to the caller's `(namespace, space)`. CREATE and
//! DELETE act on the caller's effective space (selected by `act_as` or the
//! key's bound space); LIST enumerates the caller's namespace's spaces. The
//! effective space is echoed back on every response as `space_id`.

use crate::envelope::request::WireUuid;
use crate::ops::memory::ActAs;

/// One space row in a [`SpaceListResponse`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceView {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    pub memory_count: u64,
    pub session_count: u32,
}

// ============================================================
// SPACE_CREATE
// ============================================================

/// `SPACE_CREATE_REQ`. Provisions the caller's effective space explicitly;
/// idempotent (a create for an existing space returns the existing row).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceCreateRequest {
    /// Opaque caller metadata blob (quota hints, labels). `None` for a bare
    /// provision.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<Vec<u8>>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceCreateResponse {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    /// `false` on an idempotent replay of an existing space.
    pub created: bool,
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    pub memory_count: u64,
    pub session_count: u32,
}

// ============================================================
// SPACE_LIST
// ============================================================

/// `SPACE_LIST_REQ`. Lists the caller's namespace's spaces. `limit == 0`
/// means "no cap".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceListRequest {
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceListResponse {
    pub spaces: Vec<SpaceView>,
    /// `true` when the listing covers every shard. v1 lists only the
    /// caller-shard's spaces, so this is `false` until the cross-shard
    /// scatter-gather lands — clients treat `false` as "partial".
    pub cross_shard_complete: bool,
}

// ============================================================
// SPACE_DELETE
// ============================================================

/// `SPACE_DELETE_REQ`. GDPR erasure of the caller's effective space: removes
/// every row under `(namespace, space)`. Hard/immediate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceDeleteRequest {
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceDeleteResponse {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    /// `false` when the space had no registry row.
    pub existed: bool,
    /// Number of memories tombstoned by the cascade.
    pub memories_forgotten: u64,
}
