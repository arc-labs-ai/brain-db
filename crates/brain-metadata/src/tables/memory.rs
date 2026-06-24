//! `memories` table: per-memory metadata.
//!
//! Row layout is ~140 bytes/row.
//!
//! ## Storage representation
//!
//! `MemoryMetadata` derives `rkyv::Archive`/`Serialize`/`Deserialize`.
//! Brain-core types (`MemoryId`, `AgentId`, `MemoryKind`) don't derive
//! rkyv — that would couple the data-model layer to a particular
//! encoding. Instead, the struct stores their byte representations
//! (`[u8; 16]`, `u64`, `u8`) and exposes typed getters that convert at
//! the API boundary.
//!
//! ## Deserialize-on-read, not zero-copy
//!
//! [`redb::Value::from_bytes`] returns an owned `MemoryMetadata`
//! (full rkyv deserialize) rather than rkyv's "zero-copy" path —
//! supplying a `&ArchivedMemoryMetadata` view into the redb-mmap'd
//! page. We defer that until profiling identifies a hot read path;
//! owned reads are simpler to reason about and test.

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Table definition.
// ---------------------------------------------------------------------------

/// The `memories` table. Key is the `MemoryId`'s 16-byte big-endian wire
/// form; value is [`MemoryMetadata`].
pub const MEMORIES_TABLE: TableDefinition<'static, [u8; 16], MemoryMetadata> =
    TableDefinition::new("memories");

/// Secondary timeline index keyed `(agent_id_bytes, created_at_unix_nanos
/// BE bytes, context_id BE bytes, memory_id BE bytes)` → `()`.
///
/// The TemporalEdgeWorker (`FollowedBy` auto-derivation) needs
/// to answer "most-recent memory by agent A within context C and
/// timestamp window" cheaply. Without this index the worker would
/// either full-scan `MEMORIES_TABLE` per encode or maintain an
/// in-memory shadow that can't survive recovery.
///
/// Key layout (48 bytes total) is chosen so a backward range scan
/// from `(agent, t_now, ctx, *)` yields rows in descending-time order;
/// the worker stops at the first hit that satisfies its window. The
/// trailing `memory_id` is purely a disambiguator so two memories
/// committed at exactly the same nanos can both index without
/// collision.
///
/// On encode commit the writer inserts a row here; on forget /
/// tombstone the writer must delete it so a tombstoned memory never
/// surfaces as a temporal predecessor.
pub const MEMORIES_BY_AGENT_TIMELINE_TABLE: TableDefinition<'static, &[u8], ()> =
    TableDefinition::new("memories_by_agent_timeline");

/// Encoded length: `agent(16) + created_at(8) + context(8) + memory_id(16)`.
pub const AGENT_TIMELINE_KEY_LEN: usize = 16 + 8 + 8 + 16;

/// Pack the four discriminators into the canonical key bytes.
/// `created_at_unix_nanos` is encoded big-endian so a redb range
/// scan in lexicographic order yields chronological order.
#[must_use]
pub fn agent_timeline_key(
    agent_id_bytes: [u8; 16],
    created_at_unix_nanos: u64,
    context_id: u64,
    memory_id_bytes: [u8; 16],
) -> [u8; AGENT_TIMELINE_KEY_LEN] {
    let mut k = [0u8; AGENT_TIMELINE_KEY_LEN];
    k[0..16].copy_from_slice(&agent_id_bytes);
    k[16..24].copy_from_slice(&created_at_unix_nanos.to_be_bytes());
    k[24..32].copy_from_slice(&context_id.to_be_bytes());
    k[32..48].copy_from_slice(&memory_id_bytes);
    k
}

/// Prefix matching every row for an agent — useful for cleanup and
/// for the in-context worker scan that further narrows by
/// `created_at`.
#[must_use]
pub fn agent_timeline_prefix_agent(agent_id_bytes: [u8; 16]) -> [u8; 16] {
    agent_id_bytes
}

