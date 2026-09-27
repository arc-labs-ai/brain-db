//! Per-shard LLM extractor response cache.
//!
//! ## Why a separate redb file
//!
//! The cache payload (raw LLM responses) can grow to multiple GB per
//! shard at the 10 GB default cap. Keeping it inside `metadata.redb`
//! would slow every hot-path metadata read. A separate file
//! (`llm_cache.redb`) decouples the cache's growth from the hot
//! substrate metadata.
//!
//! ## Two tables
//!
//! - [`LLM_RESPONSES_TABLE`] — `(input_hash, extractor_id,
//!   extractor_version, model_id) → LlmResponse`. The cache row itself.
//! - [`LLM_RESPONSE_TTL_TABLE`] — `(expiry_unix_secs, input_hash) → ()`.
//!   Sorted secondary index that the cache sweeper walks in
//!   `range(..=now)` order to evict expired rows.

use std::path::{Path, PathBuf};

use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};

use crate::impl_redb_rkyv_value;

// ---------------------------------------------------------------------------
// Key types.
// ---------------------------------------------------------------------------

/// Cache-key components:
///
/// - `[u8; 32]` — blake3-256 hash of the input text + relevant context.
/// - `u32`      — `ExtractorId.raw()` (interned).
/// - `u32`      — `extractor_version` (bumped on extractor change).
/// - `u64`      — `model_id`: blake3-low-64 of the model identifier
///   string (e.g. `"anthropic/claude-haiku-4-5"`). Avoids embedding a
///   variable-length string in every cache key.
pub type LlmCacheKey = ([u8; 32], u32, u32, u64);

/// Sorted-by-expiry secondary index used by the cache sweeper.
///
/// - `u64`      — `expiry_unix_secs` (NOT nanoseconds — second
///   granularity is plenty for TTL eviction and keeps the key smaller).
/// - `[u8; 32]` — the input hash, the leading component of [`LlmCacheKey`].
///   Lets the sweeper resolve back to the cache row.
pub type LlmTtlKey = (u64, [u8; 32]);

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const LLM_RESPONSES_TABLE: TableDefinition<'static, LlmCacheKey, LlmResponse> =
    TableDefinition::new("llm_responses");

pub const LLM_RESPONSE_TTL_TABLE: TableDefinition<'static, LlmTtlKey, ()> =
    TableDefinition::new("llm_response_ttl");

// ---------------------------------------------------------------------------
// Value struct.
// ---------------------------------------------------------------------------

/// One cached LLM response.
///
/// `response_blob` is opaque to this layer — it's an rkyv-encoded
/// payload that the LLM extractor parses according to its
/// schema-validated output type. The framing layer here doesn't peek
/// inside.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct LlmResponse {
    /// rkyv-encoded typed response. The LLM extractor defines the shape.
    pub response_blob: Vec<u8>,

    /// Wall-clock nanoseconds when this row was first cached.
    pub created_at_unix_nanos: u64,

    /// Wall-clock nanoseconds when this row should be evicted. The
    /// `llm_response_ttl` table's key carries seconds-granularity of
    /// this value for sweeper-side range scans.
    pub expires_at_unix_nanos: u64,

    /// Total tokens consumed by the call that produced this row. The
    /// LLM extractor uses this for per-extractor cost budgeting.
    pub token_count: u32,

    /// blake3-low-64 of the model identifier, mirrored from the cache
    /// key for fast scans without re-deriving the key.
    pub model_id: u64,
}

impl LlmResponse {
    #[must_use]
    pub fn new(
        response_blob: Vec<u8>,
        created_at_unix_nanos: u64,
        expires_at_unix_nanos: u64,
        token_count: u32,
        model_id: u64,
    ) -> Self {
        Self {
            response_blob,
            created_at_unix_nanos,
            expires_at_unix_nanos,
            token_count,
            model_id,
        }
    }
}

impl_redb_rkyv_value!(LlmResponse, "brain_metadata::LlmResponse");

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// LLM-cache errors. Smaller surface than `MetadataDbError` since the
/// cache has no schema-version table, no checkpoint integration, and
/// no transaction-buffering semantics.
#[derive(thiserror::Error, Debug)]
pub enum LlmCacheError {
    #[error("opening LLM cache redb at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: redb::DatabaseError,
    },

    #[error("initializing LLM cache table: {0}")]
    Table(#[from] redb::TableError),

    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
}

