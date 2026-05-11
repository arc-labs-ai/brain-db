//! `memories` table: per-memory metadata.
//!
//! See `spec/07_metadata_graph/03_memory_table.md`. Row layout per §1
//! (~140 bytes/row), flags per §2.7, lifecycle per §10.
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
//! (full rkyv deserialize). Spec §07/02 §5 advertises rkyv's
//! "zero-copy" path — supplying a `&ArchivedMemoryMetadata` view into
//! the redb-mmap'd page. We defer that until profiling identifies a
//! hot read path; owned reads are simpler to reason about and test.

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Table definition.
// ---------------------------------------------------------------------------

/// The `memories` table. Key is the `MemoryId`'s 16-byte big-endian wire
/// form (per spec §02/03 §2.2); value is [`MemoryMetadata`].
pub const MEMORIES_TABLE: TableDefinition<'static, [u8; 16], MemoryMetadata> =
    TableDefinition::new("memories");

// ---------------------------------------------------------------------------
// Flag bits (spec §07/03 §2.7).
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

/// Per-memory metadata row. Spec §07/03 §1.
///
/// Fields are mostly `pub` because callers do read-modify-write inside a
/// redb transaction (spec §07/03 §4.1) — wrapping every field in a
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
            salience: salience_initial,
            salience_initial,
            access_count: 0,
            embedding_model_fp,
            flags: flags::ACTIVE,
            edges_out_count: 0,
            edges_in_count: 0,
        }
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
        redb::TypeName::new("brain_metadata::MemoryMetadata::v1")
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
    use redb::{Database, ReadableDatabase, ReadableTable};

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

    #[test]
    fn get_missing_key_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        // Open the table to materialize it; nothing inserted.
        {
            let wtxn = db.begin_write().unwrap();
            let _ = wtxn.open_table(MEMORIES_TABLE).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert!(t.get(&[0u8; 16]).unwrap().is_none());
    }

    #[test]
    fn update_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut m = sample(3);
        let key = m.memory_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        // Bump salience + access count.
        m.salience = 0.9;
        m.access_count = 5;
        m.last_accessed_at_unix_nanos = 1_700_000_000_000_000_500;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.salience, 0.9);
        assert_eq!(got.access_count, 5);
    }

    #[test]
    fn delete_removes_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let m = sample(10);
        let key = m.memory_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            assert!(t.remove(&key).unwrap().is_some());
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_none());
    }

    // ----- Scan-with-filter (spec §07/03 §3.2's v1 path) -----------------

    #[test]
    fn scan_filter_counts_by_agent_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        // 5 memories: 3 for agent A in context 1, 2 for agent B in context 2.
        let mut rows = Vec::new();
        let agent_a_bytes: [u8; 16] = aid(0xAA).into();
        let agent_b_bytes: [u8; 16] = aid(0xBB).into();
        for slot in 0..3u64 {
            let mut m = sample(slot);
            m.agent_id_bytes = agent_a_bytes;
            m.context_id = 1;
            rows.push(m);
        }
        for slot in 3..5u64 {
            let mut m = sample(slot);
            m.agent_id_bytes = agent_b_bytes;
            m.context_id = 2;
            rows.push(m);
        }

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for r in &rows {
                t.insert(&r.memory_id_bytes, r).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut count_a_c1 = 0u32;
        let mut count_b_c2 = 0u32;
        for entry in t.iter().unwrap() {
            let (_k, v) = entry.unwrap();
            let r = v.value();
            if r.agent_id_bytes == agent_a_bytes && r.context_id == 1 {
                count_a_c1 += 1;
            } else if r.agent_id_bytes == agent_b_bytes && r.context_id == 2 {
                count_b_c2 += 1;
            }
        }
        assert_eq!(count_a_c1, 3);
        assert_eq!(count_b_c2, 2);
    }

    // ----- Field round-trips --------------------------------------------

    #[test]
    fn option_u64_fields_survive_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut m = sample(1);
        m.forgot_at_unix_nanos = None;
        m.tombstoned_at_unix_nanos = Some(1_700_000_000_000_000_100);
        m.consolidated_at_unix_nanos = Some(1_700_000_000_000_000_200);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MEMORIES_TABLE).unwrap();
            t.insert(&m.memory_id_bytes, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        let got = t.get(&m.memory_id_bytes).unwrap().unwrap().value();
        assert_eq!(got.forgot_at_unix_nanos, None);
        assert_eq!(
            got.tombstoned_at_unix_nanos,
            Some(1_700_000_000_000_000_100)
        );
        assert_eq!(
            got.consolidated_at_unix_nanos,
            Some(1_700_000_000_000_000_200)
        );
    }

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

    // ----- Encoding stability -------------------------------------------

    #[test]
    fn same_input_same_bytes() {
        let m = sample(123);
        let bytes_a = <MemoryMetadata as redb::Value>::as_bytes(&m);
        let bytes_b = <MemoryMetadata as redb::Value>::as_bytes(&m);
        assert_eq!(bytes_a, bytes_b);
    }
}
