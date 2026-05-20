//! `Phase` — the irreducible mutation Brain's database supports.
//!
//! A [`Phase`] is the verb. Combine several into a [`super::Write`] and
//! the writer applies them all against one `WriteTransaction`. The
//! same enum is the WAL record body (encoded via rkyv in P3) so live
//! writes and crash recovery share one apply path.
//!
//! Design rules:
//! - **Pre-allocated ids**: every id field travels inside the phase.
//!   Apply functions never call `Uuid::now_v7()` or the slot allocator.
//! - **Timestamps inside**: `created_at` / `at` / `valid_from` come
//!   from the handler that built the phase. Apply functions don't
//!   read the clock.
//! - **No external IO inside apply**: no embedder calls, no HNSW knn,
//!   no LLM, no network. Strategies (the things that compute derived
//!   phases) do that ahead of submit; apply functions only mutate redb.

use brain_core::knowledge::{
    EntityAttributes, EntityId, EntityTypeId, EvidenceEntry, EvidenceOverflowId, ExtractorId,
    PredicateId, RelationId, RelationTypeId, StatementId, StatementKind, StatementObject,
    SubjectRef,
};
use brain_core::{ContextId, EdgeKindRef, MemoryId, MemoryKind, NodeRef, Salience};
use brain_embed::VECTOR_DIM;

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

