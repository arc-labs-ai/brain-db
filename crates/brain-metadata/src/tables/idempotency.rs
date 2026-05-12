//! `idempotency` table: cached responses for state-mutating requests.
//!
//! See `spec/07_metadata_graph/06_idempotency.md` (full — table shape,
//! lookup-then-act protocol, conflict detection via canonical-form hash,
//! 24h TTL, replay-not-re-execute, scope of idempotency-required ops).
//!
//! ## What lives here
//!
//! - [`IDEMPOTENCY_TABLE`] — `RequestId` → [`IdempotencyEntry`].
//! - [`IdempotencyEntry`] — cached response bytes + a hash of the
//!   canonical request form (spec §5) + a `created_at` for the TTL sweep.
//! - [`prune_expired`] — pure helper the Phase 8 maintenance worker calls
//!   on a cadence.
//!
//! ## What does NOT live here
//!
//! - Lookup-then-act handler logic (spec §3). Phase 9 writer task.
//! - Request canonicalisation + BLAKE3 hashing (spec §5). Phase 9.
//! - Pruning scheduler / cadence (spec §6). Phase 8.
//! - The `IdempotencyConflict` wire error variant. brain-core / brain-protocol.

use redb::{Table, TableDefinition};

// ---------------------------------------------------------------------------
// Table.
// ---------------------------------------------------------------------------

/// The `idempotency` table. Key is `RequestId::to_be_bytes()` (16-byte
/// UUIDv7 per `spec/02_data_model/03_identifiers.md`).
pub const IDEMPOTENCY_TABLE: TableDefinition<'static, [u8; 16], IdempotencyEntry> =
    TableDefinition::new("idempotency");

// ---------------------------------------------------------------------------
// response_kind byte mapping. Spec §17 — idempotency-required op catalog.
// ---------------------------------------------------------------------------

/// `IdempotencyEntry::response_kind` byte values. Each value pins a
/// wire-stable identifier for one of the idempotency-required ops; once
/// shipped, do **not** renumber. `0` is reserved so a stale reader can
/// flag "unknown / future-version".
///
/// Spec §17 enumerates the eight ops below; the substrate enforces
/// idempotency-required for exactly this set.
pub mod response_kind {
    pub const UNKNOWN: u8 = 0;
    pub const ENCODE: u8 = 1;
    pub const FORGET: u8 = 2;
    pub const LINK: u8 = 3;
    pub const UNLINK: u8 = 4;
    pub const UPDATE_KIND: u8 = 5;
    pub const UPDATE_CONTEXT: u8 = 6;
    pub const TXN_BEGIN: u8 = 7;
    pub const TXN_COMMIT: u8 = 8;
}

// ---------------------------------------------------------------------------
// TTL.
// ---------------------------------------------------------------------------

/// Default 24-hour TTL for idempotency entries, in nanoseconds. Spec §6
/// allows operators to shorten or lengthen — this is the substrate's
/// default. The number is `24 * 60 * 60 * 1e9`.
pub const DEFAULT_TTL_NANOS: u64 = 24 * 60 * 60 * 1_000_000_000;

// ---------------------------------------------------------------------------
// IdempotencyEntry.
// ---------------------------------------------------------------------------

/// Cached response for a single mutating request. Spec §07/06 §2 lists
/// four fields; this v1 stores those four plus a `request_hash` so the
/// conflict-detection check spec §5 mandates can run in O(1) byte
/// compare. See SD-3.5-1 in `docs/spec-deviations.md`.
///
/// - `response_kind` — one of the [`response_kind`] u8 constants.
/// - `memory_id_bytes` — the resulting memory, if any. Spec lists this
///   as `Option<MemoryId>`; we store the 16-byte representation and
///   expose [`IdempotencyEntry::memory_id`] for typed access.
/// - `response_payload` — the original response, encoded. Spec §9
///   mandates verbatim replay; the substrate stores these bytes and
///   hands them back unchanged.
/// - `request_hash` — BLAKE3 (32 bytes) over the canonical request form.
///   Phase 9 fills this; storage just keeps the bytes.
/// - `created_at_unix_nanos` — insertion time. Drives the [`prune_expired`]
///   sweep.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct IdempotencyEntry {
    pub response_kind: u8,
    pub memory_id_bytes: Option<[u8; 16]>,
    pub response_payload: Vec<u8>,
    pub request_hash: [u8; 32],
    pub created_at_unix_nanos: u64,
}

