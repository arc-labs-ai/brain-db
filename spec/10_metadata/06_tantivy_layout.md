# 10.06 Per-shard tantivy layout

Normative storage spec for the two tantivy indexes per shard.
Extends [`./00_purpose.md`](./00_purpose.md) which introduces the
directory names and field lists.

Consumers:
- LexicalRetriever — reads. See
  [`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md).
- Text-indexer workers — writes. See
  [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md).
- Directory init + `Index::open` at shard spawn.
- Rebuild path (see §5).

## 1. Directory layout

```
data/
  shards/000/
    memory_text.tantivy/           ── live index (tantivy-managed)
    memory_text.tantivy.rebuild/   ── present during atomic rebuild only
    statements.tantivy/
    statements.tantivy.rebuild/
```

Each `*.tantivy/` directory is owned by tantivy's `IndexWriter`
and contains its segment files, `meta.json`, and write lock.
Brain's arena/WAL disciplines (see
[`../08_storage/00_purpose.md`](../08_storage/00_purpose.md)) do
**not** apply — tantivy manages its own files.

The `*.rebuild/` directories exist only during the rebuild flow
in §5 below. They are absent in steady state.

## 2. Schemas

The schemas are normative — Brain wires them verbatim, and
[`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md)
relies on the exact field set for filters.

### `memory_text.tantivy/`

| Field | Type | Options |
|---|---|---|
| `memory_id` | bytes | `INDEXED | STORED` — 16 big-endian bytes of the packed u128 `MemoryId`. Indexed so the text-indexer worker can `delete_term` by id (idempotent upsert + FORGET); stored so the lexical retriever surfaces it in `RankedItem.id`. |
| `text` | text | `TEXT` (tokenized + stemmed + indexed for BM25). Uses the custom analyzer from [`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md). |
| `space_id` | bytes | `INDEXED | STORED` (16-byte UUID; indexed for exact-match filter; not tokenized). |
| `kind` | u64 | `INT` indexed for filter (memory kind enum). |
| `created_at` | u64 | `INT` indexed; unix-ms epoch. Range queries supported. |

### `statements.tantivy/`

| Field | Type | Options |
|---|---|---|
| `statement_id` | bytes (u128) | `INDEXED | STORED` — 16 big-endian bytes of the u128 `StatementId`. Indexed so the statement text-indexer worker can `delete_term` by id (tombstone / supersede); stored so retrieval surfaces it in `RankedItem.id`. |
| `subject_name` | text | `TEXT` with the analyzer from [`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md); lets queries match the subject's canonical entity name. |
| `predicate_name` | text | `STRING` (raw, untokenised text — exact match). The predicate's `name` field, not its u64 id — exact-id filters work via the same string. |
| `predicate_id` | u64 | `INT` indexed for exact filter — alternative to `predicate_name` for downstream callers that already resolved the id. |
| `object_text` | text | `TEXT` with the same analyzer. |
| `kind` | u64 | `INT` indexed; statement kind (Fact / Preference / Event). |
| `confidence_bucket` | u64 | `INT` indexed; values 0–9, computed as `(confidence * 10).floor()`. Range queries supported. |
| `extracted_at` | u64 | `INT` indexed; unix-ms epoch. |

Buckets at 0.1 resolution match the §00 binding and keep the
index small without losing useful filter granularity.

### Schema version

Both schemas are tagged via `meta.json` writer field
`brain_schema_version: u32` (incremented on any schema change).

Mismatch on open → trigger §5 rebuild. Brain ships at
`brain_schema_version: 1`.

## 3. Commit cadence

Group-commit discipline, matching the WAL group-commit (see
[`../08_storage/02_wal.md`](../08_storage/02_wal.md)):

- **N = 256 writes** OR **T = 1 second**, whichever first.
- The text-indexer worker (see
  [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md))
  maintains a single
  `IndexWriter` per index per shard and drives `commit()` on
  this cadence.
