//! Per-shard on-disk file/dir layout.
//!
//! Centralizes the names of files and directories that live inside a
//! shard's data root. Substrate names (`arena.bin`, `metadata.redb`,
//! `wal/`, `shard.uuid`) coexist with opaque-body names.
//!
//! ## Layout (per shard)
//!
//! ```text
//! <data_dir>/<shard_id>/
//!   shard.uuid                 (substrate)
//!   arena.bin                  (substrate)
//!   metadata.redb              (substrate + typed-graph tables)
//!   wal/                       (substrate — directory)
//!   statements.tantivy/        (typed-graph — directory)
//!   memory_text.tantivy/       (typed-graph — directory)
//!   entity.hnsw                (typed-graph — file)
//!   statement.hnsw             (typed-graph — file)
//!   llm_cache.redb             (typed-graph — file)
//! ```
//!
//! Files are created by their owning module on first use (HNSW on
//! first insert, tantivy on first index write, redb on `open`). This
//! module is responsible for *directories only* — see [`ensure_dirs`].
//!
//! ## Migration note
//!
//! Some callers outside `brain-storage` and the
//! `spawn_shard` site still use literal path strings (test code,
//! integration tests). Migrating them to the constants below is a
//! separate cleanup and not blocking — the constants here remain the
//! single source of truth for production paths.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Substrate names.
// ---------------------------------------------------------------------------

/// `shard.uuid` — 16-byte file storing the shard's stable UUIDv7
/// (generated on first open).
pub const SHARD_UUID_FILE: &str = "shard.uuid";

/// `arena.bin` — memory-mapped vector arena.
pub const ARENA_FILE: &str = "arena.bin";

/// `metadata.redb` — substrate + opaque-body redb tables
pub const METADATA_DB_FILE: &str = "metadata.redb";

/// `wal/` — write-ahead log directory; segments live inside as
/// `seg-XXXXXXXXXX.wal`.
pub const WAL_DIR: &str = "wal";

// ---------------------------------------------------------------------------
// typed-graph names.
// ---------------------------------------------------------------------------

/// `entity.hnsw` — HNSW index over entity embeddings.
pub const ENTITY_HNSW_FILE: &str = "entity.hnsw";

/// `statement.hnsw` — HNSW index over statement embeddings.
pub const STATEMENT_HNSW_FILE: &str = "statement.hnsw";

/// `statements.tantivy/` — BM25 index over statement text.
pub const STATEMENTS_TANTIVY_DIR: &str = "statements.tantivy";

/// `memory_text.tantivy/` — BM25 index over memory text.
pub const MEMORY_TEXT_TANTIVY_DIR: &str = "memory_text.tantivy";

/// `llm_cache.redb` — separate redb file for LLM extractor cache.
pub const LLM_CACHE_DB_FILE: &str = "llm_cache.redb";

// ---------------------------------------------------------------------------
// Typed view.
// ---------------------------------------------------------------------------

/// Typed view of a shard's on-disk paths. Constructed from the shard's
/// root directory.
///
/// Holds the root only; getters return fresh `PathBuf`s. Cheap to
/// construct, cheap to throw away — call sites can build one per
/// invocation without worrying about lifetimes.
#[derive(Debug, Clone)]
pub struct ShardPaths {
    pub root: PathBuf,
}

impl ShardPaths {
    /// Wrap a shard's root directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- Substrate ----

    #[must_use]
    pub fn shard_uuid(&self) -> PathBuf {
        self.root.join(SHARD_UUID_FILE)
    }

    #[must_use]
    pub fn arena(&self) -> PathBuf {
        self.root.join(ARENA_FILE)
    }

    #[must_use]
    pub fn metadata_db(&self) -> PathBuf {
        self.root.join(METADATA_DB_FILE)
    }

    #[must_use]
    pub fn wal_dir(&self) -> PathBuf {
        self.root.join(WAL_DIR)
    }

    // ---- typed-graph phases ----

    #[must_use]
    pub fn entity_hnsw(&self) -> PathBuf {
        self.root.join(ENTITY_HNSW_FILE)
    }

