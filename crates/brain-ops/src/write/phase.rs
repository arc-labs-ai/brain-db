//! `Phase` — the irreducible mutation Brain's database supports.
//!
//! A [`Phase`] is the verb. Combine several into a [`super::Write`] and
//! the writer applies them all against one `WriteTransaction`. The
//! same enum is the WAL record body (encoded via rkyv) so live
//! writes and crash recovery share one apply path. (The typed-graph
//! `PhaseBody` opaque body is the exception: it is CBOR-encoded.)
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

use brain_core::{ContextId, EdgeKindRef, MemoryId, MemoryKind, NodeRef, Salience};
use brain_core::{
    Entity, EntityAttributes, EntityId, EntityTypeId, EvidenceEntry, EvidenceOverflowId,
    ExtractorId, MergeId, PredicateId, Relation, RelationId, RelationTypeId, Statement,
    StatementId, StatementKind, StatementObject, SubjectRef,
};
use brain_embed::VECTOR_DIM;

// ---------------------------------------------------------------------------
// Phase
// ---------------------------------------------------------------------------

/// One irreducible mutation — the database's grammar.
///
/// The variants are NOT classified by layer. `Link` writes the same edge
/// table whether `from` and `to` are memories, entities, or statements;
/// `Tombstone` is polymorphic over its target via [`TombstoneTarget`];
/// `Supersede` is polymorphic via [`SupersedeTarget`]. The redb-row
/// dispatch lives inside the apply function, not in the phase enum.
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
        /// Client-supplied event time (when the content actually
        /// happened), distinct from `created_at_unix_nanos` (server write
        /// time). `None` when the client didn't supply one.
        occurred_at_unix_nanos: Option<u64>,
        /// Slot in the per-shard memory arena.
        arena_slot: u64,
        /// Embedding-model fingerprint stamped on the stored row. The
        /// handler pulls this from the dispatcher that produced the
        /// vector.
        embedding_model_fp: [u8; 16],
        /// BLAKE3 of the canonical UTF-8 text. `Some` only when
        /// `deduplicate=true` — controls whether a row gets stamped
        /// into FINGERPRINTS_TABLE for content-hash dedup.
        content_hash: Option<[u8; 32]>,
        /// `true` ⇒ consult FINGERPRINTS_TABLE before write and
        /// re-use any matching MemoryId. `false`
        /// skips the dedup index entirely.
        deduplicate: bool,
    },

    /// Insert or update an entity row.
    UpsertEntity {
        id: EntityId,
        ty: EntityTypeId,
        canonical: String,
        normalized: String,
        /// Alternate surface forms for entity resolution. Empty
        /// vector = no aliases (the common case for fresh creates;
        /// rename/merge may add).
        aliases: Vec<String>,
        attributes: EntityAttributes,
        created_at_unix_nanos: u64,
    },

    /// Create a fresh statement row. Supersession of an existing
    /// statement uses [`Phase::Supersede`] + this phase together (the
    /// `Write` lists both, in order: supersede first, then upsert new).
    ///
    /// `predicate` is authoritative when `predicate_intern_hint` is
    /// `None`. When the hint is `Some`, the handler is in the schemaless
    /// path and didn't pre-intern the predicate; apply runs
    /// `predicate_intern_or_get` inside the same wtxn (folding what used
    /// to be a separate fsync-amplifying micro-commit). The hint also
    /// triggers stamping `IMPLICIT_PREDICATE` on the resulting row.
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
        /// Schemaless-path intern hint. `Some((namespace, name))` means
        /// "apply: resolve the predicate inside this wtxn and stamp the
        /// row IMPLICIT_PREDICATE." `None` is the strict / already-
        /// interned path — apply uses `predicate` as-is.
        predicate_intern_hint: Option<(String, String)>,
    },

    /// Create or supersede a typed relation row.
    ///
    /// `ty` is authoritative when `relation_type_intern_hint` is `None`.
    /// When the hint is `Some`, the handler is in the schemaless path
    /// and didn't pre-intern the relation type; apply runs
    /// `relation_type_intern_or_get` inside the same wtxn (folding what
    /// used to be a separate fsync-amplifying micro-commit).
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
        /// Opaque rkyv-encoded relation properties; `Vec::new()` for
        /// extractor-derived relations and any caller that didn't
        /// supply properties.
        properties_blob: Vec<u8>,
        /// Validity window start (inclusive). `None` = no lower bound.
        valid_from_unix_nanos: Option<u64>,
        /// Validity window end (exclusive). `None` = open-ended.
        valid_to_unix_nanos: Option<u64>,
        /// Schemaless-path intern hint. `Some((namespace, name))` means
        /// "apply: resolve the relation_type inside this wtxn." `None`
        /// is the strict / already-interned path — apply uses `ty`
        /// as-is.
        relation_type_intern_hint: Option<(String, String)>,
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

    /// Full-row replace of an existing entity. Carries the post-edit
    /// state; apply re-reads the current row to preserve immutable
    /// fields (id, entity_type, created_at) and applies the replacement
    /// under the wtxn.
    UpdateEntity {
        id: EntityId,
        canonical_name: String,
        aliases: Vec<String>,
        attributes_blob: Vec<u8>,
        at_unix_nanos: u64,
    },

    /// Rename — atomic canonical_name swap with the old name moved into
    /// aliases. Apply delegates to brain_metadata's entity_rename, which
    /// always applies the alias-trail policy.
    RenameEntity {
        id: EntityId,
        new_canonical_name: String,
        at_unix_nanos: u64,
    },

    /// Reverse a prior merge — restores `merged` as an independent
    /// entity. Apply returns the survivor the merged entity was
    /// originally merged into.
    UnmergeEntities {
        merged: EntityId,
        actor: brain_metadata::entity::merge::MergeActor,
        at_unix_nanos: u64,
    },

    /// Merge `source` into `target`. Aliases / attributes / inbound and
    /// outbound edges all rewrite. `source` ends up tombstoned in the
    /// same txn.
    ///
    /// `retain_aliases` / `retain_attributes` are reserved for future
    /// merge-policy tuning; the v1 metadata helper always merges both.
    MergeEntities {
        source: EntityId,
        target: EntityId,
        retain_aliases: bool,
        retain_attributes: bool,
        at_unix_nanos: u64,
        /// Operator-supplied confidence (≥ 0.6).
        confidence: f32,
        /// Free-form reason for the audit row.
        reason: String,
        /// Who initiated the merge — typically the caller agent's
        /// id bytes, or a system identifier.
        actor: brain_metadata::entity::merge::MergeActor,
        /// Grace window before `source` is reclaimed.
        grace_seconds: u64,
    },

    /// Approve a Pending merge proposal sitting on the
    /// `merge_review_queue`. Apply executes the underlying
    /// `merge_entity(source → candidate)` and stamps the proposal row
    /// `Approved`. The operator's `actor` flows into the `MergeRecord`
    /// audit so a downstream unmerge can trace it back.
    ApproveMerge {
        proposal_id: MergeId,
        actor: brain_metadata::entity::merge::MergeActor,
        grace_seconds: u64,
        at_unix_nanos: u64,
    },

    /// Reject a Pending merge proposal. Apply stamps the proposal row
    /// `Rejected` without invoking `merge_entity` — the source and
    /// candidate entities stay independent.
    RejectMerge {
        proposal_id: MergeId,
        at_unix_nanos: u64,
    },

    /// Toggle an extractor's enabled flag.
    SetExtractorEnabled { id: ExtractorId, enabled: bool },

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

