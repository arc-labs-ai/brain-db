//! Typed WAL payloads.
//!
//! The byte-framing layer (`record.rs`) treats the payload as opaque. This
//! module gives every record kind a typed Rust shape so callers
//! (writers, recovery, audit tools) can construct and inspect records
//! without juggling raw bytes.
//!
//! Encoding is little-endian for scalars; `MemoryId` keeps its big-endian
//! wire layout so it round-trips through `to_be_bytes`.
//!
//! Two layout choices:
//! - **`vector_dims` length prefix** in `Encode` / `Consolidate` /
//!   `MigrateEmbedding`. We add a `u16` length prefix so the file format
//!   is forward-compatible with larger embedding models. `0` means
//!   "no vector".
//! - **`UpdateSalience` count prefix**. Coalesced records carry
//!   multiple tuples. We always emit `count: u32 + tuples` (count=1 in the
//!   non-coalesced case). One uniform encoder is simpler than branching on
//!   the `flags` bit at this layer.

use brain_core::{
    AgentId, ContextId, EdgeKindRef, EdgeKindRefError, EdgeOrigin, MemoryId, MemoryKind, NodeRef,
    NodeRefError, RelationId, RelationTypeId, RequestId, TxnId,
};

/// Opaque 16-byte fingerprint of an embedding model.
pub type EmbeddingModelFp = [u8; 16];

// ---------------------------------------------------------------------------
// Per-variant payload structs.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct EncodePayload {
    pub memory_id: MemoryId,
    pub request_id: RequestId,
    pub agent_id: AgentId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub salience_initial: f32,
    pub embedding_model_fp: EmbeddingModelFp,
    pub text: String,
    /// Empty `Vec` means "vector excluded" (optional-include mode).
    pub vector: Vec<f32>,
    pub edges: Vec<EdgePayload>,
    /// blake3 of the canonical request body. Recovery uses this to
    /// repopulate the idempotency cache so a retry after restart with
    /// the same `request_id` returns the original response if the
    /// payload matches, and `Conflict` if it diverges.
    pub request_hash: [u8; 32],
    /// Bytes of the original encoded response (rkyv of EncodeResponse).
    /// Recovery replays this verbatim on idempotency hit so the client
    /// sees a byte-identical reply across restarts.
    pub response_payload: Vec<u8>,
    /// True if the originating ENCODE opted into content-hash
    /// deduplication. Recovery uses this to know whether to insert a
    /// FINGERPRINTS row (and stamp the memory's back-reference). False
    /// for ordinary ENCODE writes — the dedup index stays untouched.
    pub deduplicate: bool,
    /// Client-supplied event time (when the content actually happened),
    /// distinct from the server write time. Carried in the WAL so the
    /// field survives recovery — without it, replay would rebuild the
    /// row with `occurred_at = None` and silently lose the timeline.
    pub occurred_at_unix_nanos: Option<u64>,
}

