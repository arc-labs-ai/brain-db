# 27.02 Text indexer workers

Normative spec for the memory + statement text indexer workers
introduced by §00 (rows 15–16 of the workers table). Implements
the writes that §26/01 stores and §23/02 reads.

Phase 22 owns the implementation:
- 22.3 — `MemoryTextIndexer` worker.
- 22.4 — `StatementTextIndexer` worker.
- 22.6 — rebuild path (shared infrastructure with this file).

## 1. Two workers, one discipline

| Worker | Trigger | Source of truth | Index |
|---|---|---|---|
| `MemoryTextIndexer` | ENCODE post-WAL-commit | redb `MEMORIES_TABLE` | `memory_text.tantivy/` |
| `StatementTextIndexer` | statement_create / supersede / tombstone post-commit | redb `STATEMENTS_TABLE` + entity + predicate joins | `statements.tantivy/` |

Both run on the **near-foreground** priority lane (§27/00 §"Worker scheduling",
25% of shard time).

Both use **bounded queues** with capacity 4096 by default.

Both use **backpressure-on-overflow**, NOT drop-on-overflow.
This is the only worker class in §27 that backpressures the
foreground; every other knowledge-layer worker (classifier
extractor, LLM extractor, entity resolver, embedding workers,
audit-log sweeper) drops on queue full and records a metric.

**Justification:** lexical recall is a correctness property of
hybrid query (§24). Silent index drift — where a memory exists
in redb but not in tantivy — would mean clients see incomplete
results without any audit trail. Backpressure is preferable: the
foreground op waits a few milliseconds, the user sees a slightly
slower ENCODE, but the index stays consistent.

When the queue is at capacity:
- The post-commit pipeline `await`s on the channel send.
- ENCODE / statement_create complete only after the indexer
  receives the item. Their P99 budgets in §16/02 §2.1 and §2.3
  absorb the wait (single-shard tantivy add is ~50 µs).

## 2. MemoryTextIndexer

Input: `IndexableMemory { id: MemoryId, text: String, agent_id: AgentId, kind: MemoryKind, created_at_unix_ms: u64 }`.

Loop:

```
while let Some(item) = queue.recv().await {
    writer.delete_term(memory_id_term(item.id));   // idempotent
    writer.add_document(doc! {
        memory_id => item.id.as_u64(),
        text => item.text,
        agent_id => item.agent_id.as_bytes(),
        kind => item.kind as u64,
        created_at => item.created_at_unix_ms,
    })?;
    batch_size += 1;
    if commit_due(batch_size, last_commit_at) {
        writer.commit()?;
        batch_size = 0;
        last_commit_at = now();
    }
}
```

On FORGET: a separate channel sends `Forget { id: MemoryId }`.
The worker issues `writer.delete_term(memory_id_term(id))` and
counts it as one write toward the commit cadence.

**Memories without text** (`memory.text == None` — substrate-only
memories or memories whose text was elided) are NOT enqueued.

## 3. StatementTextIndexer

Input: `IndexableStatement { id: StatementId, op: StatementIndexOp }` where:

```
enum StatementIndexOp {
    Upsert { subject_canonical_name: String,
             predicate_name: String,
             object_text: String,
             kind: StatementKind,
             confidence: f32,
             extracted_at_unix_ms: u64 },
    Delete,
}
```

`Upsert` is delete-then-add (idempotent at replay):

```
writer.delete_term(statement_id_term(id));
if let Upsert { .. } = op {
    let bucket = ((confidence * 10.0).floor() as u8).min(9) as u64;
    writer.add_document(doc! {
        statement_id => id.to_u128(),
        subject_name => upsert.subject_canonical_name,
        predicate_name => upsert.predicate_name.as_bytes(),
        predicate_id => predicate_id_from_name_lookup,
        object_text => upsert.object_text,
        kind => upsert.kind as u64,
        confidence_bucket => bucket,
        extracted_at => upsert.extracted_at_unix_ms,
    })?;
}
```

Text representation:
```
text_repr = subject.canonical_name + " " + predicate.name + " " + object_text
```

Matches §23/00 line 90.

**Supersession** = `Delete` for the superseded statement + a fresh
`Upsert` for the new statement (same as create flow; new
`statement_id`).

**Tombstone** = `Delete` only.

## 4. Commit policy

`commit_due(batch_size, last_commit_at)` returns `true` when:

- `batch_size >= BRAIN_TANTIVY_COMMIT_N` (default 256), OR
- `now() - last_commit_at >= BRAIN_TANTIVY_COMMIT_MS` (default 1000 ms).

On `commit()` returning `Err`:
- Retry once after a 10 ms backoff.
- On second failure: **fail the shard** (text indexing is
  required correctness, per §1). The shard supervisor logs a
  fatal error, drains other workers, and surfaces the failure to
  the operator via the shard health endpoint.

This contrasts with the LLM extractor (§22/09 §4) which retries
once on validation failure and then drops — the LLM tier is
best-effort, the text indexer is not.

## 5. WAL integration

The indexer worker sits **downstream of the WAL**. The post-commit
pipeline in `brain-ops` emits indexable events only after
`wal_record.fsync()` has returned.

Ordering on the shard's post-commit fan-out (deterministic):

1. WAL fsync.
2. redb wtxn commit (substrate + knowledge tables).
3. Pattern extractor (synchronous, phase 20).
4. Classifier extractor enqueue (near-foreground, phase 20).
5. LLM extractor enqueue (background, phase 21).
6. **MemoryTextIndexer enqueue** (near-foreground).
7. **StatementTextIndexer enqueue** (near-foreground; only if
   extractors created statements, OR if this op was a direct
   STATEMENT_CREATE).

Each is a separate shard-local queue; failures don't cascade
(except text indexer failures, which §4 specifies as
shard-fatal).

## 6. Recovery on shard start

Per §26/01 §6:

1. On shard spawn, the indexer reads its on-disk `meta.json`
   commit cursor (latest `created_at_unix_ms` indexed).
2. The substrate WAL is replayed up to its fsync watermark;
   the post-commit pipeline re-emits indexable events for any
   record whose `created_at_unix_ms` exceeds the indexer's
   commit cursor.
3. `delete_term + add_document` is idempotent at replay — an
   already-indexed memory is overwritten with the same data.

If `Index::open` fails at startup, the indexer schedules the
§26/01 §5 rebuild and the corresponding scope returns
`IndexUnavailable` until the rebuild commits.

## 7. Coordination with extractors

The text indexer is **independent** of extractors:
- `MemoryTextIndexer` indexes raw memory text, NOT extractor
  outputs.
- `StatementTextIndexer` indexes statements regardless of
  which extractor (pattern / classifier / LLM) created them.

Therefore, a failing extractor doesn't prevent text indexing.
A memory whose LLM extractor times out still has its raw text
in `memory_text.tantivy` and can be found by lexical search;
statements that were never created simply aren't in
`statements.tantivy`.

## 8. Observability

Per worker (§14 obs):

- `tantivy_indexer_queue_depth{scope}` gauge.
- `tantivy_indexer_writes_total{scope}` counter.
- `tantivy_indexer_commits_total{scope, result}` counter
  (`result` ∈ `{ok, retry, fatal}`).
- `tantivy_indexer_commit_latency_seconds{scope}` histogram.
- `tantivy_indexer_backpressure_waits_total{scope}` counter —
  every time the foreground op blocked on the queue.

Logs:
- `info` on each commit (scope, batch size, duration).
- `warn` on retry.
- `error` + shard-fatal on second commit failure.
