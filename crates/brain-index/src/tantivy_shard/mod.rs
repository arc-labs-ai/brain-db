//! Per-shard tantivy index handle.
//!
//! Owns the two tantivy indexes:
//!
//! - `memory_text.tantivy/` — BM25 over raw memory text.
//! - `statements.tantivy/`  — BM25 over the statement text representation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tantivy::schema::{Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::Index;
use thiserror::Error;

pub mod retriever;
pub mod tokenizer;

pub use retriever::{
    LexicalError, LexicalFilters, LexicalQuery, LexicalRetriever, LexicalRetrieverConfig,
    RankedItem, RankedItemId, TantivyLexicalRetriever,
};
pub use tokenizer::{build_analyzer, BrainTokenizer, BRAIN_TOKENIZER_NAME};

/// Brain-side schema version stamped on the tantivy `IndexMeta::payload`.
///
/// Bumped whenever any field in the schemas defined by [`memory_text_schema`]
/// or [`statements_schema`] changes shape. Mismatch on open → `NeedsRebuild`.
pub const BRAIN_SCHEMA_VERSION: u32 = 1;

const STATEMENTS_DIR: &str = "statements.tantivy";
const MEMORY_TEXT_DIR: &str = "memory_text.tantivy";

/// Suffix for the scratch directory the rebuild worker builds the new
/// index into before the atomic swap (`<live>.rebuild`). Public so the
/// rebuild worker and the crash-recovery path agree on one spelling.
pub const REBUILD_SUFFIX: &str = ".rebuild";
/// Suffix for the previous live index, renamed aside during the swap
/// (`<live>.old`) so a crash mid-swap can fall back to it.
pub const OLD_SUFFIX: &str = ".old";

/// Scope tag carried alongside each [`IndexHandle`] so retrievers
/// can dispatch without an extra lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalScope {
    /// `memory_text.tantivy/` — `RankedItem.id` is a `MemoryId`.
    MemoryText,
    /// `statements.tantivy/` — `RankedItem.id` is a `StatementId`.
    StatementText,
}

impl LexicalScope {
    /// Directory name under `<shard_dir>/` for this scope.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::MemoryText => MEMORY_TEXT_DIR,
            Self::StatementText => STATEMENTS_DIR,
        }
    }
}

/// An open tantivy `Index` plus the scope it serves.
///
/// `tantivy::Index` is internally `Arc`-backed, so cloning is
/// cheap and shares the same underlying tokenizer / directory
/// references — required for the indexer worker
/// and the retriever to hold independent handles.
///
/// `commit_generation` is a monotonic counter the indexer bumps after every
/// successful tantivy commit; because it is an `Arc`, every clone of a handle
/// shares the same counter. The retriever reads it to decide whether the
/// writer has committed since its last `IndexReader::reload()` — reloading
/// only when the generation advanced instead of on every query, while still
/// serving every committed write (read-your-commits, invariant #7).
#[derive(Clone)]
pub struct IndexHandle {
    pub index: Index,
    pub scope: LexicalScope,
    commit_generation: Arc<AtomicU64>,
}

impl IndexHandle {
    /// Build a handle with a fresh commit-generation counter (starts at 0).
    #[must_use]
    pub fn new(index: Index, scope: LexicalScope) -> Self {
        Self {
            index,
            scope,
            commit_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Current commit generation. Advances by one on every successful commit
    /// against this index (see [`bump_commit_generation`](Self::bump_commit_generation)).
    #[must_use]
    pub fn commit_generation(&self) -> u64 {
        self.commit_generation.load(Ordering::Acquire)
    }

    /// A shared reference to this handle's commit-generation counter, so the
    /// indexer's drain loop can bump it without holding the whole handle.
    #[must_use]
    pub fn commit_generation_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.commit_generation)
    }

    /// Record that a commit has landed: advance the generation so readers
    /// sharing this counter know to reload. `Release` pairs with the
    /// retriever's `Acquire` read so the committed segments are visible once
    /// the new generation is.
    pub fn bump_commit_generation(&self) {
        self.commit_generation.fetch_add(1, Ordering::Release);
    }
}

impl std::fmt::Debug for IndexHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexHandle")
            .field("scope", &self.scope)
            .field("commit_generation", &self.commit_generation())
            .finish()
    }
}

