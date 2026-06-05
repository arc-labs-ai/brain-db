# 15.06 Typed-Graph Workers

Normative spec for the typed-graph worker families introduced by [`./00_purpose.md`](./00_purpose.md): the three extractor tiers (pattern, classifier, LLM), the memory + statement text indexers, the five low-priority sweepers (supersession, audit-log, LLM cache, stale-extraction detection, entity GC + confidence refresh), the three trigger-driven state-carrying workers (backfill, FORGET cascade, schema migration), and the entity garbage-collection sweep. This file collects each worker family's contract — dispatch, queue shape, batch discipline, idempotency, observability — that the original per-family pages described separately.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — worker overview + shared scheduling priorities.
- [`../11_extractors/`](../11_extractors/00_purpose.md) — extractor tier semantics, audit rows.
- [`../10_metadata/`](../10_metadata/00_purpose.md) — retention windows the sweepers enforce.
- [`../02_data_model/`](../02_data_model/00_purpose.md) — record types the workers operate on.

## Extractor workers

Worker-side scheduling for the three extractor tiers (pattern, classifier, LLM). Brain ships pattern + classifier + LLM workers as a single integrated dispatcher.

Cross-references:
- [`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) — pattern / classifier / LLM tier semantics.
- [`../11_extractors/04_audit.md`](../11_extractors/04_audit.md) — audit row written by each worker.

### Dispatch from ENCODE

```text
ENCODE(memory) → wtxn.commit() → emit("Encoded" event)
                                   │
                                   ▼
              ┌──────────────────────────────────────┐
              │ for each active extractor:           │
              │   if trigger(memory): dispatch       │
              └──────────────────────────────────────┘
                                   │
              ┌────────────────────┼────────────────────┐
              ▼                    ▼                    ▼
        Pattern queue       Classifier queue        LLM queue
        (foreground)        (near-foreground)       (background)

```

Dispatch order:
1. Pattern extractors run **synchronously** inside the ENCODE op handler, before the response is returned. Their outputs are already persisted when ENCODE acknowledges.
2. Classifier extractors are **enqueued** onto the near-foreground queue. ENCODE doesn't wait for them. Their outputs are visible 1–10 ms later (typical p99).
3. LLM extractors are **enqueued** onto the background queue. The LLM dispatcher drains the queue out-of-band; outputs are visible once each call completes.

### Queue shapes

```rust
pub struct ExtractorQueue {
    pub tier: ExtractorTier,
    pub capacity: usize,
    pub overflow_policy: OverflowPolicy,
    pub items: VecDeque<QueueItem>,
}

pub enum ExtractorTier { Pattern, Classifier, Llm }

pub struct QueueItem {
    pub memory_id: MemoryId,
    pub extractor_id: ExtractorId,
    pub enqueued_at_unix_nanos: u64,
    /// Deps already resolved? (per [`../11_extractors/03_resolver.md`](../11_extractors/03_resolver.md))
    pub deps_satisfied: bool,
}
```

Per-tier defaults (operator-configurable):

| Tier | Capacity | Overflow |
|---|---|---|
| Pattern | n/a (synchronous, no queue) | n/a |
| Classifier | 1 000 | `Drop` + metric |
| LLM | 10 000 | `Drop` + metric |

Overflow policy `Drop` records a metric counter and emits a warn-level trace event so the operator sees pressure. The dropped extraction writes an audit row `Skipped(reason: "queue full")`.

### Scheduling priorities

Inherited from [`./00_purpose.md`](./00_purpose.md) §"Scheduling priorities":

| Tier | Priority | Budget |
|---|---|---|
| Pattern | Foreground | shares the ENCODE op's allowance |
| Classifier | Near-foreground | 25% of shard time |
| LLM | Background | 20% of shard time |

The per-shard executor's cooperative-yield model applies — a long classifier inference yields between memories to let foreground work proceed.

### Dispatch eligibility check

Before enqueueing, the dispatcher walks the active extractors and filters:

```rust
fn is_dispatchable(ext: &ExtractorRow, mem: &Memory) -> bool {
    if !ext.enabled { return false; }
    let trigger = compile_trigger(&ext.trigger);
    if !trigger.evaluate(mem) { return false; }
    // depends_on resolution checked at dequeue time (per ../11_extractors/03_resolver.md).
    true
}
```

Pattern dispatch runs this filter synchronously in ENCODE. Classifier dispatch runs the same filter — if the trigger doesn't match, no audit row is written (no row, no skip). The condition "trigger eval error" still writes `Skipped(reason: "trigger eval error")` because the extractor was eligible but the condition itself was malformed.

### Worker loop

Per-tier worker task:

```rust
async fn run_tier(tier: ExtractorTier, ctx: &OpsContext) -> Result<(), ()> {
    loop {
        let item = ctx.queues.dequeue(tier).await?;
        if !item.deps_satisfied {
            ctx.queues.requeue(tier, item).await?;
            continue;
        }
        run_one_extraction(ctx, item).await;
    }
}
```

`run_one_extraction` is the same function for any tier; it dispatches to the right `dyn Extractor` impl based on the registry entry.

### Backpressure

On classifier queue overflow:

1. The dispatcher writes the audit row directly with `Skipped(queue full)`.
2. A `worker_queue_overflow_total{worker=classifier}` counter increments.
3. The operator sees the metric and either:
   - Lowers the trigger filter (less work).
   - Disables the offending extractor.
   - Scales out (more shards).

Brain does not implement adaptive throttling.

### Disabled extractors

`extractor.enabled = false` (set via `EXTRACTOR_DISABLE`) means dispatch skips the extractor entirely at the eligibility filter. In-flight items already in the queue dequeue and run to completion ("disabling is non-disruptive").

The audit row a disabled extractor would have written becomes `Skipped(reason: "disabled")` if the dispatcher caught it; in-flight items run normally and write `Success` / `Failure`.

### Graceful shutdown

On shard shutdown:
1. Dispatcher stops accepting new items.
2. Workers drain pending items with a 30 s timeout.
3. Items not drained get audit-row stubs `Skipped(reason: "shutdown drain timeout")` so the post-restart operator sees what was lost.

Brain ships steps 1+2; step 3 (timeout stub-writing) is deferred to avoid touching the shutdown path more than needed.

### Extractor observability

Per [`./00_purpose.md`](./00_purpose.md) (Observability) plus extractor-specific:

- `extractor_dispatch_total{tier, extractor_id}` — items dispatched.
- `extractor_skipped_total{tier, extractor_id, reason}` — filter / disabled / queue-full / dep-not-ready.
- `extractor_run_seconds{tier, extractor_id}` — histogram.
- `extractor_audit_writes_total{status}` — Success / Failure / Skipped* / SkippedDuplicate.

### Extractor tests

Brain's tests verify:

- Pattern dispatch is in-process synchronous (no queue).
- Classifier queue overflow drops + writes audit + emits metric.
- `enabled = false` causes dispatch to skip.
- `depends_on` chain blocks dequeue until parent's audit row appears.
- Shutdown drains within 30 s timeout.

## Text indexer workers

Normative spec for the memory + statement text indexer workers introduced by [`./00_purpose.md`](./00_purpose.md) (rows 15–16 of the workers table). Implements the writes that [`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md) stores and [`../13_retrievers/02_lexical_retriever.md`](../13_retrievers/02_lexical_retriever.md) reads.