/// Prefix matching every row for (agent, time, ctx) — useful for the
/// worker's "what was the predecessor" probe (a backward range scan
/// stops at the first key strictly less than the prefix).
#[must_use]
pub fn agent_timeline_prefix_agent_time(
    agent_id_bytes: [u8; 16],
    created_at_unix_nanos: u64,
) -> [u8; 24] {
    let mut p = [0u8; 24];
    p[0..16].copy_from_slice(&agent_id_bytes);
    p[16..24].copy_from_slice(&created_at_unix_nanos.to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// Flag bits.
// ---------------------------------------------------------------------------

pub mod flags {
    /// Bit 0: the memory is active (clear means tombstoned).
    pub const ACTIVE: u32 = 1 << 0;
    /// Bit 1: vector was zeroed by hard-forget.
    pub const HARD_FORGOTTEN: u32 = 1 << 1;
    /// Bit 2: memory is pinned (won't be auto-evicted).
    pub const PINNED: u32 = 1 << 2;
    /// Bit 3: vector is stale (model fingerprint changed; not re-embedded).
    pub const STALE: u32 = 1 << 3;
    /// Bits 4..=31 are reserved.
    pub const RESERVED_MASK: u32 = !(ACTIVE | HARD_FORGOTTEN | PINNED | STALE);
}

// ---------------------------------------------------------------------------
// MemoryKind ↔ u8 mapping.
// ---------------------------------------------------------------------------
//
// Duplicates `brain_storage::wal::payload::memory_kind_to_u8` (kept
// private there). If a third caller appears, promote to brain-core.

pub(crate) fn memory_kind_to_u8(k: MemoryKind) -> u8 {
    match k {
        MemoryKind::Episodic => 0,
        MemoryKind::Semantic => 1,
        MemoryKind::Consolidated => 2,
    }
}

#[allow(dead_code)] // used by `crate::sink` and tests
pub(crate) fn memory_kind_from_u8(b: u8) -> Result<MemoryKind, BadMemoryKind> {
    Ok(match b {
        0 => MemoryKind::Episodic,
        1 => MemoryKind::Semantic,
        2 => MemoryKind::Consolidated,
        other => return Err(BadMemoryKind::Invalid(other)),
    })
}

#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadMemoryKind {
    #[error("MemoryKind byte {0} is not in {{0, 1, 2}}")]
    Invalid(u8),
}

// ---------------------------------------------------------------------------
// MemoryMetadata.
// ---------------------------------------------------------------------------

/// Per-memory metadata row.
///
/// Fields are mostly `pub` because callers do read-modify-write inside a
/// redb transaction — wrapping every field in a
/// setter would add ceremony for no benefit. Typed wrappers for the
/// brain-core types come via getter methods (`memory_id()`, etc.).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MemoryMetadata {
    // -- Identity --
    pub memory_id_bytes: [u8; 16],
    pub agent_id_bytes: [u8; 16],
    pub context_id: u64,
    pub slot_id: u64,
    pub slot_version: u32,

    // -- Type and content --
    pub kind: u8,
    pub text_size: u32,

    // -- Temporal (unix nanoseconds) --
    pub created_at_unix_nanos: u64,
    pub last_accessed_at_unix_nanos: u64,
    pub forgot_at_unix_nanos: Option<u64>,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub consolidated_at_unix_nanos: Option<u64>,
    /// Client-supplied event time — when the memory's content actually
    /// happened, distinct from `created_at_unix_nanos` (server write
    /// time). `None` when the client didn't supply one. Echoed back on
    /// recall via `MemoryResult.occurred_at_unix_nanos`.
    pub occurred_at_unix_nanos: Option<u64>,

    // -- Salience --
    pub salience: f32,
    pub salience_initial: f32,
    pub access_count: u32,

    // -- Embedding --
    pub embedding_model_fp: [u8; 16],

    // -- Status flags (see [`flags`]) --
    pub flags: u32,

    // -- Denormalized edge counters --
    pub edges_out_count: u32,
    pub edges_in_count: u32,

    // -- Opt-in dedup index back-reference --
    /// `Some(BLAKE3(text)[..32])` iff this row was written by an
    /// ENCODE with `deduplicate = true` — used by `do_forget` /
    /// slot reclamation to evict the matching `FINGERPRINTS` row
    /// in the same write txn as the tombstone. `None` for the
    /// dedup-off path so we don't pay 32 B per row in
    /// no-schema deployments.
    pub content_hash: Option<[u8; 32]>,

    // -- Provenance: WAL position of the ENCODE that wrote this row --
    /// LSN of the `WalPayload::Encode` record that created this
    /// memory. `0` means "unknown" — either the writer has no WAL
    /// sink wired (test path) or the row predates the field
    /// (rkyv-schema break would surface that case differently;
    /// not a concern in the hard-cut shipping model).
    ///
    /// Surfaced through `RecallHit.encoded_at_lsn` →
    /// `MemoryResult.lsn`, letting a client chain
    /// `recall → subscribe --start-lsn lsn+1` to "follow this
    /// memory's downstream events from when it was written."
    pub encoded_at_lsn: u64,
}

