# Plan: Phase 22 — Task 06, Index rebuild worker

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Materialise the rebuild flow in §26/01 §5. Given a shard
directory and an open `MetadataDb`, rebuild either tantivy
index from the authoritative redb tables, atomic-swap it over
the live directory, and report row counts + timing.

Concrete deliverables:

1. New module `crates/brain-ops/src/ops/text_indexer/rebuild.rs`:
   - `RebuildReport { rows_processed: u64, duration: Duration }`.
   - `RebuildError` taxonomy (metadata / tantivy / io).
   - `rebuild_memory_text(shard_dir: &Path, metadata: &MetadataDb) -> Result<RebuildReport, RebuildError>`.
   - `rebuild_statements(shard_dir: &Path, metadata: &MetadataDb) -> Result<RebuildReport, RebuildError>`.
2. Both functions follow the same shape:
   1. Truncate `<live>.rebuild/` if it exists from a prior aborted attempt.
   2. `Index::create_in_dir(&rebuild_dir, schema)` with the schema from `brain-index`.
   3. Register the brain analyzer (22.2) on the new index.
   4. Stream rows from redb with a read txn; for each row, `add_document(...)` then increment a counter.
   5. Commit on the same N=256 cadence as the live indexer (no need to be elaborate — call `commit()` per chunk + once at end).
   6. Stamp the brain schema payload via `prepared.set_payload(schema_payload_json())` on the final commit.
   7. Drop the writer (releases the directory lock).
   8. Atomic rename swap: `<live>` → `<live>.old`, `<live>.rebuild` → `<live>`, `rm -rf <live>.old`.
3. Source-of-truth joins:
   - **Memory rebuild**: iterate `MEMORIES_TABLE`; skip rows with no text; project each row to the indexer document shape (same field bindings as 22.3).
   - **Statement rebuild**: iterate `STATEMENTS_TABLE`; for each, join `ENTITIES_TABLE.canonical_name` (subject) + `PREDICATES_TABLE.name` + compute object_text via the same projection 22.4 uses (`upsert_op_from_statement::object_text_for_index`).
4. Unit tests using a fresh redb + fresh tantivy directory:
   - Memory rebuild round-trip: write 5 memories via the redb writer, rebuild, search via the analyzer, assert 5 hits.
   - Statement rebuild with joins.
   - Idempotent restart: rebuild, then rebuild again — second call truncates the stale `<live>.rebuild/` and succeeds.
   - Atomic swap leaves no `<live>.old` after success.

NOT in scope:
- Coordinating with a running live writer (hot rebuild). 22.6 is for **startup-time** rebuild after `IndexStatus::NeedsRebuild`. Hot rebuild lands post-v1.
- Admin wire-op to trigger rebuild on demand — phase 28/05 admin scope.
- Cancellation / progress reporting beyond the final `RebuildReport` — post-v1.
- Cross-shard coordination — irrelevant; per-shard operation.

## 2. Spec references

- `spec/26_knowledge_storage/01_tantivy_layout.md` §5 — binding.
  Algorithm steps 1–6.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §6 — recovery
  triggers rebuild (consumed by 22.7).
- `spec/27_knowledge_workers/02_text_indexer_workers.md` §3 —
  statement text repr (`subject + predicate + object`) reused
  for the rebuild row builder.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `std::fs::rename` atomicity on Linux | POSIX `rename(2)` | Yes — atomic when source + target are on the same filesystem. Shard dir is one filesystem in v1. |
| `std::fs::remove_dir_all` | std | Yes — recurses; OK for cleanup of `<live>.old`. |
| `redb::Database::begin_read` + table iteration | redb 4 docs | Yes — `Range` iterator over the whole table. |
| Iterating `MEMORIES_TABLE` | `crates/brain-metadata/src/tables/memory.rs` | Existing helpers; if there's no public iterator we add one. |
| Iterating `STATEMENTS_TABLE` | `crates/brain-metadata/src/statement_ops.rs` | Existing `statement_list` covers a subset; for rebuild we want ALL non-tombstoned statements regardless of supersession. Add a new `statement_list_all` helper if needed. |

The redb iteration helpers are the **one open question** — see §6.

## 4. Architecture sketch

