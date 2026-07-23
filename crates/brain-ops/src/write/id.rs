//! Identifier types specific to the write pipeline.
//!
//! - [`WriteId`] — universal idempotency / WAL / audit key. One per
//!   submitted [`super::Write`]. For single-op wire requests this is
//!   derived from the request's `request_id`; for `TXN_COMMIT` it's
//!   derived from the commit's `request_id`; for worker-submitted
//!   writes the worker mints a fresh v7.
//! - [`IdKind`] / [`AllocatedId`] — what handlers ask the writer for
//!   when they need a freshly-allocated id BEFORE submit (so the id
//!   travels inside the phase and WAL recovery never re-allocates).

use std::fmt;

use brain_core::{SpaceId, EntityId, MemoryId, RelationId, RequestId, StatementId};
use uuid::Uuid;

/// Idempotency key for a [`super::Write`]. Equality determines
/// "same write, retried".
///
/// For wire-driven single-op writes the id is derived from the pair
/// `(request_id, effective_space)` via [`WriteId::from_request`] — it
/// deliberately folds the effective space id into the digest so that
/// two different effective identities that happen to reuse the same
/// client `request_id` land on distinct cache entries. Without this,
/// `act_as` would let one caller's retried `request_id` collide with a
/// different effective identity's write and leak a cached ack across
/// the tenancy boundary. Worker-submitted writes mint a fresh v7 id
/// via [`WriteId::new`], so they never share a key with the wire
/// request that spawned them.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WriteId(pub Uuid);

impl WriteId {
    /// Fresh UUIDv7 — time-ordered for sorted scans of the idempotency
    /// cache. Used by workers that submit derived writes.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Derive deterministically from a wire `RequestId` scoped to the
    /// *effective* space the write runs as. The wire surface promises
    /// that retried requests carry the same `request_id`; the writer's
    /// idempotency cache uses the matching `WriteId` to short-circuit
    /// re-application.
    ///
    /// The digest is `blake3(space_id_bytes || request_id_bytes)`
    /// truncated to the leading 16 bytes. Folding the space id in makes
    /// the key per-effective-identity: the same `(request_id, space)`
    /// always yields the same `WriteId`, while two distinct effective
    /// identities reusing one `request_id` get distinct keys — the
    /// isolation that keeps `act_as` from leaking a cached ack across a
    /// tenancy boundary. Space ids are globally-unique 16-byte UUIDs, so
    /// folding the space alone suffices (the namespace is derivable from
    /// it); there is no need to fold the namespace separately.
    #[inline]
    #[must_use]
    pub fn from_request(req: RequestId, space: SpaceId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(space.0.as_bytes());
        hasher.update(req.0.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.as_bytes()[..16]);
        Self(Uuid::from_bytes(bytes))
    }

    #[inline]
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    #[inline]
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        *self.0.as_bytes()
    }

    #[inline]
    #[must_use]
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(b))
    }
}

impl fmt::Display for WriteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of id a handler needs to reserve before submit.
///
/// The writer hands one back; the handler stamps it into the phase;
/// the apply function uses it as-is. WAL recovery sees the same id
/// from the recorded phase — no re-allocation, no drift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdKind {
    Memory,
    Entity,
    Statement,
    Relation,
    /// A monotonically increasing per-shard slot number for the
    /// memory arena. Returned wrapped in [`AllocatedId::MemorySlot`].
    MemorySlot,
}

/// Result of `reserve_id`. One variant per [`IdKind`]; the handler
/// `match`es and stamps the typed id onto the phase.
///
/// Pre-allocation matters because:
/// 1. The wire ack often needs to return the id (`encode → memory_id`).
/// 2. Phases in the same write can reference each other by id.
/// 3. WAL recovery is replay-deterministic: the recorded phase carries
///    the id, so a re-apply produces the same row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocatedId {
    Memory(MemoryId),
    Entity(EntityId),
    Statement(StatementId),
    Relation(RelationId),
    MemorySlot(u64),
}

impl AllocatedId {
    /// `IdKind` discriminant for this id. Used by tests + tracing.
    #[must_use]
    pub fn kind(self) -> IdKind {
        match self {
            Self::Memory(_) => IdKind::Memory,
            Self::Entity(_) => IdKind::Entity,
            Self::Statement(_) => IdKind::Statement,
            Self::Relation(_) => IdKind::Relation,
            Self::MemorySlot(_) => IdKind::MemorySlot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_id_from_request_is_deterministic() {
        let req = RequestId(Uuid::now_v7());
        let space = SpaceId(Uuid::now_v7());
        assert_eq!(
            WriteId::from_request(req, space),
            WriteId::from_request(req, space),
            "same (request_id, space) must yield the same WriteId"
        );
    }

    #[test]
    fn write_id_from_request_scopes_by_space() {
        let req = RequestId(Uuid::now_v7());
        let space_a = SpaceId(Uuid::now_v7());
        let space_b = SpaceId(Uuid::now_v7());
        assert_ne!(
            WriteId::from_request(req, space_a),
            WriteId::from_request(req, space_b),
            "same request_id under different effective spaces must not collide"
        );
    }

    #[test]
    fn write_id_from_request_scopes_by_request() {
        let space = SpaceId(Uuid::now_v7());
        let req_a = RequestId(Uuid::now_v7());
        let req_b = RequestId(Uuid::now_v7());
        assert_ne!(
            WriteId::from_request(req_a, space),
            WriteId::from_request(req_b, space),
            "different request_ids under one space must not collide"
        );
    }

    #[test]
    fn allocated_id_kind_matches() {
        assert_eq!(
            AllocatedId::Memory(MemoryId::pack(0, 1, 0)).kind(),
            IdKind::Memory
        );
        assert_eq!(AllocatedId::Entity(EntityId::new()).kind(), IdKind::Entity);
        assert_eq!(
            AllocatedId::Statement(StatementId::new()).kind(),
            IdKind::Statement
        );
        assert_eq!(
            AllocatedId::Relation(RelationId::new()).kind(),
            IdKind::Relation
        );
        assert_eq!(AllocatedId::MemorySlot(42).kind(), IdKind::MemorySlot);
    }
}
