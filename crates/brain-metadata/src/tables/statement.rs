//! Statement family — 8 tables.
//!
//! - [`STATEMENTS_TABLE`]                  — primary `StatementId → StatementMetadata`.
//! - [`STATEMENTS_BY_SUBJECT_TABLE`]       — subject-anchored secondary.
//! - [`STATEMENTS_BY_PREDICATE_TABLE`]     — predicate-anchored secondary.
//! - [`STATEMENTS_BY_OBJECT_ENTITY_TABLE`] — object-side reverse index.
//! - [`STATEMENTS_BY_EVENT_TIME_TABLE`]    — time-range Event queries.
//! - [`STATEMENTS_BY_EVIDENCE_TABLE`]      — reverse: which statements derive from memory M.
//! - [`STATEMENT_CHAIN_TABLE`]             — supersession-chain traversal.
//! - [`EVIDENCE_OVERFLOW_TABLE`]           — long evidence lists that don't fit inline.
//!
//! `StatementMetadata.evidence_inline` is a parallel structure
//! carrying confidence + timestamp + extractor, and the typed
//! `StatementObject` encoding is done via a private rkyv shim.

use crate::impl_redb_rkyv_value;
use brain_core::{
    EntityId, EvidenceOverflowId, ExtractorId, MemoryId, PredicateId, StatementId, StatementKind,
};
use brain_core::{
    EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
    INLINE_EVIDENCE_CAP,
};
use redb::TableDefinition;
use smallvec::SmallVec;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const STATEMENTS_TABLE: TableDefinition<'static, [u8; 16], StatementMetadata> =
    TableDefinition::new("statements");

/// `(EntityId, kind, predicate_id, is_current, statement_id)` →
/// `StatementId.to_bytes()`.
///
/// Multi-value index: the statement id is appended to the key so every
/// statement is its own row. Two Set-valued statements sharing
/// `(subject, kind, predicate_id, is_current)` — e.g. two `likes` for
/// one person — therefore each get a distinct index entry instead of
/// colliding on a single key (last-writer-wins) and losing all but one.
/// The value still holds the statement id so range scans read it as
/// before without parsing the key tuple.
#[allow(clippy::type_complexity)] // a redb composite-key tuple, not worth a type alias
pub const STATEMENTS_BY_SUBJECT_TABLE: TableDefinition<
    'static,
    ([u8; 16], u8, u32, u8, [u8; 16]),
    [u8; 16],
> = TableDefinition::new("statements_by_subject");

/// `(predicate_id, kind, confidence_bucket)` → `StatementId.to_bytes()`.
/// `confidence_bucket` is `floor(confidence * 10)` clamped to `0..=10`.
pub const STATEMENTS_BY_PREDICATE_TABLE: TableDefinition<'static, (u32, u8, u8), [u8; 16]> =
    TableDefinition::new("statements_by_predicate");

/// `(EntityId, kind)` → `StatementId.to_bytes()`. Walk this when
/// answering "what statements have X as object?".
pub const STATEMENTS_BY_OBJECT_ENTITY_TABLE: TableDefinition<'static, ([u8; 16], u8), [u8; 16]> =
    TableDefinition::new("statements_by_object_entity");

/// `(event_at_unix_nanos, subject_entity_bytes)` → `StatementId.to_bytes()`.
/// Time-range queries scan a prefix; the EntityId disambiguates same-time
/// events for the same subject.
pub const STATEMENTS_BY_EVENT_TIME_TABLE: TableDefinition<'static, (u64, [u8; 16]), [u8; 16]> =
    TableDefinition::new("statements_by_event_time");

/// `(MemoryId, StatementId)` → `()`. Reverse index for FORGET cascade.
pub const STATEMENTS_BY_EVIDENCE_TABLE: TableDefinition<'static, ([u8; 16], [u8; 16]), ()> =
    TableDefinition::new("statements_by_evidence");

/// `(chain_root, version)` → `StatementId.to_bytes()`. Walk this to
/// reconstruct the supersession chain of a statement.
pub const STATEMENT_CHAIN_TABLE: TableDefinition<'static, ([u8; 16], u32), [u8; 16]> =
    TableDefinition::new("statement_chain");