impl IdempotencyEntry {
    #[must_use]
    pub fn new(
        response_kind: u8,
        memory_id_bytes: Option<[u8; 16]>,
        response_payload: Vec<u8>,
        request_hash: [u8; 32],
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            response_kind,
            memory_id_bytes,
            response_payload,
            request_hash,
            created_at_unix_nanos,
        }
    }

    /// Typed accessor at the API boundary. Brain-core's `MemoryId`
    /// doesn't derive rkyv (orphan rule), so we store bytes inside the
    /// struct and convert here.
    #[must_use]
    pub fn memory_id(&self) -> Option<brain_core::MemoryId> {
        self.memory_id_bytes
            .map(brain_core::MemoryId::from_be_bytes)
    }

    /// Returns `true` if this entry's `created_at + ttl_nanos` is
    /// strictly less than `now_unix_nanos`. Uses saturating addition so
    /// an entry near `u64::MAX` doesn't wrap and prematurely expire.
    #[must_use]
    pub fn is_expired(&self, now_unix_nanos: u64, ttl_nanos: u64) -> bool {
        self.created_at_unix_nanos.saturating_add(ttl_nanos) < now_unix_nanos
    }
}

impl redb::Value for IdempotencyEntry {
    type SelfType<'a> = IdempotencyEntry;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        // rkyv 0.7's validation includes alignment; redb returns bytes
        // at arbitrary alignment, so copy into an AlignedVec first.
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<IdempotencyEntry>(&buf)
            .expect("IdempotencyEntry bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("IdempotencyEntry is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::IdempotencyEntry::v1")
    }
}

// ---------------------------------------------------------------------------
// Pruning.
// ---------------------------------------------------------------------------

/// Delete entries with `created_at + ttl_nanos < now`. Returns the
/// number of rows removed.
///
/// `now_unix_nanos` and `ttl_nanos` are explicit parameters so the
/// storage layer stays decision-free and testable. The Phase 8
/// maintenance worker is responsible for choosing the cadence (spec §6
/// says "every hour") and for the wall-clock source.
///
/// Implementation collects keys-to-remove first, then deletes them — we
/// can't call `remove` while `iter` borrows the table.
pub fn prune_expired(
    table: &mut Table<'_, [u8; 16], IdempotencyEntry>,
    now_unix_nanos: u64,
    ttl_nanos: u64,
) -> Result<u64, redb::StorageError> {
    use redb::ReadableTable;

    let mut victims: Vec<[u8; 16]> = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if value.value().is_expired(now_unix_nanos, ttl_nanos) {
            victims.push(key.value());
        }
    }

    let count = victims.len() as u64;
    for key in &victims {
        table.remove(key)?;
    }
    Ok(count)
}

