//! The configured page cache is the one redb enforces.
//!
//! The rule that computes the size is tested where it lives, against an
//! `Option<u64>` and a shard count, so every machine it has to survive is
//! reachable without owning one. That proves the arithmetic and nothing else:
//! a database still opened with `Database::create` would pass every one of
//! those tests while taking redb's 1 GiB regardless.
//!
//! So this file asks redb instead. It configures a cache far smaller than the
//! workload, drives that workload through `MetadataDb`, and reads redb's own
//! eviction counter — the counter can only move if redb was told to evict, and
//! redb is only told to evict by the size the database was opened with. The
//! same workload against a default-sized database is run alongside as a
//! control, so a passing assertion means "evicted because the cache was
//! small", not "evicts either way".
//!
//! The eviction counters are behind redb's `cache_metrics` feature, enabled
//! for this crate's test build only. A process that is not running tests links
//! a redb that does not count.

use brain_metadata::{page_cache, MetadataDb};
use redb::{Database, ReadableDatabase, TableDefinition};

/// Scratch table, written to purely to put pages through the cache. It lives
/// beside the real tables in the same file so the workload travels the path a
/// shard's writes travel.
const BALLAST: TableDefinition<u64, &[u8]> = TableDefinition::new("page_cache_test_ballast");

/// The cache under test. Deliberately far below the workload: with a cache
/// this small every batch has to evict, and with redb's default none of them
/// would.
const TINY_CACHE_BYTES: u64 = 1024 * 1024;

/// Rows of 4 KiB — redb's page size, so each row is at least one page of
/// cache — in batches, totalling well over the cache under test.
const ROW_BYTES: usize = 4096;
const ROWS_PER_BATCH: u64 = 256;
const BATCHES: u64 = 8;

/// Write the same bytes through any redb database.
fn drive_workload(db: &Database) {
    for batch in 0..BATCHES {
        let wtxn = db.begin_write().expect("begin write");
        {
            let mut table = wtxn.open_table(BALLAST).expect("open ballast");
            for row in 0..ROWS_PER_BATCH {
                let key = batch * ROWS_PER_BATCH + row;
                let value = vec![u8::try_from(key % 251).expect("fits a byte"); ROW_BYTES];
                table.insert(key, value.as_slice()).expect("insert");
            }
        }
        wtxn.commit().expect("commit");
    }
}

#[test]
fn the_configured_cache_size_is_the_one_redb_enforces() {
    assert!(
        page_cache::configure(TINY_CACHE_BYTES, TINY_CACHE_BYTES),
        "this test owns the process-wide page cache sizes"
    );
    assert_eq!(
        page_cache::metadata_bytes(),
        usize::try_from(TINY_CACHE_BYTES).expect("a mebibyte fits a usize")
    );

    let dir = tempfile::TempDir::new().expect("tempdir");

    // The workload against a database opened the way the engine opens one.
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    drive_workload(metadata.db());
    let evictions = metadata.db().cache_stats().evictions();
    let used = metadata.db().cache_stats().used_bytes();

    // The same workload against redb's unconfigured default, as a control.
    let unsized_db = Database::create(dir.path().join("control.redb")).expect("open control");
    drive_workload(&unsized_db);
    let control_evictions = unsized_db.cache_stats().evictions();

    assert_eq!(
        control_evictions, 0,
        "the control proves this workload fits redb's default cache, so any \
         eviction below is the configured size doing its job"
    );
    assert!(
        evictions > 0,
        "a {TINY_CACHE_BYTES}-byte cache took {} bytes of writes without \
         evicting, which means the size never reached redb",
        ROW_BYTES as u64 * ROWS_PER_BATCH * BATCHES
    );
    assert!(
        used <= usize::try_from(TINY_CACHE_BYTES).expect("fits") + ROW_BYTES,
        "cache held {used} bytes against a {TINY_CACHE_BYTES}-byte limit"
    );
}