/// One irreducible mutation. About 19 variants — the database's grammar.
///
/// The variants are NOT classified by layer. `Link` writes the same edge
/// table whether `from` and `to` are memories, entities, or statements;
/// `Tombstone` is polymorphic over its target via [`TombstoneTarget`];
/// `Supersede` is polymorphic via [`SupersedeTarget`]; `UpdateAttribute`
/// is polymorphic via [`AttributeTarget`]. The redb-row dispatch lives
/// inside the apply function, not in the phase enum.
///
/// Ownership: every field is owned (no borrows). A `Phase` crosses the
/// writer's queue, possibly the WAL, possibly recovery — borrowing would
/// pin the value to one task lifetime.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Insert or fully-replace a memory row. Carries the pre-allocated
    /// `MemoryId` (with shard prefix and slot version already packed),
    /// the encoded text, the embedding vector, and all per-memory
    /// metadata. Wire-level `Encode` produces one of these per request.
    UpsertMemory {
        id: MemoryId,
        text: String,
        vector: Box<[f32; VECTOR_DIM]>,
        kind: MemoryKind,
        salience: Salience,
        context: ContextId,
        created_at_unix_nanos: u64,
        /// Slot in the per-shard memory arena.
        arena_slot: u64,
        /// Stable fingerprint for dedupe. The handler computes this
        /// before submit (text-hash + agent + context).
        fingerprint: [u8; 16],
    },

    /// Insert or update an entity row.
    UpsertEntity {
        id: EntityId,
        ty: EntityTypeId,
        canonical: String,
        normalized: String,
        attributes: EntityAttributes,
        created_at_unix_nanos: u64,
    },

    /// Create a fresh statement row. Supersession of an existing
    /// statement uses [`Phase::Supersede`] + this phase together (the
    /// `Write` lists both, in order: supersede first, then upsert new).
    UpsertStatement {
        id: StatementId,
        kind: StatementKind,
        subject: SubjectRef,
        predicate: PredicateId,
        object: StatementObject,
        confidence: f32,
        evidence: EvidenceRefPhase,
        valid_from_unix_nanos: Option<u64>,
        extractor: ExtractorId,
        extracted_at_unix_nanos: u64,
        schema_version: u32,
    },

    /// Create or supersede a typed relation row.
    UpsertRelation {
        id: RelationId,
        ty: RelationTypeId,
        from: EntityId,
        to: EntityId,
        confidence: f32,
        evidence_memories: Vec<MemoryId>,
        is_symmetric: bool,
        extractor: ExtractorId,
        extracted_at_unix_nanos: u64,
    },

    /// Apply a schema upload — interns predicates / relation-types /
    /// entity-types, writes the schema row, flips the schema gate in
    /// the same txn.
    UpsertSchema {
        namespace: String,
        version: u32,
        /// Opaque rkyv-encoded schema blob (parsed validation already
        /// happened in the handler).
        blob: Vec<u8>,
        declared_predicates: Vec<String>,
        declared_relation_types: Vec<String>,
        declared_entity_types: Vec<String>,
        created_at_unix_nanos: u64,
    },

    /// Write one edge row (forward + auto-mirror for symmetric kinds).
    /// Used by both explicit `LINK` and every derivation strategy.
    Link {
        from: NodeRef,
        to: NodeRef,
        kind: EdgeKindRef,
        weight: f32,
        /// `EdgeOrigin` byte (0=EXPLICIT, 1=AUTO_DERIVED).
        origin: u8,
        /// `derived_by` byte (CLIENT/CONSOLIDATION/SIMILARITY/TEMPORAL/CAUSAL/...).
        derived_by: u8,
        /// Edge disambiguator (zero for builtin substrate kinds; non-zero
        /// when the same (from, kind, to) tuple can appear multiple
        /// times — e.g. typed relations).
        disambiguator: [u8; 16],
        created_at_unix_nanos: u64,
    },

    /// Remove one edge row (forward + auto-mirror).
    Unlink {
        from: NodeRef,
        to: NodeRef,
        kind: EdgeKindRef,
        disambiguator: [u8; 16],
    },

    /// Tombstone any row that supports tombstoning. Polymorphic over
    /// the target type — the apply function dispatches inside.
    Tombstone {
        target: TombstoneTarget,
        reason: u8,
        at_unix_nanos: u64,
    },

    /// Supersede a versioned row by another. Statements and relations
    /// support this; memories don't (memories use Tombstone + a new
    /// UpsertMemory to "replace").
    Supersede {
        target: SupersedeTarget,
        replacement: SupersedeReplacement,
        at_unix_nanos: u64,
    },

    /// Mutate a memory's salience. Used by AccessBoost and Decay
    /// strategies; also by the wire `UpdateSalience` op.
    UpdateSalience {
        id: MemoryId,
        new_salience: Salience,
    },

    /// Mutate a memory's kind classification.
    UpdateKind { id: MemoryId, new_kind: MemoryKind },

    /// Mutate a memory's context.
    UpdateContext {
        id: MemoryId,
        new_context: ContextId,
    },

    /// Replace a memory's embedding (used by `MigrateEmbeddings`).
    UpdateEmbedding {
        id: MemoryId,
        new_vector: Box<[f32; VECTOR_DIM]>,
    },

    /// Mutate a typed attribute on an existing row.
    UpdateAttribute {
        target: AttributeTarget,
        key: String,
        value: Vec<u8>,
    },

    /// Resolve a surface form to an entity inside the wtxn (read +
    /// write inside one txn). Used by the extractor pipeline before
    /// any statement / relation phase that references the entity.
    /// The result lands in the [`PhaseAck::Resolved`] for this phase
    /// so subsequent phases in the same Write can reference it.
    Resolve {
        surface: String,
        ty_qname: String,
        confidence: f32,
        context: ResolveContext,
    },

    /// Merge `source` into `target`. Aliases / attributes / inbound and
    /// outbound edges all rewrite. `source` ends up tombstoned in the
    /// same txn.
    MergeEntities {
        source: EntityId,
        target: EntityId,
        retain_aliases: bool,
        retain_attributes: bool,
        at_unix_nanos: u64,
    },

    /// Toggle an extractor's enabled flag.
    SetExtractorEnabled { id: ExtractorId, enabled: bool },

    /// Append an audit row (extractor pipeline outcome, schema
    /// migration, admin op). Body is opaque to the writer — typed
    /// by `kind`.
    StampAudit { kind: AuditKind, body: Vec<u8> },

    /// Free physical storage for the given memory slots. Triggered by
    /// the reclamation worker after grace period.
    ReclaimSlots { slots: Vec<u64> },
}