/// Like [`prune_expired`] but stops after collecting `max` expired
/// keys for deletion. Returns `(deleted_count, scanned_to_end)` where
/// `scanned_to_end` is `true` iff the iterator completed without
/// hitting the `max` cap.
///
/// The Phase 8 idempotency-cleanup worker (sub-task 8.6) uses this
/// to spec §11/05 §3's "1000 per txn, multiple iterations" pattern
/// without blocking the writer for too long on a large initial
/// sweep.
pub fn prune_expired_bounded(
    table: &mut Table<'_, [u8; 16], IdempotencyEntry>,
    now_unix_nanos: u64,
    ttl_nanos: u64,
    max: usize,
) -> Result<(u64, bool), redb::StorageError> {
    use redb::ReadableTable;

    if max == 0 {
        return Ok((0, false));
    }

    let mut victims: Vec<[u8; 16]> = Vec::with_capacity(max.min(1024));
    let mut scanned_to_end = true;
    for entry in table.iter()? {
        let (key, value) = entry?;
        if value.value().is_expired(now_unix_nanos, ttl_nanos) {
            victims.push(key.value());
            if victims.len() >= max {
                scanned_to_end = false;
                break;
            }
        }
    }

    let count = victims.len() as u64;
    for key in &victims {
        table.remove(key)?;
    }
    Ok((count, scanned_to_end))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase, ReadableTable};

    const T0: u64 = 1_700_000_000_000_000_000;
    const HOUR_NS: u64 = 60 * 60 * 1_000_000_000;

    fn rid(byte: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[15] = byte;
        b
    }

    fn mid(byte: u8) -> [u8; 16] {
        // Distinct prefix from `rid()` so the bytes are recognisable in
        // a hex dump if a test fails.
        let mut b = [0u8; 16];
        b[0] = 0xAB;
        b[15] = byte;
        b
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn sample(byte: u8, created_at: u64) -> IdempotencyEntry {
        IdempotencyEntry::new(
            response_kind::ENCODE,
            Some(mid(byte)),
            vec![0xAA, 0xBB, 0xCC, byte],
            [byte; 32],
            created_at,
        )
    }

    // ----- CRUD ----------------------------------------------------------

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = rid(1);
        let entry = sample(1, T0);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&key, &entry).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, entry);
        assert_eq!(got.response_kind, response_kind::ENCODE);
        assert_eq!(got.memory_id_bytes, Some(mid(1)));
    }

    #[test]
    fn missing_key_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let rtxn = db.begin_read().unwrap();
        // Open via write txn first so the table exists; redb won't
        // open a non-existent table for read.
        drop(rtxn);
        let wtxn = db.begin_write().unwrap();
        {
            let _t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert!(t.get(&rid(99)).unwrap().is_none());
    }

    #[test]
    fn update_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = rid(7);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&key, &sample(7, T0)).unwrap();
        }
        wtxn.commit().unwrap();

        let mut updated = sample(7, T0);
        updated.response_payload = vec![0xFF; 64];

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&key, &updated).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.response_payload, vec![0xFF; 64]);
    }

    // ----- Value-shape round-trips --------------------------------------

    #[test]
    fn memory_id_optional_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let mut with_some = sample(1, T0);
        with_some.memory_id_bytes = Some(mid(0xAB));
        let mut with_none = sample(2, T0);
        with_none.memory_id_bytes = None;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&rid(1), &with_some).unwrap();
            t.insert(&rid(2), &with_none).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert_eq!(
            t.get(&rid(1)).unwrap().unwrap().value().memory_id_bytes,
            Some(mid(0xAB))
        );
        assert_eq!(
            t.get(&rid(2)).unwrap().unwrap().value().memory_id_bytes,
            None
        );
    }

    #[test]
    fn response_payload_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut entry = sample(5, T0);
        entry.response_payload = (0u8..=255).cycle().take(256).collect();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&rid(5), &entry).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let got = t.get(&rid(5)).unwrap().unwrap().value();
        assert_eq!(got.response_payload.len(), 256);
        assert_eq!(got.response_payload, entry.response_payload);
    }

    #[test]
    fn request_hash_byte_compare() {
        let mut a = sample(1, T0);
        let mut b = sample(1, T0);
        // Identical hashes -> entries match.
        a.request_hash = [0x42; 32];
        b.request_hash = [0x42; 32];
        assert_eq!(a, b);
        // Differing hash -> entries differ (conflict-detection path).
        b.request_hash = [0x43; 32];
        assert_ne!(a, b);
    }

    // ----- Pruning ------------------------------------------------------

    #[test]
    fn prune_expired_removes_old() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&rid(1), &sample(1, T0)).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let removed = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired(&mut t, T0 + 25 * HOUR_NS, DEFAULT_TTL_NANOS).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(removed, 1);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert!(t.get(&rid(1)).unwrap().is_none());
    }

    #[test]
    fn prune_expired_keeps_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&rid(1), &sample(1, T0)).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let removed = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired(&mut t, T0 + HOUR_NS, DEFAULT_TTL_NANOS).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(removed, 0);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert!(t.get(&rid(1)).unwrap().is_some());
    }

    #[test]
    fn prune_expired_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        // now = T0 + 48h, TTL = 24h, so threshold (created_at < T0+24h)
        // partitions entries cleanly: 1/2/3 are well outside the window,
        // 4/5 are well inside.
        let now = T0 + 48 * HOUR_NS;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            // Three old (created > 24h before `now`).
            t.insert(&rid(1), &sample(1, T0)).unwrap();
            t.insert(&rid(2), &sample(2, T0 + HOUR_NS)).unwrap();
            t.insert(&rid(3), &sample(3, T0 + 2 * HOUR_NS)).unwrap();
            // Two fresh (created within the TTL window).
            t.insert(&rid(4), &sample(4, T0 + 25 * HOUR_NS)).unwrap();
            t.insert(&rid(5), &sample(5, T0 + 26 * HOUR_NS)).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let removed = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired(&mut t, now, DEFAULT_TTL_NANOS).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(removed, 3);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        let mut remaining: Vec<u8> = t
            .iter()
            .unwrap()
            .map(|e| {
                let (k, _v) = e.unwrap();
                k.value()[15]
            })
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec![4, 5]);
    }

    #[test]
    fn prune_saturating() {
        // Entry created at u64::MAX must not panic when ttl is added.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            t.insert(&rid(1), &sample(1, u64::MAX)).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let removed = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            // now=0 < created_at+ttl (saturated at u64::MAX) -> keep.
            prune_expired(&mut t, 0, DEFAULT_TTL_NANOS).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(removed, 0);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert!(t.get(&rid(1)).unwrap().is_some());
    }

    // ----- prune_expired_bounded -----------------------------------------

    #[test]
    fn prune_expired_bounded_empty_table_returns_zero_scanned_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let now = T0 + 100 * HOUR_NS;
        let ttl = 24 * HOUR_NS;

        let wtxn = db.begin_write().unwrap();
        let (deleted, scanned_to_end) = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired_bounded(&mut t, now, ttl, 100).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(deleted, 0);
        assert!(scanned_to_end);
    }

    #[test]
    fn prune_expired_bounded_caps_at_max_and_reports_not_scanned_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let now = T0 + 100 * HOUR_NS;
        let ttl = 24 * HOUR_NS;

        // Seed 50 expired entries (created 50h ago each).
        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            for i in 0..50u8 {
                t.insert(rid(i), sample(i, T0 + 50 * HOUR_NS)).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let (deleted, scanned_to_end) = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired_bounded(&mut t, now, ttl, 10).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(deleted, 10);
        assert!(!scanned_to_end);

        // 40 still in table.
        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
        assert_eq!(t.iter().unwrap().count(), 40);
    }

    #[test]
    fn prune_expired_bounded_returns_scanned_to_end_when_all_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let now = T0 + 100 * HOUR_NS;
        let ttl = 24 * HOUR_NS;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            for i in 0..50u8 {
                t.insert(rid(i), sample(i, T0 + 50 * HOUR_NS)).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let (deleted, scanned_to_end) = {
            let mut t = wtxn.open_table(IDEMPOTENCY_TABLE).unwrap();
            prune_expired_bounded(&mut t, now, ttl, 1000).unwrap()
        };
        wtxn.commit().unwrap();
        assert_eq!(deleted, 50);
        assert!(scanned_to_end);
    }

    // ----- Type-name guard ----------------------------------------------

    #[test]
    fn type_name_includes_v1() {
        // The type_name must include "::v1" so a future schema bump
        // that changes the field layout fails fast on mismatched-type
        // open instead of silently corrupting reads.
        let name = <IdempotencyEntry as redb::Value>::type_name();
        let s = format!("{name:?}");
        assert!(s.contains("v1"), "type_name missing v1 marker: {s}");
    }
}
