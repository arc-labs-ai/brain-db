//! SESSION_CREATE / SESSION_LIST / SESSION_DELETE — session registry ops.
//!
//! Non-admin, scoped to the caller's `(namespace, space)`. A session is a
//! conversation/run grouping within a space — soft grouping, never an
//! isolation boundary. `session_id` is the client's opaque `u64`. LIST
//! returns one space's sessions newest-first (by last_active).

use crate::envelope::request::{WireSessionId, WireUuid};
use crate::ops::memory::ActAs;

/// One session row in a [`SessionListResponse`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub session_id: WireSessionId,
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    pub memory_count: u32,
}

// ============================================================
// SESSION_CREATE
// ============================================================

/// `SESSION_CREATE_REQ`. Provisions a session under the caller's effective
/// space explicitly; idempotent.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCreateRequest {
    pub session_id: WireSessionId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCreateResponse {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    pub session_id: WireSessionId,
    /// `false` on an idempotent replay of an existing session.
    pub created: bool,
    pub created_at_unix_nanos: u64,
    pub last_active_unix_nanos: u64,
    pub memory_count: u32,
}

// ============================================================
// SESSION_LIST
// ============================================================

/// `SESSION_LIST_REQ`. Lists one `(namespace, space)`'s sessions
/// newest-first. `limit == 0` means "no cap".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionListRequest {
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionListResponse {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    pub sessions: Vec<SessionView>,
}

// ============================================================
// SESSION_DELETE
// ============================================================

/// `SESSION_DELETE_REQ`. Removes a session's memories + graph rows. Defaults
/// to soft (7-day grace); `hard = true` zeroes immediately. The default
/// session (`session_id = 0`) is non-deletable.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionDeleteRequest {
    pub session_id: WireSessionId,
    /// `true` ⇒ hard tombstone (immediate); `false` (default) ⇒ soft.
    #[serde(default)]
    pub hard: bool,
    #[serde(with = "serde_bytes")]
    pub request_id: WireUuid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub act_as: Option<ActAs>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionDeleteResponse {
    #[serde(with = "serde_bytes")]
    pub space_id: WireUuid,
    pub session_id: WireSessionId,
    /// `false` when the session had no registry row.
    pub existed: bool,
    /// Number of memories tombstoned by the cascade.
    pub memories_forgotten: u64,
}
