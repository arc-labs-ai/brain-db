//! Per-shard LLM extractor response cache.
//!
//! See `spec/26_knowledge_storage/00_purpose.md` ("LLM extractor cache").
//!
//! ## Why a separate redb file
//!
//! The cache payload (raw LLM responses) can grow to multiple GB per
//! shard at the spec'd 10 GB default cap. Keeping it inside
//! `metadata.redb` would slow every hot-path metadata read. A separate
//! file (`llm_cache.redb`) decouples the cache's growth from the hot
//! substrate metadata.
//!
//! ## Two tables
//!
//! - [`LLM_RESPONSES_TABLE`] — `(input_hash, extractor_id,
//!   extractor_version, model_id) → LlmResponse`. The cache row itself.
//! - [`LLM_RESPONSE_TTL_TABLE`] — `(expiry_unix_secs, input_hash) → ()`.
//!   Sorted secondary index that the cache sweeper (phase 24) walks
//!   in `range(..=now)` order to evict expired rows.
//!
//! ## What this sub-task does (15.4)
//!
//! - Define the table sigs and value type.
//! - Open the redb file on `spawn_shard`; tables initialize.
//! - Round-trip + idempotency tests.
//!
//! The cache **writer** (LLM extractor with retry + budget) lands in
//! phase 21. The **sweeper** (TTL eviction + LRU when over capacity)
//! lands in phase 24. 15.4 is purely the file + schema.

use std::path::{Path, PathBuf};

use redb::{Database, ReadTransaction, ReadableDatabase, TableDefinition, WriteTransaction};

use crate::impl_redb_rkyv_value;

// ---------------------------------------------------------------------------
// Key types.
// ---------------------------------------------------------------------------

/// Cache-key components per spec §26:
///
/// - `[u8; 32]` — blake3-256 hash of the input text + relevant context.
/// - `u32`      — `ExtractorId.raw()` (interned per 15.1).
/// - `u32`      — `extractor_version` (bumped on extractor change per
///   AUTONOMY §23).
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
/// `response_blob` is opaque to 15.4 — it's an rkyv-encoded payload
/// that phase 21 (the LLM extractor) parses according to its
/// schema-validated output type. The framing layer here doesn't peek
/// inside.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct LlmResponse {
    /// rkyv-encoded typed response. Phase 21 defines the shape.
    pub response_blob: Vec<u8>,

    /// Wall-clock nanoseconds when this row was first cached.
    pub created_at_unix_nanos: u64,

    /// Wall-clock nanoseconds when this row should be evicted. The
    /// `llm_response_ttl` table's key carries seconds-granularity of
    /// this value for sweeper-side range scans.
    pub expires_at_unix_nanos: u64,

    /// Total tokens consumed by the call that produced this row. Phase
    /// 21 uses this for per-extractor cost budgeting.
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

impl_redb_rkyv_value!(LlmResponse, "brain_metadata::LlmResponse::v1");

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
/// discipline: only one writer task per shard can call [`write_txn`].
/// Many concurrent readers are allowed via [`read_txn`].
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
        let _ = rtxn.open_table(LLM_RESPONSES_TABLE).expect("responses table exists");
        let _ = rtxn.open_table(LLM_RESPONSE_TTL_TABLE).expect("ttl table exists");
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
        let got = t.get(&sample_key()).unwrap().expect("row present after re-open");
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
        // Phase 24 (sweeper) walks the TTL index in `range(..=now)`
        // order. Verify the sort + scan semantics work with our key
        // shape.
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