Brain ships:
- `MemoryTextIndexer` worker.
- `StatementTextIndexer` worker.
- A shared rebuild path used by both.

### Two workers, one discipline

| Worker | Trigger | Source of truth | Index |
|---|---|---|---|
| `MemoryTextIndexer` | ENCODE post-WAL-commit | redb `MEMORIES_TABLE` | `memory_text.tantivy/` |
| `StatementTextIndexer` | statement_create / supersede / tombstone post-commit | redb `STATEMENTS_TABLE` + entity + predicate joins | `statements.tantivy/` |

Both run on the **near-foreground** priority lane (see [`./00_purpose.md`](./00_purpose.md), 25% of shard time).

Both use **bounded queues** with capacity 4096 by default.

Both use **backpressure-on-overflow**, NOT drop-on-overflow. This is the only worker class in this section that backpressures the foreground; every other typed-graph worker (classifier extractor, LLM extractor, entity resolver, embedding workers, audit-log sweeper) drops on queue full and records a metric.

**Justification:** lexical recall is a correctness property of query (see [`../13_retrievers/05_retrieval_query.md`](../13_retrievers/05_retrieval_query.md)). Silent index drift — where a memory exists in redb but not in tantivy — would mean clients see incomplete results without any audit trail. Backpressure is preferable: the foreground op waits a few milliseconds, the user sees a slightly slower ENCODE, but the index stays consistent.

When the queue is at capacity:
- The post-commit pipeline `await`s on the channel send.
- ENCODE / statement_create complete only after the indexer receives the item. Their P99 budgets (see [`../19_benchmarks/02_performance_targets.md`](../19_benchmarks/02_performance_targets.md)) absorb the wait (single-shard tantivy add is ~50 µs).

### MemoryTextIndexer

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

On FORGET: a separate channel sends `Forget { id: MemoryId }`. The worker issues `writer.delete_term(memory_id_term(id))` and counts it as one write toward the commit cadence.

**Memories without text** (`memory.text == None` — schemaless memories or memories whose text was elided) are NOT enqueued.

### StatementTextIndexer

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
    // Canonical bucket: floor(c*10).clamp(0,10), 0..=10 — same formula as
    // the redb statements_by_predicate index (§10/02), so both indexes agree.
    let bucket = u64::from(confidence_bucket(confidence));
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

Matches [`../13_retrievers/00_purpose.md`](../13_retrievers/00_purpose.md).

**Supersession** = `Delete` for the superseded statement + a fresh `Upsert` for the new statement (same as create flow; new `statement_id`).

**Tombstone** = `Delete` only.

### Commit policy

`commit_due(batch_size, last_commit_at)` returns `true` when:

- `batch_size >= BRAIN_TANTIVY_COMMIT_N` (default 256), OR
- `now() - last_commit_at >= BRAIN_TANTIVY_COMMIT_MS` (default 1000 ms).

On `commit()` returning `Err`:
- Retry once after a 10 ms backoff.
- On second failure: **fail the shard** (text indexing is required correctness, per "Two workers, one discipline"). The shard supervisor logs a fatal error, drains other workers, and surfaces the failure to the operator via the shard health endpoint.