/// Per-shard handle bundle. Always carries both indexes — a shard
/// without one of them is not a valid opaque-body shard.
#[derive(Debug)]
pub struct TantivyShard {
    pub memory_text: IndexHandle,
    pub statements: IndexHandle,
}

/// Result of [`TantivyShard::open`]. The status arms feed the rebuild
/// scheduler.
#[derive(Debug)]
pub struct TantivyShardStartup {
    pub shard: Arc<TantivyShard>,
    pub memory_status: IndexStatus,
    pub statements_status: IndexStatus,
}

/// Per-index readiness reported by [`TantivyShard::open`].
#[derive(Debug)]
pub enum IndexStatus {
    /// Index opened cleanly; schema version matches.
    Ready,
    /// Caller must rebuild before reads are valid.
    NeedsRebuild { reason: RebuildReason },
}

/// Why an index needs to be rebuilt.
#[derive(Debug)]
pub enum RebuildReason {
    /// The directory existed but tantivy could not open it.
    OpenFailed(String),
    /// `meta.json` payload mismatched [`BRAIN_SCHEMA_VERSION`].
    SchemaVersionMismatch { found: u32, expected: u32 },
    /// `meta.json` payload was non-empty but could not be parsed as
    /// the brain-side wrapper. Treated as corruption.
    PayloadCorrupt(String),
}

#[derive(Debug, Error)]
pub enum TantivyShardError {
    #[error("create shard directory `{path}`: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("create tantivy index at `{path}`: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: tantivy::TantivyError,
    },
}

/// Schema for `memory_text.tantivy/`.
#[must_use]
pub fn memory_text_schema() -> Schema {
    let mut sb = Schema::builder();
    // MemoryId is u128 (16 bytes big-endian);
    // bytes field, INDEXED so the indexer worker can
    // delete_term by id on FORGET / re-Upsert, STORED so the
    // retriever surfaces it in `RankedItem.id`.
    sb.add_bytes_field("memory_id", INDEXED | STORED);
    sb.add_text_field("text", TEXT);
    // 16-byte space UUID — bytes field, indexed for exact-match
    // filter and stored so retrieval round-trips it.
    sb.add_bytes_field("space_id", INDEXED | STORED);
    sb.add_u64_field("kind", INDEXED);
    sb.add_u64_field("created_at", INDEXED | FAST);
    // Context (session/conversation) id — the read-path scope tag that
    // lets the front filter narrow a query to a single context before
    // any expensive stage runs. INDEXED for exact term filter, FAST
    // for selectivity-driven set ops.
    sb.add_u64_field("session", INDEXED | FAST);
    sb.build()
}

/// Schema for `statements.tantivy/`.
#[must_use]
pub fn statements_schema() -> Schema {
    let mut sb = Schema::builder();
    // 16-byte u128 statement id; INDEXED so the indexer worker
    // can delete_term by id on tombstone / supersede,
    // STORED so retrieval surfaces it in `RankedItem.id`.
    sb.add_bytes_field("statement_id", INDEXED | STORED);
    sb.add_text_field("subject_name", TEXT);
    // predicate_name is a human-readable identifier (e.g.
    // "lives_in"); tantivy's STRING text option indexes the
    // whole value as one untokenised term, giving exact-match
    // semantics without leaving the text-field analyzer path.
    sb.add_text_field("predicate_name", STRING);
    sb.add_u64_field("predicate_id", INDEXED);
    sb.add_text_field("object_text", TEXT);
    sb.add_u64_field("kind", INDEXED);
    sb.add_u64_field("confidence_bucket", INDEXED | FAST);
    sb.add_u64_field("extracted_at", INDEXED | FAST);
    sb.build()
}

/// JSON payload written into the tantivy `IndexMeta::payload` field
/// by the indexer worker on first commit. The open path only reads it.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainSchemaPayload {
    pub brain_schema_version: u32,
}

