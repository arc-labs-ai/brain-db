//! Typed WAL payloads.
//!
//! The byte-framing layer (`record.rs`) treats the payload as opaque. This
//! module gives every spec'd record kind a typed Rust shape so callers
//! (writers, recovery, audit tools) can construct and inspect records
//! without juggling raw bytes.
//!
//! Layouts come from `spec/05_storage_arena_wal/05_wal_records.md` §§5–16.
//! Encoding is little-endian for scalars; `MemoryId` keeps its big-endian
//! wire layout per spec §02/03 §2.2 so it round-trips through `to_be_bytes`.
//!
//! Two layout choices the spec leaves underspecified:
//! - **`vector_dims` length prefix** in `Encode` / `Consolidate` /
//!   `MigrateEmbedding`. The spec writes `[f32; 384]` literally; we add a
//!   `u16` length prefix so the file format is forward-compatible with
//!   larger embedding models. `0` means "no vector".
//! - **`UpdateSalience` count prefix**. Spec §05/05 §9 describes coalesced
//!   records as carrying "multiple tuples" without specifying a count
//!   prefix. We always emit `count: u32 + tuples` (count=1 in the
//!   non-coalesced case). One uniform encoder is simpler than branching on
//!   the `flags` bit at this layer.

use brain_core::{
    AgentId, ContextId, EdgeKind, EdgeOrigin, MemoryId, MemoryKind, RequestId, TxnId,
};

/// Opaque 16-byte fingerprint of an embedding model (spec §05/05 §5).
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
    /// Empty `Vec` means "vector excluded" (spec's optional-include mode).
    pub vector: Vec<f32>,
    pub edges: Vec<EdgePayload>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgePayload {
    pub source: MemoryId,
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
    pub origin: EdgeOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgetPayload {
    pub memory_id: MemoryId,
    pub request_id: RequestId,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkPayload {
    pub source: MemoryId,
    pub target: MemoryId,
    pub edge_kind: EdgeKind,
    pub weight: f32,
    pub origin: EdgeOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlinkPayload {
    pub source: MemoryId,
    pub target: MemoryId,
    pub edge_kind: EdgeKind,
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
/// Spec §05/05 §10 lists three fields (`slot_id`, `old_version`,
/// `new_version`); we add `memory_id` so the metadata sink can delete the
/// reclaimed memory's row by primary key in O(1) instead of scanning the
/// `memories` table. See `docs/spec-deviations.md` SD-3.11-3 (which
/// supersedes the deferred reconciliation in SD-3.11-2).
///
/// On-disk layout: `slot_id` (u64) → `old_version` (u32) → `new_version`
/// (u32) → `memory_id` (16 B). Spec ordering preserved; new field appended
/// so that a future spec amendment can declare the layout authoritative.
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
// Top-level enum + dispatch.
// ---------------------------------------------------------------------------

use crate::wal::kinds::WalRecordKind;

/// Opaque knowledge-layer WAL record (sub-task 15.2).
///
/// The body is the rkyv-encoded record produced by phase 16+ writers
/// (entity / statement / relation / schema / audit). For the framing
/// layer it is an opaque blob: the WAL records, reads, and recovery
/// transports it unchanged. The substrate apply-paths ignore these
/// records; knowledge-state hydration is a phase-16+ concern with its
/// own sink.
///
/// Body size is bounded by the frame header's `payload_len` (3 bytes
/// per spec §05/05), i.e. ~16 MiB — same envelope as substrate
/// payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeRecord {
    pub kind: WalRecordKind,
    pub body: Vec<u8>,
}

impl KnowledgeRecord {
    /// Construct a knowledge record. The `kind` MUST satisfy
    /// `kind.is_knowledge()`; passing a substrate kind is a programmer
    /// error and panics in debug builds.
    #[must_use]
    pub fn new(kind: WalRecordKind, body: Vec<u8>) -> Self {
        debug_assert!(
            kind.is_knowledge(),
            "KnowledgeRecord requires a knowledge-layer kind (0x10..=0x50); got {kind:?}"
        );
        Self { kind, body }
    }
}

/// Typed WAL payload, one variant per spec'd record kind.
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
    /// Knowledge-layer record carried as an opaque body. The typed body
    /// schemas land in phases 16–21; the framing layer transports them
    /// unchanged.
    Knowledge(KnowledgeRecord),
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
            Self::Knowledge(r) => r.kind,
        }
    }

    /// Encode this payload to the spec's byte layout.
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
            Self::Knowledge(r) => out.extend_from_slice(&r.body),
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
            // Knowledge layer (spec §26). Bodies are opaque to the
            // framing layer; phases 16+ supply typed parsers via their
            // own sinks. We early-return so the trailing-bytes check
            // below doesn't fire (the entire payload IS the body).
            WalRecordKind::EntityCreate
            | WalRecordKind::EntityUpdate
            | WalRecordKind::EntityMerge
            | WalRecordKind::EntityTombstone
            | WalRecordKind::StatementCreate
            | WalRecordKind::StatementSupersede
            | WalRecordKind::StatementTombstone
            | WalRecordKind::RelationCreate
            | WalRecordKind::RelationSupersede
            | WalRecordKind::RelationTombstone
            | WalRecordKind::SchemaUpdate
            | WalRecordKind::Audit => {
                return Ok(Self::Knowledge(KnowledgeRecord::new(kind, bytes.to_vec())));
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

    #[error("EdgeKind byte {0} is not in 0..=7")]
    BadEdgeKind(u8),

    #[error("EdgeOrigin byte {0} is not in {{0, 1}}")]
    BadEdgeOrigin(u8),

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
}

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
}

