//! `memories` table: per-memory metadata.
//!
//! Row layout is ~140 bytes/row.
//!
//! ## Storage representation
//!
//! `MemoryMetadata` derives `rkyv::Archive`/`Serialize`/`Deserialize`.
//! Brain-core types (`MemoryId`, `SpaceId`, `MemoryKind`) don't derive
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

use std::ops::Bound;

use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
use redb::{ReadTransaction, TableDefinition};

use crate::tables::scope::RowScope;

// ---------------------------------------------------------------------------
// Table definition.
// ---------------------------------------------------------------------------

/// The `memories` table. Key is the `MemoryId`'s 16-byte big-endian wire
/// form; value is [`MemoryMetadata`].
pub const MEMORIES_TABLE: TableDefinition<'static, [u8; 16], MemoryMetadata> =
    TableDefinition::new("memories");

/// Secondary timeline index keyed `(space_id_bytes, created_at_unix_nanos
/// BE bytes, session_id BE bytes, memory_id BE bytes)` → `()`.
///
/// The TemporalEdgeWorker (`FollowedBy` auto-derivation) needs
/// to answer "most-recent memory by space A within context C and
/// timestamp window" cheaply. Without this index the worker would
/// either full-scan `MEMORIES_TABLE` per encode or maintain an
/// in-memory shadow that can't survive recovery.
///
/// Key layout (48 bytes total) is chosen so a backward range scan
/// from `(space, t_now, ctx, *)` yields rows in descending-time order;
/// the worker stops at the first hit that satisfies its window. The
/// trailing `memory_id` is purely a disambiguator so two memories
/// committed at exactly the same nanos can both index without
/// collision.
///
/// On encode commit the writer inserts a row here; on forget /
/// tombstone the writer must delete it so a tombstoned memory never
/// surfaces as a temporal predecessor.
pub const MEMORIES_BY_SPACE_TIMELINE_TABLE: TableDefinition<'static, &[u8], ()> =
    TableDefinition::new("memories_by_space_timeline");

/// Encoded length:
/// `namespace(4) + space(16) + created_at(8) + context(8) + memory_id(16)`.
/// The leading `namespace_id` makes each tenant's timeline a contiguous
/// keyspace, so a scan for one `(namespace, space)` never traverses
/// another tenant's rows.
pub const SPACE_TIMELINE_KEY_LEN: usize = 4 + 16 + 8 + 8 + 16;

/// Pack the discriminators into the canonical key bytes.
/// `created_at_unix_nanos` is encoded big-endian so a redb range
/// scan in lexicographic order yields chronological order.
#[must_use]
pub fn space_timeline_key(
    namespace_id: u32,
    space_id_bytes: [u8; 16],
    created_at_unix_nanos: u64,
    session_id: u64,
    memory_id_bytes: [u8; 16],
) -> [u8; SPACE_TIMELINE_KEY_LEN] {
    let mut k = [0u8; SPACE_TIMELINE_KEY_LEN];
    k[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    k[4..20].copy_from_slice(&space_id_bytes);
    k[20..28].copy_from_slice(&created_at_unix_nanos.to_be_bytes());
    k[28..36].copy_from_slice(&session_id.to_be_bytes());
    k[36..52].copy_from_slice(&memory_id_bytes);
    k
}

/// Prefix matching every row for a `(namespace, space)` — useful for
/// cleanup and for the in-context worker scan that further narrows by
/// `created_at`.
#[must_use]
pub fn space_timeline_prefix_space(namespace_id: u32, space_id_bytes: [u8; 16]) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    p[4..20].copy_from_slice(&space_id_bytes);
    p
}

