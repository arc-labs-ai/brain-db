//! Per-shard tantivy index handle (phase 22.1).
//!
//! Owns the two tantivy indexes laid out in `spec/26_knowledge_storage/01_tantivy_layout.md`:
//!
//! - `memory_text.tantivy/` — BM25 over raw memory text (§26/01 §2).
//! - `statements.tantivy/`  — BM25 over the statement text representation.
//!
//! Phase 22.1 lands the open / schema-version check / startup-status
//! plumbing. Subsequent sub-tasks plug in:
//!
//! - 22.2 — tokenizer registration (URL + code-ID preservation, Porter stemmer).
//! - 22.3 / 22.4 — `IndexWriter` allocation + commit cadence.
//! - 22.5 — `LexicalRetriever` trait + impl reads through these handles.
//! - 22.6 — rebuild worker acts on the `IndexStatus::NeedsRebuild` arm.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tantivy::schema::{Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::Index;
use thiserror::Error;

pub mod tokenizer;

pub use tokenizer::{build_analyzer, BrainTokenizer, BRAIN_TOKENIZER_NAME};

/// Brain-side schema version stamped on the tantivy `IndexMeta::payload`.
///
/// Bumped whenever any field in the schemas defined by [`memory_text_schema`]
/// or [`statements_schema`] changes shape. Mismatch on open → `NeedsRebuild`
/// (§26/01 §2 + §6).
pub const BRAIN_SCHEMA_VERSION: u32 = 1;

const MEMORY_TEXT_DIR: &str = "memory_text.tantivy";
const STATEMENTS_DIR: &str = "statements.tantivy";

/// Scope tag carried alongside each [`IndexHandle`] so retrievers (§23/02 §4)
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
/// references — required for the indexer worker (22.3 / 22.4)
/// and the retriever (22.5) to hold independent handles.
#[derive(Clone)]
pub struct IndexHandle {
    pub index: Index,
    pub scope: LexicalScope,
}

impl std::fmt::Debug for IndexHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexHandle")
            .field("scope", &self.scope)
            .finish()
    }
}

/// Per-shard handle bundle. Always carries both indexes — a shard
/// without one of them is not a valid knowledge-layer shard.
#[derive(Debug)]
pub struct TantivyShard {
    pub memory_text: IndexHandle,
    pub statements: IndexHandle,
}

/// Result of [`TantivyShard::open`]. The status arms feed the rebuild
/// scheduler in 22.6.
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
    /// Caller (22.6) must rebuild before reads are valid.
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

/// Schema for `memory_text.tantivy/`. Pinned by §26/01 §2.
#[must_use]
pub fn memory_text_schema() -> Schema {
    let mut sb = Schema::builder();
    // MemoryId is u128 (16 bytes big-endian, §02/03 packing);
    // bytes field, INDEXED so the indexer worker (22.3) can
    // delete_term by id on FORGET / re-Upsert, STORED so the
    // retriever surfaces it in `RankedItem.id`.
    sb.add_bytes_field("memory_id", INDEXED | STORED);
    sb.add_text_field("text", TEXT);
    // 16-byte agent UUID — bytes field, indexed for exact-match
    // filter (§23/02 §5) and stored so retrieval round-trips it.
    sb.add_bytes_field("agent_id", INDEXED | STORED);
    sb.add_u64_field("kind", INDEXED);
    sb.add_u64_field("created_at", INDEXED | FAST);
    sb.build()
}

/// Schema for `statements.tantivy/`. Pinned by §26/01 §2.
#[must_use]
pub fn statements_schema() -> Schema {
    let mut sb = Schema::builder();
    // 16-byte u128 statement id; INDEXED so the indexer worker
    // (22.4) can delete_term by id on tombstone / supersede,
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
/// by the indexer worker on first commit (22.3 / 22.4). 22.1 only
/// reads it.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainSchemaPayload {
    pub brain_schema_version: u32,
}

impl TantivyShard {
    /// Open (or create) the two tantivy indexes under `shard_dir`.
    ///
    /// * If a directory is absent: create a fresh `Index` with the
    ///   bound schema. Payload stays empty until the first commit
    ///   by the indexer worker (22.3 / 22.4) — status reports
    ///   `Ready`.
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

        // Phase 22.2: register the brain analyzer on both
        // indexes. Override of tantivy's built-in `"default"`
        // name so 22.1's TEXT fields pick it up without a
        // schema-version bump.
        memory_index
            .tokenizers()
            .register(BRAIN_TOKENIZER_NAME, build_analyzer());
        statements_index
            .tokenizers()
            .register(BRAIN_TOKENIZER_NAME, build_analyzer());

        let shard = Arc::new(TantivyShard {
            memory_text: IndexHandle {
                index: memory_index,
                scope: LexicalScope::MemoryText,
            },
            statements: IndexHandle {
                index: statements_index,
                scope: LexicalScope::StatementText,
            },
        });

        Ok(TantivyShardStartup {
            shard,
            memory_status,
            statements_status,
        })
    }
}

/// Returns `(Index, IndexStatus)`. The `Index` value is always returned
/// (created fresh on `OpenFailed` so 22.6 can rebuild into the live dir
/// without re-creating it); the status drives whether reads are allowed.
fn open_or_create(
    shard_dir: &Path,
    scope: LexicalScope,
    schema: Schema,
) -> Result<(Index, IndexStatus), TantivyShardError> {
    let dir = shard_dir.join(scope.dir_name());

    fs::create_dir_all(&dir).map_err(|source| TantivyShardError::Mkdir {
        path: dir.clone(),
        source,
    })?;

    // A bare mkdir (e.g. phase-15.3's `ShardPaths::ensure`) leaves
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
            // 22.6 worker rebuilds into `<live>.rebuild/` and
            // atomic-swaps over the corrupt directory (§26/01 §5).
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

/// Inspect a freshly opened `Index`'s metadata payload for our schema
/// version. Returns `Ready` if version matches OR if payload is empty
/// (an index that's been created but never committed against — phase
/// 22.1 sees this on fresh dirs, 22.3 will populate it on first commit).
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

/// Serialise the schema-version payload for writers (22.3 / 22.4) to
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