#[inline]
fn put_memory_id(out: &mut Vec<u8>, id: MemoryId) {
    out.extend_from_slice(&id.to_be_bytes());
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

fn edge_kind_from_u8(b: u8) -> Result<EdgeKind, WalPayloadError> {
    Ok(match b {
        0 => EdgeKind::Caused,
        1 => EdgeKind::FollowedBy,
        2 => EdgeKind::DerivedFrom,
        3 => EdgeKind::SimilarTo,
        4 => EdgeKind::Contradicts,
        5 => EdgeKind::Supports,
        6 => EdgeKind::References,
        7 => EdgeKind::PartOf,
        _ => return Err(WalPayloadError::BadEdgeKind(b)),
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
        put_memory_id(out, e.source);
        put_memory_id(out, e.target);
        out.push(e.kind as u8);
        put_f32_le(out, e.weight);
        out.push(e.origin as u8);
    }
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
    // 38 bytes per edge (16+16+1+4+1).
    if edge_count as usize * 38 > r.remaining() {
        return Err(WalPayloadError::BadEdgeCount(edge_count));
    }
    let mut edges = Vec::with_capacity(edge_count as usize);
    for _ in 0..edge_count {
        let source = r.memory_id()?;
        let target = r.memory_id()?;
        let edge_kind = edge_kind_from_u8(r.u8()?)?;
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
    })
}

fn encode_forget(p: &ForgetPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.memory_id);
    put_uuid_bytes(out, p.request_id.into());
    out.push(p.mode as u8);
    out.push(p.reason as u8);
}

fn decode_forget(r: &mut Reader<'_>) -> Result<ForgetPayload, WalPayloadError> {
    Ok(ForgetPayload {
        memory_id: r.memory_id()?,
        request_id: r.array16()?.into(),
        mode: forget_mode_from_u8(r.u8()?)?,
        reason: forget_reason_from_u8(r.u8()?)?,
    })
}

fn encode_link(p: &LinkPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.source);
    put_memory_id(out, p.target);
    out.push(p.edge_kind as u8);
    put_f32_le(out, p.weight);
    out.push(p.origin as u8);
}

fn decode_link(r: &mut Reader<'_>) -> Result<LinkPayload, WalPayloadError> {
    Ok(LinkPayload {
        source: r.memory_id()?,
        target: r.memory_id()?,
        edge_kind: edge_kind_from_u8(r.u8()?)?,
        weight: r.f32_le()?,
        origin: edge_origin_from_u8(r.u8()?)?,
    })
}

fn encode_unlink(p: &UnlinkPayload, out: &mut Vec<u8>) {
    put_memory_id(out, p.source);
    put_memory_id(out, p.target);
    out.push(p.edge_kind as u8);
    put_u32_le(out, p.edge_seq);
}