This contrasts with the LLM extractor (see [`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md)) which retries once on validation failure and then drops — the LLM tier is best-effort, the text indexer is not.

### WAL integration

The indexer worker sits **downstream of the WAL**. The post-commit pipeline in `brain-ops` emits indexable events only after `wal_record.fsync()` has returned.

Ordering on the shard's post-commit fan-out (deterministic):

1. WAL fsync.
2. redb wtxn commit (memory + typed-graph tables).
3. Pattern extractor (synchronous).
4. Classifier extractor enqueue (near-foreground).
5. LLM extractor enqueue (background).
6. **MemoryTextIndexer enqueue** (near-foreground).
7. **StatementTextIndexer enqueue** (near-foreground; only if extractors created statements, OR if this op was a direct STATEMENT_CREATE).

Each is a separate shard-local queue; failures don't cascade (except text indexer failures, which "Commit policy" specifies as shard-fatal).

### Recovery on shard start

Per [`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md):

1. On shard spawn, the indexer reads its on-disk `meta.json` commit cursor (latest `created_at_unix_ms` indexed).
2. Brain WAL is replayed up to its fsync watermark; the post-commit pipeline re-emits indexable events for any record whose `created_at_unix_ms` exceeds the indexer's commit cursor.
3. `delete_term + add_document` is idempotent at replay — an already-indexed memory is overwritten with the same data.

If `Index::open` fails at startup, the indexer schedules the tantivy rebuild (see [`../10_metadata/06_tantivy_layout.md`](../10_metadata/06_tantivy_layout.md)) and the corresponding scope returns `IndexUnavailable` until the rebuild commits.

### Coordination with extractors

The text indexer is **independent** of extractors:
- `MemoryTextIndexer` indexes raw memory text, NOT extractor outputs.
- `StatementTextIndexer` indexes statements regardless of which extractor (pattern / classifier / LLM) created them.

Therefore, a failing extractor doesn't prevent text indexing. A memory whose LLM extractor times out still has its raw text in `memory_text.tantivy` and can be found by lexical search; statements that were never created simply aren't in `statements.tantivy`.

### Text-indexer observability

Per worker:

- `tantivy_indexer_queue_depth{scope}` gauge.
- `tantivy_indexer_writes_total{scope}` counter.
- `tantivy_indexer_commits_total{scope, result}` counter (`result` ∈ `{ok, retry, fatal}`).
- `tantivy_indexer_commit_latency_seconds{scope}` histogram.
- `tantivy_indexer_backpressure_waits_total{scope}` counter — every time the foreground op blocked on the queue.

Logs:
- `info` on each commit (scope, batch size, duration).
- `warn` on retry.
- `error` + shard-fatal on second commit failure.

## Sweeper workers

Normative spec for the **periodic low-priority sweepers** introduced by [`./00_purpose.md`](./00_purpose.md) (rows for supersession sweeper, audit log sweeper, cache sweeper, stale extraction detection, entity GC, confidence refresh). Implemented here:

- `SupersessionSweeper`.
- `StaleExtractionDetector`.
- `LlmCacheSweeper`.
- `EntityGcWorker` (off by default).
- `AuditLogSweeper`.
- Confidence-sweep worker (refreshes statement confidence to track decay).

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — worker overview + scheduling priorities.
- [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) §"Retention" — the retention windows these sweepers enforce.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §"LLM cache" — the table layout for the LLM cache sweeper.

### Shared sweeper discipline

All sweepers obey the same invariants. Per-worker sections below document only what differs from this baseline.

#### Priority and budget

Low priority lane per [`./00_purpose.md`](./00_purpose.md) (≤ 5% of shard time). The scheduler yields after each batch.

#### Cadence

Clock-triggered. Configurable per worker via `BRAIN_<NAME>_PERIOD_SECONDS`. The scheduler computes the next tick from the previous tick's completion time; a slow tick doesn't accumulate backlog beyond one cycle.

#### Bounded batches

Each tick performs at most one batch of `batch_cap` rows (default 256 — varies per worker, documented per section). Larger backlogs clear across multiple ticks. The batch cap keeps a single redb `WriteTransaction` small enough that foreground operations aren't blocked beyond ms-scale.

#### Dry-run mode

Every sweeper exposes a `dry_run: bool` config. When `true`, the sweeper scans + counts but skips deletes. Returns a `SweepSummary { scanned, deleted, dry_run_would_delete, skipped_reasons }` shape the worker logs and exports as metrics.

The acceptance gate runs in dry-run by default; operators flip to live mode once metrics confirm the predicate is what they expect.

#### Sweeper metrics

Per-worker, prefixed `sweeper_*`:

- `sweeper_swept_total{worker, kind}` — counter of deletions (per kind where applicable; e.g. supersession sweeper distinguishes `statement` vs `relation`).
- `sweeper_skipped_total{worker, reason}` — counter of rows scanned-but-not-deleted, by reason (`within_retention`, `merge_log_exempt`, `still_referenced`, etc.).
- `sweeper_latency_seconds{worker}` — histogram of per-tick wall-time.
- `sweeper_scan_size{worker}` — gauge of last scan's row count.

#### Sweeper idempotency

Sweepers re-scan the relevant table from scratch each tick. There is no persistent cursor; if a tick is interrupted mid-batch, the next tick re-scans from the beginning. The batch cap bounds wasted re-scan work.

#### Sweeper failure handling

Per-row failures (redb errors, audit-row write failures) are logged at `warn` and the sweeper continues with the next row. A failed tick returns `Err` to the scheduler, which records the error and schedules the next tick normally — sweepers are not fail-fatal.

#### Sweeper restart semantics

Sweepers are stateless. On shard restart they re-attach to the scheduler and run their next tick at the configured period.

### Supersession sweeper

**Spec source:** [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention) — superseded statements + relations are retained "Forever" by default; operators opt in to sweeping.

**Cadence default:** daily (`BRAIN_SUPERSESSION_SWEEPER_PERIOD_SECONDS = 86400`).

**Retention default:** `0` seconds — disabled. The worker checks the config at the top of each tick; if zero, returns `Ok(())` immediately.

**Batch cap default:** 256 rows.

**Scan procedure:**

```
for entry in STATEMENTS_TABLE.iter():
    if entry.superseded_by != [0; 16]
       AND now_ns - entry.superseded_at_ns >= retention_ns:
        delete; audit append AuditOp::Tombstoned
                with reason=SupersededRetentionExpired
    if collected == batch_cap: break

(same for RELATIONS_TABLE)
```

**Audit row:** one per deleted record, in the same wtxn as the delete. The audit log sweeper is responsible for eventual audit retention of these rows.

**Failure mode:** any redb error aborts the current batch; next tick retries from the beginning.

### Audit log sweeper

**Spec source:** [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention) — extraction + resolution audit logs at 90 d default; merge logs forever.

**Cadence default:** daily (`BRAIN_AUDIT_SWEEPER_PERIOD_SECONDS = 86400`).

**Retention default:** 90 d (`BRAIN_AUDIT_RETENTION_SECONDS = 7_776_000`).

**Batch cap default:** 1024 rows (audit rows are small; larger batches are cheap).

**Merge-log exemption:** rows with `operation_tag ∈ { Merged, Unmerged }` are skipped unconditionally. Per [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention) merge logs are "Forever (small, valuable)".

**Scan procedure:**

```
cutoff_ns = now_ns - retention_seconds * 1_000_000_000
for entry in AUDIT_TABLE.iter():
    if entry.timestamp_ns > cutoff_ns: skip
    if matches!(entry.operation_tag, Merged | Unmerged):
        skipped += 1 with reason=merge_log_exempt; continue
    delete
    if collected == batch_cap: break
```

UUIDv7 keys make the audit table time-ordered. A range-scan optimisation (`[`0..(cutoff_uuid)`]`) is deferred to a future version; full scan at production volume is sub-second.

**No audit row** for sweeping the audit table — that would defeat the purpose.

### LLM cache sweeper

**Spec source:** [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention) — LLM cache 90 d default; [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) — table layout.

**Cadence default:** hourly (`BRAIN_LLM_CACHE_SWEEPER_PERIOD_SECONDS = 3600`).

**Two passes per tick:**

1. **TTL expiry pass.** Drop rows with `created_at_ns <= now_ns - ttl_seconds * 1e9`.
2. **Capacity enforcement pass.** Sum total bytes of the `LLM_CACHE_TABLE`. If `> max_bytes`, evict by ascending `last_used_at_ns` (or `created_at_ns` — see eviction note below) until under cap.

Both passes share a single `WriteTransaction` per tick.

**Defaults:**

- `BRAIN_LLM_CACHE_TTL_SECONDS = 7_776_000` (90 d).
- `BRAIN_LLM_CACHE_MAX_BYTES = 1_073_741_824` (1 GiB). `0` disables capacity enforcement.

**Batch cap default:** 1024 per pass.

**No-op when slot is None.** If `OpsContext.llm_cache` is `None` (schemaless deployment — no LLM extractors configured), the worker returns `Ok(())` immediately.

**Eviction note — `last_used_at`.** Brain does not currently refresh `last_used_at_ns` on cache reads. Eviction order is effectively by `created_at_ns`. Acceptable for the 90 d TTL the cache uses; touch-on-read is a future optimisation.

**No audit row** for cache eviction — the cache is derived state, not a system-of-record.

### Stale extraction detector

**Spec source:** [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) — flag statements whose `schema_version` or `extractor_version` is behind the current registry.

**Cadence default:** hourly (`BRAIN_STALE_DETECTOR_PERIOD_SECONDS = 3600`).

**Batch cap default:** 512 rows.

**Predicate:**

```
fn is_stale(stmt, current_versions) -> bool {
    let ns = predicate_namespace_of(stmt.predicate_id);
    stmt.schema_version < current_versions.schema_for(ns)
    || stmt.extractor_version
       < current_versions.extractor_for(stmt.extractor_id)
}
```

`current_versions` is loaded once per tick by reading `SCHEMA_ACTIVE_VERSIONS_TABLE` and the extractor registry, then cached for the scan.

**Effect:** sets `STATEMENT_FLAG_STALE_EXTRACTION = 1 << 3` on `StatementRow.flags`. **Does not re-extract.** The schema-migration worker (see "Schema migration worker" below) is the side that re-extracts when an operator triggers a migration plan.

**Transition counting:** `SweepSummary.flagged_now` counts only `false → true` transitions (rows already flagged are counted as `flagged_already` for observability).

**Scope:** active rows only. Tombstoned statements are skipped.

**No audit row** for staleness flagging — the flag itself is the audit signal; the schema-migration worker emits `AuditOp::SchemaUpgraded` when re-extraction happens.

### Entity GC sweeper

**Spec source:** [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) + [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention).

**Cadence default:** daily (`BRAIN_ENTITY_GC_PERIOD_SECONDS = 86400`).

**Off by default** (`BRAIN_ENTITY_GC_ENABLED = false`). The worker checks the flag at the top of each tick; if false, returns `Ok(())` immediately and logs `debug` once at startup.

**Grace period default:** 30 d (`BRAIN_ENTITY_GC_GRACE_SECONDS = 2_592_000`).

**Eligibility predicate:**

```
entity.tombstoned == false
AND now_ns - entity.created_at_ns >= grace_seconds * 1e9
AND inbound_reference_count(entity_id) == 0
```

where `inbound_reference_count` sums:
- Active statements where `subject = entity_id`.
- Active relations where `from = entity_id`.
- Active relations where `to = entity_id`.
- `entity_mentions` rows pointing at the entity.

Tombstoned-but-within-grace inbound rows count as references (prevents flapping; they may still be reverted).

**Race-safety:** the worker collects eligible ids under a `ReadTransaction`, then opens a `WriteTransaction` and **re-checks** each candidate's inbound count under the wtxn before tombstoning. A concurrent statement write between the rtxn and wtxn is caught (the wtxn is the redb single writer; the re-check sees the new state).

**Reversal on inbound reference.** When a statement / relation / entity_mention is created targeting an entity-tombstoned-within-grace, the entity ops layer clears the tombstone flag and writes an `AuditOp::Restored` row (implementation lives in entity_ops, documented here as the contract).

**Audit row:** one `AuditOp::Tombstoned` per swept entity, reason `EntityGcEligible`.

**Hard delete:** the GC worker only tombstones. Eventual hard-reclamation rides on Brain's existing tombstone-grace-then-reclaim flow.

### Confidence sweep worker

**Spec source:** [`../02_data_model/07_statement.md`](../02_data_model/07_statement.md) §"Confidence aggregation" — stored confidence is a snapshot; decay manifests via periodic refresh.

**Cadence default:** hourly (`BRAIN_CONFIDENCE_SWEEP_PERIOD_SECONDS = 3600`).

**Batch cap default:** 512 rows.

**Scan procedure:**

```
for entry in STATEMENTS_TABLE.iter():
    if entry.tombstoned: skip
    new = aggregate_confidence(entry.evidence, now_ns, entry.kind, &config)
    if (new - entry.confidence).abs() > 0.05:
        rewrite STATEMENTS_TABLE with confidence = new
        update STATEMENTS_BY_PREDICATE_TABLE if confidence_bucket changed
    if collected == batch_cap: break
```

The `0.05` threshold mirrors the index-churn gate — small drifts don't pay for an index rewrite.

**Effect on the ranker:** the query router uses `confidence` as one of the post-fusion ranking inputs (see [`../13_retrievers/01_rrf_fusion.md`](../13_retrievers/01_rrf_fusion.md)). A long-running deployment without the sweep keeps stale-but-once-confident Preferences ranking high; with it, decay propagates into ranks at the worker's cadence.

**No audit row** for confidence refresh — the rewrite is a derived value adjustment, not a system-of-record event.

### Sweeper configuration summary

| Env var | Default | Worker |
|---|---|---|
| `BRAIN_SUPERSESSION_RETENTION_SECONDS` | `0` (disabled) | Supersession sweeper |
| `BRAIN_SUPERSESSION_SWEEPER_PERIOD_SECONDS` | `86400` | Supersession sweeper |
| `BRAIN_AUDIT_RETENTION_SECONDS` | `7_776_000` (90 d) | Audit log sweeper |
| `BRAIN_AUDIT_SWEEPER_PERIOD_SECONDS` | `86400` | Audit log sweeper |
| `BRAIN_AUDIT_SWEEPER_BATCH_CAP` | `1024` | Audit log sweeper |
| `BRAIN_LLM_CACHE_TTL_SECONDS` | `7_776_000` (90 d) | LLM cache sweeper |
| `BRAIN_LLM_CACHE_MAX_BYTES` | `1073741824` (1 GiB) | LLM cache sweeper |
| `BRAIN_LLM_CACHE_SWEEPER_PERIOD_SECONDS` | `3600` | LLM cache sweeper |
| `BRAIN_STALE_DETECTOR_PERIOD_SECONDS` | `3600` | Stale extraction detector |
| `BRAIN_ENTITY_GC_ENABLED` | `false` | Entity GC sweeper |
| `BRAIN_ENTITY_GC_GRACE_SECONDS` | `2_592_000` (30 d) | Entity GC sweeper |
| `BRAIN_ENTITY_GC_PERIOD_SECONDS` | `86400` | Entity GC sweeper |
| `BRAIN_CONFIDENCE_SWEEP_PERIOD_SECONDS` | `3600` | Confidence sweep |
| `BRAIN_CONFIDENCE_SWEEP_BATCH_CAP` | `512` | Confidence sweep |

All values are operator-overridable. Tests use small values (seconds, not days) to keep wall-time bounded.

### Sweeper open questions

Tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md):