/// The replacement value carried inside [`Phase::Supersede`].
///
/// Carries the FULL value (boxed to keep the Phase enum size in check)
/// because the brain-metadata `statement_supersede` / `relation_supersede`
/// helpers take a `&Statement` / `&Relation` — they insert the new row
/// and flip the chain in one txn. Carrying just the id would force the
/// apply function to re-load the new row out of the wtxn, which is
/// awkward when supersession happens in the same write that inserts it.
#[derive(Clone, Debug, PartialEq)]
pub enum SupersedeReplacement {
    Statement(Box<Statement>),
    Relation(Box<Relation>),
}

impl SupersedeReplacement {
    /// Lightweight id reference — what the [`PhaseAck::Superseded`]
    /// carries back to the caller.
    #[must_use]
    pub fn id(&self) -> SupersedeReplacementId {
        match self {
            Self::Statement(s) => SupersedeReplacementId::Statement(s.id),
            Self::Relation(r) => SupersedeReplacementId::Relation(r.id),
        }
    }
}

/// Lightweight id form of [`SupersedeReplacement`] — used in
/// [`PhaseAck::Superseded`] so the ack stays cheap to clone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupersedeReplacementId {
    Statement(StatementId),
    Relation(RelationId),
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
    /// A row was tombstoned. Carries the post-commit timestamp the apply
    /// stamped onto the row so handlers can echo it in the wire response
    /// without a post-commit re-read (and so idempotency replays surface
    /// the original timestamp rather than today's clock — the cached ack
    /// is what gets returned on replay).
    Tombstoned {
        target: TombstoneTarget,
        tombstoned_at_unix_nanos: u64,
    },
    Superseded(SupersedeTarget, SupersedeReplacementId),
    SalienceUpdated,
    KindUpdated,
    ContextUpdated,
    EmbeddingUpdated,
    EntityUpdated {
        id: EntityId,
        /// Snapshotted post-commit entity row so the handler can return
        /// the persisted view without a re-read RPC.
        snapshot: Box<Entity>,
    },
    EntityRenamed {
        id: EntityId,
        old_canonical_name: String,
        snapshot: Box<Entity>,
    },
    EntitiesUnmerged {
        restored: EntityId,
        /// The survivor the merged entity was originally merged into.
        survivor: EntityId,
    },
    EntityMerged {
        source: EntityId,
        target: EntityId,
        /// Audit row id minted inside `merge_entity` — surfaced so the
        /// wire response can include it without a post-commit lookup
        /// (the audit log is keyed by `(timestamp, merge_id)`, not by
        /// survivor/merged, so reverse lookup is awkward).
        audit_id: MergeId,
    },
    /// A merge proposal was promoted: the underlying merge was applied
    /// and the proposal row stamped Approved (or AutoApplied for the
    /// worker path — the apply function takes the status as input).
    MergeProposalApproved {
        proposal_id: MergeId,
        /// Audit row id minted by the inner `merge_entity` call.
        audit_id: MergeId,
    },
    /// A merge proposal was rejected without merging.
    MergeProposalRejected {
        proposal_id: MergeId,
    },
    ExtractorEnabledSet {
        id: ExtractorId,
        enabled: bool,
    },
    SlotsReclaimed {
        count: usize,
    },
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
            Self::UpdateEntity { .. } => "update_entity",
            Self::RenameEntity { .. } => "rename_entity",
            Self::UnmergeEntities { .. } => "unmerge_entities",
            Self::MergeEntities { .. } => "merge_entities",
            Self::ApproveMerge { .. } => "approve_merge",
            Self::RejectMerge { .. } => "reject_merge",
            Self::SetExtractorEnabled { .. } => "set_extractor_enabled",
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
            Self::UpdateEntity {
                canonical_name,
                aliases,
                attributes_blob,
                ..
            } => {
                canonical_name.len()
                    + aliases.iter().map(String::len).sum::<usize>()
                    + attributes_blob.len()
            }
            Self::RenameEntity {
                new_canonical_name, ..
            } => new_canonical_name.len(),
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
            occurred_at_unix_nanos: None,
            arena_slot: 42,
            embedding_model_fp: [0xAA; 16],
            content_hash: None,
            deduplicate: false,
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
            occurred_at_unix_nanos: None,
            arena_slot: 0,
            embedding_model_fp: [0; 16],
            content_hash: None,
            deduplicate: false,
        };
        let small = make("x");
        let big = make(&"x".repeat(1024));
        assert!(big.approximate_byte_size() > small.approximate_byte_size());
        // The delta is at least the extra text bytes.
        assert!(big.approximate_byte_size() - small.approximate_byte_size() >= 1023);
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