pub const EVIDENCE_OVERFLOW_TABLE: TableDefinition<'static, [u8; 16], EvidenceOverflow> =
    TableDefinition::new("evidence_overflow");

/// Queue of statement ids awaiting Statement-HNSW embedding.
///
/// Populated by `insert_new_statement` (statement create + supersede paths)
/// and drained by the per-shard `StatementEmbedWorker`. Tombstone removes
/// the row so a forget cascade doesn't pull a doomed statement into the
/// HNSW. The value is the enqueue timestamp in unix nanos — used only for
/// observability (worker logs "oldest pending" age), not for ordering.
///
/// A redb table rather than an in-memory channel for two reasons:
/// crash-safe — a shard that restarts after extractor commit but before
/// the worker drains still has the queue rows; and naturally idempotent —
/// re-running the worker on the same row does not double-insert because
/// the worker removes the row only after a successful HNSW write.
pub const STATEMENT_EMBED_QUEUE_TABLE: TableDefinition<'static, [u8; 16], u64> =
    TableDefinition::new("statement_embed_queue");

// ---------------------------------------------------------------------------
// Tombstone-reason discriminant.
// ---------------------------------------------------------------------------

/// `StatementMetadata::tombstone_reason` byte values.
pub mod tombstone_reason {
    pub const NOT_TOMBSTONED: u8 = 0;
    pub const SOURCE_MEMORY_FORGOTTEN: u8 = 1;
    pub const USER_REQUEST: u8 = 2;
    pub const SCHEMA_INVALIDATION: u8 = 3;
    pub const EXTRACTOR_RETRACTION: u8 = 4;
    /// Hard-delete intent (`STATEMENT_RETRACT` / `FORGET_STATEMENT`).
    /// The reclamation GC worker selects only rows carrying this byte
    /// so plain tombstones and superseded rows stay put for audit.
    pub const RETRACT: u8 = 5;
}

// ---------------------------------------------------------------------------
// EvidenceEntryRow — rkyv-archived row form of brain-core `EvidenceEntry`.
// ---------------------------------------------------------------------------

/// Per-evidence row stored inside `StatementMetadata.evidence_inline`.
/// Mirrors `brain_core::EvidenceEntry`; uses `confidence_milli`
/// (u16) so the rkyv-archived shape is fixed-width and cache-friendly.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EvidenceEntryRow {
    pub memory_id_bytes: [u8; 16],
    pub confidence_milli: u16,
    pub timestamp_unix_nanos: u64,
    pub extractor_id: u32,
}

impl EvidenceEntryRow {
    #[must_use]
    pub fn from_entry(e: &EvidenceEntry) -> Self {
        Self {
            memory_id_bytes: e.memory_id.to_be_bytes(),
            confidence_milli: e.confidence_milli,
            timestamp_unix_nanos: e.timestamp_unix_nanos,
            extractor_id: e.extractor_id.raw(),
        }
    }