- Secondary indexes on `last_used_at_ns` (LLM cache) and `timestamp_ns` (audit table) for faster scans.
- Touch-on-read for `last_used_at_ns` to make LLM cache LRU exact.
- Per-statement-kind retention windows.
- Watermark optimisation for stale extraction (skip re-flagging already-flagged rows).

## State-carrying workers

Normative spec for the three **trigger-driven workers** that carry persistent checkpoint state across restarts. Implemented here:

- `BackfillWorker`.
- `ForgetCascadeWorker`.
- `SchemaMigrationWorker`.

These workers differ from the sweepers in three ways:

1. **Triggered, not periodic.** An external event (admin RPC, FORGET commit, SCHEMA_UPLOAD commit) places a unit of work in a queue. The worker drains the queue at the configured priority.
2. **Granular per-item state.** Each "unit of work" within a single trigger (e.g. one memory × extractor pair within a backfill plan) has its own checkpoint row so the work is resumable mid-run.
3. **Restartable.** On shard restart, a worker re-attaches to its checkpoint table and resumes from the first `Pending` row. Idempotency from [`./00_purpose.md`](./00_purpose.md) makes re-runs of completed items safe.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — worker overview.
- [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) §"Cascading effects of FORGET" + §"Re-extraction workflow".
- [`../03_schema/00_purpose.md`](../03_schema/00_purpose.md) §"Migration semantics".