// ---------------------------------------------------------------------------
// Polymorphic targets — the "what does this verb act on" axis.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TombstoneTarget {
    Memory { id: MemoryId, mode: TombstoneMode },
    Entity(EntityId),
    Statement(StatementId),
    Relation(RelationId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TombstoneMode {
    /// Soft tombstone — row remains for the grace period, slot
    /// reclamation later zeroes the bytes.
    Soft,
    /// Hard tombstone — zero bytes immediately, no grace.
    Hard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupersedeTarget {
    Statement(StatementId),
    Relation(RelationId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupersedeReplacement {
    Statement(StatementId),
    Relation(RelationId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttributeTarget {
    Memory(MemoryId),
    Entity(EntityId),
    Statement(StatementId),
    Relation(RelationId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResolveContext {
    /// Within the substrate's entity registry, no namespace constraint.
    Global,
    /// Restrict to a specific namespace + version.
    Namespaced { namespace: String, version: u32 },
}

/// Audit kind discriminant. Matches the existing
/// `brain_metadata::tables::extractor_audit` byte values so persisted
/// rows don't shift on this refactor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AuditKind {
    ExtractorPipeline = 1,
    SchemaMigration = 2,
    AdminOp = 3,
}

/// Evidence carried inside a [`Phase::UpsertStatement`].
///
/// Inline form is used for the common case (≤8 backing memories). The
/// overflow form points at an existing row; the writer doesn't allocate
/// overflow rows on its own — the extractor (which knows when its
/// statement has many evidence entries) preallocates and references
/// the resulting id here.
#[derive(Clone, Debug, PartialEq)]
pub enum EvidenceRefPhase {
    Inline(Vec<EvidenceEntry>),
    Overflow(EvidenceOverflowId),
}

// ---------------------------------------------------------------------------
// EntityAttributesUpdate — the value carried by `UpdateAttribute` for
// entity rows. Kept as its own type because attribute updates are
// merge-vs-replace and the apply function needs to know which.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityAttributesUpdate {
    Replace,
    Merge,
}

// ---------------------------------------------------------------------------
// PhaseAck
// ---------------------------------------------------------------------------

/// Per-phase ack returned alongside the redb commit. Phases that
/// allocate ids inside the wtxn (`Resolve`) put the resolved id here;
/// phases that already carry their ids return a minimal marker.
#[derive(Clone, Debug, PartialEq)]
pub enum PhaseAck {
    UpsertedMemory(MemoryId),
    UpsertedEntity(EntityId),
    UpsertedStatement(StatementId, u32 /* version */),
    UpsertedRelation(RelationId, u32 /* version */),
    UpsertedSchema {
        namespace: String,
        version: u32,
    },
    Linked,
    Unlinked,
    Tombstoned(TombstoneTarget),
    Superseded(SupersedeTarget, SupersedeReplacement),
    SalienceUpdated,
    KindUpdated,
    ContextUpdated,
    EmbeddingUpdated,
    AttributeUpdated,
    Resolved {
        result_id: EntityId,
        tier: ResolveTier,
    },
    EntityMerged {
        source: EntityId,
        target: EntityId,
    },
    ExtractorEnabledSet {
        id: ExtractorId,
        enabled: bool,
    },
    AuditStamped(AuditKind),
    SlotsReclaimed {
        count: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveTier {
    Exact,
    Alias,
    Fuzzy,
    Created,
}

impl Phase {
    /// Lightweight tag, used for metric labels and tracing.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::UpsertMemory { .. } => "upsert_memory",
            Self::UpsertEntity { .. } => "upsert_entity",
            Self::UpsertStatement { .. } => "upsert_statement",
            Self::UpsertRelation { .. } => "upsert_relation",
            Self::UpsertSchema { .. } => "upsert_schema",
            Self::Link { .. } => "link",
            Self::Unlink { .. } => "unlink",
            Self::Tombstone { .. } => "tombstone",
            Self::Supersede { .. } => "supersede",
            Self::UpdateSalience { .. } => "update_salience",
            Self::UpdateKind { .. } => "update_kind",
            Self::UpdateContext { .. } => "update_context",
            Self::UpdateEmbedding { .. } => "update_embedding",
            Self::UpdateAttribute { .. } => "update_attribute",
            Self::Resolve { .. } => "resolve",
            Self::MergeEntities { .. } => "merge_entities",
            Self::SetExtractorEnabled { .. } => "set_extractor_enabled",
            Self::StampAudit { .. } => "stamp_audit",
            Self::ReclaimSlots { .. } => "reclaim_slots",
        }
    }

    /// Total bytes-of-heap-data estimate for backpressure decisions.
    /// Not exact; used to keep the writer's queue depth honest when
    /// a single Write carries a large payload (e.g. a big schema blob
    /// or a hot-text memory).
    #[must_use]
    pub fn approximate_byte_size(&self) -> usize {
        // Stack-resident enum tag + the dominant heap allocations.
        // Conservative — overestimates rather than under.
        use std::mem::size_of;
        let base = size_of::<Self>();
        let heap = match self {
            Self::UpsertMemory { text, vector, .. } => text.len() + vector.len() * size_of::<f32>(),
            Self::UpsertEntity {
                canonical,
                normalized,
                attributes,
                ..
            } => canonical.len() + normalized.len() + attributes.as_bytes().len(),
            Self::UpsertStatement { evidence, .. } => match evidence {
                EvidenceRefPhase::Inline(v) => v.len() * size_of::<EvidenceEntry>(),
                EvidenceRefPhase::Overflow(_) => 0,
            },
            Self::UpsertRelation {
                evidence_memories, ..
            } => evidence_memories.len() * size_of::<MemoryId>(),
            Self::UpsertSchema {
                namespace,
                blob,
                declared_predicates,
                declared_relation_types,
                declared_entity_types,
                ..
            } => {
                namespace.len()
                    + blob.len()
                    + declared_predicates.iter().map(String::len).sum::<usize>()
                    + declared_relation_types
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                    + declared_entity_types.iter().map(String::len).sum::<usize>()
            }
            Self::UpdateEmbedding { new_vector, .. } => new_vector.len() * size_of::<f32>(),
            Self::UpdateAttribute { key, value, .. } => key.len() + value.len(),
            Self::Resolve {
                surface, ty_qname, ..
            } => surface.len() + ty_qname.len(),
            Self::StampAudit { body, .. } => body.len(),
            Self::ReclaimSlots { slots } => slots.len() * size_of::<u64>(),
            _ => 0,
        };
        base + heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upsert_memory() -> Phase {
        Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: "hello".into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            context: ContextId(7),
            created_at_unix_nanos: 1_700_000_000_000,
            arena_slot: 42,
            fingerprint: [0xAA; 16],
        }
    }

    #[test]
    fn phase_tag_distinct_per_variant() {
        // Every variant constructed below maps to a unique tag string.
        // Adding a new Phase variant without updating Phase::tag is a
        // compile-time error (the match is exhaustive).
        let cases: Vec<(&'static str, Phase)> = vec![
            ("upsert_memory", sample_upsert_memory()),
            (
                "tombstone",
                Phase::Tombstone {
                    target: TombstoneTarget::Memory {
                        id: MemoryId::pack(0, 1, 0),
                        mode: TombstoneMode::Soft,
                    },
                    reason: 0,
                    at_unix_nanos: 0,
                },
            ),
            ("reclaim_slots", Phase::ReclaimSlots { slots: vec![1, 2] }),
            (
                "set_extractor_enabled",
                Phase::SetExtractorEnabled {
                    id: ExtractorId::from(7),
                    enabled: true,
                },
            ),
        ];
        for (expected_tag, phase) in cases {
            assert_eq!(phase.tag(), expected_tag);
        }
    }

    #[test]
    fn approximate_size_grows_with_text_len() {
        let make = |text: &str| Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: text.into(),
            vector: Box::new([0.0_f32; VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 0,
            arena_slot: 0,
            fingerprint: [0; 16],
        };
        let small = make("x");
        let big = make(&"x".repeat(1024));
        assert!(big.approximate_byte_size() > small.approximate_byte_size());
        // The delta is at least the extra text bytes.
        assert!(big.approximate_byte_size() - small.approximate_byte_size() >= 1023);
    }

    #[test]
    fn sample_phase_smoke() {
        let p = sample_upsert_memory();
        assert_eq!(p.tag(), "upsert_memory");
        // Bytes estimate is bounded — sanity that approximate_byte_size
        // returns a plausible value for a small memory.
        assert!(p.approximate_byte_size() < 32 * 1024);
    }

    #[test]
    fn tombstone_target_polymorphic() {
        // Every target variant compiles in.
        let _m = TombstoneTarget::Memory {
            id: MemoryId::pack(0, 1, 0),
            mode: TombstoneMode::Soft,
        };
        let _e = TombstoneTarget::Entity(EntityId::new());
        let _s = TombstoneTarget::Statement(StatementId::new());
        let _r = TombstoneTarget::Relation(RelationId::new());
    }
}