impl MemoryMetadata {
    /// Construct a fresh active memory row.
    ///
    /// Sets `flags = ACTIVE`; all temporal optionals are `None`; salience
    /// equals `salience_initial`; access count is 0; edge counts are 0.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_active(
        memory_id: MemoryId,
        agent_id: AgentId,
        context_id: ContextId,
        slot_id: u64,
        slot_version: u32,
        kind: MemoryKind,
        embedding_model_fp: [u8; 16],
        salience_initial: f32,
        text_size: u32,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            memory_id_bytes: memory_id.to_be_bytes(),
            agent_id_bytes: agent_id.into(),
            context_id: context_id.raw(),
            slot_id,
            slot_version,
            kind: memory_kind_to_u8(kind),
            text_size,
            created_at_unix_nanos,
            last_accessed_at_unix_nanos: created_at_unix_nanos,
            forgot_at_unix_nanos: None,
            tombstoned_at_unix_nanos: None,
            consolidated_at_unix_nanos: None,
            occurred_at_unix_nanos: None,
            salience: salience_initial,
            salience_initial,
            access_count: 0,
            embedding_model_fp,
            flags: flags::ACTIVE,
            edges_out_count: 0,
            edges_in_count: 0,
            content_hash: None,
            // Default to 0 = "unknown LSN". Live writers stamp the
            // real value via `with_encoded_at_lsn` after they get
            // the LSN back from `wal_sink.append`. Test fixtures
            // that bypass the WAL get 0, which is fine — they
            // don't exercise the `RecallHit.encoded_at_lsn` flow.
            encoded_at_lsn: 0,
        }
    }

    /// Stamp the content hash on this row. Called by `do_encode`
    /// when the ENCODE opted in to fingerprint dedup; the value is
    /// later read by `do_forget` (and the slot-reclamation worker)
    /// to evict the matching `FINGERPRINTS` entry.
    pub fn with_content_hash(mut self, content_hash: [u8; 32]) -> Self {
        self.content_hash = Some(content_hash);
        self
    }

    /// Stamp the WAL LSN this memory was encoded at. Called by
    /// `do_encode` after `wal_sink.append` returns. Builder-style so
    /// the field defaults to `0` (unknown) and callers without WAL
    /// access don't need to thread a value through.
    pub fn with_encoded_at_lsn(mut self, lsn: u64) -> Self {
        self.encoded_at_lsn = lsn;
        self
    }

    /// Stamp the client-supplied event time on this row. Builder-style so
    /// `new_active` defaults it to `None` and callers that don't have an
    /// event time (the common case) need not thread a value through.
    #[must_use]
    pub fn with_occurred_at(mut self, occurred_at_unix_nanos: Option<u64>) -> Self {
        self.occurred_at_unix_nanos = occurred_at_unix_nanos;
        self
    }

    // ---- Typed accessors for the brain-core fields ----

    #[must_use]
    pub fn memory_id(&self) -> MemoryId {
        MemoryId::from_be_bytes(self.memory_id_bytes)
    }

    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        AgentId::from(self.agent_id_bytes)
    }

    #[must_use]
    pub fn context(&self) -> ContextId {
        ContextId(self.context_id)
    }

    pub fn kind(&self) -> Result<MemoryKind, BadMemoryKind> {
        memory_kind_from_u8(self.kind)
    }

    // ---- Flag helpers ----

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.flags & flags::ACTIVE != 0
    }
    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        !self.is_active()
    }
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.flags & flags::PINNED != 0
    }
    #[must_use]
    pub fn is_hard_forgotten(&self) -> bool {
        self.flags & flags::HARD_FORGOTTEN != 0
    }
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.flags & flags::STALE != 0
    }

    /// Set or clear a flag bit (or combination via `|`).
    pub fn set_flag(&mut self, mask: u32, on: bool) {
        if on {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }
}