    #[must_use]
    pub fn to_entry(&self) -> EvidenceEntry {
        EvidenceEntry {
            memory_id: MemoryId::from_be_bytes(self.memory_id_bytes),
            confidence_milli: self.confidence_milli,
            timestamp_unix_nanos: self.timestamp_unix_nanos,
            extractor_id: ExtractorId::from(self.extractor_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Object encoding shim — keeps brain-core rkyv-free.
// ---------------------------------------------------------------------------

/// Private rkyv shim for `brain_core::StatementValue`.
///
/// One variant byte + one populated payload field; the rest are zero/
/// empty. Stable byte layout so readers can skim past the payload
/// without a full deserialize when only the discriminant matters.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
struct StatementValueBlob {
    /// `1=Text / 2=Integer / 3=Float / 4=Bool / 5=UnixNanos / 6=Blob`.
    discriminant: u8,
    text: String,
    integer: i64,
    float: f64,
    boolean: u8,
    unix_nanos: u64,
    blob: Vec<u8>,
}

impl StatementValueBlob {
    fn from_value(v: &StatementValue) -> Self {
        let mut out = Self {
            discriminant: 0,
            text: String::new(),
            integer: 0,
            float: 0.0,
            boolean: 0,
            unix_nanos: 0,
            blob: Vec::new(),
        };
        match v {
            StatementValue::Text(s) => {
                out.discriminant = 1;
                out.text = s.clone();
            }
            StatementValue::Integer(n) => {
                out.discriminant = 2;
                out.integer = *n;
            }
            StatementValue::Float(f) => {
                out.discriminant = 3;
                out.float = *f;
            }
            StatementValue::Bool(b) => {
                out.discriminant = 4;
                out.boolean = u8::from(*b);
            }
            StatementValue::UnixNanos(n) => {
                out.discriminant = 5;
                out.unix_nanos = *n;
            }
            StatementValue::Blob(b) => {
                out.discriminant = 6;
                out.blob = b.clone();
            }
        }
        out
    }

    fn to_value(&self) -> Option<StatementValue> {
        Some(match self.discriminant {
            1 => StatementValue::Text(self.text.clone()),
            2 => StatementValue::Integer(self.integer),
            3 => StatementValue::Float(self.float),
            4 => StatementValue::Bool(self.boolean != 0),
            5 => StatementValue::UnixNanos(self.unix_nanos),
            6 => StatementValue::Blob(self.blob.clone()),
            _ => return None,
        })
    }
}

/// Private rkyv shim for `brain_core::StatementObject`.
///
/// `discriminant`:
/// - `1` = `Entity(EntityId)` — payload in `entity_bytes`.
/// - `2` = `Value(StatementValue)` — payload in `value`.
/// - `3` = `Memory(MemoryId)` — payload in `memory_bytes`.
/// - `4` = `Statement(StatementId)` — payload in `statement_bytes`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
struct StatementObjectBlob {
    discriminant: u8,
    entity_bytes: [u8; 16],
    value: StatementValueBlob,
    memory_bytes: [u8; 16],
    statement_bytes: [u8; 16],
}

impl StatementObjectBlob {
    fn from_object(o: &StatementObject) -> Self {
        let mut out = Self {
            discriminant: 0,
            entity_bytes: [0u8; 16],
            value: StatementValueBlob::from_value(&StatementValue::Bool(false)),
            memory_bytes: [0u8; 16],
            statement_bytes: [0u8; 16],
        };
        // The `value` field defaults to a Bool(false) shim — the
        // discriminant gates whether it's meaningful on the read
        // side. This keeps the struct fixed-shape regardless of
        // variant.
        out.value.discriminant = 0;
        out.value.text.clear();
        out.value.integer = 0;
        out.value.float = 0.0;
        out.value.boolean = 0;
        out.value.unix_nanos = 0;
        out.value.blob.clear();
        match o {
            StatementObject::Entity(id) => {
                out.discriminant = 1;
                out.entity_bytes = id.to_bytes();
            }
            StatementObject::Value(v) => {
                out.discriminant = 2;
                out.value = StatementValueBlob::from_value(v);
            }
            StatementObject::Memory(m) => {
                out.discriminant = 3;
                out.memory_bytes = m.to_be_bytes();
            }
            StatementObject::Statement(s) => {
                out.discriminant = 4;
                out.statement_bytes = s.to_bytes();
            }
        }
        out
    }

    fn to_object(&self) -> Option<StatementObject> {
        Some(match self.discriminant {
            1 => StatementObject::Entity(EntityId::from_bytes(self.entity_bytes)),
            2 => StatementObject::Value(self.value.to_value()?),
            3 => StatementObject::Memory(MemoryId::from_be_bytes(self.memory_bytes)),
            4 => StatementObject::Statement(StatementId::from_bytes(self.statement_bytes)),
            _ => return None,
        })
    }
}

/// Encode a `StatementObject` to bytes for `StatementMetadata.object_blob`.
#[must_use]
pub fn encode_object(o: &StatementObject) -> Vec<u8> {
    let blob = StatementObjectBlob::from_object(o);
    rkyv::to_bytes::<_, 256>(&blob)
        .expect("StatementObjectBlob is rkyv-serializable")
        .into_vec()
}

/// Decode a `StatementObject` from `StatementMetadata.object_blob`.
/// Returns `None` if the bytes fail validation or the discriminant is
/// out of range — caller surfaces as `Storage` corruption.
#[must_use]
pub fn decode_object(bytes: &[u8]) -> Option<StatementObject> {
    let mut aligned = rkyv::AlignedVec::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    let blob: StatementObjectBlob = rkyv::from_bytes::<StatementObjectBlob>(&aligned).ok()?;
    blob.to_object()
}

/// Map a `[0, 1]` confidence float to the 11-bucket coarse quantisation
/// used by `STATEMENTS_BY_PREDICATE_TABLE`.
#[must_use]
pub fn confidence_bucket(c: f32) -> u8 {
    let scaled = (c.clamp(0.0, 1.0) * 10.0).floor() as i32;
    scaled.clamp(0, 10) as u8
}

// ---------------------------------------------------------------------------
// Value structs.
// ---------------------------------------------------------------------------

/// Primary statement record. Carries every schema field in
/// rkyv-archived form.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct StatementMetadata {
    pub statement_id_bytes: [u8; 16],
    pub chain_root_bytes: [u8; 16],
    pub version: u32,
    /// Fact=0 / Preference=1 / Event=2 per `brain_core::StatementKind`.
    pub kind: u8,
    pub subject_entity_bytes: [u8; 16],
    /// Subject kind: `0` = `SubjectRef::Entity`, `1` = `SubjectRef::Pending`
    /// (in which case `subject_entity_bytes` holds the pending audit id),
    /// `2` = `SubjectRef::Memory` (the bytes hold the source memory id).
    /// rkyv is positional, so this repurposes the former
    /// `subject_is_pending` slot in place. Readers that asked
    /// `subject_is_pending == 0` ("is an entity subject") stay correct as
    /// `subject_kind == 0`.
    pub subject_kind: u8,
    pub predicate_id: u32,
    /// rkyv-encoded `StatementObject` (via [`encode_object`]).
    pub object_blob: Vec<u8>,
    pub object_discriminant: u8,
    pub confidence: f32,
    pub extractor_id: u32,
    pub schema_version: u32,
    pub extracted_at_unix_nanos: u64,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    /// Required for Event kind; `None` otherwise.
    pub event_at_unix_nanos: Option<u64>,
    pub superseded_by_bytes: Option<[u8; 16]>,
    pub supersedes_bytes: Option<[u8; 16]>,
    /// Inline evidence list. Bounded length (`INLINE_EVIDENCE_CAP = 8`
    /// ); overflow spills into `evidence_overflow`.
    pub evidence_inline: Vec<EvidenceEntryRow>,
    pub evidence_overflow_id_bytes: Option<[u8; 16]>,
    pub tombstoned: u8,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub tombstone_reason: u8,
    /// Record-time invalidation. `Some(t)` means the substrate stopped
    /// believing the row at unix-nanos `t` (supersession / tombstone /
    /// FORGET cascade). `None` for rows the substrate still believes —
    /// it is the record-time analogue of `valid_to_unix_nanos`, which
    /// tracks object-time.
    pub record_invalidated_at_unix_nanos: Option<u64>,
    pub is_current: u8,
    /// Bit flags. Bits in use:
    /// - bit 0: row was authored from an open-vocabulary write
    ///   (predicate origin is `ImplicitFromWrite`).
    /// - bit 1: `OUTSIDE_ACTIVE_SCHEMA` — set lazily by SCHEMA_UPLOAD
    ///   when the row's predicate is not in the new active schema
    ///   version. Allows readers to surface "pre-schema" data while
    ///   schema-strict queries can opt to filter it out.
    pub flags: u32,
    /// `1` if the row is stateful (per-statement signal), `0` otherwise.
    /// Copied from `PredicateDefinition.is_stateful` at write time.
    pub is_stateful: u8,
}

/// Bit flags written to [`StatementMetadata::flags`].
pub mod statement_flags {
    /// The statement was created against a predicate that was interned
    /// implicitly (schemaless write path). Distinct from
    /// `OUTSIDE_ACTIVE_SCHEMA` because a later SCHEMA_UPLOAD might
    /// adopt the predicate, in which case `OUTSIDE_ACTIVE_SCHEMA` is
    /// cleared but `IMPLICIT_PREDICATE` remains as historical signal.
    pub const IMPLICIT_PREDICATE: u32 = 1 << 0;
    /// The statement's predicate is not present in the namespace's
    /// active schema version. Set on SCHEMA_UPLOAD for pre-existing
    /// rows whose predicate is missing from the new schema, and on
    /// open-vocabulary STATEMENT_CREATE when the predicate gets
    /// interned but a schema is already active in some other
    /// namespace. Readers must keep returning these rows; queries
    /// running in strict mode use the flag to decide whether to
    /// drop them.
    pub const OUTSIDE_ACTIVE_SCHEMA: u32 = 1 << 1;
}

impl StatementMetadata {
    #[must_use]
    pub fn statement_id(&self) -> StatementId {
        StatementId::from(self.statement_id_bytes)
    }

    #[must_use]
    pub fn chain_root(&self) -> StatementId {
        StatementId::from(self.chain_root_bytes)
    }

    pub fn kind(&self) -> Option<StatementKind> {
        // Every byte decodes to a valid kind now (builtin 0..=5, else
        // Custom). The `Option` is retained so existing callers keep their
        // `?` / `ok_or` ergonomics; it is always `Some`.
        Some(StatementKind::from_u8(self.kind))
    }

    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        self.tombstoned != 0
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.is_current != 0
    }
}

impl StatementMetadata {
    /// Convenience: read the named [`statement_flags`] bit.
    #[must_use]
    pub fn has_flag(&self, bit: u32) -> bool {
        self.flags & bit != 0
    }

    /// Set the named bit, returning whether the flag word changed.
    pub fn set_flag(&mut self, bit: u32) -> bool {
        let old = self.flags;
        self.flags |= bit;
        old != self.flags
    }

    /// Clear the named bit, returning whether the flag word changed.
    pub fn clear_flag(&mut self, bit: u32) -> bool {
        let old = self.flags;
        self.flags &= !bit;
        old != self.flags
    }
}

impl_redb_rkyv_value!(StatementMetadata, "brain_metadata::StatementMetadata");

/// Overflow row for statements whose inline evidence list outgrew the
/// `INLINE_EVIDENCE_CAP = 8` inline budget. Four parallel vectors per
/// — one entry across all = one `EvidenceEntry`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EvidenceOverflow {
    pub overflow_id_bytes: [u8; 16],
    pub memory_ids: Vec<[u8; 16]>,
    pub extractor_ids: Vec<u32>,
    pub confidences_milli: Vec<u16>,
    pub timestamps_unix_nanos: Vec<u64>,
    pub created_at_unix_nanos: u64,
}

impl EvidenceOverflow {
    /// Build an overflow row from a slice of brain-core
    /// `EvidenceEntry` values. The four parallel vectors line up:
    /// `memory_ids[i]`, `confidences_milli[i]`, `timestamps[i]`,
    /// `extractor_ids[i]` together describe entry `i`.
    #[must_use]
    pub fn from_entries(
        overflow_id: EvidenceOverflowId,
        entries: &[EvidenceEntry],
        created_at_unix_nanos: u64,
    ) -> Self {
        let n = entries.len();
        let mut memory_ids = Vec::with_capacity(n);
        let mut extractor_ids = Vec::with_capacity(n);
        let mut confidences_milli = Vec::with_capacity(n);
        let mut timestamps_unix_nanos = Vec::with_capacity(n);
        for e in entries {
            memory_ids.push(e.memory_id.to_be_bytes());
            extractor_ids.push(e.extractor_id.raw());
            confidences_milli.push(e.confidence_milli);
            timestamps_unix_nanos.push(e.timestamp_unix_nanos);
        }
        Self {
            overflow_id_bytes: overflow_id.to_bytes(),
            memory_ids,
            extractor_ids,
            confidences_milli,
            timestamps_unix_nanos,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn overflow_id(&self) -> EvidenceOverflowId {
        EvidenceOverflowId::from(self.overflow_id_bytes)
    }

    /// Project the four parallel vectors back into a `Vec<EvidenceEntry>`.
    /// Truncates to the shortest vector length defensively — corrupt
    /// rows should never reach here (rkyv `check_bytes` validates).
    #[must_use]
    pub fn to_entries(&self) -> Vec<EvidenceEntry> {
        let n = self
            .memory_ids
            .len()
            .min(self.extractor_ids.len())
            .min(self.confidences_milli.len())
            .min(self.timestamps_unix_nanos.len());
        (0..n)
            .map(|i| EvidenceEntry {
                memory_id: MemoryId::from_be_bytes(self.memory_ids[i]),
                confidence_milli: self.confidences_milli[i],
                timestamp_unix_nanos: self.timestamps_unix_nanos[i],
                extractor_id: ExtractorId::from(self.extractor_ids[i]),
            })
            .collect()
    }
}

impl_redb_rkyv_value!(EvidenceOverflow, "brain_metadata::EvidenceOverflow");

// ---------------------------------------------------------------------------
// Projections — Statement (brain-core) ↔ StatementMetadata (rkyv row).
// ---------------------------------------------------------------------------

/// `Statement → StatementMetadata`. Derives the `is_current` byte from
/// `superseded_by / tombstoned` only — validity-window timing is left
/// to query-time.
#[must_use]
pub fn metadata_from_statement(s: &Statement) -> StatementMetadata {
    let (subject_entity_bytes, subject_kind) = match s.subject {
        SubjectRef::Entity(id) => (id.to_bytes(), 0u8),
        SubjectRef::Pending(audit) => (audit.to_bytes(), 1u8),
        SubjectRef::Memory(id) => (id.to_be_bytes(), 2u8),
    };
    let object_discriminant = s.object.discriminant() + 1;
    let object_blob = encode_object(&s.object);

    let (evidence_inline, evidence_overflow_id_bytes) = match &s.evidence {
        EvidenceRef::Inline(entries) => {
            let rows: Vec<EvidenceEntryRow> =
                entries.iter().map(EvidenceEntryRow::from_entry).collect();
            (rows, None)
        }
        EvidenceRef::Overflow(id) => (Vec::new(), Some(id.to_bytes())),
    };

    let tombstoned = u8::from(s.tombstoned);
    let tombstone_reason = s
        .tombstone_reason
        .map(|r| r.as_u8())
        .unwrap_or(tombstone_reason::NOT_TOMBSTONED);

    let is_current = u8::from(!s.tombstoned && s.superseded_by.is_none());

    StatementMetadata {
        statement_id_bytes: s.id.to_bytes(),
        chain_root_bytes: s.chain_root.to_bytes(),
        version: s.version,
        kind: s.kind.as_u8(),
        subject_entity_bytes,
        subject_kind,
        predicate_id: s.predicate.raw(),
        object_blob,
        object_discriminant,
        confidence: s.confidence,
        extractor_id: s.extractor_id.raw(),
        schema_version: s.schema_version,
        extracted_at_unix_nanos: s.extracted_at_unix_nanos,
        valid_from_unix_nanos: s.valid_from_unix_nanos,
        valid_to_unix_nanos: s.valid_to_unix_nanos,
        event_at_unix_nanos: s.event_at_unix_nanos,
        superseded_by_bytes: s.superseded_by.map(StatementId::to_bytes),
        supersedes_bytes: s.supersedes.map(StatementId::to_bytes),
        evidence_inline,
        evidence_overflow_id_bytes,
        tombstoned,
        tombstoned_at_unix_nanos: s.tombstoned_at_unix_nanos,
        tombstone_reason,
        record_invalidated_at_unix_nanos: s.record_invalidated_at_unix_nanos,
        is_current,
        // Flags are owned by the wire handler / sweepers — neither
        // `IMPLICIT_PREDICATE` nor `OUTSIDE_ACTIVE_SCHEMA` is derivable
        // from the brain-core `Statement` alone (both need registry
        // and active-schema lookups). Default to no flags here; the
        // STATEMENT_CREATE handler / SCHEMA_UPLOAD will OR in the
        // right bits after `metadata_from_statement` returns.
        flags: 0,
        is_stateful: u8::from(s.is_stateful),
    }
}

/// `StatementMetadata → Statement`. Decodes the `object_blob` and the
/// inline-evidence rows. Overflow evidence is returned as
/// `EvidenceRef::Overflow(id)` — caller resolves to inline values via
/// a follow-up `evidence_overflow_load` call.
#[must_use]
pub fn statement_from_metadata(m: &StatementMetadata) -> Option<Statement> {
    let kind = m.kind()?;
    let object = decode_object(&m.object_blob)?;

    let subject = match m.subject_kind {
        0 => SubjectRef::Entity(EntityId::from_bytes(m.subject_entity_bytes)),
        2 => SubjectRef::Memory(brain_core::MemoryId::from_raw(u128::from_be_bytes(
            m.subject_entity_bytes,
        ))),
        // 1 (and any unknown byte, defensively) → Pending.
        _ => SubjectRef::Pending(brain_core::AuditId::from_bytes(m.subject_entity_bytes)),
    };

    let evidence = if let Some(bytes) = m.evidence_overflow_id_bytes {
        EvidenceRef::Overflow(EvidenceOverflowId::from(bytes))
    } else {
        let entries: SmallVec<[EvidenceEntry; INLINE_EVIDENCE_CAP]> = m
            .evidence_inline
            .iter()
            .map(EvidenceEntryRow::to_entry)
            .collect();
        EvidenceRef::Inline(Box::new(entries))
    };

    let tombstone_reason = brain_core::TombstoneReason::from_u8(m.tombstone_reason);

    Some(Statement {
        id: m.statement_id(),
        kind,
        subject,
        predicate: PredicateId::from(m.predicate_id),
        object,
        confidence: m.confidence,
        evidence,
        extractor_id: ExtractorId::from(m.extractor_id),
        extracted_at_unix_nanos: m.extracted_at_unix_nanos,
        schema_version: m.schema_version,
        valid_from_unix_nanos: m.valid_from_unix_nanos,
        valid_to_unix_nanos: m.valid_to_unix_nanos,
        event_at_unix_nanos: m.event_at_unix_nanos,
        version: m.version,
        superseded_by: m.superseded_by_bytes.map(StatementId::from),
        supersedes: m.supersedes_bytes.map(StatementId::from),
        chain_root: m.chain_root(),
        tombstoned: m.is_tombstoned(),
        tombstoned_at_unix_nanos: m.tombstoned_at_unix_nanos,
        tombstone_reason,
        record_invalidated_at_unix_nanos: m.record_invalidated_at_unix_nanos,
        is_stateful: m.is_stateful != 0,
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::{ContextId, EntityId, MemoryId};
    use redb::ReadableDatabase;

    fn sample_evidence_entry(byte: u8, confidence_milli: u16) -> EvidenceEntry {
        EvidenceEntry {
            memory_id: MemoryId::pack(byte as u16, ContextId::DEFAULT.into(), 0),
            confidence_milli,
            timestamp_unix_nanos: 1_700_000_000_000_000_000,
            extractor_id: ExtractorId::from(0),
        }
    }

    #[test]
    fn object_encoding_round_trip_all_variants() {
        let cases = [
            StatementObject::Entity(EntityId::new()),
            StatementObject::Value(StatementValue::Text("hello".into())),
            StatementObject::Value(StatementValue::Integer(-42)),
            StatementObject::Value(StatementValue::Float(3.5)),
            StatementObject::Value(StatementValue::Bool(true)),
            StatementObject::Value(StatementValue::UnixNanos(1_700_000_000)),
            StatementObject::Value(StatementValue::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF])),
            StatementObject::Memory(MemoryId::pack(7, ContextId::DEFAULT.into(), 0)),
            StatementObject::Statement(StatementId::new()),
        ];
        for o in cases {
            let bytes = encode_object(&o);
            let back = decode_object(&bytes).unwrap();
            assert_eq!(back, o);
        }
    }

    #[test]
    fn confidence_bucket_clamps() {
        assert_eq!(confidence_bucket(0.0), 0);
        assert_eq!(confidence_bucket(0.05), 0);
        assert_eq!(confidence_bucket(0.5), 5);
        assert_eq!(confidence_bucket(0.95), 9);
        assert_eq!(confidence_bucket(1.0), 10);
        assert_eq!(confidence_bucket(-0.5), 0);
        assert_eq!(confidence_bucket(2.0), 10);
        assert_eq!(confidence_bucket(f32::NAN), 0);
    }

    #[test]
    fn evidence_overflow_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EvidenceOverflowId::new();
        let entries: Vec<EvidenceEntry> = (1..=10)
            .map(|i| sample_evidence_entry(i as u8, (i as u16) * 100))
            .collect();
        let row = EvidenceOverflow::from_entries(id, &entries, 1_700_000_000_000_000_000);
        let key = row.overflow_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(EVIDENCE_OVERFLOW_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        let back = got.to_entries();
        assert_eq!(back, entries);
    }
}