impl TantivyShard {
    /// Open (or create) the two tantivy indexes under `shard_dir`.
    ///
    /// * If a directory is absent: create a fresh `Index` with the
    ///   bound schema. Payload stays empty until the first commit
    ///   by the indexer worker — status reports `Ready`.
    /// * If a directory exists and opens cleanly: parse the
    ///   `meta.json` payload. Match → `Ready`. Mismatch / corrupt
    ///   → `NeedsRebuild`.
    /// * If `tantivy::Index::open_in_dir` fails: `NeedsRebuild` with
    ///   the `tantivy::TantivyError` message attached.
    pub fn open(shard_dir: &Path) -> Result<TantivyShardStartup, TantivyShardError> {
        let (memory_index, memory_status) =
            open_or_create(shard_dir, LexicalScope::MemoryText, memory_text_schema())?;
        let (statements_index, statements_status) =
            open_or_create(shard_dir, LexicalScope::StatementText, statements_schema())?;

        // Register the brain analyzer on both indexes. Override
        // of tantivy's built-in `"default"` name so the TEXT
        // fields pick it up without a schema-version bump.
        memory_index
            .tokenizers()
            .register(BRAIN_TOKENIZER_NAME, build_analyzer());
        statements_index
            .tokenizers()
            .register(BRAIN_TOKENIZER_NAME, build_analyzer());

        let shard = Arc::new(TantivyShard {
            memory_text: IndexHandle::new(memory_index, LexicalScope::MemoryText),
            statements: IndexHandle::new(statements_index, LexicalScope::StatementText),
        });

        Ok(TantivyShardStartup {
            shard,
            memory_status,
            statements_status,
        })
    }
}

/// Returns `(Index, IndexStatus)`. The `Index` value is always returned
/// (created fresh on `OpenFailed` so the rebuild worker can rebuild into
/// the live dir without re-creating it); the status drives whether reads
/// are allowed.
fn open_or_create(
    shard_dir: &Path,
    scope: LexicalScope,
    schema: Schema,
) -> Result<(Index, IndexStatus), TantivyShardError> {
    let dir = shard_dir.join(scope.dir_name());

    // Crash recovery: the rebuild worker swaps a freshly-built index over
    // the live directory with two non-atomic renames (live→`.old`, then
    // `.rebuild`→live). A crash between them leaves the live directory
    // absent while a complete replacement sits in `.rebuild` (or the prior
    // data in `.old`). Reconcile *before* the create-or-open decision below,
    // otherwise this open would mistake the missing live dir for a fresh
    // shard and create an empty index — silently discarding the completed
    // rebuild (invariant #7). This is a no-op unless a leftover swap dir is
    // present.
    recover_interrupted_swap(&dir).map_err(|source| TantivyShardError::Mkdir {
        path: dir.clone(),
        source,
    })?;

    fs::create_dir_all(&dir).map_err(|source| TantivyShardError::Mkdir {
        path: dir.clone(),
        source,
    })?;

    // A bare mkdir (e.g. `ShardPaths::ensure`) leaves
    // the directory empty. Treat empty as fresh-create — only a
    // dir with a `meta.json` is a previously-committed index.
    let needs_create = !dir.join("meta.json").exists();

    if needs_create {
        let index = create_fresh(&dir, schema)?;
        return Ok((index, IndexStatus::Ready));
    }

    match Index::open_in_dir(&dir) {
        Ok(index) => {
            let status = inspect_payload(&index);
            Ok((index, status))
        }
        Err(err) => {
            // Existing directory is unopenable (DataCorruption,
            // missing segments, schema deserialise failure …).
            // Return a RAM-backed placeholder index that satisfies
            // the type contract; reads against it short-circuit
            // because the rebuild status is `NeedsRebuild`. The
            // rebuild worker rebuilds into `<live>.rebuild/` and
            // atomic-swaps over the corrupt directory.
            let placeholder = Index::create_in_ram(schema);
            Ok((
                placeholder,
                IndexStatus::NeedsRebuild {
                    reason: RebuildReason::OpenFailed(err.to_string()),
                },
            ))
        }
    }
}

