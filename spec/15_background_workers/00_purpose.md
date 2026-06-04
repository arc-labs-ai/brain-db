# 15. Background Workers

> **TL;DR.** Per-shard async tasks that maintain state outside the request path. Memory workers handle decay, consolidation, HNSW maintenance, idempotency sweep, slot reclamation, WAL retention. Typed-graph workers (active once a schema is declared) handle pattern/classifier/LLM extraction, entity resolution, entity and statement embedding, tantivy indexing, FORGET cascade, supersession sweep, schema migration, entity GC. Bounded queues with explicit overflow policy, four priority lanes, cooperative yielding, idempotent on restart. Workers are lower-priority than request handlers.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Engine implementers; operators |
| Voice | Third-person factual + RFC2119 normative |
| Depends on | [08. Storage](../08_storage/00_purpose.md), [09. Indexing](../09_indexing/00_purpose.md), [10. Metadata + Graph Store](../10_metadata/00_purpose.md), [14. Concurrency](../14_concurrency/00_purpose.md) |
| Referenced by | [17. Observability](../17_observability/00_purpose.md) |

## What this spec defines

Brain's background work — the periodic tasks that maintain state, decay, consolidate, and clean up. These run alongside the request-handling pipeline but at lower priority.

### Workers in this spec

- **Decay** — applies time-based decay to memory salience.
- **Access boost** — applies salience boost from recent accesses.
- **Consolidation** — promotes Episodic memories to Consolidated when criteria met.
- **Index maintenance** — rebuilds the index when degraded.
- **Idempotency cleanup** — prunes expired idempotency records.
- **Slot reclamation** — reclaims tombstoned slots after grace period.
- **WAL retention** — deletes old WAL segments after checkpoint.
- **Edge scrub** — removes orphan edges.
- **Counter reconciliation** — verifies denormalized counters.
- **Statistics update** — refreshes per-shard stats.

Typed-graph workers (active once a schema is declared): pattern / classifier / LLM extractors, entity resolver, entity + statement embedding, tantivy indexers, FORGET cascade, supersession sweeper, schema migration, entity GC, audit sweeper.

This document specifies Brain's background workers — the asynchronous tasks that maintain state without being directly triggered by client requests.

## What this document covers

- The architecture of the worker infrastructure.
- Each worker's role, scheduling, and behavior.
- Failure handling in workers.
- The interaction between workers and the request-handling pipeline.

## What this document does not cover

- **The data the workers operate on.** Defined in [02. Data Model](../02_data_model/00_purpose.md) and [08. Storage](../08_storage/00_purpose.md) / [10. Metadata + Graph Store](../10_metadata/00_purpose.md).
- **The concurrency rules workers follow.** Defined in [14. Concurrency](../14_concurrency/00_purpose.md).
- **The metrics workers expose.** Defined in [17. Observability](../17_observability/00_purpose.md).

## 1. Why background workers

Some maintenance can't (or shouldn't) happen in the request path:

- **Decay** of salience: doesn't make sense to do on every read. Periodic batch update is right.
- **Consolidation** of memories: an aggregate operation that takes minutes; can't block client requests.
- **HNSW rebuild**: takes 5-30 seconds at scale; must run async.
- **Idempotency pruning**: a TTL-driven cleanup; doesn't need to happen on every write.

Background workers handle these.

## 2. The worker model

Each worker is a long-running async task on the shard's Glommio executor. It:

- Wakes up periodically (or on a trigger).
- Does its work.
- Sleeps until the next cycle.

```rust
async fn worker_loop(state: Arc<ShardState>) {
    let interval = Duration::from_secs(60);
    loop {
        if let Err(e) = do_one_cycle(&state).await {
            log::warn!("Worker error: {:?}", e);
        }
        sleep(interval).await;
    }
}
```

## 3. Per-shard workers

Most workers are per-shard. Each shard has its own instance of each worker. The shard-local worker operates only on that shard's data.

Some workers are global (one instance for the whole server):

- Cluster topology refresh (in distributed deployments).
- Cross-shard load balancing decisions.

The vast majority are per-shard.

## 4. Worker priority

Workers run at lower priority than request handlers. Glommio's task priority system enforces this:

- High: request handlers, the writer task.
- Medium: cross-shard call handlers.
- Low: background workers.

When the shard is busy with requests, workers wait. When there's spare capacity, they run.

## 5. Worker resource limits

Workers can consume:
- CPU.
- Disk I/O (writes during consolidation, reads during reconciliation).
- Memory (during HNSW rebuild, both old and new index in memory).

Brain caps:
- Per-worker concurrent operations.
- Total background-work CPU (default 50% of the shard's core).
- Memory consumption (depends on worker; HNSW rebuild has explicit limits).

If a worker would exceed limits, it pauses or splits its work.

## 6. The "idle" state

When a worker has no work, it sleeps. Wake-ups are timer-based (most common) or event-based (e.g., post-write triggers a deferred consolidation check).

Sleep periods are configurable; defaults are conservative.

## 7. Worker observability

Each worker exposes:
- `last_run_at`, `last_run_duration_ms` — for monitoring.
- `pending_work` — what the worker has queued.
- `errors_total` — counter of errors.
- `progress_indicator` — for long-running cycles.

These appear in Brain's metrics endpoint.

## 8. The work-coalescing pattern

Where possible, workers coalesce work:

- The decay worker processes thousands of memories in a single transaction.
- The idempotency sweep processes thousands of expired entries in one batch.

Per-record overhead is amortized; the worker is more efficient.

## 9. The sequencing

Workers are independent. Two workers may run concurrently (on different sub-tasks). They don't coordinate among themselves; the underlying storage's transactions handle isolation.

For workers that conflict (e.g., decay and consolidation both update the same memories), the storage layer's transactions serialize them. Conflicts are rare in practice.

## 10. Recovery after restart

After a Brain restart:

- Workers start fresh.
- They immediately begin their first cycle (or after a brief delay for stagger).
- Any in-progress state from before the crash is lost; the worker re-discovers what to do.

This is OK because workers are idempotent: running the decay worker a second time on the same memory just produces the same result.

## 11. The "no work to do" case

When Brain is empty (no memories), workers don't do much:
- Decay: scans the empty memories table, finds nothing.
- Consolidation: same.
- index maintenance: HNSW is empty.

These all complete quickly with low overhead.

## 12. The "very busy" case

When Brain is under heavy load:
- Workers wait for capacity.
- Their cycles may extend.
- Some workers may shed work (e.g., decay might process fewer memories per cycle).

Brain prioritizes serving client requests over keeping workers fully on schedule.

## 13. Worker correctness expectations

Workers should:
- Be **safe to interrupt** at any point (no half-done state corruption).
- Be **idempotent** (running a cycle twice produces the same end state).
- Be **incremental** (large work can be broken into smaller pieces).
- **Yield** generously (per [10.07 Yields](../14_concurrency/04_yields.md)).

## 14. Per-worker chapters

Workers are documented across these files:

- [Memory maintenance: decay, consolidation, HNSW maintenance](02_memory_maintenance.md)
- [Substrate sweepers: idempotency cleanup, slot reclamation, WAL retention](03_substrate_sweepers.md)
- [Misc workers: edge scrub, counter reconciliation, statistics, embedder cache, snapshot](04_misc_workers.md)
- [Typed-graph workers: extractors, text indexers, sweepers, state-carrying, entity GC](06_typed_graph_workers.md)

## 15. Typed-graph workers

When a schema is declared, the same shard runs additional workers that maintain
the typed graph. They share Brain's worker scheduling discipline
above: cooperative yielding, bounded queues with explicit overflow handling,
priority lanes (foreground / near-foreground / background / low), per-shard
executor with no cross-shard scheduling, and a shared I/O budget no single
worker can monopolize.

### 15.1. Worker inventory

| Worker | Trigger | Priority | Backpressure |
|---|---|---|---|
| **Pattern extractor** | On ENCODE | Foreground (sync) | None |
| **Classifier extractor** | On ENCODE | Near-foreground | Bounded queue (1000) |
| **LLM extractor** | On ENCODE | Background | Bounded queue (10000), drops on overflow with metric |
| **Entity resolver** | On extractor output mentioning entities | Near-foreground | Bounded queue |
| **Entity embedding** | On entity create/rename | Background | Bounded queue |
| **Statement embedding** | On statement create/update | Background | Bounded queue |
| **Statement indexer (tantivy)** | On statement create/update/tombstone | Near-foreground | Bounded queue |
| **Memory text indexer (tantivy)** | On ENCODE | Near-foreground | Bounded queue |
| **FORGET cascade** | On Memory FORGET | Background | None (rare events) |
| **Supersession sweeper** | Periodic | Low | None |
| **Cache sweeper (LLM)** | Periodic | Low | None |
| **Schema migration** | On schema update | Background | None |
| **Entity GC** | Periodic (off by default) | Low | None |
| **Ambiguity resolver** | Periodic | Low | None |
| **Audit log sweeper** | Periodic (daily) | Low | None |
| **Entity merge** | On manual/admin trigger | Foreground (sync) | None |
| **Stale extraction detection** | Periodic | Low | None |

Detailed mechanics:

- The **three extractor workers** (pattern / classifier / LLM), the **entity / statement / embedding workers**, the **two text indexer workers** (memory + statement → tantivy), and the **state-carrying workers** (backfill, FORGET cascade, schema migration) are documented in [`./06_typed_graph_workers.md`](./06_typed_graph_workers.md).
- The **periodic sweepers** (supersession sweeper, audit log sweeper, cache sweeper (LLM), entity GC, stale extraction detection) are documented in [`./03_substrate_sweepers.md`](./03_substrate_sweepers.md).

### 15.2. Pattern extractor

```rust
fn run_pattern_extractor(memory: &Memory, ext: &PatternExtractor) -> Vec<ExtractedItem> {
    let mut out = Vec::new();
    for pattern in &ext.patterns {
        for capture in pattern.find_iter(&memory.text) {
            let candidate = capture.text();
            let resolved = resolve_entity(candidate, &memory.text_context(capture), ext.target_type, &ext.resolver_config);
            out.push(ExtractedItem { ... });
        }
    }
    out
}
```

Runs synchronously during ENCODE. Output is immediately visible. Cost: tens of µs per memory.

### 15.3. Statement embedding worker

Statements created or updated by an extractor are enqueued onto the `statement_embed` worker's bounded queue. The worker dequeues in batches, embeds the statement's surface form via the same embedder pool the encode path uses, and inserts the resulting vector into the per-shard **statement HNSW** (parameters in [`../09_indexing/01_hnsw_basics.md`](../09_indexing/01_hnsw_basics.md)).

```rust
async fn run_statement_embed(ctx: &OpsContext) -> Result<(), ()> {
    loop {
        let batch = ctx.queues.dequeue_batch(StatementEmbed, BATCH_CAP).await?;
        let texts: Vec<&str> = batch.iter().map(|i| i.surface.as_str()).collect();
        let vectors = ctx.embedder.embed_batch(&texts).await?;
        for (item, vector) in batch.iter().zip(vectors) {
            ctx.statement_hnsw.insert(item.statement_id, vector)?;
            ctx.metadata.statement_set_embedded_at(item.statement_id, now_ns())?;
        }
    }
}
```

The worker tracks `embedded_at_unix_nanos` on each statement row so a re-scan after restart skips already-embedded statements (idempotency on restart, per the Idempotency reminders section below).

Without this worker the statement HNSW stays empty and the SemanticRetriever's statement-corpus mode returns no candidates — retrieval degrades to BM25 + graph only. Populating it is what makes the typed graph pull its weight in RECALL.

### 15.4. Classifier extractor

```rust
fn run_classifier_extractor(memory: &Memory, ext: &ClassifierExtractor) -> Vec<ExtractedItem> {
    let features = ext.feature_extractor.extract(memory);
    let prediction = ext.model.predict(&features);
    if prediction.confidence >= ext.threshold {
        let item = decode_prediction(prediction, ext.target);
        vec![item]
    } else {
        vec![]
    }
}
```

Runs in near-foreground task. Latency: 1-10 ms. May be batched if the model supports batching (deferred to future versions).

### 15.5. LLM extractor

```rust
async fn run_llm_extractor(memory: &Memory, ext: &LLMExtractor) -> Vec<ExtractedItem> {
    let input_hash = blake3(&memory.text);
    let cache_key = (input_hash, ext.id, ext.version, ext.model);
    
    // Cache check
    if let Some(cached) = llm_cache.get(&cache_key)? {
        return parse_extraction(&cached, ext);
    }
    
    // Cost budget check
    let projected = ext.cost_estimator.estimate(&memory.text);
    if !budget.allows(projected) {
        metrics.skipped_over_budget.inc();
        return vec![];
    }
    
    // LLM call
    let response = ext.model.call(&compose_prompt(ext, memory)).await?;
    
    // Schema validation
    let parsed = match validate(&response, &ext.schema) {
        Ok(p) => p,
        Err(e) => {
            // Single retry with error in prompt
            let response2 = ext.model.call(&retry_prompt(ext, memory, &e)).await?;
            validate(&response2, &ext.schema).ok()?
        }
    };
    
    // Cache and decode
    llm_cache.put(&cache_key, &response, ext.cache_ttl)?;
    decode_extraction(parsed, ext)
}
```

Runs in background queue. Latency: 100ms to 10s per memory. Throughput is constrained by external LLM API or local model.

Cost budgeting: each call's estimated cost (from token count heuristics) is checked against the per-extractor budget and the global daily budget.

Schema validation is strict: malformed outputs are retried once, then dropped.

### 15.6. Worker queue semantics

Each worker has a bounded queue:

```rust
struct WorkerQueue<T> {
    capacity: usize,
    items: VecDeque<T>,
    overflow_policy: OverflowPolicy,
}

enum OverflowPolicy {
    Drop { metric: &'static str },     // record and discard new items
    DropOldest { metric: &'static str }, // discard oldest, accept new
    Block { timeout: Duration },        // backpressure into caller
}
```

For LLM extractors specifically, the default is `Drop` with a prominent metric: if the queue is overflowing, the operator sees it and tunes (lower extraction density, raise LLM budget, scale down extraction rate).

### 15.7. Idempotency reminders

Every worker that produces persistent state must be idempotent on its inputs:

- Pattern extractor: deterministic by regex over text.
- Classifier: deterministic by pinned model + features.
- LLM extractor: deterministic via cache; cache miss leads to potential drift.
- Statement embedder: deterministic by text → embedding model.
- Tantivy indexer: deterministic by record content.

Idempotency is enforced by checking existence/staleness before writing. A re-run that produces identical output is a no-op write.

### 15.8. Scheduling priorities and budgets

| Priority | Use | Budget |
|---|---|---|
| Foreground | Pattern extraction, entity resolver tier-1/2, sync index writes | 50% of shard time |
| Near-foreground | Classifier, tantivy indexing | 25% of shard time |
| Background | LLM, embedding, FORGET cascade | 20% of shard time |
| Low | Sweepers, GC, audit log sweeper | 5% of shard time |

Within a priority, FIFO within each worker's queue. Across priorities, higher priority preempts lower at yield points.

Configurable per deployment; defaults work for most workloads.

### 15.9. Observability

Per-worker metrics:
- `worker_queue_depth{worker}` — current items.
- `worker_queue_overflow_total{worker}` — overflows.
- `worker_latency_seconds{worker}` — histogram.
- `worker_throughput{worker}` — items/sec.
- `worker_errors_total{worker}` — error count.

Per-extractor metrics (in addition):
- `extractor_extractions_total{extractor, status}` — by status (success, failure, skipped_budget).
- `extractor_cost_usd_total{extractor}` — LLM cost tracking.
- `extractor_cache_hit_rate{extractor}` — LLM cache hits.

### 15.10. Graceful shutdown

On shutdown:
1. Stop accepting new work into queues.
2. Drain pending work (with timeout, e.g., 30 s).
3. Persist queue state to disk if not drained.
4. On restart, restore queue state.

Persisted queues handle: pending LLM extractions that are mid-call, pending tantivy commits, pending HNSW updates. WAL handles persistence of completed work; queue persistence handles in-flight work.

---

*Continue to [`01_worker_architecture.md`](01_worker_architecture.md) for the worker infrastructure.*