    #[must_use]
    pub fn statement_hnsw(&self) -> PathBuf {
        self.root.join(STATEMENT_HNSW_FILE)
    }

    #[must_use]
    pub fn statements_tantivy(&self) -> PathBuf {
        self.root.join(STATEMENTS_TANTIVY_DIR)
    }

    #[must_use]
    pub fn memory_text_tantivy(&self) -> PathBuf {
        self.root.join(MEMORY_TEXT_TANTIVY_DIR)
    }

    #[must_use]
    pub fn llm_cache_db(&self) -> PathBuf {
        self.root.join(LLM_CACHE_DB_FILE)
    }
}

// ---------------------------------------------------------------------------
// WAL segment accounting.
// ---------------------------------------------------------------------------

/// Sum the on-disk size of every WAL segment in `wal_dir` and count
/// them. Returns `(total_bytes, segment_count)`.
///
/// A segment is any regular file whose extension is `wal` — the same
/// key the recovery scan and rollover writer use. Files that vanish
/// mid-scan (a retention sweep rotating a segment away under us) are
/// skipped rather than failing the whole count, so a `/metrics` scrape
/// never errors on a benign race; a single missing segment only
/// understates the total for one scrape. A `read_dir` failure on the
/// directory itself yields `(0, 0)`.
///
/// ```no_run
/// use brain_storage::ShardPaths;
/// let paths = ShardPaths::at("/data/shard-0");
/// let (bytes, segments) = brain_storage::wal_segment_stats(&paths.wal_dir());
/// println!("wal: {bytes} bytes across {segments} segments");
/// ```
#[must_use]
pub fn wal_segment_stats(wal_dir: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(wal_dir) else {
        return (0, 0);
    };
    let mut total_bytes = 0u64;
    let mut segment_count = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wal") {
            continue;
        }
        // A segment rotated away between read_dir and metadata is a
        // benign race on the scrape path: skip it.
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                total_bytes += meta.len();
                segment_count += 1;
            }
        }
    }
    (total_bytes, segment_count)
}

// ---------------------------------------------------------------------------
// Directory bootstrap.
// ---------------------------------------------------------------------------

