//! [`TriggerEvent`] — what the writer publishes onto the per-shard
//! worker channel after a successful commit.
//!
//! Workers (Edge, Reclamation, Lifecycle, ...) drain triggers and
//! decide whether they have work to do. Each worker filters by
//! [`TriggerMask`]; matching strategies inside each worker do the
//! actual derivation.
//!
//! Triggers carry minimal data — typically just ids + timestamps +
//! anything cheap enough to copy and useful enough to avoid a redb
//! read in the worker's hot path. The HNSW vector travels inline on
//! `MemoryUpserted` because the SimilarTo strategy needs it for knn
//! and re-reading the vector from the arena across the writer/worker
//! boundary would require a separate read path.

use std::sync::Arc;

use brain_core::knowledge::{
    EntityId, ExtractorId, PredicateId, StatementId, StatementObject, SubjectRef,
};
use brain_core::{AgentId, ContextId, MemoryId};
use brain_embed::VECTOR_DIM;

/// One commit-time signal. Published by the writer post-commit, one
/// per phase that has post-commit consumers. The publisher does not
/// know which workers are listening; the channel fan-out is the
/// shard's responsibility.
#[derive(Clone, Debug)]
pub enum TriggerEvent {
    /// A memory was upserted (either freshly encoded or re-encoded via
    /// `MigrateEmbedding`). Drives the SimilarTo, FollowedBy, and
    /// extractor-pipeline derivations.
    MemoryUpserted {
        id: MemoryId,
        agent: AgentId,
        context: ContextId,
        created_at_unix_nanos: u64,
        /// Cloned-arced vector. The SimilarTo strategy reads it for
        /// HNSW knn without re-opening the arena.
        vector: Arc<[f32; VECTOR_DIM]>,
        /// `text` lives only when extraction is enabled — keeps the
        /// trigger small for substrate-only deployments. `None` means
        /// "fetch from redb if you need it" (extractor worker does).
        text: Option<Arc<str>>,
    },

    /// A new statement landed. Drives the causal-edge derivation when
    /// the statement's predicate is in the causal-edge strategy's
    /// whitelist.
    StatementUpserted {
        id: StatementId,
        predicate: PredicateId,
        subject: SubjectRef,
        object: StatementObject,
        confidence: f32,
        extractor: ExtractorId,
        agent: AgentId,
        at_unix_nanos: u64,
    },

    /// A memory was tombstoned. Drives forget-cascade strategy (clean
    /// up evidence-only statements, dangling edges, etc.).
    MemoryTombstoned {
        id: MemoryId,
        agent: AgentId,
        at_unix_nanos: u64,
    },

    /// A statement was superseded by a fresher one. Drives the
    /// stale-extraction detector and (in v2) supersession-aware
    /// edge retraction.
    StatementSuperseded {
        old: StatementId,
        new: StatementId,
        at_unix_nanos: u64,
    },

    /// Two entities merged. Drives downstream cleanup — duplicate-edge
    /// detection on the merged entity, etc.
    EntityMerged {
        source: EntityId,
        target: EntityId,
        at_unix_nanos: u64,
    },
}

impl TriggerEvent {
    /// Discriminant tag — used by [`TriggerMask`] for membership tests
    /// and by metric labels.
    #[inline]
    #[must_use]
    pub fn kind(&self) -> TriggerKind {
        match self {
            Self::MemoryUpserted { .. } => TriggerKind::MemoryUpserted,
            Self::StatementUpserted { .. } => TriggerKind::StatementUpserted,
            Self::MemoryTombstoned { .. } => TriggerKind::MemoryTombstoned,
            Self::StatementSuperseded { .. } => TriggerKind::StatementSuperseded,
            Self::EntityMerged { .. } => TriggerKind::EntityMerged,
        }
    }
}

/// Discriminant for [`TriggerEvent`]. Bit values fit in a `u8`; the
/// [`TriggerMask`] interpretation packs them into a single byte for
/// fast membership tests in the worker's drain loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum TriggerKind {
    MemoryUpserted = 1 << 0,
    StatementUpserted = 1 << 1,
    MemoryTombstoned = 1 << 2,
    StatementSuperseded = 1 << 3,
    EntityMerged = 1 << 4,
}

impl TriggerKind {
    #[inline]
    #[must_use]
    pub const fn bit(self) -> u8 {
        self as u8
    }
}

/// Bitmask of trigger kinds a strategy listens for. Constructed by
/// `or`-ing kinds together. The worker's drain loop tests
/// `mask.contains(trigger.kind())` to decide whether to invoke the
/// strategy — a one-byte AND.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TriggerMask(u8);

impl TriggerMask {
    /// Empty mask — strategy listens to nothing (useful for disabled
    /// strategies that still register).
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Mask containing just `kind`.
    #[inline]
    #[must_use]
    pub const fn only(kind: TriggerKind) -> Self {
        Self(kind.bit())
    }

    /// Add `kind` to the mask, returning a new mask.
    #[inline]
    #[must_use]
    pub const fn with(self, kind: TriggerKind) -> Self {
        Self(self.0 | kind.bit())
    }

    /// `true` if `kind` is in the mask.
    #[inline]
    #[must_use]
    pub const fn contains(self, kind: TriggerKind) -> bool {
        (self.0 & kind.bit()) != 0
    }

    /// `true` if the mask is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_membership() {
        let m = TriggerMask::only(TriggerKind::MemoryUpserted).with(TriggerKind::StatementUpserted);
        assert!(m.contains(TriggerKind::MemoryUpserted));
        assert!(m.contains(TriggerKind::StatementUpserted));
        assert!(!m.contains(TriggerKind::MemoryTombstoned));
        assert!(!m.contains(TriggerKind::EntityMerged));
    }

    #[test]
    fn empty_mask_contains_nothing() {
        let m = TriggerMask::empty();
        for k in [
            TriggerKind::MemoryUpserted,
            TriggerKind::StatementUpserted,
            TriggerKind::MemoryTombstoned,
            TriggerKind::StatementSuperseded,
            TriggerKind::EntityMerged,
        ] {
            assert!(!m.contains(k));
        }
        assert!(m.is_empty());
    }

    #[test]
    fn trigger_kind_bits_distinct() {
        let bits = [
            TriggerKind::MemoryUpserted.bit(),
            TriggerKind::StatementUpserted.bit(),
            TriggerKind::MemoryTombstoned.bit(),
            TriggerKind::StatementSuperseded.bit(),
            TriggerKind::EntityMerged.bit(),
        ];
        let mut sorted = bits;
        sorted.sort_unstable();
        sorted.iter().zip(sorted.iter().skip(1)).for_each(|(a, b)| {
            assert_ne!(a, b, "bit values must be distinct");
        });
    }
}