// ---------------------------------------------------------------------------
// redb::Value impl (rkyv-backed; deserialize-on-read).
// ---------------------------------------------------------------------------

impl redb::Value for MemoryMetadata {
    type SelfType<'a> = MemoryMetadata;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        // rkyv-encoded bytes have alignment-driven variability; not fixed.
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        // `#[archive(check_bytes)]` enables validation, which includes an
        // alignment check; redb returns bytes at arbitrary alignment, so
        // we copy into an AlignedVec first. Corrupt bytes here indicate
        // a broken redb file (much bigger problem than a single row),
        // so panic is the right failure mode.
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<MemoryMetadata>(&buf)
            .expect("MemoryMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        // 256-byte scratch is roomy for the ~140-byte struct; rkyv grows
        // if needed.
        rkyv::to_bytes::<_, 256>(value)
            .expect("MemoryMetadata is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        // Embed schema version so type-confused mismatches surface early.
        redb::TypeName::new("brain_metadata::MemoryMetadata")
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> AgentId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn sample(slot: u64) -> MemoryMetadata {
        MemoryMetadata::new_active(
            MemoryId::pack(1, slot, 1),
            aid(slot as u8),
            ContextId(0xCAFE),
            slot,
            1,
            MemoryKind::Episodic,
            [0xAB; 16],
            0.5,
            42,
            1_700_000_000_000_000_000,
        )
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    // ----- Round-trip ----------------------------------------------------

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(7);
        let key = m.memory_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let row = t.get(&key).unwrap().expect("row present");
        assert_eq!(row.value(), m);
    }

    // ----- Flag + type logic (no DB) ------------------------------------

    #[test]
    fn flag_bit_manipulation() {
        let mut m = sample(1);
        assert!(m.is_active());
        m.set_flag(flags::ACTIVE, false);
        assert!(!m.is_active());
        assert!(m.is_tombstoned());

        m.set_flag(flags::PINNED, true);
        assert!(m.is_pinned());
        assert!(!m.is_active()); // unaffected

        m.set_flag(flags::HARD_FORGOTTEN | flags::STALE, true);
        assert!(m.is_hard_forgotten());
        assert!(m.is_stale());
    }

    #[test]
    fn brain_core_type_round_trip() {
        let memory_id = MemoryId::pack(7, 0x1234_5678, 42);
        let agent_id = aid(0x33);
        let context = ContextId(99);

        let m = MemoryMetadata::new_active(
            memory_id,
            agent_id,
            context,
            0x1234_5678,
            42,
            MemoryKind::Semantic,
            [0; 16],
            0.5,
            0,
            0,
        );
        assert_eq!(m.memory_id(), memory_id);
        assert_eq!(m.agent_id(), agent_id);
        assert_eq!(m.context(), context);
        assert_eq!(m.kind().unwrap(), MemoryKind::Semantic);
    }
}