```rust
// crates/brain-ops/src/ops/text_indexer/rebuild.rs

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use brain_index::{
    build_analyzer, memory_text_schema, schema_payload_json, statements_schema,
    BRAIN_TOKENIZER_NAME, LexicalScope,
};
use brain_metadata::MetadataDb;
use tantivy::{Index, IndexWriter, TantivyDocument, TantivyError};
use thiserror::Error;

const REBUILD_SUFFIX: &str = ".rebuild";
const OLD_SUFFIX: &str = ".old";
const COMMIT_CHUNK: usize = 1024;

#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub scope: LexicalScope,
    pub rows_processed: u64,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum RebuildError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy: {0}")]
    Tantivy(#[from] TantivyError),
    #[error("metadata: {0}")]
    Metadata(String),
}

pub fn rebuild_memory_text(
    shard_dir: &Path,
    metadata: &MetadataDb,
) -> Result<RebuildReport, RebuildError> { ... }

pub fn rebuild_statements(
    shard_dir: &Path,
    metadata: &MetadataDb,
) -> Result<RebuildReport, RebuildError> { ... }

// Shared inner:
fn rebuild_with<F>(
    live: &Path,
    schema: tantivy::schema::Schema,
    iterate_and_index: F,
    scope: LexicalScope,
) -> Result<RebuildReport, RebuildError>
where
    F: FnOnce(&mut IndexWriter, &MetadataDb) -> Result<u64, RebuildError>,
{
    let started = Instant::now();
    let rebuild_dir = path_with_suffix(live, REBUILD_SUFFIX);
    let old_dir = path_with_suffix(live, OLD_SUFFIX);

    // Step 1: truncate stale rebuild dir.
    if rebuild_dir.exists() {
        std::fs::remove_dir_all(&rebuild_dir)?;
    }
    std::fs::create_dir_all(&rebuild_dir)?;

    // Step 2-3: open + register tokenizer.
    let index = Index::create_in_dir(&rebuild_dir, schema)?;
    index.tokenizers().register(BRAIN_TOKENIZER_NAME, build_analyzer());

    // Step 4-6: iterate, write, commit + stamp payload.
    let mut writer = index.writer_with_num_threads(1, 50_000_000)?;
    let rows = iterate_and_index(&mut writer, /* injected metadata */)?;
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&schema_payload_json());
    prepared.commit()?;
    drop(writer);
    drop(index);

    // Step 7-8: atomic swap.
    if live.exists() {
        if old_dir.exists() {
            std::fs::remove_dir_all(&old_dir)?;
        }
        std::fs::rename(live, &old_dir)?;
    }
    std::fs::rename(&rebuild_dir, live)?;
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir)?;
    }

    Ok(RebuildReport { scope, rows_processed: rows, duration: started.elapsed() })
}
```

Per-scope iterator:

```rust
fn iterate_memory(writer: &mut IndexWriter, metadata: &MetadataDb)
    -> Result<u64, RebuildError>
{
    let rtxn = metadata.read_txn().map_err(|e| RebuildError::Metadata(e.to_string()))?;
    let table = rtxn.open_table(MEMORIES_TABLE).map_err(...)?;
    let mut count = 0u64;
    let mut chunk = 0usize;
    for entry in table.iter().map_err(...)? {
        let (id_bytes, meta) = entry.map_err(...)?.value_pair();
        let Some(text) = meta.text.as_ref() else { continue };
        let doc = memory_doc(id_bytes, text, &meta);
        writer.add_document(doc)?;
        count += 1;
        chunk += 1;
        if chunk >= COMMIT_CHUNK {
            // intermediate commit — keeps segment count manageable
            writer.commit()?;
            chunk = 0;
        }
    }
    Ok(count)
}
```

