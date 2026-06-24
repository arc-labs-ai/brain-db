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

use brain_core::{EntityId, MemoryId, RelationId, RequestId, StatementId};
use uuid::Uuid;

/// Idempotency key for a [`super::Write`]. Equality determines
/// "same write, retried". A WriteId NEVER mixes with `RequestId` in
/// storage so a retried wire request and the worker write it spawned
/// remain distinct cache entries (the writer derives one from the
/// other via [`WriteId::from_request`]).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WriteId(pub Uuid);

impl WriteId {
    /// Fresh UUIDv7 — time-ordered for sorted scans of the idempotency
    /// cache. Used by workers that submit derived writes.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Derive deterministically from a wire `RequestId`. The wire
    /// surface promises that retried requests carry the same
    /// `request_id`; the writer's idempotency cache uses the matching
    /// `WriteId` to short-circuit re-application.
    #[inline]
    #[must_use]
    pub fn from_request(req: RequestId) -> Self {
        Self(req.0)
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
    fn write_id_from_request_preserves_uuid() {
        let req = RequestId(Uuid::now_v7());
        let w = WriteId::from_request(req);
        assert_eq!(w.as_uuid(), req.0);
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