### The `worker_checkpoints` table

Shared across all state-carrying workers.

```rust
pub const WORKER_CHECKPOINTS_TABLE:
    TableDefinition<'_, (&str, &[u8]), WorkerCheckpointRow>
    = TableDefinition::new("worker_checkpoints");

pub struct WorkerCheckpointRow {
    /// 0 = Pending, 1 = Started, 2 = Completed, 3 = Failed.
    pub status: u8,
    pub attempts: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub last_error: Option<String>,
}
```

#### Key composition

- First component is the worker id (`"backfill"`, `"forget_cascade"`, `"schema_migration"`) — a stable string constant per worker.
- Second component is the per-item byte key. Composition depends on the worker:
  - Backfill: `memory_id.to_be_bytes() ‖ extractor_id.to_le_bytes()`
  - FORGET cascade: `memory_id.to_be_bytes() ‖ statement_id.to_bytes()` (one row per (memory, dependent statement) pair).
  - Schema migration: same layout as backfill — `memory_id ‖ extractor_id` — because migration is fundamentally a re-extraction.

The composite key lets a single redb table host all workers' checkpoints without name collisions.

#### Status transitions

```
Pending  ──started──> Started
Started  ──ok─────>  Completed
Started  ──err────>  Failed (attempts++; retry if < max)
Failed   ──retry──>  Started
Completed            (terminal)
```