// ---------------------------------------------------------------------------
// LlmCacheDb wrapper.
// ---------------------------------------------------------------------------

/// Per-shard LLM extractor response cache.
///
/// Mirrors [`crate::db::MetadataDb`]'s `&mut self` single-writer
/// discipline: only one writer task per shard can call `write_txn`.
/// Many concurrent readers are allowed via `read_txn`.
///
/// (`write_txn` is the imported method name; in Rust this section
/// would link via `[`LlmCacheDb::write_txn`]` — keeping the prose
/// link-free since this comment lives inline.)
pub struct LlmCacheDb {
    db: Database,
    path: PathBuf,
}

impl LlmCacheDb {
    /// Open or create the cache file. Idempotent — opening a
    /// pre-existing file with both tables initialized completes in
    /// microseconds.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LlmCacheError> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path).map_err(|source| LlmCacheError::Open {
            path: path.clone(),
            source,
        })?;

        // Touch both tables so they exist after open(). Idempotent:
        // redb skips the create if a table with the same name + sigs
        // is already present.
        let wtxn = db.begin_write()?;
        {
            let _ = wtxn.open_table(LLM_RESPONSES_TABLE)?;
        }
        {
            let _ = wtxn.open_table(LLM_RESPONSE_TTL_TABLE)?;
        }
        wtxn.commit()?;

        Ok(Self { db, path })
    }

    /// Begin a read transaction. Many can coexist (redb MVCC).
    pub fn read_txn(&self) -> Result<ReadTransaction, redb::TransactionError> {
        self.db.begin_read()
    }

    /// Begin a write transaction. `&mut self` enforces
    /// single-writer-per-shard at compile time.
    pub fn write_txn(&mut self) -> Result<WriteTransaction, redb::TransactionError> {
        self.db.begin_write()
    }

    /// Path the cache was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Escape hatch; same caveat as `MetadataDb::db` — don't call
    /// `begin_write` through this and bypass the single-writer
    /// discipline.
    #[doc(hidden)]
    #[must_use]
    pub fn db(&self) -> &Database {
        &self.db
    }
}

// ---------------------------------------------------------------------------
// Sweeping.
// ---------------------------------------------------------------------------

