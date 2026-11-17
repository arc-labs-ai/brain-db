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
moment of crash. Those writes are still in redb, which is
authoritative, so nothing is lost from the database — but the
lexical index is the one place they are missing, and the only thing
that puts them back is a §5 rebuild from the redb tables. The
indexer keeps no durable position to resume from, so a crash leaves
no record of where its batch stopped.

This is why the gap does not close by itself. A directory that
reopens cleanly at the current schema version reports `Ready`, and a
`Ready` scope is not rebuilt (§6) — so an unindexed tail stays
unindexed until a rebuild is triggered, and a lexical read over that
window silently returns fewer hits than redb could support. Shrinking
N shrinks the window.

Configurable per shard via `BRAIN_TANTIVY_COMMIT_N` (default 256)
and `BRAIN_TANTIVY_COMMIT_MS` (default 1000) env vars.

## 4. Segment merge

Default `LogMergePolicy` from tantivy. v1 does NOT schedule
merges in low-traffic windows — the policy runs as part of
tantivy's background merger threads, which participate in the
shard's I/O budget via OS scheduling.

Segment-merge windowing (running merges only during periodic
low-traffic intervals) is a post-v1 improvement.

## 5. Rebuild from authoritative state

A rebuild is the only path that mutates a tantivy directory
without going through the live `IndexWriter`. It runs:

- On startup, for any scope whose open reports `NeedsRebuild` —
  the directory would not open, its stamped schema version is not
  the current one, or its payload would not parse (§6).
- On explicit admin trigger.

An absent or empty scope directory is **not** a rebuild trigger: it
is a fresh index, created in place and reported ready.

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
5. After all rows are indexed, fsync the directory and swap it in
   with two renames: `<live>` → `<live>.old`, then
   `<live>.rebuild` → `<live>`. Then remove `<live>.old`.
6. Re-open the live index and publish the new reader bundle.

The swap in step 5 is **two renames, not one atomic operation**, so
a crash can land between them and leave `<live>` missing or empty
with the real data stranded in a sibling. Every open therefore
reconciles that state first, before deciding whether to create or
open, resolving to the newest *complete* index: promote
`<live>.rebuild` if it is complete (the swap had got as far as
committing the new index), otherwise restore `<live>.old`, then
remove whatever siblings remain. Reconciling before the
create-or-open decision is what matters — an open that mistook the
missing live directory for a fresh shard would create an empty
index and silently discard the completed rebuild.

"Complete" here means the directory opens as a tantivy index **and**
carries a stamped brain schema-version payload. Tantivy writes a
`meta.json` at creation but the payload is stamped only at the final
commit, so a half-built rebuild is never mistaken for a finished
one.

Reads are never interrupted by a rebuild. The retriever holds its
open shard and both cached `IndexReader`s in one `ArcSwap`, and the
swap that publishes a rebuilt index builds the whole bundle —
shard plus both readers — *before* storing it. So a `retrieve`
straddling the swap sees either the complete pre-rebuild index or
the complete post-rebuild index, never a mixed or empty one, and a
bundle that fails to build leaves the prior one serving untouched.
A reader is bound to the `Index` it was opened from, which is why a
post-rebuild index needs fresh readers rather than a reload of the
old ones.

The rebuild worker emits progress metrics (rows indexed, ETA) and a
final completion log line.

Idempotency: a rebuild is safe to restart from scratch at any
point — the `<live>.rebuild/` directory is truncated on entry.

## 6. Recovery on startup

On shard spawn:

1. `Index::open(memory_text.tantivy)` and `Index::open(statements.tantivy)`.
2. If both succeed AND schema version matches: ready.
3. Each scope's open returns its `Index` paired with a status:
   `Ready`, or `NeedsRebuild` carrying why. The reason is
   `SchemaVersionMismatch { found, expected }` or `PayloadCorrupt`
   when the directory opened but its stamped payload is not the
   current one or will not parse, and `OpenFailed` when the
   directory would not open at all. In that last case there is no
   real index to hand back, so the open yields a RAM-backed
   placeholder to satisfy the type contract. The two scopes' statuses
   are independent, so one scope needing a rebuild leaves the other
   available.
4. Any scope reporting `NeedsRebuild` is rebuilt **inline, here**,
   before the shard serves anything, and the directory is then
   re-opened. Recovery's contract is that it returns a shard whose
   scopes are both `Ready`; it asserts that on the re-open rather
   than trusting it. A shard therefore never begins serving with a
   scope in a non-ready state, and the placeholder from step 3 is
   replaced before any read could reach it.

Recovery has exactly one repair, and it is the §5 rebuild from the
redb tables. There is no WAL replay on this path: the lexical indexes
take no part in the WAL's durability story, so recovery has no log
position to resume from and no per-write replay to perform. A scope is
either `Ready` and left alone, or rebuilt whole. (The memory HNSW is
the opposite case — its snapshot records the LSN it was taken at and
recovery replays the WAL past that point — so the two derived indexes
do not recover the same way.)

An empty scope directory is a fresh index, not a broken one: only a
directory holding a committed `meta.json` is a previously-built
index, and a newly created index that has never committed reports
`Ready` because the first writer commit is what stamps the payload.

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
backup will reflect only committed segments. Operators do NOT need a
quiesce-before-snapshot ritual: a backup that comes back *stale* —
stamped at an older schema version, or unopenable — reports
`NeedsRebuild` and is rebuilt from redb on next start (§6), so it
self-heals.

A backup that comes back **missing** does not. An absent or empty
scope directory is indistinguishable from a fresh shard, so it is
created empty and reported `Ready`, and a `Ready` scope is never
rebuilt. The shard then serves lexical reads against an empty index
without complaining. Restoring a shard without its `*.tantivy/`
directories therefore needs an explicit rebuild, and restoring them
is the cheaper path.