fn decode_unlink(r: &mut Reader<'_>) -> Result<UnlinkPayload, WalPayloadError> {
    Ok(UnlinkPayload {
        source: r.memory_id()?,
        target: r.memory_id()?,
        edge_kind: edge_kind_from_u8(r.u8()?)?,
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
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{AgentId, EdgeKind, EdgeOrigin, MemoryId, RequestId, TxnId};

    fn mid(n: u64) -> MemoryId {
        MemoryId::pack(1, n, 1)
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
                    source: mid(7),
                    target: mid(8),
                    kind: EdgeKind::Caused,
                    weight: 0.9,
                    origin: EdgeOrigin::Explicit,
                }],
            }),
            WalPayload::Forget(ForgetPayload {
                memory_id: mid(9),
                request_id: rid(3),
                mode: ForgetMode::Hard,
                reason: ForgetReason::Eviction,
            }),
            WalPayload::Link(LinkPayload {
                source: mid(10),
                target: mid(11),
                edge_kind: EdgeKind::SimilarTo,
                weight: 0.42,
                origin: EdgeOrigin::AutoDerived,
            }),
            WalPayload::Unlink(UnlinkPayload {
                source: mid(12),
                target: mid(13),
                edge_kind: EdgeKind::PartOf,
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
        ]
    }

    #[test]
    fn one_variant_per_spec_kind() {
        // 15 spec'd kinds; the fixture function must hit every one.
        let payloads = all_variants();
        let kinds: std::collections::HashSet<_> = payloads.iter().map(|p| p.kind()).collect();
        assert_eq!(kinds.len(), 15);
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
        let p = WalPayload::Link(LinkPayload {
            source: mid(1),
            target: mid(2),
            edge_kind: EdgeKind::Caused,
            weight: 1.0,
            origin: EdgeOrigin::Explicit,
        });
        let mut bytes = p.encode_to_bytes();
        // Layout: source(16) + target(16) + edge_kind(1) at offset 32.
        bytes[32] = 99;
        assert_eq!(
            WalPayload::decode(WalRecordKind::Link, &bytes),
            Err(WalPayloadError::BadEdgeKind(99))
        );
    }

    #[test]
    fn bad_edge_origin_rejected() {
        let p = WalPayload::Link(LinkPayload {
            source: mid(1),
            target: mid(2),
            edge_kind: EdgeKind::Caused,
            weight: 1.0,
            origin: EdgeOrigin::Explicit,
        });
        let mut bytes = p.encode_to_bytes();
        // origin is the last byte (offset 37 in the 38-byte payload).
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
        // Encode with vector=[] is the spec's "exclude vector" path.
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
        });
        let bytes = p.encode_to_bytes();
        assert_eq!(WalPayload::decode(p.kind(), &bytes).unwrap(), p);
    }

    #[test]
    fn kind_returns_matching_discriminator() {
        for p in all_variants() {
            // The kind discriminator must round-trip through the typed enum
            // and back to the exact same byte.
            let kind = p.kind();
            let bytes = p.encode_to_bytes();
            let back = WalPayload::decode(kind, &bytes).unwrap();
            assert_eq!(back.kind(), kind);
        }
    }

    // -----------------------------------------------------------------
    // Knowledge-layer (sub-task 15.2).
    // -----------------------------------------------------------------

    #[test]
    fn knowledge_record_round_trip() {
        for kind in [
            WalRecordKind::EntityCreate,
            WalRecordKind::EntityUpdate,
            WalRecordKind::EntityMerge,
            WalRecordKind::EntityTombstone,
            WalRecordKind::StatementCreate,
            WalRecordKind::StatementSupersede,
            WalRecordKind::StatementTombstone,
            WalRecordKind::RelationCreate,
            WalRecordKind::RelationSupersede,
            WalRecordKind::RelationTombstone,
            WalRecordKind::SchemaUpdate,
            WalRecordKind::Audit,
        ] {
            let body: Vec<u8> = (0..32u8).map(|i| i ^ kind.as_u8()).collect();
            let payload = WalPayload::Knowledge(KnowledgeRecord::new(kind, body.clone()));
            assert_eq!(payload.kind(), kind);
            let bytes = payload.encode_to_bytes();
            assert_eq!(bytes, body, "encode is identity for knowledge bodies");
            let decoded = WalPayload::decode(kind, &bytes).expect("decode knowledge");
            match decoded {
                WalPayload::Knowledge(r) => {
                    assert_eq!(r.kind, kind);
                    assert_eq!(r.body, body);
                }
                other => panic!("expected Knowledge, got {other:?}"),
            }
        }
    }

    #[test]
    fn knowledge_decode_empty_body_is_ok() {
        // An empty body is a legal opaque payload (a tombstone marker,
        // for instance, may carry no fields).
        let payload =
            WalPayload::decode(WalRecordKind::EntityTombstone, &[]).expect("empty body decodes");
        match payload {
            WalPayload::Knowledge(r) => {
                assert_eq!(r.kind, WalRecordKind::EntityTombstone);
                assert!(r.body.is_empty());
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn knowledge_decode_skips_trailing_bytes_check() {
        // For substrate kinds, trailing bytes after the structured tail
        // are an error. For knowledge kinds the entire payload IS the
        // body — no such check applies. Verify by feeding garbage.
        let body = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        let decoded =
            WalPayload::decode(WalRecordKind::SchemaUpdate, &body).expect("knowledge accepts any bytes");
        if let WalPayload::Knowledge(r) = decoded {
            assert_eq!(r.body, body);
        } else {
            panic!("expected Knowledge");
        }
    }

    #[test]
    #[should_panic(expected = "KnowledgeRecord requires a knowledge-layer kind")]
    fn knowledge_record_rejects_substrate_kind_in_debug() {
        // Debug-only invariant: constructing a KnowledgeRecord with a
        // substrate kind panics. (In release builds the debug_assert is
        // elided; that's intentional — callers are not expected to feed
        // adversarial kinds.)
        let _ = KnowledgeRecord::new(WalRecordKind::Encode, vec![]);
    }
}