Workers consult `get(worker_id, item_key)` before each unit of work:

- `None` → write `Pending` then `Started`; do the work.
- `Pending` / `Started` (stale, e.g. left after crash) → treat as fresh work; transition to `Started`.
- `Completed` → skip; count as `skipped_already_completed`.
- `Failed` with `attempts < MAX_ATTEMPTS` → retry; transition to `Started`.
- `Failed` with `attempts >= MAX_ATTEMPTS` → skip; count as `skipped_failed`.

`MAX_ATTEMPTS = 3` default; configurable.

#### State-carrying retry policy

Exponential backoff between attempts is applied at the worker level (the checkpoint table stores only the attempt count). Workers compute the backoff as `min(60 s, 2^attempts * 100 ms)` before re-enqueueing a `Failed` item.

#### State-carrying cancellation

Each running plan carries a cancel flag (process-local; flipped by an admin RPC or by the worker's own abort logic). Workers check the flag at each item boundary; the current item completes (so its checkpoint reaches a terminal state); subsequent items are not processed.

A cancelled plan leaves a mix of `Completed` + `Pending` rows; a subsequent admin re-enqueue of the same request id resumes from the first non-`Completed` row.

#### Checkpoint cleanup

Completed checkpoints accumulate. Brain keeps them indefinitely today (audit value). A future sweeper similar to the audit log sweeper hard-deletes `Completed` rows past a configurable retention; tracked as an open question.

### Shared state-carrying discipline

#### Priority

Background lane per [`./00_purpose.md`](./00_purpose.md) (≤ 20% of shard time). Cooperative yielding via the `Worker` trait's `run` future.

#### Per-item flow

Workers walk their input items via a generic helper (`brain_workers::workers::common::run_checkpointed`):

```
for item in plan.items:
    if cancelled: break
    let row = checkpoint::get(worker_id, item.key())?;
    match row:
        Some(Completed) => skipped_complete += 1; continue
        Some(Failed { attempts >= MAX }) => skipped_failed += 1; continue
        _ => checkpoint::mark_started(worker_id, item.key(), now)?
    let result = process_item(item, ctx).await;
    match result:
        Ok(()) => checkpoint::mark_completed(worker_id, item.key(), now)?
        Err(e) => checkpoint::mark_failed(worker_id, item.key(), e, now)?
    yield_now().await;
```

The yield between items lets the scheduler interleave higher-priority work.

#### State-carrying metrics

Per-worker, prefixed `worker_*`:

- `worker_progress{worker, status}` — counter per status transition.
- `worker_items_total{worker, status}` — gauge of current plan's per-status counts.
- `worker_latency_seconds{worker}` — histogram of per-item wall-time.
- `worker_resume_total{worker}` — counter incremented on shard startup when a worker resumes a partially-completed plan.
- `worker_failure_rate{worker, request_id}` — gauge to drive bad-extractor abort logic.

### Backfill worker

#### Backfill trigger

An admin request — wire opcode or CLI subcommand; the spec documents the worker's input contract, not the surface:

```rust
pub struct BackfillRequest {
    pub request_id: BackfillId,                    // UUIDv7
    pub memory_range: BackfillRange,               // ById | All
    pub extractor_ids: SmallVec<[ExtractorId; 4]>,
    pub priority: WorkerPriority,                  // overrides default
    pub dry_run: bool,
}

pub enum BackfillRange {
    All,
    ById { start: MemoryId, end: MemoryId },
}
```

#### Backfill per-item granularity

`(memory_id, extractor_id)` — the smallest replayable unit. Two memories × three extractors → six checkpoint rows.

#### Bad-extractor abort

After the first 100 items, if `failed / processed > 0.5`, the worker aborts the plan with a single `warn` log + a `BackfillAborted { reason: HighFailureRate }` event on the change feed. Prevents a misconfigured extractor from spending hours hammering a million memories.

#### Backfill concurrency

Brain runs **one backfill at a time per shard**. Additional requests queue on `BackfillWorker.pending_requests`. Multiple shards run independently.

#### Backfill dry-run

Dry-run marks each item `Completed` without invoking the extractor pipeline. Used for plan validation + cost preview before live runs.

#### Backfill cancellation

`AdminCancelBackfill(request_id)` flips the running plan's cancel flag. The current item's extractor call completes; the worker writes that item's checkpoint to a terminal state and stops dequeueing further items.

### FORGET cascade worker

#### FORGET cascade trigger

`handle_forget` enqueues one `ForgetCascadeJob` per FORGET **post-commit**:

```rust
pub struct ForgetCascadeJob {
    pub memory_id: MemoryId,
    pub mode: ForgetMode,           // Soft | Hard
    pub kind: CascadeKind,          // Apply | Revert
    pub forgot_at_unix_nanos: u64,
}
```

#### FORGET cascade per-job procedure

```
1. Open a read txn; gather statement_ids + relation_ids
   whose evidence contains `memory_id`.
2. For each dependent record:
     checkpoint::mark_started("forget_cascade",
                              memory_id ‖ record_id, now)
3. Open a write txn (batched, ≤ 256 records per txn).
4. For each record in the batch:
     a. Drop `memory_id` from `evidence`.
     b. Recompute `confidence` per ../10_metadata/00_purpose.md.
     c. If evidence.is_empty():
          - confidence >= threshold:
              mark `stale_evidence` flag; keep row.
          - else:
              tombstone with reason=SourceMemoryForgotten;
              audit row.
     d. mark_completed in the same wtxn.
5. Commit. If more than 256 dependents remain, enqueue a
   continuation job for the leftover.
```

#### Soft vs hard cascade

- **Soft FORGET** (Brain's default with a grace window): the cascade marks dependent rows with the same grace expiry. If the FORGET is reverted within grace, the cascade receives a `CascadeKind::Revert` job and rolls back the pending-tombstone flag on each affected row.
- **Hard FORGET**: the cascade hard-tombstones immediately.

#### Confidence threshold

`BRAIN_CASCADE_CONFIDENCE_THRESHOLD = 0.2` default. Below this, an empty-evidence statement is tombstoned; above this, it survives with the `stale_evidence` flag set (operator can re-extract or accept the staleness).

#### Continuation jobs

A FORGET against a heavily-referenced memory (e.g. 10K statements) is split across multiple jobs of up to 256 dependents each. Continuation jobs carry the same `memory_id` + a `start_after_record_id` cursor so the worker resumes correctly under cancellation.

#### FORGET cascade audit rows

One `AuditOp::Tombstoned` per dependent that gets tombstoned. One `AuditOp::Superseded` per dependent that gets `stale_evidence` (because the row still exists but its content is now derived from a smaller evidence set).

### Schema migration worker

#### Schema migration trigger

Post-commit hook on `handle_schema_upload`. When the new schema version invalidates existing extraction state (per [`../03_schema/05_versioning.md`](../03_schema/05_versioning.md)), the handler:

1. Computes a `MigrationPlan { items: Vec<MigrationItem> }` where each item is a `(memory_id, extractor_id)` pair that needs re-extraction.
2. Returns the plan summary in the `SchemaUploadResponse`.
3. **If not dry-run**, enqueues the plan on the migration worker.

#### Schema migration per-item procedure

```
for item in plan.items:
    checkpoint::mark_started("schema_migration",
                             item.memory_id ‖ item.extractor_id, now)
    let outcome = reextract_memory(
        wtxn, item.memory_id, item.extractor_id, ctx, now,
    )?;
    audit_row_for(outcome);
    checkpoint::mark_completed(...)?
    yield_now().await;
```

`reextract_memory` returns a `ReextractOutcome` per [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md):

- `Refreshed { statement_id, new_confidence }` — same kind + subject + predicate + object as an existing statement; bump version + confidence.
- `Superseded { old, new }` — preference / fact with same identity but new value.
- `Created { new }` — no matching existing statement (new extractor or new content).
- `PotentiallyRetracted { statement_id }` — old statement no longer produced by the new extractor; flagged for operator review.
- `NoOp` — checkpoint state says already-done; skip.

#### Schema migration cost budget

Schema migrations that touch LLM extractors respect the per-extractor cost budget. Items over budget are skipped with a `skipped_over_budget` metric; operators can re-run after raising the budget.

#### Schema migration cancellation

`AdminCancelSchemaMigration(request_id)`. Same shape as backfill cancellation.

#### Schema migration audit rows

One `AuditOp::SchemaUpgraded` per plan, written once at plan start. Per-item audit rows per the `ReextractOutcome`'s normal extraction-audit semantics (`AuditOp::Extracted` / `AuditOp::Superseded`).

### State-carrying open questions

Tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md):

- Sweep of `Completed` checkpoint rows past retention.
- Multi-item-per-txn batching for backfill / schema-migration (current: one item per txn for clean isolation; batching is a perf optimisation).
- Concurrent backfill / migration plans within a shard.
- Per-extractor parallelism within a single migration plan.

## Entity garbage collection

Tombstone semantics and the optional periodic GC sweep that reclaims storage for tombstoned / fully-merged entities. Wire opcode: `ENTITY_TOMBSTONE` (`0x0138`); see [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §"ENTITY_TOMBSTONE".

### Two levels of "gone"

| Level | State | Reversible? | Recovers space? |
|---|---|---|---|
| **Tombstoned** | `flags & TOMBSTONED != 0`. Row preserved; secondary indexes torn down. | Yes, via `ENTITY_UPDATE` clearing the flag (manual op). | No. |
| **Reclaimed** | Row removed from `entities` table. Audit retained. | No. | Yes. |

Tombstoning is a wire-exposed soft delete. Reclamation is an operator-driven GC pass (off by default).

### Tombstone mechanics

`entity_tombstone(id, reason, now)`:

```text
1. Load entity row.
2. If already tombstoned: return success (idempotent).
3. Tear down secondary indexes (same as merge §"Mechanics" step 12):
       - Remove from entity_by_canonical_name.
       - Remove from entity_aliases (one row per alias).
       - Remove from entity_trigrams (one row per trigram in the canonical+aliases set).
       - HNSW: tombstone-mark the embedding entry (HNSW rebuild reclaims).
4. Update row:
       - flags |= TOMBSTONED
       - tombstoned_at = now
       - tombstone_reason = reason
       - aliases = vec![]   // clear so future ENTITY_UPDATE doesn't double-index
5. Write audit row to entity_resolution_audit (kind=Tombstoned).
6. Commit redb txn.
7. Post-commit: emit ENTITY_TOMBSTONED event.
```

#### What stays queryable

A tombstoned entity:

- **Is still readable via `ENTITY_GET`**. Returns the row with `flags & TOMBSTONED != 0`. Clients filter or display accordingly.
- **Is not returned by `ENTITY_LIST`** unless `include_tombstoned = true`.
- **Is not a resolver target**. Tier 1 / 2 / 3 ignore tombstoned entities (their secondary indexes are gone).
- **Is not re-routable**. `ENTITY_MERGE` rejects tombstoned entities (`ENTITY_MERGE_CONFLICT`).

#### Statements / relations referencing tombstoned entities

Statements / relations are **not** automatically tombstoned or rewritten. They retain their references to the tombstoned id. Queries that follow the references see a tombstoned row.

Operators may use `ADMIN_LIST_STALE_STATEMENTS` ([`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md) §"ADMIN_LIST_STALE_STATEMENTS") to find statements whose subject / object is now tombstoned and decide whether to tombstone the statements too.

### GC eligibility

An entity becomes a GC candidate when **all** of:

| Condition | Where checked |
|---|---|
| `flags & TOMBSTONED != 0` | entity row |
| `now - tombstoned_at >= GC_TOMBSTONE_GRACE` (default 90 days) | entity row + clock |
| No active (non-tombstoned, non-superseded) statement has `subject = id` or `object = Entity(id)` | `statements_by_subject` + `statements_by_object` scans |
| No active relation has `from_entity = id` or `to_entity = id` | `relations_by_from` + `relations_by_to` scans |
| `mention_count == 0` after the scans (re-counted; the stored counter may be stale) | recomputed during GC |
| If `merged_into.is_some()`: corresponding merge audit's grace period expired | `entity_merge_log` |

The cumulative test is conservative — false negatives are fine (an entity stays around longer than necessary); false positives would orphan references and are forbidden.

### GC sweep

The sweep is a background worker, **off by default**. Operators enable it via deployment config (`brain.gc.entities.enabled = true`).

#### Frequency

Default: daily. Configurable per deployment. The sweep doesn't need to run frequently — entities are cheap and the cost of orphaning identity is high.

#### Per-sweep work

```text
For each shard:
    Scan entities table for tombstoned rows.
    For each candidate:
        Verify eligibility conditions inside a read txn.
        If eligible:
            Add to reclamation batch.
    Sort batch by entity_id (deterministic order).
    For each batch chunk (up to N entities, default N=100):
        Open write txn.
        For each entity in chunk:
            Re-verify eligibility (TOCTOU).
            DELETE row from entities.
            DELETE any straggler index rows (defensive; tombstone should have cleaned).
            DELETE entity_mentions entries.
        Commit chunk.
```

#### What doesn't get reclaimed

- The `entity_resolution_audit` rows touching this entity. Audits are kept indefinitely.
- The `entity_merge_log` rows where this entity was survivor or merged. Same — kept indefinitely.
- Statements / relations that still reference the id, **if** an operator chose not to tombstone them. The GC sweep refuses to reclaim such entities; operators must address the references first.

#### Conservative defaults

- `GC_TOMBSTONE_GRACE` is 90 days, not 7. Entities are cheap to keep; recovering identity after hard delete is expensive.
- The sweep is **off by default**. Most deployments don't enable it — entity churn is low and the savings small.
- High-churn deployments (test data, ephemeral mentions, scratch workloads) flip it on.

### Hard delete (RETRACT_ENTITY)

There is **no** `RETRACT_ENTITY` wire opcode. The only path to physical removal is:

1. `ENTITY_TOMBSTONE` (sets flag).
2. Wait `GC_TOMBSTONE_GRACE`.
3. GC sweep (if enabled) reclaims.

Operators that need immediate hard delete (privacy law compliance, etc.) drop to offline tooling against the redb file directly. The wire protocol intentionally has no privacy-immediate-erasure path — privacy-driven deletes use `STATEMENT_RETRACT` for statements about the entity and leave the entity row in place.

### Reclamation audit

When the GC sweep reclaims an entity, it writes a final audit:

```rust
pub struct ReclamationAudit {
    pub audit_id: AuditId,
    pub entity_id: EntityId,
    pub entity_type_id: EntityTypeId,
    pub last_canonical_name: String,
    pub last_known_state_blob: Vec<u8>,  // rkyv-encoded final Entity row
    pub tombstoned_at: u64,
    pub reclaimed_at: u64,
    pub reclaim_actor: Actor,            // System (GC worker)
}
```

Kept in `entity_resolution_audit` with discriminator `kind = Reclamation`. Lets operators trace what was removed and when.

### Entity GC tests

GC sweep test coverage (alongside the optional worker):

- Eligibility:
  - Tombstoned + grace expired + no references → reclaimed.
  - Tombstoned but `mention_count > 0` after recount → not reclaimed.
  - Tombstoned but active statement still references → not reclaimed.
  - Tombstoned but merge audit grace not expired → not reclaimed.
- TOCTOU: another op modifies the entity mid-sweep → sweep skips and retries next pass.
- Batch chunking: 1000 candidates, sweep processes in 10 chunks of 100.
- Disabled by default: fresh deployment never runs the sweep without explicit enable.

Exercised in `crates/brain-workers/tests/entity_gc.rs`.