/// Inline edge attached to an ENCODE payload.
///
/// Endpoints widened from `MemoryId` to `NodeRef` and the label widened
/// from `EdgeKind` to `EdgeKindRef` so a single ENCODE record can attach
/// substrate edges, mention edges (`Memory →(mentions)→ Entity`), or
/// typed-relation references in one shot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgePayload {
    pub source: NodeRef,
    pub target: NodeRef,
    pub kind: EdgeKindRef,
    pub weight: f32,
    pub origin: EdgeOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgetPayload {
    pub memory_id: MemoryId,
    pub request_id: RequestId,
    /// Agent the FORGET ran under. Carried in the WAL so subscribe-
    /// replay can route Forgotten events through the same per-agent
    /// allowlist that filters live publishes — without it, a
    /// multi-tenant subscriber filtering by `agents` would silently
    /// drop every replayed forget.
    pub agent_id: AgentId,
    pub mode: ForgetMode,
    pub reason: ForgetReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ForgetMode {
    Soft = 0,
    Hard = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ForgetReason {
    ClientRequest = 0,
    Eviction = 1,
}

/// LINK WAL record.
///
/// Endpoints widened to `NodeRef` and the label widened to `EdgeKindRef`
/// so the unified edge table can accept memory-to-memory, mention, and
/// typed-relation edges through the same WAL kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkPayload {
    pub source: NodeRef,
    pub target: NodeRef,
    pub edge_kind: EdgeKindRef,
    pub weight: f32,
    pub origin: EdgeOrigin,
}

/// UNLINK WAL record.
///
/// Endpoints widened to `NodeRef` and the label widened to `EdgeKindRef`
/// so the unified edge table can resolve any edge kind by its key tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlinkPayload {
    pub source: NodeRef,
    pub target: NodeRef,
    pub edge_kind: EdgeKindRef,
    pub edge_seq: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateSaliencePayload {
    pub updates: Vec<SalienceUpdate>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SalienceUpdate {
    pub memory_id: MemoryId,
    pub new_salience: f32,
    pub reason: SalienceReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SalienceReason {
    Access = 0,
    Decay = 1,
    Explicit = 2,
}

/// Reclaim payload.
///
/// Carries `slot_id`, `old_version`, and `new_version`; we add
/// `memory_id` so the metadata sink can delete the reclaimed memory's
/// row by primary key in O(1) instead of scanning the `memories` table.
///
/// On-disk layout: `slot_id` (u64) → `old_version` (u32) → `new_version`
/// (u32) → `memory_id` (16 B). The base field ordering is preserved with
/// the new field appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimPayload {
    pub slot_id: u64,
    pub old_version: u32,
    pub new_version: u32,
    pub memory_id: MemoryId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidatePayload {
    pub new_memory_id: MemoryId,
    pub source_memory_ids: Vec<MemoryId>,
    pub text: String,
    pub vector: Vec<f32>,
    pub embedding_model_fp: EmbeddingModelFp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateKindPayload {
    pub memory_id: MemoryId,
    pub new_kind: MemoryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateContextPayload {
    pub memory_id: MemoryId,
    pub new_context_id: ContextId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointBeginPayload {
    pub checkpoint_id: u64,
    pub started_at_unix_nanos: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointEndPayload {
    pub checkpoint_id: u64,
    pub durable_lsn: u64,
    pub arena_capacity: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnBeginPayload {
    pub txn_id: TxnId,
    pub expected_record_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnCommitPayload {
    pub txn_id: TxnId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnAbortPayload {
    pub txn_id: TxnId,
    pub reason_code: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MigrateEmbeddingPayload {
    pub memory_id: MemoryId,
    pub old_fingerprint: EmbeddingModelFp,
    pub new_fingerprint: EmbeddingModelFp,
    pub new_vector: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Typed-relation payloads.
//
// First-class WAL representation for relation create / supersede /
// tombstone. The sidecar metadata is carried inline so recovery can replay
// the relation without re-reading any other table.
// ---------------------------------------------------------------------------

/// Typed-relation creation. Carries every sidecar field that is not in
/// the unified edge row's `EdgeData`, so recovery rebuilds the sidecar
/// deterministically without any extra read.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationLinkPayload {
    pub relation_id: RelationId,
    pub from: NodeRef,
    pub to: NodeRef,
    pub relation_type_id: RelationTypeId,
    pub chain_root: RelationId,
    pub confidence: f32,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub supersedes: Option<RelationId>,
    pub evidence: Vec<MemoryId>,
    pub extractor_id: u32,
    pub is_symmetric: bool,
    pub properties_blob: Vec<u8>,
    /// Agent the relation create ran under. Subscribe-replay routes the
    /// EdgeAdded event through the per-agent allowlist using this id;
    /// without it, a multi-tenant subscriber would silently drop every
    /// replayed relation create.
    pub agent_id: AgentId,
    /// Schemaless-path intern hint: `Some((namespace, name))` when the
    /// relation type was not declared at write time, so `relation_type_id`
    /// holds the pre-intern placeholder and recovery re-resolves it
    /// (deterministic in LSN order, idempotent `relation_type_intern_or_get`).
    /// `None` means `relation_type_id` is authoritative (strict path).
    pub relation_type_intern_hint: Option<(String, String)>,
}

/// Typed-relation supersession. Carries the id of the row being
/// superseded and the full new relation inline so recovery can mark the
/// sidecar `is_current = 0` on the old and write the new in one step.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationSupersedePayload {
    pub old_relation_id: RelationId,
    pub new: RelationLinkPayload,
}

/// Typed-relation tombstone. The edge row stays; the sidecar flips
/// `tombstoned = 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationTombstonePayload {
    pub relation_id: RelationId,
    pub reason: String,
    pub at_unix_nanos: u64,
    pub agent_id: AgentId,
}

// ---------------------------------------------------------------------------
// Top-level enum + dispatch.
// ---------------------------------------------------------------------------

use crate::wal::kinds::WalRecordKind;

/// Opaque opaque-body WAL record.
///
/// The body is the rkyv-encoded record produced by opaque-body
/// writers (entity / statement / relation / schema / audit). For the
/// framing layer it is an opaque blob: the WAL records, reads, and
/// recovery transports it unchanged. The substrate apply-paths ignore
/// these records; typed-graph-state hydration has its own sink.
///
/// Body size is bounded by the frame header's `payload_len` (3 bytes),
/// i.e. ~16 MiB — same envelope as substrate payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseBodyRecord {
    pub kind: WalRecordKind,
    /// Agent the typed-graph mutation ran under. Carried in the WAL
    /// alongside the opaque body so subscribe-replay can route
    /// typed-graph events through the per-agent `agents` filter the
    /// same way it routes substrate events. Without it, a multi-
    /// tenant subscriber would silently drop every replayed
    /// typed-graph event.
    pub agent_id: AgentId,
    pub body: Vec<u8>,
}

impl PhaseBodyRecord {
    /// Construct a typed-graph record. The `kind` MUST satisfy
    /// `kind.has_opaque_body()`; passing a substrate kind is a programmer
    /// error and panics in debug builds.
    #[must_use]
    pub fn new(kind: WalRecordKind, agent_id: AgentId, body: Vec<u8>) -> Self {
        debug_assert!(
            kind.has_opaque_body(),
            "PhaseBodyRecord requires a opaque-body kind (0x10..=0x50); got {kind:?}"
        );
        Self {
            kind,
            agent_id,
            body,
        }
    }
}

/// Typed WAL payload, one variant per record kind.
#[derive(Debug, Clone, PartialEq)]
pub enum WalPayload {
    Encode(EncodePayload),
    Forget(ForgetPayload),
    Link(LinkPayload),
    Unlink(UnlinkPayload),
    UpdateSalience(UpdateSaliencePayload),
    Reclaim(ReclaimPayload),
    Consolidate(ConsolidatePayload),
    UpdateKind(UpdateKindPayload),
    UpdateContext(UpdateContextPayload),
    CheckpointBegin(CheckpointBeginPayload),
    CheckpointEnd(CheckpointEndPayload),
    TxnBegin(TxnBeginPayload),
    TxnCommit(TxnCommitPayload),
    TxnAbort(TxnAbortPayload),
    MigrateEmbedding(MigrateEmbeddingPayload),
    /// Typed-relation create. These are first-class instead of
    /// being carried as an opaque `PhaseBody` body so recovery can
    /// rebuild the unified edge row + sidecar atomically.
    RelationLink(RelationLinkPayload),
    /// Typed-relation supersession.
    RelationSupersede(RelationSupersedePayload),
    /// Typed-relation tombstone.
    RelationTombstone(RelationTombstonePayload),
    /// opaque-body record carried as an opaque body. Used for the
    /// entity / statement / schema / audit kinds whose typed body
    /// schemas are layered above; the framing layer transports them
    /// unchanged.
    PhaseBody(PhaseBodyRecord),
}

impl WalPayload {
    /// The discriminator byte for the framing-layer header.
    #[must_use]
    pub fn kind(&self) -> WalRecordKind {
        match self {
            Self::Encode(_) => WalRecordKind::Encode,
            Self::Forget(_) => WalRecordKind::Forget,
            Self::Link(_) => WalRecordKind::Link,
            Self::Unlink(_) => WalRecordKind::Unlink,
            Self::UpdateSalience(_) => WalRecordKind::UpdateSalience,
            Self::Reclaim(_) => WalRecordKind::Reclaim,
            Self::Consolidate(_) => WalRecordKind::Consolidate,
            Self::UpdateKind(_) => WalRecordKind::UpdateKind,
            Self::UpdateContext(_) => WalRecordKind::UpdateContext,
            Self::CheckpointBegin(_) => WalRecordKind::CheckpointBegin,
            Self::CheckpointEnd(_) => WalRecordKind::CheckpointEnd,
            Self::TxnBegin(_) => WalRecordKind::TxnBegin,
            Self::TxnCommit(_) => WalRecordKind::TxnCommit,
            Self::TxnAbort(_) => WalRecordKind::TxnAbort,
            Self::MigrateEmbedding(_) => WalRecordKind::MigrateEmbedding,
            Self::RelationLink(_) => WalRecordKind::RelationCreate,
            Self::RelationSupersede(_) => WalRecordKind::RelationSupersede,
            Self::RelationTombstone(_) => WalRecordKind::RelationTombstone,
            Self::PhaseBody(r) => r.kind,
        }
    }

    /// Encode this payload to its byte layout.
    #[must_use]
    pub fn encode_to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Encode(p) => encode_encode(p, &mut out),
            Self::Forget(p) => encode_forget(p, &mut out),
            Self::Link(p) => encode_link(p, &mut out),
            Self::Unlink(p) => encode_unlink(p, &mut out),
            Self::UpdateSalience(p) => encode_update_salience(p, &mut out),
            Self::Reclaim(p) => encode_reclaim(p, &mut out),
            Self::Consolidate(p) => encode_consolidate(p, &mut out),
            Self::UpdateKind(p) => encode_update_kind(p, &mut out),
            Self::UpdateContext(p) => encode_update_context(p, &mut out),
            Self::CheckpointBegin(p) => encode_checkpoint_begin(p, &mut out),
            Self::CheckpointEnd(p) => encode_checkpoint_end(p, &mut out),
            Self::TxnBegin(p) => encode_txn_begin(p, &mut out),
            Self::TxnCommit(p) => encode_txn_commit(p, &mut out),
            Self::TxnAbort(p) => encode_txn_abort(p, &mut out),
            Self::MigrateEmbedding(p) => encode_migrate_embedding(p, &mut out),
            Self::RelationLink(p) => encode_relation_link(p, &mut out),
            Self::RelationSupersede(p) => encode_relation_supersede(p, &mut out),
            Self::RelationTombstone(p) => encode_relation_tombstone(p, &mut out),
            Self::PhaseBody(r) => {
                // Layout: agent_id (16 B) || opaque body.
                put_uuid_bytes(&mut out, r.agent_id.into());
                out.extend_from_slice(&r.body);
            }
        }
        out
    }

    /// Decode a payload of the given `kind` from `bytes`.
    ///
    /// Trailing bytes after the structured fields are an error — the framing
    /// layer's `payload_length` told us exactly how many bytes belong to
    /// this record, so a mismatch means corruption or schema drift.
    pub fn decode(kind: WalRecordKind, bytes: &[u8]) -> Result<Self, WalPayloadError> {
        let mut r = Reader::new(bytes);
        let payload = match kind {
            WalRecordKind::Encode => Self::Encode(decode_encode(&mut r)?),
            WalRecordKind::Forget => Self::Forget(decode_forget(&mut r)?),
            WalRecordKind::Link => Self::Link(decode_link(&mut r)?),
            WalRecordKind::Unlink => Self::Unlink(decode_unlink(&mut r)?),
            WalRecordKind::UpdateSalience => Self::UpdateSalience(decode_update_salience(&mut r)?),
            WalRecordKind::Reclaim => Self::Reclaim(decode_reclaim(&mut r)?),
            WalRecordKind::Consolidate => Self::Consolidate(decode_consolidate(&mut r)?),
            WalRecordKind::UpdateKind => Self::UpdateKind(decode_update_kind(&mut r)?),
            WalRecordKind::UpdateContext => Self::UpdateContext(decode_update_context(&mut r)?),
            WalRecordKind::CheckpointBegin => {
                Self::CheckpointBegin(decode_checkpoint_begin(&mut r)?)
            }
            WalRecordKind::CheckpointEnd => Self::CheckpointEnd(decode_checkpoint_end(&mut r)?),
            WalRecordKind::TxnBegin => Self::TxnBegin(decode_txn_begin(&mut r)?),
            WalRecordKind::TxnCommit => Self::TxnCommit(decode_txn_commit(&mut r)?),
            WalRecordKind::TxnAbort => Self::TxnAbort(decode_txn_abort(&mut r)?),
            WalRecordKind::MigrateEmbedding => {
                Self::MigrateEmbedding(decode_migrate_embedding(&mut r)?)
            }
            // Typed-relation kinds are first-class: they carry
            // a fully-typed payload rather than an opaque body, so
            // recovery can rebuild the unified edge row + sidecar in
            // one step.
            WalRecordKind::RelationCreate => Self::RelationLink(decode_relation_link(&mut r)?),
            WalRecordKind::RelationSupersede => {
                Self::RelationSupersede(decode_relation_supersede(&mut r)?)
            }
            WalRecordKind::RelationTombstone => {
                Self::RelationTombstone(decode_relation_tombstone(&mut r)?)
            }
            // Remaining typed-graph kinds keep the opaque body. Their
            // typed schemas land in later phases; the framing layer
            // transports them unchanged. We early-return so the
            // trailing-bytes check below doesn't fire (the entire
            // payload IS the body).
            WalRecordKind::EntityCreate
            | WalRecordKind::EntityUpdate
            | WalRecordKind::EntityMerge
            | WalRecordKind::EntityTombstone
            | WalRecordKind::EntityRename
            | WalRecordKind::EntityUnmerge
            | WalRecordKind::StatementCreate
            | WalRecordKind::StatementSupersede
            | WalRecordKind::StatementTombstone
            | WalRecordKind::SchemaUpdate
            | WalRecordKind::ExtractorToggle
            | WalRecordKind::Audit => {
                // Layout: agent_id (16 B) || opaque body. The body
                // remains opaque to the framing layer; phases 16+
                // supply typed parsers via their own sinks.
                let agent_id: AgentId = r.array16()?.into();
                let body = bytes[r.cursor..].to_vec();
                return Ok(Self::PhaseBody(PhaseBodyRecord {
                    kind,
                    agent_id,
                    body,
                }));
            }
        };
        if !r.is_at_end() {
            return Err(WalPayloadError::TrailingBytes(r.remaining()));
        }
        Ok(payload)
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WalPayloadError {
    #[error("payload underrun: needed {needed} more byte(s), had {had}")]
    Underrun { needed: usize, had: usize },

    #[error("payload had {0} trailing byte(s) after structured fields")]
    TrailingBytes(usize),

    #[error("MemoryKind byte {0} is not in {{0, 1, 2}}")]
    BadMemoryKind(u8),

    #[error("EdgeOrigin byte {0} is not in {{0, 1}}")]
    BadEdgeOrigin(u8),

    #[error("NodeRef decode failed: {0}")]
    BadNodeRef(NodeRefError),

    #[error("EdgeKindRef decode failed: {0}")]
    BadEdgeKindRef(EdgeKindRefError),

    #[error("evidence_count {0} exceeds remaining payload bytes")]
    BadEvidenceCount(u32),

    #[error("Option<u64> tag byte {0} is not in {{0, 1}}")]
    BadOptionTag(u8),

    #[error("ForgetMode byte {0} is not in {{0, 1}}")]
    BadForgetMode(u8),

    #[error("ForgetReason byte {0} is not in {{0, 1}}")]
    BadForgetReason(u8),

    #[error("SalienceReason byte {0} is not in {{0, 1, 2}}")]
    BadSalienceReason(u8),

    #[error("text_length {0} exceeds remaining payload bytes")]
    BadTextLength(u32),

    #[error("text was not valid UTF-8")]
    BadUtf8,

    #[error("vector_dims {0} too large (cap {VECTOR_DIMS_MAX})")]
    BadVectorDims(u16),

    #[error("source_count {0} exceeds remaining payload bytes")]
    BadSourceCount(u32),

    #[error("salience update_count {0} exceeds remaining payload bytes")]
    BadUpdateCount(u32),

    #[error("edge_count {0} exceeds remaining payload bytes")]
    BadEdgeCount(u16),

    #[error("blob_length {0} exceeds cap or remaining payload bytes")]
    BadBlobLength(u32),
}

/// Cap on `response_payload` length (in bytes). 1 MiB is far past any
/// rkyv-encoded substrate response; a larger value is corruption.
pub const RESPONSE_BLOB_MAX: u32 = 1024 * 1024;

/// Cap on `vector_dims` (in f32 elements). 4096 dims = 16 KiB vector — well
/// past the largest production embedding model we expect to encounter. A
/// larger value is almost certainly corruption.
pub const VECTOR_DIMS_MAX: u16 = 4096;

// ---------------------------------------------------------------------------
// Reader / writer helpers.
// ---------------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn is_at_end(&self) -> bool {
        self.cursor == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.cursor
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WalPayloadError> {
        if self.remaining() < n {
            return Err(WalPayloadError::Underrun {
                needed: n,
                had: self.remaining(),
            });
        }
        let s = &self.bytes[self.cursor..self.cursor + n];
        self.cursor += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, WalPayloadError> {
        Ok(self.take(1)?[0])
    }

    fn u16_le(&mut self) -> Result<u16, WalPayloadError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32_le(&mut self) -> Result<u32, WalPayloadError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64_le(&mut self) -> Result<u64, WalPayloadError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn f32_le(&mut self) -> Result<f32, WalPayloadError> {
        Ok(f32::from_bits(self.u32_le()?))
    }

    fn array16(&mut self) -> Result<[u8; 16], WalPayloadError> {
        let b = self.take(16)?;
        let mut arr = [0u8; 16];
        arr.copy_from_slice(b);
        Ok(arr)
    }

    fn memory_id(&mut self) -> Result<MemoryId, WalPayloadError> {
        Ok(MemoryId::from_be_bytes(self.array16()?))
    }

    fn node_ref(&mut self) -> Result<NodeRef, WalPayloadError> {
        let bytes = self.take(NodeRef::BYTES)?;
        let mut arr = [0u8; NodeRef::BYTES];
        arr.copy_from_slice(bytes);
        NodeRef::from_bytes(arr).map_err(WalPayloadError::BadNodeRef)
    }

    fn edge_kind_ref(&mut self) -> Result<EdgeKindRef, WalPayloadError> {
        let remaining = &self.bytes[self.cursor..];
        let (k, consumed) =
            EdgeKindRef::decode_from(remaining).map_err(WalPayloadError::BadEdgeKindRef)?;
        self.cursor += consumed;
        Ok(k)
    }
}

#[inline]
fn put_memory_id(out: &mut Vec<u8>, id: MemoryId) {
    out.extend_from_slice(&id.to_be_bytes());
}

#[inline]
fn put_node_ref(out: &mut Vec<u8>, n: NodeRef) {
    out.extend_from_slice(&n.to_bytes());
}

#[inline]
fn put_edge_kind_ref(out: &mut Vec<u8>, k: EdgeKindRef) {
    k.encode_into(out);
}

#[inline]
fn put_uuid_bytes(out: &mut Vec<u8>, bytes: [u8; 16]) {
    out.extend_from_slice(&bytes);
}

#[inline]
fn put_u32_le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u64_le(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u16_le(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_f32_le(out: &mut Vec<u8>, v: f32) {
    put_u32_le(out, v.to_bits());
}

fn put_vector(out: &mut Vec<u8>, v: &[f32]) {
    let dims = u16::try_from(v.len()).expect("invariant: vector len fits in u16");
    put_u16_le(out, dims);
    for &x in v {
        put_f32_le(out, x);
    }
}

fn read_vector(r: &mut Reader<'_>) -> Result<Vec<f32>, WalPayloadError> {
    let dims = r.u16_le()?;
    if dims > VECTOR_DIMS_MAX {
        return Err(WalPayloadError::BadVectorDims(dims));
    }
    let needed = dims as usize * 4;
    if r.remaining() < needed {
        return Err(WalPayloadError::Underrun {
            needed,
            had: r.remaining(),
        });
    }
    let mut v = Vec::with_capacity(dims as usize);
    for _ in 0..dims {
        v.push(r.f32_le()?);
    }
    Ok(v)
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    put_u32_le(
        out,
        u32::try_from(bytes.len()).expect("invariant: text fits in u32"),
    );
    out.extend_from_slice(bytes);
}

fn read_text(r: &mut Reader<'_>) -> Result<String, WalPayloadError> {
    let len = r.u32_le()?;
    if len as usize > r.remaining() {
        return Err(WalPayloadError::BadTextLength(len));
    }
    let bytes = r.take(len as usize)?;
    std::str::from_utf8(bytes)
        .map(|s| s.to_owned())
        .map_err(|_| WalPayloadError::BadUtf8)
}

fn put_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32_le(
        out,
        u32::try_from(bytes.len()).expect("invariant: blob fits in u32"),
    );
    out.extend_from_slice(bytes);
}

fn read_blob(r: &mut Reader<'_>) -> Result<Vec<u8>, WalPayloadError> {
    let len = r.u32_le()?;
    if len > RESPONSE_BLOB_MAX || len as usize > r.remaining() {
        return Err(WalPayloadError::BadBlobLength(len));
    }
    Ok(r.take(len as usize)?.to_vec())
}

// MemoryKind <-> u8 mapping. Mirrors brain_protocol::MemoryKindWire so the
// wire and storage representations agree on the byte values, but kept local
// to brain-storage so this crate doesn't depend on the protocol crate.
fn memory_kind_to_u8(k: MemoryKind) -> u8 {
    match k {
        MemoryKind::Episodic => 0,
        MemoryKind::Semantic => 1,
        MemoryKind::Consolidated => 2,
    }
}

fn memory_kind_from_u8(b: u8) -> Result<MemoryKind, WalPayloadError> {
    Ok(match b {
        0 => MemoryKind::Episodic,
        1 => MemoryKind::Semantic,
        2 => MemoryKind::Consolidated,
        _ => return Err(WalPayloadError::BadMemoryKind(b)),
    })
}

fn edge_origin_from_u8(b: u8) -> Result<EdgeOrigin, WalPayloadError> {
    Ok(match b {
        0 => EdgeOrigin::Explicit,
        1 => EdgeOrigin::AutoDerived,
        _ => return Err(WalPayloadError::BadEdgeOrigin(b)),
    })
}

fn forget_mode_from_u8(b: u8) -> Result<ForgetMode, WalPayloadError> {
    Ok(match b {
        0 => ForgetMode::Soft,
        1 => ForgetMode::Hard,
        _ => return Err(WalPayloadError::BadForgetMode(b)),
    })
}

fn forget_reason_from_u8(b: u8) -> Result<ForgetReason, WalPayloadError> {
    Ok(match b {
        0 => ForgetReason::ClientRequest,
        1 => ForgetReason::Eviction,
        _ => return Err(WalPayloadError::BadForgetReason(b)),
    })
}

fn salience_reason_from_u8(b: u8) -> Result<SalienceReason, WalPayloadError> {
    Ok(match b {
        0 => SalienceReason::Access,
        1 => SalienceReason::Decay,
        2 => SalienceReason::Explicit,
        _ => return Err(WalPayloadError::BadSalienceReason(b)),
    })
}

// ---------------------------------------------------------------------------
// Per-variant encoders / decoders.
// ---------------------------------------------------------------------------

fn encode_encode(p: &EncodePayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    put_uuid_bytes(out, p.request_id.into());
    put_uuid_bytes(out, p.agent_id.into());
    put_u64_le(out, p.context_id.raw());
    out.push(memory_kind_to_u8(p.kind));
    put_f32_le(out, p.salience_initial);
    out.extend_from_slice(&p.embedding_model_fp);
    put_text(out, &p.text);
    put_vector(out, &p.vector);
    let edge_count = u16::try_from(p.edges.len()).expect("invariant: edges len fits in u16");
    put_u16_le(out, edge_count);
    for e in &p.edges {
        // Edge layout (variable-length): 17 (source NodeRef) + 17 (target
        // NodeRef) + (1..17) (kind EdgeKindRef) + 4 (weight LE) + 1
        // (origin) = 40..56 bytes.
        put_node_ref(out, e.source);
        put_node_ref(out, e.target);
        put_edge_kind_ref(out, e.kind);
        put_f32_le(out, e.weight);
        out.push(e.origin as u8);
    }
    out.extend_from_slice(&p.request_hash);
    put_blob(out, &p.response_payload);
    out.push(u8::from(p.deduplicate));
    put_option_u64(out, p.occurred_at_unix_nanos);
}

fn decode_encode(r: &mut Reader<'_>) -> Result<EncodePayload, WalPayloadError> {
    let memory_id = r.memory_id()?;
    let request_id: RequestId = r.array16()?.into();
    let agent_id: AgentId = r.array16()?.into();
    let context_id = ContextId::from(r.u64_le()?);
    let kind = memory_kind_from_u8(r.u8()?)?;
    let salience_initial = r.f32_le()?;
    let embedding_model_fp = r.array16()?;
    let text = read_text(r)?;
    let vector = read_vector(r)?;
    let edge_count = r.u16_le()?;
    // Edge size is variable now (40..56); the conservative lower bound
    // is 40 bytes. We only reject when there is no possible way the
    // remaining bytes can fit `edge_count` edges plus the fixed-size
    // tail (32 request_hash + 4 blob_len + 1 dedup = 37 bytes).
    if (edge_count as usize).saturating_mul(40).saturating_add(37) > r.remaining() {
        return Err(WalPayloadError::BadEdgeCount(edge_count));
    }
    let mut edges = Vec::with_capacity(edge_count as usize);
    for _ in 0..edge_count {
        let source = r.node_ref()?;
        let target = r.node_ref()?;
        let edge_kind = r.edge_kind_ref()?;
        let weight = r.f32_le()?;
        let origin = edge_origin_from_u8(r.u8()?)?;
        edges.push(EdgePayload {
            source,
            target,
            kind: edge_kind,
            weight,
            origin,
        });
    }
    let request_hash = {
        let b = r.take(32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(b);
        arr
    };
    let response_payload = read_blob(r)?;
    let deduplicate = r.u8()? != 0;
    let occurred_at_unix_nanos = read_option_u64(r)?;
    Ok(EncodePayload {
        memory_id,
        request_id,
        agent_id,
        context_id,
        kind,
        salience_initial,
        embedding_model_fp,
        text,
        vector,
        edges,
        request_hash,
        response_payload,
        deduplicate,
        occurred_at_unix_nanos,
    })
}

fn encode_forget(p: &ForgetPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    put_uuid_bytes(out, p.request_id.into());
    put_uuid_bytes(out, p.agent_id.into());
    out.push(p.mode as u8);
    out.push(p.reason as u8);
}

fn decode_forget(r: &mut Reader<'_>) -> Result<ForgetPayload, WalPayloadError> {
    Ok(ForgetPayload {
        memory_id: r.memory_id()?,
        request_id: r.array16()?.into(),
        agent_id: r.array16()?.into(),
        mode: forget_mode_from_u8(r.u8()?)?,
        reason: forget_reason_from_u8(r.u8()?)?,
    })
}

fn encode_link(p: &LinkPayload, out: &mut Vec<u8>) {
    // Layout: 17 (source NodeRef) + 17 (target NodeRef) + (1..17) (kind
    // EdgeKindRef) + 4 (weight LE) + 1 (origin) = 40..56 bytes.
    put_node_ref(out, p.source);
    put_node_ref(out, p.target);
    put_edge_kind_ref(out, p.edge_kind);
    put_f32_le(out, p.weight);
    out.push(p.origin as u8);
}

fn decode_link(r: &mut Reader<'_>) -> Result<LinkPayload, WalPayloadError> {
    Ok(LinkPayload {
        source: r.node_ref()?,
        target: r.node_ref()?,
        edge_kind: r.edge_kind_ref()?,
        weight: r.f32_le()?,
        origin: edge_origin_from_u8(r.u8()?)?,
    })
}

fn encode_unlink(p: &UnlinkPayload, out: &mut Vec<u8>) {
    // Layout: 17 (source NodeRef) + 17 (target NodeRef) + (1..17) (kind
    // EdgeKindRef) + 4 (edge_seq LE) = 39..55 bytes.
    put_node_ref(out, p.source);
    put_node_ref(out, p.target);
    put_edge_kind_ref(out, p.edge_kind);
    put_u32_le(out, p.edge_seq);
}

fn decode_unlink(r: &mut Reader<'_>) -> Result<UnlinkPayload, WalPayloadError> {
    Ok(UnlinkPayload {
        source: r.node_ref()?,
        target: r.node_ref()?,
        edge_kind: r.edge_kind_ref()?,
        edge_seq: r.u32_le()?,
    })
}

fn encode_update_salience(p: &UpdateSaliencePayload, out: &mut Vec<u8>) {
    let n = u32::try_from(p.updates.len()).expect("invariant: updates fits in u32");
    put_u32_le(out, n);
    for u in &p.updates {
        put_memory_id(out, u.memory_id);
        put_f32_le(out, u.new_salience);
        out.push(u.reason as u8);
    }
}

fn decode_update_salience(r: &mut Reader<'_>) -> Result<UpdateSaliencePayload, WalPayloadError> {
    let count = r.u32_le()?;
    // 21 bytes per update.
    if (count as usize).saturating_mul(21) > r.remaining() {
        return Err(WalPayloadError::BadUpdateCount(count));
    }
    let mut updates = Vec::with_capacity(count as usize);
    for _ in 0..count {
        updates.push(SalienceUpdate {
            memory_id: r.memory_id()?,
            new_salience: r.f32_le()?,
            reason: salience_reason_from_u8(r.u8()?)?,
        });
    }
    Ok(UpdateSaliencePayload { updates })
}

fn encode_reclaim(p: &ReclaimPayload, out: &mut Vec<u8>) {
    put_u64_le(out, p.slot_id);
    put_u32_le(out, p.old_version);
    put_u32_le(out, p.new_version);
    put_memory_id(out, p.memory_id);
}

fn decode_reclaim(r: &mut Reader<'_>) -> Result<ReclaimPayload, WalPayloadError> {
    Ok(ReclaimPayload {
        slot_id: r.u64_le()?,
        old_version: r.u32_le()?,
        new_version: r.u32_le()?,
        memory_id: r.memory_id()?,
    })
}

fn encode_consolidate(p: &ConsolidatePayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.new_memory_id);
    let src_count =
        u32::try_from(p.source_memory_ids.len()).expect("invariant: source_memory_ids fits in u32");
    put_u32_le(out, src_count);
    for id in &p.source_memory_ids {
        put_memory_id(out, *id);
    }
    put_text(out, &p.text);
    put_vector(out, &p.vector);
    out.extend_from_slice(&p.embedding_model_fp);
}

fn decode_consolidate(r: &mut Reader<'_>) -> Result<ConsolidatePayload, WalPayloadError> {
    let new_memory_id = r.memory_id()?;
    let src_count = r.u32_le()?;
    if (src_count as usize).saturating_mul(16) > r.remaining() {
        return Err(WalPayloadError::BadSourceCount(src_count));
    }
    let mut source_memory_ids = Vec::with_capacity(src_count as usize);
    for _ in 0..src_count {
        source_memory_ids.push(r.memory_id()?);
    }
    let text = read_text(r)?;
    let vector = read_vector(r)?;
    let embedding_model_fp = r.array16()?;
    Ok(ConsolidatePayload {
        new_memory_id,
        source_memory_ids,
        text,
        vector,
        embedding_model_fp,
    })
}

fn encode_update_kind(p: &UpdateKindPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    out.push(memory_kind_to_u8(p.new_kind));
}

fn decode_update_kind(r: &mut Reader<'_>) -> Result<UpdateKindPayload, WalPayloadError> {
    Ok(UpdateKindPayload {
        memory_id: r.memory_id()?,
        new_kind: memory_kind_from_u8(r.u8()?)?,
    })
}

fn encode_update_context(p: &UpdateContextPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    put_u64_le(out, p.new_context_id.raw());
}

fn decode_update_context(r: &mut Reader<'_>) -> Result<UpdateContextPayload, WalPayloadError> {
    Ok(UpdateContextPayload {
        memory_id: r.memory_id()?,
        new_context_id: ContextId::from(r.u64_le()?),
    })
}

fn encode_checkpoint_begin(p: &CheckpointBeginPayload, out: &mut Vec<u8>) {
    put_u64_le(out, p.checkpoint_id);
    put_u64_le(out, p.started_at_unix_nanos);
}

fn decode_checkpoint_begin(r: &mut Reader<'_>) -> Result<CheckpointBeginPayload, WalPayloadError> {
    Ok(CheckpointBeginPayload {
        checkpoint_id: r.u64_le()?,
        started_at_unix_nanos: r.u64_le()?,
    })
}

fn encode_checkpoint_end(p: &CheckpointEndPayload, out: &mut Vec<u8>) {
    put_u64_le(out, p.checkpoint_id);
    put_u64_le(out, p.durable_lsn);
    put_u64_le(out, p.arena_capacity);
}

fn decode_checkpoint_end(r: &mut Reader<'_>) -> Result<CheckpointEndPayload, WalPayloadError> {
    Ok(CheckpointEndPayload {
        checkpoint_id: r.u64_le()?,
        durable_lsn: r.u64_le()?,
        arena_capacity: r.u64_le()?,
    })
}

fn encode_txn_begin(p: &TxnBeginPayload, out: &mut Vec<u8>) {
    put_uuid_bytes(out, p.txn_id.into());
    put_u32_le(out, p.expected_record_count);
}

fn decode_txn_begin(r: &mut Reader<'_>) -> Result<TxnBeginPayload, WalPayloadError> {
    Ok(TxnBeginPayload {
        txn_id: r.array16()?.into(),
        expected_record_count: r.u32_le()?,
    })
}

fn encode_txn_commit(p: &TxnCommitPayload, out: &mut Vec<u8>) {
    put_uuid_bytes(out, p.txn_id.into());
}

fn decode_txn_commit(r: &mut Reader<'_>) -> Result<TxnCommitPayload, WalPayloadError> {
    Ok(TxnCommitPayload {
        txn_id: r.array16()?.into(),
    })
}

fn encode_txn_abort(p: &TxnAbortPayload, out: &mut Vec<u8>) {
    put_uuid_bytes(out, p.txn_id.into());
    put_u32_le(out, p.reason_code);
}

fn decode_txn_abort(r: &mut Reader<'_>) -> Result<TxnAbortPayload, WalPayloadError> {
    Ok(TxnAbortPayload {
        txn_id: r.array16()?.into(),
        reason_code: r.u32_le()?,
    })
}

fn encode_migrate_embedding(p: &MigrateEmbeddingPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    out.extend_from_slice(&p.old_fingerprint);
    out.extend_from_slice(&p.new_fingerprint);
    put_vector(out, &p.new_vector);
}

fn decode_migrate_embedding(
    r: &mut Reader<'_>,
) -> Result<MigrateEmbeddingPayload, WalPayloadError> {
    Ok(MigrateEmbeddingPayload {
        memory_id: r.memory_id()?,
        old_fingerprint: r.array16()?,
        new_fingerprint: r.array16()?,
        new_vector: read_vector(r)?,
    })
}

// ---------------------------------------------------------------------------
// Typed-relation helpers.
// ---------------------------------------------------------------------------

#[inline]
fn put_option_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        None => out.push(0),
        Some(x) => {
            out.push(1);
            put_u64_le(out, x);
        }
    }
}

fn read_option_u64(r: &mut Reader<'_>) -> Result<Option<u64>, WalPayloadError> {
    let tag = r.u8()?;
    Ok(match tag {
        0 => None,
        1 => Some(r.u64_le()?),
        _ => return Err(WalPayloadError::BadOptionTag(tag)),
    })
}

#[inline]
fn put_option_relation_id(out: &mut Vec<u8>, v: Option<RelationId>) {
    match v {
        None => out.push(0),
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_bytes());
        }
    }
}

fn read_option_relation_id(r: &mut Reader<'_>) -> Result<Option<RelationId>, WalPayloadError> {
    let tag = r.u8()?;
    Ok(match tag {
        0 => None,
        1 => Some(RelationId::from_bytes(r.array16()?)),
        _ => return Err(WalPayloadError::BadOptionTag(tag)),
    })
}

fn encode_relation_link(p: &RelationLinkPayload, out: &mut Vec<u8>) {
    // Layout:
    //   relation_id (16) || from NodeRef (17) || to NodeRef (17) ||
    //   relation_type_id (4 LE) || chain_root (16) || confidence (4 LE) ||
    //   valid_from Option<u64> (1 or 9) || valid_to Option<u64> (1 or 9) ||
    //   supersedes Option<RelationId> (1 or 17) ||
    //   evidence_count (4 LE) + evidence_ids (16 * N) ||
    //   extractor_id (4 LE) || is_symmetric (1) ||
    //   properties_blob (4 LE len + bytes) || agent_id (16).
    out.extend_from_slice(&p.relation_id.to_bytes());
    put_node_ref(out, p.from);
    put_node_ref(out, p.to);
    put_u32_le(out, p.relation_type_id.raw());
    out.extend_from_slice(&p.chain_root.to_bytes());
    put_f32_le(out, p.confidence);
    put_option_u64(out, p.valid_from_unix_nanos);
    put_option_u64(out, p.valid_to_unix_nanos);
    put_option_relation_id(out, p.supersedes);
    let ev_count = u32::try_from(p.evidence.len()).expect("invariant: evidence len fits in u32");
    put_u32_le(out, ev_count);
    for id in &p.evidence {
        put_memory_id(out, *id);
    }
    put_u32_le(out, p.extractor_id);
    out.push(u8::from(p.is_symmetric));
    put_blob(out, &p.properties_blob);
    put_uuid_bytes(out, p.agent_id.into());
    // relation_type_intern_hint: tag byte then two length-prefixed strings.
    match &p.relation_type_intern_hint {
        None => out.push(0),
        Some((namespace, name)) => {
            out.push(1);
            put_blob(out, namespace.as_bytes());
            put_blob(out, name.as_bytes());
        }
    }
}

fn decode_relation_link(r: &mut Reader<'_>) -> Result<RelationLinkPayload, WalPayloadError> {
    let relation_id = RelationId::from_bytes(r.array16()?);
    let from = r.node_ref()?;
    let to = r.node_ref()?;
    let relation_type_id = RelationTypeId::from(r.u32_le()?);
    let chain_root = RelationId::from_bytes(r.array16()?);
    let confidence = r.f32_le()?;
    let valid_from_unix_nanos = read_option_u64(r)?;
    let valid_to_unix_nanos = read_option_u64(r)?;
    let supersedes = read_option_relation_id(r)?;
    let ev_count = r.u32_le()?;
    if (ev_count as usize).saturating_mul(16) > r.remaining() {
        return Err(WalPayloadError::BadEvidenceCount(ev_count));
    }
    let mut evidence = Vec::with_capacity(ev_count as usize);
    for _ in 0..ev_count {
        evidence.push(r.memory_id()?);
    }
    let extractor_id = r.u32_le()?;
    let is_symmetric = r.u8()? != 0;
    let properties_blob = read_blob(r)?;
    let agent_id: AgentId = r.array16()?.into();
    let relation_type_intern_hint = match r.u8()? {
        0 => None,
        1 => {
            let namespace =
                String::from_utf8(read_blob(r)?).map_err(|_| WalPayloadError::BadUtf8)?;
            let name = String::from_utf8(read_blob(r)?).map_err(|_| WalPayloadError::BadUtf8)?;
            Some((namespace, name))
        }
        other => return Err(WalPayloadError::BadOptionTag(other)),
    };
    Ok(RelationLinkPayload {
        relation_id,
        from,
        to,
        relation_type_id,
        chain_root,
        confidence,
        valid_from_unix_nanos,
        valid_to_unix_nanos,
        supersedes,
        evidence,
        extractor_id,
        is_symmetric,
        properties_blob,
        agent_id,
        relation_type_intern_hint,
    })
}

fn encode_relation_supersede(p: &RelationSupersedePayload, out: &mut Vec<u8>) {
    // Layout: old_relation_id (16) || encoded RelationLinkPayload.
    out.extend_from_slice(&p.old_relation_id.to_bytes());
    encode_relation_link(&p.new, out);
}

fn decode_relation_supersede(
    r: &mut Reader<'_>,
) -> Result<RelationSupersedePayload, WalPayloadError> {
    let old_relation_id = RelationId::from_bytes(r.array16()?);
    let new = decode_relation_link(r)?;
    Ok(RelationSupersedePayload {
        old_relation_id,
        new,
    })
}

fn encode_relation_tombstone(p: &RelationTombstonePayload, out: &mut Vec<u8>) {
    // Layout: relation_id (16) || reason (text) || at_unix_nanos (8 LE)
    //   || agent_id (16).
    out.extend_from_slice(&p.relation_id.to_bytes());
    put_text(out, &p.reason);
    put_u64_le(out, p.at_unix_nanos);
    put_uuid_bytes(out, p.agent_id.into());
}

fn decode_relation_tombstone(
    r: &mut Reader<'_>,
) -> Result<RelationTombstonePayload, WalPayloadError> {
    Ok(RelationTombstonePayload {
        relation_id: RelationId::from_bytes(r.array16()?),
        reason: read_text(r)?,
        at_unix_nanos: r.u64_le()?,
        agent_id: r.array16()?.into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{
        AgentId, EdgeKind, EdgeKindRef, EdgeOrigin, EntityId, MemoryId, NodeRef, RelationId,
        RelationTypeId, RequestId, TxnId,
    };
    use proptest::prelude::*;

    fn mid(n: u64) -> MemoryId {
        MemoryId::pack(1, n, 1)
    }

    fn mnode(n: u64) -> NodeRef {
        NodeRef::Memory(mid(n))
    }

    fn enode(byte: u8) -> NodeRef {
        let mut b = [0u8; 16];
        b[15] = byte;
        NodeRef::Entity(EntityId::from_bytes(b))
    }

    fn rid(byte: u8) -> RequestId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn tid(byte: u8) -> TxnId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn relid(byte: u8) -> RelationId {
        let mut b = [0u8; 16];
        b[15] = byte;
        RelationId::from_bytes(b)
    }

    fn rtype(byte: u8) -> RelationTypeId {
        RelationTypeId::from(u32::from(byte))
    }

    fn fp(byte: u8) -> EmbeddingModelFp {
        [byte; 16]
    }

    /// Build one fully-populated payload per variant. The fixtures intentionally
    /// vary in field values so a sloppy field-order bug shows up as a mismatch.
    fn all_variants() -> Vec<WalPayload> {
        vec![
            WalPayload::Encode(EncodePayload {
                memory_id: mid(7),
                request_id: rid(1),
                agent_id: aid(2),
                context_id: ContextId(0xCAFE),
                kind: MemoryKind::Episodic,
                salience_initial: 0.5,
                embedding_model_fp: fp(0xAA),
                text: "hello world".into(),
                vector: (0..8).map(|i| i as f32 * 0.25).collect(),
                edges: vec![EdgePayload {
                    source: mnode(7),
                    target: mnode(8),
                    kind: EdgeKindRef::Builtin(EdgeKind::Caused),
                    weight: 0.9,
                    origin: EdgeOrigin::Explicit,
                }],
                request_hash: [0x11; 32],
                response_payload: b"response-bytes".to_vec(),
                deduplicate: true,
                occurred_at_unix_nanos: None,
            }),
            WalPayload::Forget(ForgetPayload {
                memory_id: mid(9),
                request_id: rid(3),
                agent_id: aid(4),
                mode: ForgetMode::Hard,
                reason: ForgetReason::Eviction,
            }),
            WalPayload::Link(LinkPayload {
                source: mnode(10),
                target: mnode(11),
                edge_kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                weight: 0.42,
                origin: EdgeOrigin::AutoDerived,
            }),
            WalPayload::Unlink(UnlinkPayload {
                source: mnode(12),
                target: mnode(13),
                edge_kind: EdgeKindRef::Builtin(EdgeKind::PartOf),
                edge_seq: 7,
            }),
            WalPayload::UpdateSalience(UpdateSaliencePayload {
                updates: vec![
                    SalienceUpdate {
                        memory_id: mid(14),
                        new_salience: 0.1,
                        reason: SalienceReason::Decay,
                    },
                    SalienceUpdate {
                        memory_id: mid(15),
                        new_salience: 0.95,
                        reason: SalienceReason::Access,
                    },
                ],
            }),
            WalPayload::Reclaim(ReclaimPayload {
                slot_id: 0xDEAD_BEEF,
                old_version: 3,
                new_version: 4,
                memory_id: mid(0xDEAD_BEEF),
            }),
            WalPayload::Consolidate(ConsolidatePayload {
                new_memory_id: mid(20),
                source_memory_ids: vec![mid(1), mid(2), mid(3)],
                text: "consolidated summary".into(),
                vector: (0..4).map(|i| i as f32).collect(),
                embedding_model_fp: fp(0xBB),
            }),
            WalPayload::UpdateKind(UpdateKindPayload {
                memory_id: mid(21),
                new_kind: MemoryKind::Semantic,
            }),
            WalPayload::UpdateContext(UpdateContextPayload {
                memory_id: mid(22),
                new_context_id: ContextId(99),
            }),
            WalPayload::CheckpointBegin(CheckpointBeginPayload {
                checkpoint_id: 100,
                started_at_unix_nanos: 1_700_000_000_000_000_000,
            }),
            WalPayload::CheckpointEnd(CheckpointEndPayload {
                checkpoint_id: 100,
                durable_lsn: 12345,
                arena_capacity: 1 << 20,
            }),
            WalPayload::TxnBegin(TxnBeginPayload {
                txn_id: tid(5),
                expected_record_count: 4,
            }),
            WalPayload::TxnCommit(TxnCommitPayload { txn_id: tid(5) }),
            WalPayload::TxnAbort(TxnAbortPayload {
                txn_id: tid(6),
                reason_code: 42,
            }),
            WalPayload::MigrateEmbedding(MigrateEmbeddingPayload {
                memory_id: mid(30),
                old_fingerprint: fp(0xCC),
                new_fingerprint: fp(0xDD),
                new_vector: (0..6).map(|i| (i as f32) * 1.5).collect(),
            }),
            WalPayload::RelationLink(sample_relation_link()),
            WalPayload::RelationSupersede(RelationSupersedePayload {
                old_relation_id: relid(0xA0),
                new: sample_relation_link(),
            }),
            WalPayload::RelationTombstone(RelationTombstonePayload {
                relation_id: relid(0xA2),
                reason: "duplicate".into(),
                at_unix_nanos: 1_800_000_000_000_000_000,
                agent_id: aid(0xA3),
            }),
        ]
    }

    fn sample_relation_link() -> RelationLinkPayload {
        RelationLinkPayload {
            relation_id: relid(0xB0),
            from: enode(0xC0),
            to: enode(0xC1),
            relation_type_id: RelationTypeId::from(42),
            chain_root: relid(0xB0),
            confidence: 0.9,
            valid_from_unix_nanos: Some(1_700_000_000_000_000_000),
            valid_to_unix_nanos: None,
            supersedes: Some(relid(0xAA)),
            evidence: vec![mid(1), mid(2), mid(3)],
            extractor_id: 7,
            is_symmetric: false,
            properties_blob: vec![1, 2, 3, 4, 5],
            agent_id: aid(0xC5),
            relation_type_intern_hint: None,
        }
    }

    #[test]
    fn one_variant_per_spec_kind() {
        // 15 substrate kinds + 3 typed-relation kinds = 18 first-class
        // payload kinds. The fixture function must hit every one.
        let payloads = all_variants();
        let kinds: std::collections::HashSet<_> = payloads.iter().map(|p| p.kind()).collect();
        assert_eq!(kinds.len(), 18);
    }

    #[test]
    fn round_trip_every_variant() {
        for p in all_variants() {
            let bytes = p.encode_to_bytes();
            let decoded = WalPayload::decode(p.kind(), &bytes).expect("decode");
            assert_eq!(decoded, p, "round-trip mismatch for {:?}", p.kind());
        }
    }

    #[test]
    fn every_short_prefix_underruns() {
        // For every variant: every prefix shorter than the full encoding must
        // return Underrun, never silently succeed.
        for p in all_variants() {
            let bytes = p.encode_to_bytes();
            for n in 0..bytes.len() {
                match WalPayload::decode(p.kind(), &bytes[..n]) {
                    Err(WalPayloadError::Underrun { .. }) => {}
                    Err(other) => {
                        // A `Bad*` validation error from a partially-read byte
                        // is acceptable iff the under-read has reached the
                        // validating field. Tighten this if we see flakiness:
                        // for now we accept any error on a short prefix, but
                        // *not* a silent Ok.
                        let _ = other;
                    }
                    Ok(_) => panic!(
                        "decoded {:?} from {n}-byte prefix of {}-byte payload",
                        p.kind(),
                        bytes.len(),
                    ),
                }
            }
        }
    }

    #[test]
    fn trailing_bytes_rejected() {
        for p in all_variants() {
            let mut bytes = p.encode_to_bytes();
            bytes.push(0); // one stray byte
            assert_eq!(
                WalPayload::decode(p.kind(), &bytes),
                Err(WalPayloadError::TrailingBytes(1)),
                "expected TrailingBytes for {:?}",
                p.kind(),
            );
        }
    }

    #[test]
    fn bad_memory_kind_rejected() {
        // UpdateKind has a single MemoryKind byte at offset 16.
        let p = WalPayload::UpdateKind(UpdateKindPayload {
            memory_id: mid(1),
            new_kind: MemoryKind::Episodic,
        });
        let mut bytes = p.encode_to_bytes();
        bytes[16] = 99;
        assert_eq!(
            WalPayload::decode(WalRecordKind::UpdateKind, &bytes),
            Err(WalPayloadError::BadMemoryKind(99))
        );
    }

    #[test]
    fn bad_edge_kind_rejected() {
        // Widened layout: 17 (source NodeRef) + 17 (target NodeRef) +
        // EdgeKindRef tag at offset 34. For a Builtin label the kind
        // byte sits at offset 35.
        let p = WalPayload::Link(LinkPayload {
            source: mnode(1),
            target: mnode(2),
            edge_kind: EdgeKindRef::Builtin(EdgeKind::Caused),
            weight: 1.0,
            origin: EdgeOrigin::Explicit,
        });
        let mut bytes = p.encode_to_bytes();
        // Offset 34 = EdgeKindRef tag (0 = Builtin). Offset 35 = kind.
        assert_eq!(bytes[34], 0, "Builtin tag");
        bytes[35] = 99;
        assert_eq!(
            WalPayload::decode(WalRecordKind::Link, &bytes),
            Err(WalPayloadError::BadEdgeKindRef(
                brain_core::EdgeKindRefError::InvalidEdgeKind(99)
            ))
        );
    }

    #[test]
    fn bad_edge_origin_rejected() {
        // Widened layout: 17 + 17 + 2 (Builtin EdgeKindRef) + 4 (weight)
        // + 1 (origin) = 41 bytes. Origin is the last byte.
        let p = WalPayload::Link(LinkPayload {
            source: mnode(1),
            target: mnode(2),
            edge_kind: EdgeKindRef::Builtin(EdgeKind::Caused),
            weight: 1.0,
            origin: EdgeOrigin::Explicit,
        });
        let mut bytes = p.encode_to_bytes();
        let last = bytes.len() - 1;
        bytes[last] = 99;
        assert_eq!(
            WalPayload::decode(WalRecordKind::Link, &bytes),
            Err(WalPayloadError::BadEdgeOrigin(99))
        );
    }

    #[test]
    fn bad_utf8_text_rejected() {
        // Encode produces a valid UTF-8 sequence; we substitute an invalid
        // byte sequence in the text region of an Encode payload.
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.0,
            embedding_model_fp: fp(0),
            text: "ab".into(),
            vector: vec![],
            edges: vec![],
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let mut bytes = p.encode_to_bytes();
        // Text starts after MemoryId(16) + RequestId(16) + AgentId(16)
        //   + ContextId(8) + kind(1) + salience(4) + fp(16) + text_len(4)
        // = 81. Replace the 2-byte text with an invalid UTF-8 lead byte.
        let text_start = 16 + 16 + 16 + 8 + 1 + 4 + 16 + 4;
        bytes[text_start] = 0xC0; // illegal UTF-8 lead
        bytes[text_start + 1] = 0xC0;
        assert_eq!(
            WalPayload::decode(WalRecordKind::Encode, &bytes),
            Err(WalPayloadError::BadUtf8)
        );
    }

    #[test]
    fn bad_vector_dims_rejected() {
        // MigrateEmbedding carries a vector at the tail; an oversize dims
        // value must be rejected even before the underrun check (so that
        // recovery fails fast instead of allocating a giant Vec).
        let p = WalPayload::MigrateEmbedding(MigrateEmbeddingPayload {
            memory_id: mid(1),
            old_fingerprint: fp(0),
            new_fingerprint: fp(0),
            new_vector: vec![0.0],
        });
        let mut bytes = p.encode_to_bytes();
        // vector_dims is at offset MemoryId(16) + old_fp(16) + new_fp(16) = 48.
        let dims_off = 48;
        let huge = (VECTOR_DIMS_MAX + 1).to_le_bytes();
        bytes[dims_off..dims_off + 2].copy_from_slice(&huge);
        match WalPayload::decode(WalRecordKind::MigrateEmbedding, &bytes) {
            Err(WalPayloadError::BadVectorDims(d)) => {
                assert_eq!(d, VECTOR_DIMS_MAX + 1)
            }
            other => panic!("expected BadVectorDims, got {other:?}"),
        }
    }

    #[test]
    fn empty_vector_round_trips() {
        // Encode with vector=[] is the "exclude vector" path.
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.0,
            embedding_model_fp: fp(0),
            text: String::new(),
            vector: vec![],
            edges: vec![],
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let bytes = p.encode_to_bytes();
        assert_eq!(WalPayload::decode(p.kind(), &bytes).unwrap(), p);
    }

    #[test]
    fn encode_payload_with_request_hash_round_trips() {
        // request_hash is a 32-byte blake3 of the canonical request body;
        // recovery needs it byte-identical so the idempotency cache can
        // detect a same-id-different-params conflict.
        let hash: [u8; 32] = std::array::from_fn(|i| i as u8);
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.0,
            embedding_model_fp: fp(0),
            text: "hi".into(),
            vector: vec![],
            edges: vec![],
            request_hash: hash,
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let bytes = p.encode_to_bytes();
        match WalPayload::decode(WalRecordKind::Encode, &bytes).unwrap() {
            WalPayload::Encode(decoded) => assert_eq!(decoded.request_hash, hash),
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    #[test]
    fn encode_payload_with_response_payload_round_trips() {
        // response_payload is an rkyv blob; recovery replays it verbatim
        // to a retried client.
        let body: Vec<u8> = (0..256u16).map(|i| (i & 0xFF) as u8).collect();
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.0,
            embedding_model_fp: fp(0),
            text: "hi".into(),
            vector: vec![],
            edges: vec![],
            request_hash: [0; 32],
            response_payload: body.clone(),
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let bytes = p.encode_to_bytes();
        match WalPayload::decode(WalRecordKind::Encode, &bytes).unwrap() {
            WalPayload::Encode(decoded) => assert_eq!(decoded.response_payload, body),
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    #[test]
    fn legacy_encode_payload_without_new_fields_errors_cleanly() {
        // The hard-cut design means a WAL written by a pre-PR binary
        // lacks the trailing request_hash + response_payload tail. A
        // load attempt must fail loudly so operators see "wipe and
        // restart," not silent zeros.
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(0),
            agent_id: aid(0),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.0,
            embedding_model_fp: fp(0),
            text: String::new(),
            vector: vec![],
            edges: vec![],
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let full = p.encode_to_bytes();
        // Drop the 32-byte hash + 4-byte response_payload length + 1-byte
        // deduplicate flag tail (37 bytes total) — what a pre-PR binary
        // would have written. Any structured failure is acceptable; we
        // only require that the decoder refuses (the exact variant may
        // be Underrun, BadEdgeCount, or BadBlobLength depending on how
        // the missing tail rolls into the next size-prefixed field).
        let legacy_len = full.len() - 32 - 4 - 1;
        let legacy = &full[..legacy_len];
        assert!(
            WalPayload::decode(WalRecordKind::Encode, legacy).is_err(),
            "expected legacy-tail decode to fail",
        );
    }

    // -----------------------------------------------------------------
    // opaque-body.
    // -----------------------------------------------------------------

    #[test]
    fn graph_record_round_trip() {
        // The three RelationCreate/Supersede/Tombstone kinds are
        // first-class typed payloads — not opaque-body typed-graph
        // records — so they are excluded from this fixture.
        for kind in [
            WalRecordKind::EntityCreate,
            WalRecordKind::EntityUpdate,
            WalRecordKind::EntityMerge,
            WalRecordKind::EntityTombstone,
            WalRecordKind::StatementCreate,
            WalRecordKind::StatementSupersede,
            WalRecordKind::StatementTombstone,
            WalRecordKind::SchemaUpdate,
            WalRecordKind::Audit,
        ] {
            let body: Vec<u8> = (0..32u8).map(|i| i ^ kind.as_u8()).collect();
            let agent = aid(kind.as_u8());
            let payload = WalPayload::PhaseBody(PhaseBodyRecord::new(kind, agent, body.clone()));
            assert_eq!(payload.kind(), kind);
            let bytes = payload.encode_to_bytes();
            // Layout: 16-byte agent_id, then the opaque body.
            assert_eq!(bytes.len(), 16 + body.len());
            assert_eq!(&bytes[16..], body.as_slice());
            let decoded = WalPayload::decode(kind, &bytes).expect("decode typed-graph");
            match decoded {
                WalPayload::PhaseBody(r) => {
                    assert_eq!(r.kind, kind);
                    assert_eq!(r.agent_id, agent);
                    assert_eq!(r.body, body);
                }
                other => panic!("expected PhaseBody, got {other:?}"),
            }
        }
    }

    #[test]
    fn graph_decode_empty_body_is_ok() {
        // An empty body is a legal opaque payload (a tombstone marker,
        // for instance, may carry no fields). The 16-byte agent_id
        // prefix is still mandatory.
        let agent_bytes = [0u8; 16];
        let payload = WalPayload::decode(WalRecordKind::EntityTombstone, &agent_bytes)
            .expect("empty body decodes");
        match payload {
            WalPayload::PhaseBody(r) => {
                assert_eq!(r.kind, WalRecordKind::EntityTombstone);
                assert_eq!(r.agent_id, AgentId::from(agent_bytes));
                assert!(r.body.is_empty());
            }
            other => panic!("expected PhaseBody, got {other:?}"),
        }
    }

    #[test]
    fn graph_decode_rejects_short_prefix() {
        // Anything shorter than the 16-byte agent_id prefix must
        // underrun rather than silently succeeding.
        for n in 0..16 {
            match WalPayload::decode(WalRecordKind::EntityTombstone, &vec![0u8; n]) {
                Err(WalPayloadError::Underrun { .. }) => {}
                other => panic!("expected Underrun for {n}-byte payload, got {other:?}"),
            }
        }
    }

    #[test]
    fn graph_decode_skips_trailing_bytes_check() {
        // For substrate kinds, trailing bytes after the structured tail
        // are an error. For typed-graph kinds the bytes following the
        // 16-byte agent_id prefix are the opaque body — no such check
        // applies. Verify by feeding garbage past the prefix.
        let mut bytes = vec![0u8; 16];
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
        let decoded = WalPayload::decode(WalRecordKind::SchemaUpdate, &bytes)
            .expect("typed-graph accepts any bytes after prefix");
        if let WalPayload::PhaseBody(r) = decoded {
            assert_eq!(r.body, bytes[16..].to_vec());
        } else {
            panic!("expected PhaseBody");
        }
    }

    #[test]
    #[should_panic(expected = "PhaseBodyRecord requires a opaque-body kind")]
    fn graph_record_rejects_substrate_kind_in_debug() {
        // Debug-only invariant: constructing a PhaseBodyRecord with a
        // substrate kind panics. (In release builds the debug_assert is
        // elided; that's intentional — callers are not expected to feed
        // adversarial kinds.)
        let _ = PhaseBodyRecord::new(WalRecordKind::Encode, AgentId::default(), vec![]);
    }

    // -----------------------------------------------------------------
    // NodeRef / EdgeKindRef widening + typed-relation payloads.
    // -----------------------------------------------------------------

    #[test]
    fn widened_link_payload_round_trips_memory_to_memory() {
        let p = WalPayload::Link(LinkPayload {
            source: mnode(1),
            target: mnode(2),
            edge_kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: EdgeOrigin::Explicit,
        });
        let bytes = p.encode_to_bytes();
        // 17 + 17 + 2 + 4 + 1 = 41 bytes.
        assert_eq!(bytes.len(), 41);
        assert_eq!(WalPayload::decode(WalRecordKind::Link, &bytes).unwrap(), p);
    }

    #[test]
    fn widened_link_payload_round_trips_memory_to_entity() {
        let p = WalPayload::Link(LinkPayload {
            source: mnode(7),
            target: enode(0xAB),
            edge_kind: EdgeKindRef::Mentions,
            weight: 1.0,
            origin: EdgeOrigin::AutoDerived,
        });
        let bytes = p.encode_to_bytes();
        // 17 + 17 + 1 + 4 + 1 = 40 bytes (Mentions is the shortest kind).
        assert_eq!(bytes.len(), 40);
        assert_eq!(WalPayload::decode(WalRecordKind::Link, &bytes).unwrap(), p);
    }

    #[test]
    fn widened_link_payload_round_trips_entity_to_entity() {
        let p = WalPayload::Link(LinkPayload {
            source: enode(0xAA),
            target: enode(0xBB),
            edge_kind: EdgeKindRef::Typed(rtype(0xCC)),
            weight: 0.42,
            origin: EdgeOrigin::Explicit,
        });
        let bytes = p.encode_to_bytes();
        // 17 + 17 + 5 + 4 + 1 = 44 bytes (Typed kind = tag(1) + RelationTypeId(4)).
        assert_eq!(bytes.len(), 44);
        assert_eq!(WalPayload::decode(WalRecordKind::Link, &bytes).unwrap(), p);
    }

    #[test]
    fn widened_edge_payload_in_encode_round_trips_all_node_combos() {
        let cases = vec![
            (mnode(1), mnode(2), EdgeKindRef::Builtin(EdgeKind::Caused)),
            (mnode(1), enode(0x11), EdgeKindRef::Mentions),
            (enode(0x20), enode(0x21), EdgeKindRef::Typed(rtype(0x22))),
            (
                enode(0x30),
                mnode(99),
                EdgeKindRef::Builtin(EdgeKind::References),
            ),
        ];
        let edges: Vec<EdgePayload> = cases
            .into_iter()
            .map(|(source, target, kind)| EdgePayload {
                source,
                target,
                kind,
                weight: 0.5,
                origin: EdgeOrigin::Explicit,
            })
            .collect();
        let p = WalPayload::Encode(EncodePayload {
            memory_id: mid(1),
            request_id: rid(1),
            agent_id: aid(1),
            context_id: ContextId(0),
            kind: MemoryKind::Episodic,
            salience_initial: 0.5,
            embedding_model_fp: fp(0),
            text: "ENCODE".into(),
            vector: vec![],
            edges,
            request_hash: [0; 32],
            response_payload: vec![],
            deduplicate: false,
            occurred_at_unix_nanos: None,
        });
        let bytes = p.encode_to_bytes();
        assert_eq!(
            WalPayload::decode(WalRecordKind::Encode, &bytes).unwrap(),
            p
        );
    }

    fn arb_node_ref() -> impl Strategy<Value = NodeRef> {
        prop_oneof![
            proptest::array::uniform16(any::<u8>())
                .prop_map(|b| NodeRef::Memory(MemoryId::from_be_bytes(b))),
            proptest::array::uniform16(any::<u8>())
                .prop_map(|b| NodeRef::Entity(EntityId::from_bytes(b))),
        ]
    }

    fn arb_edge_kind_ref() -> impl Strategy<Value = EdgeKindRef> {
        prop_oneof![
            (0u8..=7).prop_map(|b| {
                let k = match b {
                    0 => EdgeKind::Caused,
                    1 => EdgeKind::FollowedBy,
                    2 => EdgeKind::DerivedFrom,
                    3 => EdgeKind::SimilarTo,
                    4 => EdgeKind::Contradicts,
                    5 => EdgeKind::Supports,
                    6 => EdgeKind::References,
                    _ => EdgeKind::PartOf,
                };
                EdgeKindRef::Builtin(k)
            }),
            Just(EdgeKindRef::Mentions),
            any::<u32>().prop_map(|n| EdgeKindRef::Typed(RelationTypeId::from(n))),
        ]
    }

    fn arb_edge_origin() -> impl Strategy<Value = EdgeOrigin> {
        prop_oneof![Just(EdgeOrigin::Explicit), Just(EdgeOrigin::AutoDerived)]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

        #[test]
        fn link_payload_round_trips_arbitrary(
            source in arb_node_ref(),
            target in arb_node_ref(),
            edge_kind in arb_edge_kind_ref(),
            weight in proptest::num::f32::NORMAL | proptest::num::f32::ZERO,
            origin in arb_edge_origin(),
        ) {
            let p = WalPayload::Link(LinkPayload { source, target, edge_kind, weight, origin });
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(WalPayload::decode(WalRecordKind::Link, &bytes).unwrap(), p);
        }

        #[test]
        fn unlink_payload_round_trips_arbitrary(
            source in arb_node_ref(),
            target in arb_node_ref(),
            edge_kind in arb_edge_kind_ref(),
            edge_seq in any::<u32>(),
        ) {
            let p = WalPayload::Unlink(UnlinkPayload { source, target, edge_kind, edge_seq });
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(WalPayload::decode(WalRecordKind::Unlink, &bytes).unwrap(), p);
        }

        #[test]
        fn edge_payload_in_encode_round_trips_arbitrary(
            edges in proptest::collection::vec(
                (
                    arb_node_ref(),
                    arb_node_ref(),
                    arb_edge_kind_ref(),
                    proptest::num::f32::NORMAL | proptest::num::f32::ZERO,
                    arb_edge_origin(),
                )
                    .prop_map(|(source, target, kind, weight, origin)| EdgePayload { source, target, kind, weight, origin }),
                0..6,
            ),
        ) {
            let p = WalPayload::Encode(EncodePayload {
                memory_id: mid(1),
                request_id: rid(0),
                agent_id: aid(0),
                context_id: ContextId(0),
                kind: MemoryKind::Episodic,
                salience_initial: 0.0,
                embedding_model_fp: fp(0),
                text: String::new(),
                vector: vec![],
                edges,
                request_hash: [0; 32],
                response_payload: vec![],
                deduplicate: false,
                occurred_at_unix_nanos: None,
            });
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(WalPayload::decode(WalRecordKind::Encode, &bytes).unwrap(), p);
        }

        #[test]
        fn relation_link_round_trips_arbitrary(
            from in arb_node_ref(),
            to in arb_node_ref(),
            confidence in proptest::num::f32::NORMAL | proptest::num::f32::ZERO,
            valid_from in proptest::option::of(any::<u64>()),
            valid_to in proptest::option::of(any::<u64>()),
            evidence_ids in proptest::collection::vec(any::<u64>(), 0..8),
            extractor_id in any::<u32>(),
            is_symmetric in any::<bool>(),
            properties_blob in proptest::collection::vec(any::<u8>(), 0..32),
            relation_type_id in any::<u32>(),
        ) {
            let evidence: Vec<MemoryId> = evidence_ids.into_iter().map(mid).collect();
            let rl = RelationLinkPayload {
                relation_id: relid(0x01),
                from,
                to,
                relation_type_id: RelationTypeId::from(relation_type_id),
                chain_root: relid(0x02),
                confidence,
                valid_from_unix_nanos: valid_from,
                valid_to_unix_nanos: valid_to,
                supersedes: None,
                evidence,
                extractor_id,
                is_symmetric,
                properties_blob,
                agent_id: aid(0x09),
                relation_type_intern_hint: None,
            };
            let p = WalPayload::RelationLink(rl);
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(WalPayload::decode(WalRecordKind::RelationCreate, &bytes).unwrap(), p);
        }

        /// `RelationSupersedePayload` carries an inline new
        /// `RelationLinkPayload`; the codec must preserve every field
        /// across the supersession boundary so recovery can flip the
        /// sidecar deterministically.
        #[test]
        fn relation_supersede_round_trips_arbitrary(
            from in arb_node_ref(),
            to in arb_node_ref(),
            evidence_ids in proptest::collection::vec(any::<u64>(), 0..4),
            properties_blob in proptest::collection::vec(any::<u8>(), 0..16),
            old_rid_byte in any::<u8>(),
            new_rid_byte in any::<u8>(),
        ) {
            let mut new_link = sample_relation_link();
            new_link.from = from;
            new_link.to = to;
            new_link.evidence = evidence_ids.into_iter().map(mid).collect();
            new_link.properties_blob = properties_blob;
            new_link.relation_id = relid(new_rid_byte);
            let p = WalPayload::RelationSupersede(RelationSupersedePayload {
                old_relation_id: relid(old_rid_byte),
                new: new_link,
            });
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(
                WalPayload::decode(WalRecordKind::RelationSupersede, &bytes).unwrap(),
                p,
            );
        }

        /// Tombstone carries a free-form reason string; the codec
        /// must preserve arbitrary UTF-8 bytes.
        #[test]
        fn relation_tombstone_round_trips_arbitrary(
            rid_byte in any::<u8>(),
            reason in ".*",
            at in any::<u64>(),
            agent_byte in any::<u8>(),
        ) {
            let p = WalPayload::RelationTombstone(RelationTombstonePayload {
                relation_id: relid(rid_byte),
                reason,
                at_unix_nanos: at,
                agent_id: aid(agent_byte),
            });
            let bytes = p.encode_to_bytes();
            prop_assert_eq!(
                WalPayload::decode(WalRecordKind::RelationTombstone, &bytes).unwrap(),
                p,
            );
        }
    }

    #[test]
    fn forget_payload_agent_id_round_trips() {
        // The new `agent_id` field on ForgetPayload must survive a
        // full encode_to_bytes → decode round-trip so subscribe-
        // replay can populate EventEnvelope.agent_id from a
        // non-default value (the live-publish path already stamps
        // it from the writer; replay used to fall back to nil and
        // silently drop forgets for any `agents`-filtered
        // subscriber).
        let agent = aid(0x42);
        let payload = WalPayload::Forget(ForgetPayload {
            memory_id: mid(99),
            request_id: rid(7),
            agent_id: agent,
            mode: ForgetMode::Hard,
            reason: ForgetReason::ClientRequest,
        });
        let bytes = payload.encode_to_bytes();
        match WalPayload::decode(WalRecordKind::Forget, &bytes).unwrap() {
            WalPayload::Forget(p) => {
                assert_eq!(p.agent_id, agent);
                assert_eq!(p.memory_id, mid(99));
                assert_eq!(p.mode, ForgetMode::Hard);
                assert_eq!(p.reason, ForgetReason::ClientRequest);
            }
            other => panic!("expected Forget, got {other:?}"),
        }
    }
}