/// Sweep expired rows from the LLM response cache.
///
/// Walks [`LLM_RESPONSE_TTL_TABLE`] for keys whose expiry timestamp is
/// `<= now_unix_secs` and, for each, deletes the matching cache row(s)
/// from [`LLM_RESPONSES_TABLE`] along with the TTL index entry itself.
///
/// All deletes commit in a single write transaction — partial state is
/// never observable. If the wtxn fails the database is unchanged.
///
/// Individual `remove` calls that miss (row already gone, e.g. a race
/// with a fresh `put` rewriting the same key) are logged at debug level
/// and skipped: the sweep is idempotent and a re-run is always safe.
///
/// Returns the number of TTL index entries removed.
pub fn sweep_expired(db: &mut LlmCacheDb, now_unix_secs: u64) -> Result<usize, LlmCacheError> {
    let wtxn = db.write_txn()?;
    let expired_ttl_keys: Vec<LlmTtlKey> = {
        let ttl = wtxn.open_table(LLM_RESPONSE_TTL_TABLE)?;
        let lo: LlmTtlKey = (0u64, [0u8; 32]);
        let hi: LlmTtlKey = (now_unix_secs, [0xFFu8; 32]);
        let mut acc = Vec::new();
        for entry in ttl.range(lo..=hi).map_err(redb::TableError::from)? {
            match entry {
                Ok((k, _)) => acc.push(k.value()),
                Err(e) => {
                    tracing::debug!(
                        target: "brain_metadata::llm_cache",
                        error = %e,
                        "ttl scan entry error; continuing",
                    );
                }
            }
        }
        acc
    };

    let mut removed = 0usize;
    {
        let mut responses = wtxn.open_table(LLM_RESPONSES_TABLE)?;
        let mut ttl = wtxn.open_table(LLM_RESPONSE_TTL_TABLE)?;
        for (expiry, hash) in &expired_ttl_keys {
            // The main-table key is (hash, extractor_id, extractor_version,
            // model_id). A single input hash may match multiple rows if
            // the same input has been cached under different extractor /
            // version / model triples. Range-scan the hash-prefix to find
            // them, collect, then remove — never delete while a borrow on
            // the table iterator is alive.
            //
            // Each candidate row carries its own authoritative expiry. A
            // row is deleted only when *its* expiry is `<= now`: re-caching
            // the same key with a later TTL overwrites the row (extending
            // its expiry) but leaves the earlier TTL hint behind, so this
            // stale hint must not evict a row that is now live again.
            let lo_key = (*hash, 0u32, 0u32, 0u64);
            let hi_key = (*hash, u32::MAX, u32::MAX, u64::MAX);
            let mut main_keys: Vec<LlmCacheKey> = Vec::new();
            match responses.range(lo_key..=hi_key) {
                Ok(iter) => {
                    for entry in iter {
                        match entry {
                            Ok((k, v)) => {
                                let row_expiry_secs =
                                    v.value().expires_at_unix_nanos / 1_000_000_000;
                                if row_expiry_secs <= now_unix_secs {
                                    main_keys.push(k.value());
                                }
                            }
                            Err(e) => tracing::debug!(
                                target: "brain_metadata::llm_cache",
                                error = %e,
                                "main scan entry error; continuing",
                            ),
                        }
                    }
                }
                Err(e) => tracing::debug!(
                    target: "brain_metadata::llm_cache",
                    error = %e,
                    "main range error; continuing",
                ),
            }
            for k in main_keys {
                if let Err(e) = responses.remove(&k) {
                    tracing::debug!(
                        target: "brain_metadata::llm_cache",
                        error = %e,
                        "main remove error; continuing",
                    );
                }
            }
            match ttl.remove(&(*expiry, *hash)) {
                Ok(_) => removed += 1,
                Err(e) => tracing::debug!(
                    target: "brain_metadata::llm_cache",
                    error = %e,
                    "ttl remove error; continuing",
                ),
            }
        }
    }
    wtxn.commit()?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    fn cache_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("llm_cache.redb")
    }

    fn sample_response() -> LlmResponse {
        LlmResponse::new(
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_000 + 86_400_000_000_000, // +1 day
            512,
            0x0123_4567_89AB_CDEF,
        )
    }

    fn sample_key() -> LlmCacheKey {
        let mut hash = [0u8; 32];
        for (i, b) in hash.iter_mut().enumerate() {
            *b = i as u8;
        }
        (hash, 7, 1, 0x0123_4567_89AB_CDEF)
    }

    #[test]
    fn open_creates_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);
        assert!(!path.exists(), "precondition: file shouldn't exist");
        let db = LlmCacheDb::open(&path).expect("open");
        assert!(path.exists(), "redb file should be on disk after open");
        assert_eq!(db.path(), path);

        // Tables exist — opening for read should not return TableDoesNotExist.
        let rtxn = db.read_txn().unwrap();
        let _ = rtxn
            .open_table(LLM_RESPONSES_TABLE)
            .expect("responses table exists");
        let _ = rtxn
            .open_table(LLM_RESPONSE_TTL_TABLE)
            .expect("ttl table exists");
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(&dir);

        // First open: insert a row.
        {
            let mut db = LlmCacheDb::open(&path).unwrap();
            let key = sample_key();
            let resp = sample_response();
            let wtxn = db.write_txn().unwrap();
            {
                let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
                t.insert(&key, &resp).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Second open: row must still be there.
        let db = LlmCacheDb::open(&path).expect("re-open");
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        let got = t
            .get(&sample_key())
            .unwrap()
            .expect("row present after re-open");
        assert_eq!(got.value(), sample_response());
    }

    #[test]
    fn llm_response_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let key = sample_key();
        let resp = sample_response();

        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.insert(&key, &resp).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, resp);
    }

    #[test]
    fn ttl_index_range_scan() {
        // The sweeper walks the TTL index in `range(..=now)` order.
        // Verify the sort + scan semantics work with our key shape.
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();

        let hash_a = [0xAA; 32];
        let hash_b = [0xBB; 32];
        let hash_c = [0xCC; 32];

        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
            t.insert(&(100u64, hash_a), &()).unwrap(); // expires earliest
            t.insert(&(200u64, hash_b), &()).unwrap();
            t.insert(&(300u64, hash_c), &()).unwrap(); // expires latest
        }
        wtxn.commit().unwrap();

        // Scan up to expiry=200 — should see hash_a and hash_b only.
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
        let lo: LlmTtlKey = (0u64, [0u8; 32]);
        let hi: LlmTtlKey = (200u64, [0xFFu8; 32]);
        let mut expired = Vec::new();
        for entry in t.range(lo..=hi).unwrap() {
            let (k, _) = entry.unwrap();
            expired.push(k.value());
        }
        assert_eq!(expired.len(), 2);
        assert_eq!(expired[0], (100, hash_a));
        assert_eq!(expired[1], (200, hash_b));
    }

    fn put_with_ttl(db: &mut LlmCacheDb, key: LlmCacheKey, expiry_secs: u64, resp: &LlmResponse) {
        // Keep the stored row's authoritative expiry consistent with the
        // TTL-index key. `sweep_expired` deletes a row only when the row's
        // own expiry is `<= now`, so the two must agree for the sweep to
        // observe the intended expiry.
        let mut resp = resp.clone();
        resp.expires_at_unix_nanos = expiry_secs.saturating_mul(1_000_000_000);
        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.insert(&key, &resp).unwrap();
        }
        {
            let mut ttl = wtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
            ttl.insert(&(expiry_secs, key.0), &()).unwrap();
        }
        wtxn.commit().unwrap();
    }

    fn count_main(db: &LlmCacheDb) -> usize {
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        t.iter().unwrap().count()
    }

    fn count_ttl(db: &LlmCacheDb) -> usize {
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
        t.iter().unwrap().count()
    }

    #[test]
    fn sweep_expired_removes_only_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let resp = sample_response();
        let hash_a = [0x11u8; 32];
        let hash_b = [0x22u8; 32];
        let hash_c = [0x33u8; 32];
        put_with_ttl(&mut db, (hash_a, 1, 1, 0), 100, &resp);
        put_with_ttl(&mut db, (hash_b, 1, 1, 0), 200, &resp);
        put_with_ttl(&mut db, (hash_c, 1, 1, 0), 300, &resp);

        // now = 150 — only hash_a (expiry=100) qualifies.
        let removed = sweep_expired(&mut db, 150).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count_main(&db), 2);
        assert_eq!(count_ttl(&db), 2);

        let rtxn = db.read_txn().unwrap();
        let main = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        assert!(main.get(&(hash_a, 1u32, 1u32, 0u64)).unwrap().is_none());
        assert!(main.get(&(hash_b, 1u32, 1u32, 0u64)).unwrap().is_some());
        assert!(main.get(&(hash_c, 1u32, 1u32, 0u64)).unwrap().is_some());
    }

    #[test]
    fn sweep_expired_handles_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let removed = sweep_expired(&mut db, 1_000_000_000).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(count_main(&db), 0);
        assert_eq!(count_ttl(&db), 0);
    }

    #[test]
    fn sweep_expired_handles_all_expired() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let resp = sample_response();
        for i in 0..5 {
            let mut h = [0u8; 32];
            h[0] = i as u8;
            put_with_ttl(&mut db, (h, 1, 1, 0), 100 + i, &resp);
        }
        assert_eq!(count_main(&db), 5);
        assert_eq!(count_ttl(&db), 5);

        let removed = sweep_expired(&mut db, 10_000).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(count_main(&db), 0);
        assert_eq!(count_ttl(&db), 0);
    }

    #[test]
    fn sweep_expired_atomic_with_main_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let resp = sample_response();
        let hash = [0x99u8; 32];
        put_with_ttl(&mut db, (hash, 7, 2, 0xABCD), 500, &resp);

        // Pre-condition: row in both tables.
        {
            let rtxn = db.read_txn().unwrap();
            let main = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            assert!(main.get(&(hash, 7u32, 2u32, 0xABCDu64)).unwrap().is_some());
            let ttl = rtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
            assert!(ttl.get(&(500u64, hash)).unwrap().is_some());
        }

        let removed = sweep_expired(&mut db, 1_000).unwrap();
        assert_eq!(removed, 1);

        // Post-condition: neither table has the row.
        let rtxn = db.read_txn().unwrap();
        let main = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        assert!(main.get(&(hash, 7u32, 2u32, 0xABCDu64)).unwrap().is_none());
        let ttl = rtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
        assert!(ttl.get(&(500u64, hash)).unwrap().is_none());
    }

    #[test]
    fn sweep_expired_deletes_all_extractor_versions_sharing_a_hash() {
        // Multiple cache rows can share an input hash if the same input
        // was cached under different extractor versions or models. The
        // TTL index keys on the hash alone, so a single sweep must
        // delete every main-table row matching that hash.
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        // All three rows share both the input hash and the same expiry
        // (50s), consistent with the single TTL hint below.
        let mut resp = sample_response();
        resp.expires_at_unix_nanos = 50 * 1_000_000_000;
        let hash = [0x55u8; 32];

        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.insert(&(hash, 1u32, 1u32, 0u64), &resp).unwrap();
            t.insert(&(hash, 1u32, 2u32, 0u64), &resp).unwrap();
            t.insert(&(hash, 2u32, 1u32, 0u64), &resp).unwrap();
        }
        {
            let mut ttl = wtxn.open_table(LLM_RESPONSE_TTL_TABLE).unwrap();
            ttl.insert(&(50u64, hash), &()).unwrap();
        }
        wtxn.commit().unwrap();
        assert_eq!(count_main(&db), 3);

        let removed = sweep_expired(&mut db, 100).unwrap();
        assert_eq!(removed, 1, "one TTL row removed");
        assert_eq!(count_main(&db), 0, "all three main rows removed");
    }

    #[test]
    fn sweep_retains_live_sibling_with_same_hash_different_expiry() {
        // WK5 regression. Re-caching the same key with a longer TTL
        // overwrites the single main row (extending its expiry) but leaves
        // the earlier TTL hint in place — the TTL table now holds two rows
        // for one hash at different expiry seconds. Sweeping at a time
        // between the two expiries must drop only the stale hint and keep
        // the still-live row readable.
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let hash = [0x77u8; 32];
        let key = (hash, 1u32, 1u32, 0u64);

        // First put: expiry at 100s, token_count 111.
        let mut resp1 = sample_response();
        resp1.token_count = 111;
        put_with_ttl(&mut db, key, 100, &resp1);

        // Re-put same key: expiry extended to 300s, token_count 222. The
        // main row is overwritten; the TTL table now has (100, hash) and
        // (300, hash).
        let mut resp2 = sample_response();
        resp2.token_count = 222;
        put_with_ttl(&mut db, key, 300, &resp2);

        assert_eq!(count_main(&db), 1, "one main row after re-put");
        assert_eq!(count_ttl(&db), 2, "two TTL hints for one hash");

        // Sweep at 200: (100, hash) is stale, (300, hash) is still live.
        let removed = sweep_expired(&mut db, 200).unwrap();
        assert_eq!(removed, 1, "only the stale TTL hint is removed");
        assert_eq!(count_ttl(&db), 1, "live TTL hint retained");
        assert_eq!(count_main(&db), 1, "live main row retained");

        // The retained row is the extended re-put, still readable.
        let rtxn = db.read_txn().unwrap();
        let main = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        let got = main.get(&key).unwrap().expect("live row still readable");
        assert_eq!(got.value().token_count, 222);

        // Sweeping past the live expiry finally evicts the row.
        drop(main);
        drop(rtxn);
        let removed2 = sweep_expired(&mut db, 400).unwrap();
        assert_eq!(removed2, 1, "live hint now expired");
        assert_eq!(count_main(&db), 0, "no dangling main row");
        assert_eq!(count_ttl(&db), 0, "no dangling TTL hint");
    }

    #[test]
    fn cache_key_components_distinguish_rows() {
        // Two cache rows with the same input_hash but different
        // extractor_versions are distinct: cache-key collisions must
        // be a true equality on all four fields.
        let dir = tempfile::tempdir().unwrap();
        let mut db = LlmCacheDb::open(cache_path(&dir)).unwrap();
        let (h, ext_id, _v, model) = sample_key();
        let key_v1 = (h, ext_id, 1u32, model);
        let key_v2 = (h, ext_id, 2u32, model);

        let mut resp_v1 = sample_response();
        resp_v1.token_count = 100;
        let mut resp_v2 = sample_response();
        resp_v2.token_count = 200;

        let wtxn = db.write_txn().unwrap();
        {
            let mut t = wtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
            t.insert(&key_v1, &resp_v1).unwrap();
            t.insert(&key_v2, &resp_v2).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(LLM_RESPONSES_TABLE).unwrap();
        assert_eq!(t.get(&key_v1).unwrap().unwrap().value().token_count, 100);
        assert_eq!(t.get(&key_v2).unwrap().unwrap().value().token_count, 200);
    }
}