/// Complete or roll back an interrupted rebuild swap for a single
/// index directory.
///
/// The rebuild worker replaces `live` with two non-atomic renames:
/// `live`→`<live>.old`, then `<live>.rebuild`→`live`, then it removes
/// `<live>.old`. A crash anywhere in that sequence can leave the `live`
/// directory missing (or present-but-empty) with the real data stranded
/// in a sibling. This resolves that state deterministically to the
/// newest *complete* index:
///
/// - If `live` is not a completed index and `<live>.rebuild` is complete,
///   promote `.rebuild` → `live` (the swap had reached the point where the
///   new index was fully committed).
/// - Else if `live` is not complete and `<live>.old` is complete, restore
///   `.old` → `live` (the swap failed before the new index committed).
/// - Any leftover `.rebuild` / `.old` directories are removed afterward.
///
/// "Complete" means the directory opens as a tantivy index *and* carries a
/// stamped brain schema-version payload — an in-progress rebuild has a
/// `meta.json` (tantivy writes one at creation) but no payload until its
/// final commit, so a half-built rebuild is never mistaken for a finished
/// one. A no-op when neither sibling exists.
fn recover_interrupted_swap(live: &Path) -> std::io::Result<()> {
    let rebuild = path_with_suffix(live, REBUILD_SUFFIX);
    let old = path_with_suffix(live, OLD_SUFFIX);

    // Nothing to reconcile unless a swap sibling is lying around.
    if !rebuild.exists() && !old.exists() {
        return Ok(());
    }

    if !is_completed_index(live) {
        if is_completed_index(&rebuild) {
            promote_swap_dir(&rebuild, live)?;
        } else if is_completed_index(&old) {
            promote_swap_dir(&old, live)?;
        }
    }

    // Clean up whatever remains so a later run starts from a clean slate.
    if rebuild.exists() {
        fs::remove_dir_all(&rebuild)?;
    }
    if old.exists() {
        fs::remove_dir_all(&old)?;
    }
    Ok(())
}

/// Move `src` onto `live`, replacing any incomplete `live` directory.
fn promote_swap_dir(src: &Path, live: &Path) -> std::io::Result<()> {
    if live.exists() {
        fs::remove_dir_all(live)?;
    }
    fs::rename(src, live)
}

/// Append `suffix` to a path's final component (e.g. `foo` → `foo.rebuild`).
fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut buf = p.as_os_str().to_owned();
    buf.push(suffix);
    PathBuf::from(buf)
}

/// True iff `dir` is a fully-committed brain index: it opens as a tantivy
/// index and its `meta.json` carries our stamped schema-version payload.
/// A freshly-created-but-never-committed index (no payload) reads as
/// incomplete, which is exactly what keeps a half-built `.rebuild` from
/// being promoted over live data.
fn is_completed_index(dir: &Path) -> bool {
    if !dir.join("meta.json").exists() {
        return false;
    }
    let Ok(index) = Index::open_in_dir(dir) else {
        return false;
    };
    let Ok(meta) = index.load_metas() else {
        return false;
    };
    let Some(raw) = meta.payload.as_ref() else {
        return false;
    };
    matches!(
        serde_json::from_str::<BrainSchemaPayload>(raw),
        Ok(payload) if payload.brain_schema_version == BRAIN_SCHEMA_VERSION
    )
}

/// Inspect a freshly opened `Index`'s metadata payload for our schema
/// version. Returns `Ready` if version matches OR if payload is empty
/// (an index that's been created but never committed against — the open
/// path sees this on fresh dirs, the indexer populates it on first commit).
fn inspect_payload(index: &Index) -> IndexStatus {
    let meta = match index.load_metas() {
        Ok(m) => m,
        Err(err) => {
            return IndexStatus::NeedsRebuild {
                reason: RebuildReason::OpenFailed(err.to_string()),
            };
        }
    };

    let Some(raw) = meta.payload.as_ref() else {
        // Newly created and never committed; treat as Ready —
        // first writer commit stamps the payload.
        return IndexStatus::Ready;
    };

    let parsed: Result<BrainSchemaPayload, _> = serde_json::from_str(raw);
    match parsed {
        Ok(payload) if payload.brain_schema_version == BRAIN_SCHEMA_VERSION => IndexStatus::Ready,
        Ok(payload) => IndexStatus::NeedsRebuild {
            reason: RebuildReason::SchemaVersionMismatch {
                found: payload.brain_schema_version,
                expected: BRAIN_SCHEMA_VERSION,
            },
        },
        Err(err) => IndexStatus::NeedsRebuild {
            reason: RebuildReason::PayloadCorrupt(err.to_string()),
        },
    }
}

fn create_fresh(dir: &Path, schema: Schema) -> Result<Index, TantivyShardError> {
    Index::create_in_dir(dir, schema).map_err(|source| TantivyShardError::Create {
        path: dir.to_path_buf(),
        source,
    })
}

/// Serialise the schema-version payload for writers to
/// stamp on first commit. Exposed here so the writer side doesn't
/// re-define the JSON shape.
#[must_use]
pub fn schema_payload_json() -> String {
    serde_json::to_string(&BrainSchemaPayload {
        brain_schema_version: BRAIN_SCHEMA_VERSION,
    })
    .expect("invariant: BrainSchemaPayload always serialises")
}

#[cfg(test)]
mod tests;