Statement iterator: open STATEMENTS_TABLE, for each row look up
entity canonical_name + predicate.name from their respective
tables (in the same rtxn — redb supports nested table opens).
Skip tombstoned statements (lexical index doesn't carry them).

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Atomic dir-swap (this plan) | Reuses tantivy's directory semantics; no per-segment merge gymnastics; matches §26/01 §5 verbatim | Requires same-filesystem for the rename | ✓ |
| In-place rewrite of the live index | One less directory shuffle | Coordinates with live writers; tantivy lock contention | rejected — startup-only context doesn't need it |
| Hot rebuild via tantivy's segment-merging hooks | Online | Complex; requires writer cooperation | rejected — post-v1 |
| Cancel/resume support | Operator-friendly for long rebuilds | Adds state machine + persistent progress; v1 scale doesn't need it | rejected — post-v1 |
| Combined rebuild fn that drives both indexes | One call site | Failure of one shouldn't block the other; separate fns let 22.7 schedule each independently | rejected |

## 6. Risks / open questions

- **Risk:** `MEMORIES_TABLE` / `STATEMENTS_TABLE` iteration helpers may not exist publicly. **Mitigation:** add minimal `pub fn iter_*` helpers in brain-metadata if missing.
- **Risk:** Statement reconstruction includes tombstoned rows. **Resolution:** skip rows where `statement.tombstoned == true` (lexical layer carries only live statements per §27/02 §3).
- **Risk:** During the rename, an interrupted process leaves `<live>.old` or `<live>.rebuild` behind. **Mitigation:** every rebuild start cleans both; recovery (22.7) calls rebuild after detecting stale state.
- **Open question:** Should the rebuild stamp the **last_indexed_unix_ms** cursor for 22.7's partial-replay use? **Resolution:** v1 has no partial replay (22.7 decides full-rebuild-or-nothing), so the payload stays at just `brain_schema_version`. If 22.7's plan changes its mind, this plan grows the payload shape.

## 7. Test plan

Unit tests in `crates/brain-ops/src/ops/text_indexer/rebuild/tests.rs`:

- `rebuild_memory_text_round_trip` — write 3 memories with text + 2 without via redb, run `rebuild_memory_text`, open the new index, verify exactly 3 docs (skipping the textless ones) and BM25 query returns them.
- `rebuild_memory_text_idempotent` — run twice in a row; second call wipes the stale rebuild dir; succeeds.
- `rebuild_statements_with_joins` — set up an entity (canonical_name="Alice") + predicate ("lives_in") + one statement referencing them; rebuild; query `subject_name="alice"`; one hit.
- `rebuild_statements_skips_tombstoned` — two statements, tombstone one; rebuild; one hit only.
- `rebuild_after_corrupt_live` — write garbage to `<live>/meta.json`; run rebuild; live index is replaced; subsequent `Index::open_in_dir(<live>)` succeeds.
- `atomic_swap_no_stale_dirs` — after a successful rebuild, neither `<live>.rebuild/` nor `<live>.old/` exists on disk.
- `rebuild_creates_payload_for_22_1_reopen` — after rebuild, open the live dir via `TantivyShard::open` and assert `IndexStatus::Ready`.

## 8. Commit shape

```
feat(ops): 22.6 — index rebuild worker

- crates/brain-ops/src/ops/text_indexer/rebuild.rs (new):
  rebuild_memory_text + rebuild_statements + shared
  rebuild_with helper. RebuildReport + RebuildError.
  Atomic-swap via std::fs::rename per §26/01 §5; cleans stale
  `.rebuild/` + `.old/` on entry. Stamps schema payload on
  final commit so 22.1 / 22.7 see `Ready` post-rebuild.
- crates/brain-ops/src/ops/text_indexer/rebuild/tests.rs (new):
  7 tests — round-trip, idempotent, joins, tombstone skip,
  corrupt-live recovery, no stale dirs, post-rebuild Ready.
- crates/brain-ops/src/ops/text_indexer/mod.rs: re-export
  `RebuildReport`, `RebuildError`, `rebuild_memory_text`,
  `rebuild_statements`.
- crates/brain-metadata: small `iter_memories` / `iter_statements_all`
  helpers if not already public.
```

## 9. Confirmation

Please confirm:

1. **Module lives in `brain-ops`** alongside the 22.3/22.4 indexer modules (vs `brain-workers` or `brain-index`).
2. **Startup-only rebuild** for v1; hot rebuild explicitly cut.
3. **Atomic rename** is the swap mechanism (vs in-place rewrite); same-filesystem constraint accepted.
4. **Tombstoned statements skipped** during rebuild (lexical index is "live statements only").
5. **No cancellation / progress reporting** beyond the final `RebuildReport`.
