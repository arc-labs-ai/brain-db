# Workers — Knowledge Layer

## New worker types added here

The substrate workers (section 11) handle: HNSW maintenance, decay sweeping, salience updates, FORGET grace, consolidation. The knowledge layer adds:

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

## Worker scheduling

The knowledge layer uses the substrate's worker scheduling discipline (section 11):
- Cooperative yielding (no preemption).
- Bounded queues with explicit overflow handling.
- Priority lanes (foreground, near-foreground, background, low).
- Per-shard executor; no cross-shard scheduling.
- All workers participate in the shard's I/O budget; no worker can monopolize.

## Pattern extractor

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

## Classifier extractor

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

Runs in near-foreground task. Latency: 1-10 ms. May be batched if the model supports batching (defer to future versions).

## LLM extractor

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

## Worker queue semantics

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

## Idempotency reminders for workers

Every worker that produces persistent state must be idempotent on its inputs:

- Pattern extractor: deterministic by regex over text.
- Classifier: deterministic by pinned model + features.
- LLM extractor: deterministic via cache; cache miss leads to potential drift.
- Statement embedder: deterministic by text → embedding model.
- Tantivy indexer: deterministic by record content.

Idempotency is enforced by checking existence/staleness before writing. A re-run that produces identical output is a no-op write.

## Scheduling priorities and budgets

| Priority | Use | Budget |
|---|---|---|
| Foreground | Pattern extraction, entity resolver tier-1/2, sync index writes | 50% of shard time |
| Near-foreground | Classifier, tantivy indexing | 25% of shard time |
| Background | LLM, embedding, FORGET cascade | 20% of shard time |
| Low | Sweepers, GC, audit log sweeper | 5% of shard time |

Within a priority, FIFO within each worker's queue. Across priorities, higher priority preempts lower at yield points.

Configurable per deployment; defaults work for most workloads.

## Observability

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

## Graceful shutdown

On shutdown:
1. Stop accepting new work into queues.
2. Drain pending work (with timeout, e.g., 30 s).
3. Persist queue state to disk if not drained.
4. On restart, restore queue state.

Persisted queues handle: pending LLM extractions that are mid-call, pending tantivy commits, pending HNSW updates. WAL handles persistence of completed work; queue persistence handles in-flight work.