/// Idempotent mkdir for every directory the shard layout requires:
/// the root, `wal/`, and the two opaque-body tantivy directories.
///
/// Files (`arena.bin`, `metadata.redb`, `*.hnsw`, `llm_cache.redb`)
/// are NOT created here — their owning modules open or create them on
/// demand. Existing files in the shard root are left untouched.
///
/// Returns `Ok(())` when every directory is present after the call.
pub fn ensure_dirs(root: &Path) -> std::io::Result<()> {
    let p = ShardPaths::at(root);
    std::fs::create_dir_all(p.root())?;
    std::fs::create_dir_all(p.wal_dir())?;
    std::fs::create_dir_all(p.statements_tantivy())?;
    std::fs::create_dir_all(p.memory_text_tantivy())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Tests create real directories (`mkdir`). Gated out under miri, which cannot
// perform those syscalls; the syscall-free tests in other modules still run.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn shard_paths_join_correctly() {
        let p = ShardPaths::at("/data/shard-007");
        assert_eq!(p.shard_uuid(), PathBuf::from("/data/shard-007/shard.uuid"));
        assert_eq!(p.arena(), PathBuf::from("/data/shard-007/arena.bin"));
        assert_eq!(
            p.metadata_db(),
            PathBuf::from("/data/shard-007/metadata.redb")
        );
        assert_eq!(p.wal_dir(), PathBuf::from("/data/shard-007/wal"));
        assert_eq!(
            p.entity_hnsw(),
            PathBuf::from("/data/shard-007/entity.hnsw")
        );
        assert_eq!(
            p.statement_hnsw(),
            PathBuf::from("/data/shard-007/statement.hnsw")
        );
        assert_eq!(
            p.statements_tantivy(),
            PathBuf::from("/data/shard-007/statements.tantivy")
        );
        assert_eq!(
            p.memory_text_tantivy(),
            PathBuf::from("/data/shard-007/memory_text.tantivy")
        );
        assert_eq!(
            p.llm_cache_db(),
            PathBuf::from("/data/shard-007/llm_cache.redb")
        );
    }

    #[test]
    fn ensure_dirs_creates_all_required_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shard-0");
        ensure_dirs(&root).expect("ensure_dirs");

        let p = ShardPaths::at(&root);
        assert!(p.root().is_dir(), "shard root should exist");
        assert!(p.wal_dir().is_dir(), "wal/ should exist");
        assert!(
            p.statements_tantivy().is_dir(),
            "statements.tantivy/ should exist"
        );
        assert!(
            p.memory_text_tantivy().is_dir(),
            "memory_text.tantivy/ should exist"
        );

        // Files are NOT created by ensure_dirs.
        assert!(!p.shard_uuid().exists(), "shard.uuid not created here");
        assert!(!p.arena().exists(), "arena.bin not created here");
        assert!(!p.metadata_db().exists(), "metadata.redb not created here");
        assert!(!p.entity_hnsw().exists(), "entity.hnsw not created here");
        assert!(
            !p.statement_hnsw().exists(),
            "statement.hnsw not created here"
        );
        assert!(
            !p.llm_cache_db().exists(),
            "llm_cache.redb not created here"
        );
    }

    #[test]
    fn ensure_dirs_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shard-0");
        ensure_dirs(&root).expect("first call");
        ensure_dirs(&root).expect("second call must also succeed");
        let p = ShardPaths::at(&root);
        assert!(p.wal_dir().is_dir());
        assert!(p.statements_tantivy().is_dir());
        assert!(p.memory_text_tantivy().is_dir());
    }

    #[test]
    fn wal_segment_stats_sums_only_dot_wal_files() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("wal");
        std::fs::create_dir_all(&wal).unwrap();

        std::fs::write(wal.join("0000000000.wal"), vec![0u8; 100]).unwrap();
        std::fs::write(wal.join("0000000001.wal"), vec![0u8; 250]).unwrap();
        // Non-segment files must not be counted.
        std::fs::write(wal.join("scratch.tmp"), vec![0u8; 9999]).unwrap();
        std::fs::write(wal.join("notes.txt"), vec![0u8; 9999]).unwrap();

        let (bytes, segments) = wal_segment_stats(&wal);
        assert_eq!(bytes, 350);
        assert_eq!(segments, 2);
    }

    #[test]
    fn wal_segment_stats_missing_dir_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("does-not-exist");
        assert_eq!(wal_segment_stats(&absent), (0, 0));
    }

    #[test]
    fn ensure_dirs_preserves_existing_substrate_files() {
        // Simulate an upgrade: a pre-existing shard with no schema
        // declared yet — arena.bin, metadata.redb, shard.uuid, and a
        // WAL segment, but no opaque-body files on disk.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shard-0");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(WAL_DIR)).unwrap();

        let arena = root.join(ARENA_FILE);
        let metadata = root.join(METADATA_DB_FILE);
        let uuid_file = root.join(SHARD_UUID_FILE);
        let seg = root.join(WAL_DIR).join("seg-0000000001.wal");
        std::fs::write(&arena, b"existing-arena-marker").unwrap();
        std::fs::write(&metadata, b"existing-metadata-marker").unwrap();
        std::fs::write(&uuid_file, [0xAB; 16]).unwrap();
        std::fs::write(&seg, b"existing-wal-segment").unwrap();

        ensure_dirs(&root).expect("ensure_dirs over existing shard");

        // Pre-existing files: byte-for-byte untouched.
        assert_eq!(std::fs::read(&arena).unwrap(), b"existing-arena-marker");
        assert_eq!(
            std::fs::read(&metadata).unwrap(),
            b"existing-metadata-marker"
        );
        assert_eq!(std::fs::read(&uuid_file).unwrap(), [0xAB; 16]);
        assert_eq!(std::fs::read(&seg).unwrap(), b"existing-wal-segment");

        // New dirs landed.
        let p = ShardPaths::at(&root);
        assert!(p.statements_tantivy().is_dir());
        assert!(p.memory_text_tantivy().is_dir());
    }
}