- `commit()` is fsync-anchored by tantivy; after `Ok(())` the
  documents are durable.

**Loss bound at crash:** up to N-1 writes are unindexed at the
moment of crash. Those writes are recovered by §5 §"Recovery"
which replays from the authoritative redb tables and the WAL
tail.

Configurable per shard via `BRAIN_TANTIVY_COMMIT_N` (default 256)
and `BRAIN_TANTIVY_COMMIT_MS` (default 1000) env vars.

## 4. Segment merge

Default `LogMergePolicy` from tantivy. v1 does NOT schedule
merges in low-traffic windows — the policy runs as part of
tantivy's background merger threads, which participate in the
shard's I/O budget via OS scheduling.

Segment-merge windowing (running merges only during periodic
low-traffic intervals) is a post-v1 improvement (tracked in
[`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md)).

## 5. Rebuild from authoritative state

A rebuild is the only path that mutates a tantivy directory
without going through the live `IndexWriter`. It runs:

- On startup if `Index::open` returns `Err` (corrupt segments,
  missing files, schema-version mismatch).
- On explicit admin trigger.

Algorithm:

1. Compute target directory: `<live>.rebuild/` (e.g.
   `memory_text.tantivy.rebuild/`). Truncate if it exists from
   a previous aborted rebuild.
2. Open a fresh `Index` in that directory with the schema from §2.
3. Iterate the authoritative redb source for the scope:
   - Memory text scope: redb `MEMORIES_TABLE`, project `text`
     column + metadata fields (`space_id`, `kind`, `created_at`).
   - Statement text scope: redb `STATEMENTS_TABLE` joined with
     `ENTITIES_TABLE.canonical_name` (for `subject_name`) and
     `PREDICATES_TABLE.name` (for `predicate_name`). Compute the
     text repr at index time.
4. Bulk-add into the rebuild writer; commit on the standard
   cadence (§3).
5. After all rows are indexed, fsync the directory and **atomic
   rename**: `<live>` → `<live>.old`, `<live>.rebuild` →
   `<live>`. Then remove `<live>.old`.
6. Re-open the live index. Readers that held a handle to the
   old `Index` keep operating against in-memory segments; new
   readers via the standard `Index::open` pick up the new tree.

During step 1–5, `LexicalRetriever::retrieve()` on that scope
returns `LexicalError::IndexUnavailable`. The rebuild worker
emits progress metrics (rows indexed, ETA) and a final completion
log line.

Idempotency: a rebuild is safe to restart from scratch at any
point — the `<live>.rebuild/` directory is truncated on entry.

## 6. Recovery on startup

On shard spawn:

1. `Index::open(memory_text.tantivy)` and `Index::open(statements.tantivy)`.
2. If both succeed AND schema version matches: ready.
3. Replay WAL tail — any post-commit memory or statement writes
   that landed in redb but not yet in tantivy (because the
   indexer was mid-batch). The indexer drains them through the
   standard write path; `delete_term(id)` then `add_document(...)`
   ensures idempotency on replay of an already-committed write.
4. If `Index::open` fails for either index: schedule §5 rebuild
   for the failed index. The other scope remains available.
5. Reads on a scope under rebuild return `IndexUnavailable`
   until the rebuild commits.

## 7. Size budgets (informational)

Repeat of [`./00_purpose.md`](./00_purpose.md) §7 for cross-reference:

| Index | At 1M docs |
|---|---|
| `memory_text.tantivy` | ~500 MB |
| `statements.tantivy` | ~100 MB |

Statement entries are denser (shorter text, fixed-shape fields)
hence smaller per-doc.

## 8. Operator-visible files

The two `*.tantivy/` directories are **safe to back up** with
filesystem snapshot tools while the shard is running, but the
backup will reflect only committed segments. Brain
recovery path treats any missing or stale tantivy backup as
"rebuild on next start" (§6) — operators do NOT need a
quiesce-before-snapshot ritual.