/// Prefix matching every row for (namespace, space, time) — useful for
/// the worker's "what was the predecessor" probe (a backward range scan
/// stops at the first key strictly less than the prefix).
#[must_use]
pub fn space_timeline_prefix_space_time(
    namespace_id: u32,
    space_id_bytes: [u8; 16],
    created_at_unix_nanos: u64,
) -> [u8; 28] {
    let mut p = [0u8; 28];
    p[0..4].copy_from_slice(&namespace_id.to_be_bytes());
    p[4..20].copy_from_slice(&space_id_bytes);
    p[20..28].copy_from_slice(&created_at_unix_nanos.to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// MEMORY_LIST keyset enumeration.
// ---------------------------------------------------------------------------

/// Errors from the memory enumeration scan.
#[derive(thiserror::Error, Debug)]
pub enum MemoryListError {
    #[error("redb table: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage: {0}")]
    Storage(#[from] redb::StorageError),
}

/// Row-level predicates applied during a [`memory_timeline_page`] scan.
/// Time bounds are on the `created_at` axis (the axis the timeline index
/// orders by); the `occurred_at` axis has no memory index yet.
#[derive(Clone, Debug, Default)]
pub struct MemoryTimelineFilter {
    /// Empty = all kinds; otherwise only these raw kind bytes (0/1/2) pass.
    pub kinds: Vec<u8>,
    /// When false, tombstoned rows are skipped.
    pub include_tombstoned: bool,
    /// Inclusive `created_at` lower bound (unix-nanos); `None` = no bound.
    pub created_from: Option<u64>,
    /// Inclusive `created_at` upper bound (unix-nanos); `None` = no bound.
    pub created_to: Option<u64>,
    /// Inclusive salience floor.
    pub salience_min: f32,
    /// Inclusive salience ceiling.
    pub salience_max: f32,
}

impl MemoryTimelineFilter {
    /// True when `row` passes every predicate.
    fn admits(&self, row: &MemoryMetadata) -> bool {
        if !self.include_tombstoned && !row.is_active() {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&row.kind) {
            return false;
        }
        if let Some(from) = self.created_from {
            if row.created_at_unix_nanos < from {
                return false;
            }
        }
        if let Some(to) = self.created_to {
            if row.created_at_unix_nanos > to {
                return false;
            }
        }
        if row.salience < self.salience_min || row.salience > self.salience_max {
            return false;
        }
        true
    }
}

/// One page of the timeline enumeration: the matching rows plus a flag
/// telling the caller whether more matching rows exist beyond this page.
pub struct MemoryTimelinePage {
    pub rows: Vec<MemoryMetadata>,
    /// True when the scan found at least one more matching row after the
    /// page limit — the caller should mint a resume cursor from
    /// [`Self::last_key`].
    pub has_more: bool,
    /// The exact timeline-index key bytes of the last returned row. The
    /// resume cursor MUST be built from this, not reconstructed from the
    /// row's fields: the index key and the row can disagree on
    /// `created_at_unix_nanos` (they are stamped from separate reads at
    /// write time), and a reconstructed key would land between real keys —
    /// breaking the exclusive-resume boundary (descending re-emits it).
    pub last_key: Option<[u8; SPACE_TIMELINE_KEY_LEN]>,
}

/// Lexicographic successor of `prefix`: the smallest byte string strictly
/// greater than every string that starts with `prefix`. `None` when the
/// prefix is all `0xFF` (no finite successor — the range is unbounded
/// above). Used to bound a prefix scan without a trailing sentinel.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last != 0xFF {
            *last += 1;
            return Some(out);
        }
        out.pop();
    }
    None
}

/// Keyset (seek) page over one `(namespace, space)`'s memory timeline.
///
/// Ranges [`MEMORIES_BY_SPACE_TIMELINE_TABLE`] within the tenant's
/// contiguous keyspace and, for each timeline key, loads the memory row
/// and applies `filter`. `descending` walks newest-first; `after_key`,
/// when present, resumes strictly after that timeline key (the previous
/// page's last key) — so page N costs the same as page 1 and pages stay
/// stable under concurrent writes.
///
/// Returns up to `limit` rows in scan order and a `has_more` flag (the
/// scan peeks one row past the limit to set it). The trailing
/// `memory_id` in the timeline key disambiguates rows written at the
/// same nanosecond, so pagination is exact even under created-time ties.
pub fn memory_timeline_page(
    rtxn: &ReadTransaction,
    scope: RowScope,
    descending: bool,
    after_key: Option<&[u8]>,
    limit: usize,
    filter: &MemoryTimelineFilter,
) -> Result<MemoryTimelinePage, MemoryListError> {
    let timeline_t = rtxn.open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)?;
    let memories_t = rtxn.open_table(MEMORIES_TABLE)?;

    let prefix = space_timeline_prefix_space(scope.namespace_id, scope.space_id_bytes).to_vec();
    let upper = prefix_successor(&prefix);

    // Build the range bounds. The scan is always confined to the tenant's
    // 20-byte `(namespace, space)` prefix; `after_key` narrows one end so
    // the resume is strictly exclusive of the previous page's last key.
    let (lower_bound, upper_bound): (Bound<&[u8]>, Bound<&[u8]>) = if descending {
        // Newest-first: keys below `after_key` (exclusive) down to the
        // tenant prefix.
        let lo = Bound::Included(prefix.as_slice());
        let hi = match after_key {
            Some(k) => Bound::Excluded(k),
            None => match &upper {
                Some(u) => Bound::Excluded(u.as_slice()),
                None => Bound::Unbounded,
            },
        };
        (lo, hi)
    } else {
        // Oldest-first: keys above `after_key` (exclusive) up to the
        // tenant prefix successor.
        let lo = match after_key {
            Some(k) => Bound::Excluded(k),
            None => Bound::Included(prefix.as_slice()),
        };
        let hi = match &upper {
            Some(u) => Bound::Excluded(u.as_slice()),
            None => Bound::Unbounded,
        };
        (lo, hi)
    };

    let range = timeline_t.range::<&[u8]>((lower_bound, upper_bound))?;

    // Peek one past `limit` so `has_more` reflects a genuine next page,
    // not merely a full page.
    let mut rows: Vec<MemoryMetadata> = Vec::with_capacity(limit.min(128));
    let mut has_more = false;
    let mut last_key: Option<[u8; SPACE_TIMELINE_KEY_LEN]> = None;

    // A closure over one timeline entry: load the row, tenant-check,
    // filter, and either push or signal has_more. Returns true to stop.
    let mut consume = |key: &[u8]| -> Result<bool, MemoryListError> {
        if key.len() != SPACE_TIMELINE_KEY_LEN {
            return Ok(false);
        }
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&key[36..52]);
        let Some(row) = memories_t.get(&id_bytes)?.map(|g| g.value()) else {
            return Ok(false);
        };
        // Tenant wall (defense-in-depth): the range prefix already
        // isolates the scope, but re-check the row's own owner so a
        // corrupt index key can never leak a foreign row.
        if row.namespace_id != scope.namespace_id || row.space_id_bytes != scope.space_id_bytes {
            return Ok(false);
        }
        if !filter.admits(&row) {
            return Ok(false);
        }
        if rows.len() == limit {
            has_more = true;
            return Ok(true);
        }
        // Record the exact key bytes so the resume cursor is the real
        // index key (see `MemoryTimelinePage::last_key`).
        let mut kb = [0u8; SPACE_TIMELINE_KEY_LEN];
        kb.copy_from_slice(key);
        last_key = Some(kb);
        rows.push(row);
        Ok(false)
    };

    if descending {
        for entry in range.rev() {
            let (k, _) = entry?;
            if consume(k.value())? {
                break;
            }
        }
    } else {
        for entry in range {
            let (k, _) = entry?;
            if consume(k.value())? {
                break;
            }
        }
    }

    Ok(MemoryTimelinePage {
        rows,
        has_more,
        last_key,
    })
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
pub struct MemoryMetadata {
    // -- Identity --
    pub memory_id_bytes: [u8; 16],
    /// Owning namespace (tenant) — the outer half of the
    /// `(namespace, space)` scope key. `0` is the reserved `brain`
    /// system namespace; stamped by the writer via
    /// [`Self::new_active`].
    pub namespace_id: u32,
    pub space_id_bytes: [u8; 16],
    pub session_id: u64,
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
        namespace_id: NamespaceId,
        space_id: SpaceId,
        session_id: SessionId,
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
            // The owning tenant — required, never defaulted. The server
            // derives it from the authenticated connection's `(namespace,
            // space)` scope and threads it here; a row can never be built
            // without naming its namespace (fail-closed by construction).
            namespace_id: namespace_id.raw(),
            space_id_bytes: space_id.into(),
            session_id: session_id.raw(),
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
    pub fn space_id(&self) -> SpaceId {
        SpaceId::from(self.space_id_bytes)
    }

    #[must_use]
    pub fn namespace(&self) -> brain_core::NamespaceId {
        brain_core::NamespaceId::from(self.namespace_id)
    }

    #[must_use]
    pub fn session(&self) -> SessionId {
        SessionId(self.session_id)
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
        // rkyv validation (the `bytecheck` feature, auto-derived in 0.8)
        // includes an alignment check; redb returns bytes at arbitrary
        // alignment, so we copy into an AlignedVec first. Corrupt bytes here
        // indicate a broken redb file (much bigger problem than a single
        // row), so panic is the right failure mode.
        let mut buf = rkyv::util::AlignedVec::<16>::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<MemoryMetadata, rkyv::rancor::Error>(&buf)
            .expect("MemoryMetadata bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        // 256-byte scratch is roomy for the ~140-byte struct; rkyv grows
        // if needed.
        rkyv::to_bytes::<rkyv::rancor::Error>(value)
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
    use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
    use redb::{Database, ReadableDatabase};

    fn aid(byte: u8) -> SpaceId {
        let mut b = [0u8; 16];
        b[15] = byte;
        b.into()
    }

    fn sample(slot: u64) -> MemoryMetadata {
        MemoryMetadata::new_active(
            MemoryId::pack(1, slot, 1),
            NamespaceId::SYSTEM,
            aid(slot as u8),
            SessionId(0xCAFE),
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

    // ----- Keyset pagination: exclusive resume in both directions --------

    #[test]
    fn timeline_pagination_no_duplicates_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ns = NamespaceId::SYSTEM;
        let mut space_b = [0u8; 16];
        space_b[15] = 0x55;
        let space: SpaceId = space_b.into();
        const N: u64 = 25;
        const BASE: u64 = 1_700_000_000_000_000_000;

        let wtxn = db.begin_write().unwrap();
        {
            let mut mt = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let mut tt = wtxn.open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE).unwrap();
            for i in 0..N {
                // Reproduce the real write-path hazard: the timeline index key
                // and the memory row are stamped from SEPARATE `created_at`
                // reads a few nanos apart, so the key's created_at is NOT the
                // row's created_at. A resume cursor reconstructed from the
                // row's fields would then miss the real key and the descending
                // page would re-emit its boundary row. The key is built with
                // `key_created`; the row stores `row_created` (offset by +37).
                let key_created = BASE + i * 1_000;
                let row_created = key_created + 37;
                let m = MemoryMetadata::new_active(
                    MemoryId::pack(1, i, 1),
                    ns,
                    space,
                    SessionId(0),
                    i,
                    1,
                    MemoryKind::Episodic,
                    [0xAB; 16],
                    0.5,
                    42,
                    row_created,
                );
                mt.insert(&m.memory_id_bytes, &m).unwrap();
                let tk = space_timeline_key(
                    m.namespace_id,
                    m.space_id_bytes,
                    key_created, // deliberately != m.created_at_unix_nanos
                    m.session_id,
                    m.memory_id_bytes,
                );
                tt.insert(tk.as_slice(), &()).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let scope = RowScope::from_bytes(ns.raw(), space_b);
        let filter = MemoryTimelineFilter {
            salience_max: 1.0,
            ..Default::default()
        };

        // A full keyset walk must return every row exactly once regardless of
        // direction. The resume cursor is taken from `page.last_key` (the
        // real index key) — not reconstructed from the row — so the
        // key/row `created_at` divergence above does not corrupt the boundary.
        for descending in [false, true] {
            let mut ids: Vec<[u8; 16]> = Vec::new();
            let mut after: Option<[u8; SPACE_TIMELINE_KEY_LEN]> = None;
            loop {
                let page = memory_timeline_page(
                    &rtxn,
                    scope,
                    descending,
                    after.as_ref().map(|k| k.as_slice()),
                    10,
                    &filter,
                )
                .unwrap();
                for r in &page.rows {
                    ids.push(r.memory_id_bytes);
                }
                if !page.has_more {
                    break;
                }
                after = page.last_key;
            }
            let unique: std::collections::HashSet<_> = ids.iter().collect();
            assert_eq!(
                ids.len(),
                N as usize,
                "descending={descending}: returned {} rows, expected {N}",
                ids.len()
            );
            assert_eq!(
                unique.len(),
                N as usize,
                "descending={descending}: {} duplicate rows",
                ids.len() - unique.len()
            );
        }
    }

    #[test]
    fn brain_core_type_round_trip() {
        let memory_id = MemoryId::pack(7, 0x1234_5678, 42);
        let space_id = aid(0x33);
        let context = SessionId(99);

        let m = MemoryMetadata::new_active(
            memory_id,
            NamespaceId::SYSTEM,
            space_id,
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
        assert_eq!(m.space_id(), space_id);
        assert_eq!(m.session(), context);
        assert_eq!(m.kind().unwrap(), MemoryKind::Semantic);
    }
}
