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
              │ enqueue memory once (non-blocking)   │
              │ onto the per-shard extractor queue   │
              └──────────────────────────────────────┘
                                   │
                                   │ drained by the ExtractorWorker
                                   ▼ on its scheduler cadence
              ┌──────────────────────────────────────┐
              │ run tiers in order, per memory:      │
              │   pattern → classifier → LLM         │
              └──────────────────────────────────────┘

```

Dispatch model — **all three tiers run asynchronously**. ENCODE never runs an extractor inline:

1. On commit, the writer's post-commit step performs a **single non-blocking enqueue** of the memory onto the per-shard extractor queue, then ENCODE acknowledges. No extracted entity / statement / relation exists when ENCODE returns.
2. The per-shard **ExtractorWorker** drains the queue on its scheduler cadence and runs the tiers **in order — pattern → classifier → LLM —** over each drained memory. Pattern output is cheap (~µs) but it too is produced by the worker, so it becomes visible only after the worker processes the memory (typically within one worker cycle), not at ENCODE-ack.
3. The LLM tier runs last in the same pass; its outputs land once each (possibly cached) call completes.

Why fully async: it keeps ENCODE's p99 independent of extraction cost. Pattern is microseconds, but the classifier forward pass (GLiNER, tens of ms to seconds on CPU) and the LLM round-trips vary by orders of magnitude — coupling any tier to the write path would make ENCODE latency hostage to model and inference cost. One queue gives one back-pressure / drop surface and one idempotency guard (the per-memory audit row); running the tiers together per memory lets later tiers supplement or replace earlier candidates and resolves `depends_on` in a single pass, and batching amortizes the GLiNER backbone GEMM across memories.

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

Queue defaults (operator-configurable):

| Queue | Capacity | Overflow |
|---|---|---|
| Per-shard extractor queue (feeds all three tiers) | 1 024 | `Drop` + metric |

There is **one** physical queue per shard: the memory is enqueued once and the worker runs every eligible tier on it. Overflow policy `Drop` records a metric counter and emits a warn-level trace event so the operator sees pressure. The dropped extraction writes an audit row `Skipped(reason: "queue full")`.

### Scheduling priorities

The ExtractorWorker runs under the per-shard scheduler's background allowance — it is never on the foreground (ENCODE) path. All three tiers share that one budget; there is no separate per-tier lane.

| Worker | Priority | Budget |
|---|---|---|
| ExtractorWorker (pattern + classifier + LLM) | Background | shares the shard's worker allowance |

The per-shard executor's cooperative-yield model applies — a long classifier or LLM inference yields between memories so foreground ops (ENCODE / RECALL) keep their latency.

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

The ExtractorWorker runs this filter when it drains each memory, once per eligible extractor — if the trigger doesn't match, no audit row is written (no row, no skip). The condition "trigger eval error" still writes `Skipped(reason: "trigger eval error")` because the extractor was eligible but the condition itself was malformed.

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

### Bounded extraction retries

A memory whose extraction fails is retried, but never forever and never at the cycle rate. The worker runs two independent ladders, because the two failure populations have different evidence available about whose fault they are.

Both are bounded by policy rather than by hardware: nothing about the machine determines any of these numbers, so they are four operator-settable `[workers.extractor]` keys with documented defaults.

| Key | Default | Bounds |
|---|---|---|
| `max_apply_attempts_per_memory` | `3` | apply / gate infra-failure attempts for one memory |
| `provider_reachable_window_ms` | `900000` | how recently the LLM tier must have answered for a failure to be the memory's fault |
| `max_item_attributable_failures` | `5` | transient failures charged to the memory itself |
| `max_transient_attempts` | `48` | absolute attempts for one memory, whatever the cause |

All four are validated non-zero at startup. Zero is a configuration error, not a way to disable a ladder: a memory with no attempt budget would be abandoned on its first failure, and "retry nothing, ever" is not a setting worth making reachable by typo.

#### Ladder 1 — apply and gate infra failures

The apply transaction rolled back, or the idempotency gate errored before any tier ran. This ladder lives in a per-shard in-memory map of `{ attempts, last_attempt_unix_nanos, next_due_unix_nanos }`.

- **Backoff** is `min(60 s, 5 s << (attempts − 1))`. The cycle interval is one second, so without a ladder a rolled-back apply re-runs at 1 Hz; the 5 s base sits well above the interval so a busy shard or a contended write transaction gets room to clear before the budget is spent on it.
- **A deterministic failure spends the whole budget at once.** A redb failure that reproduces byte-for-byte on every retry — the store reporting the file damaged, a value over redb's 3 GiB per-value ceiling, or engine/migration misuse such as a wrong-typed table open, a table opened twice in one write transaction, or an invalid savepoint — would only delay the same verdict while re-running the pipeline each time. Everything else, including every failure nobody has classified, is treated as transient and retryable: a misclassification then costs retries rather than grounding.
- **The map is bounded by construction**, not by the assumption that few memories fail: at most 4 096 tracked memories, entries older than one hour expired first, then the least-recently-failed entry evicted, so a memory still actively walking its ladder is the last thing dropped.
- **It is deliberately in-memory.** A restart clears the ladder, and that is the behaviour worth having — a process restart is exactly the kind of event that clears the conditions this ladder exists for. The worst case it allows is `max_apply_attempts_per_memory` further attempts per restart, never the unbounded 1 Hz replay an absent ladder gives.
- **A committed apply clears the entry**, so attempts count *consecutive* failures. A memory that fails twice, succeeds, and fails again starts from zero rather than inheriting a nearly spent budget from an unrelated earlier incident.

#### Ladder 2 — transient LLM-tier failures

Durable, carried as two counters on the memory's `ExtractorPipelineAuditEntry`: `attempts` and `item_attributable_failures`. Backoff is `min(1 h, 1 s << (attempts − 1))`, and a memory is due again only once that interval has elapsed since its last attempt.

**Two counters, not one, and that is the point.** One counter cannot tell a memory that keeps failing on its own content from a memory caught behind a dead provider. With a single budget, a provider outage would walk every queued memory to exhaustion and convert a transient outage into permanent derivation loss across the whole shard.

- A failure is **attributable to the memory** only when the provider was demonstrably reachable: an LLM tier ran successfully on this shard within `provider_reachable_window_ms` of it. The success timestamp is recorded before the apply loop, so a failure in the same batch as a success is charged to the memory. The evidence is in-memory, so a restart falls back to charging the provider — the direction that preserves a memory's grounding.
- An attributable failure increments both counters; a failure with no such evidence increments only `attempts`. It still widens the backoff, but leaves the per-item budget intact.
- **Either bound alone ends the retries.** `max_item_attributable_failures` (5) catches a memory failing on its own content — a failure landing while other memories' calls are succeeding is the memory's own, and the same text fails the same way next time, so this is small by choice. `max_transient_attempts` (48) catches a provider that never comes back; it is derived, not chosen: the ladder widens to one attempt per capped hour, so covering a stated two-day outage window is 48 hours ÷ 1 hour.
- A **permanent** classification — bad or absent key, no balance, malformed request, schema mismatch — is terminal on the first failure. Retrying cannot help and only hides the problem from the operator.

#### Exhaustion is durable and visible

When either ladder gives up, the memory leaves the extraction queue and the fact is written down rather than inferred:

- A durable audit row with `status = FAILURE` and `failure_class = PERMANENT`. That pairing is what makes the idempotency gate report the memory terminal, so the queue row can be dropped without the next cycle re-queueing it.
- On the apply ladder, all three tier bytes are `SKIPPED` — the transaction rolled back or the gate errored before a tier was reached, so no tier's output survives to describe — and item counts and spend are zero, because the batch reports its provider spend whole and double-counting it on the memory's own row would misattribute it. On the LLM ladder the status keeps describing what the pattern and classifier tiers did commit; only the failure class carries terminality.
- The abandonment record is committed in its own small write transaction, ahead of the queue-row removal, so the two cannot be made atomic. That ordering is safe: a crash between them leaves a stale queue row that the next cycle's gate folds into `AlreadyExtracted` and removes. Writing it is best-effort — a failure to record must not fail the cycle that is making progress on every other memory in the batch.
- `brain_extractor_apply_dropped_total{reason}` counts the drop. Any nonzero value is signal loss. The reasons distinguish the five ways a memory can be abandoned: `apply_abandoned_deterministic`, `apply_abandoned_exhausted`, `gate_abandoned`, and — from the LLM ladder — `llm_abandoned_item` when the per-item bound tripped first, `llm_abandoned_provider` when the absolute bound did. The split is operationally load-bearing: the first says to look at the memory, the second says to look at the provider.
- An `error`-level event names the memory, the attempt counts and the reason, and states that backfill is the recovery path. `max_apply_attempts_per_memory` **defaults** to the backfill worker's own per-item cap, deliberately: sharing the number is what makes the two paths give up together, so an operator reasoning about one is reasoning about both. Overriding the key breaks that correspondence, which is a reason to override it knowingly rather than a reason not to.

### HyPE generation and the refresh sweep

HyPE rides inside the extractor worker rather than running as a worker of its own: a server shard wires it unconditionally at spawn and it shares the extractor's cycle. There is no operator flag that turns it off. A deployment that provides no generator at all — a bare substrate embedding, a test fixture — leaves the worker's slot empty and both the generation and the sweep become no-ops. For each freshly-encoded memory it asks the namespace's model for several diverse questions whose answer is that memory, embeds each locally with the same BGE model the rest of the write path uses, persists the vectors and inserts them into the live per-shard HyPE HNSW. At read time the query vector probes that pool, and a hit maps back to the owning memory.

The credential is the owning tenant's, resolved per namespace. HyPE is mandatory and runs on every write, so it is the largest model cost on the write path and the one most visibly mis-billed if the key is wrong. A namespace with no registered key still writes — the tier reports that it could not run rather than failing the write.

HyPE is best-effort throughout. Any failure — transport, empty reply, embed error, redb error — is logged and yields a zero outcome rather than failing the surrounding extraction cycle. It is a recall enhancement, never a correctness dependency.

#### The refresh sweep

A memory is first HyPE'd against whatever neighborhood existed at encode time, which is often empty: the connecting facts arrive in later memories. The bridge question a multi-hop read needs — "Where did Niraj's manager work before?" — can only be written once both the reports-to and the prior-employer facts exist, and they landed in different memories at different times.

So the worker also sweeps already-encoded memories in round-robin, recomputes each one's current neighborhood, and regenerates only when it actually changed. Eight memories are examined per cycle, with a cursor advancing across cycles. The number is compiled in rather than exposed as a TOML key: examining a memory is one cheap hash check, and the only thing a larger window buys is more LLM calls in the one cycle where a neighborhood changed — a trickle is the point. Internally `0` disables the sweep, which is also what an empty generator slot amounts to.

A window that does not fill wraps to the start of the table to complete the round — so on a small corpus **every memory is examined every cycle**, and the cycle is one second.

#### The refresh gate stores the input, not the outcome

That cadence is what makes the gate's exact contents load-bearing. The gate is a stored hash of `(text, neighborhood)`: on each pass the worker recomputes the hash and, unless it differs from the stored one, returns without calling the model — one redb point read and no spend. A changed neighborhood misses, which is the intent, and the LLM cache is keyed on the same pair so a genuinely-changed neighborhood misses there too.

**The hash stored is the hash of the input already put to the model** — not the hash of an input that produced questions. A model can answer for an exact input and the answer can hold no usable question; the call was still billed. Storing the hash only on a question-producing reply would leave that input ungated, so the next pass one second later would put the identical question to the model and be billed again, every second, for as long as the memory and its neighborhood sit unchanged. Recording the input as answered is what makes a billed-but-empty reply cost exactly once.

Both paths do it: a first generation whose reply parses to zero questions, and a refresh whose reply does the same. A failure to record is logged at `warn` and does not fail the cycle.

Spend from both paths lands in the `llm_spend` ledger under distinct stages — `HYPE_GENERATE` and `HYPE_REFRESH` — keyed by namespace, because neither produces an extraction audit row to carry a cost. Regeneration spend is also folded into the same cost metric the extraction tiers report.

Generated questions are clamped to 16 per memory. The storage key's index byte allows 256; the clamp is defensive, so a runaway reply cannot overflow the index.

### Always-on extractors

Extraction is always-on and non-configurable — there is no per-tier enable flag and no `EXTRACTOR_DISABLE` / `EXTRACTOR_ENABLE` op. Every registered extractor runs on every eligible item; there is no "disabled" eligibility-filter branch. A tier whose model can't load is a hard shard-spawn failure, not a silent skip (see [`../11_extractors/00_purpose.md`](../11_extractors/00_purpose.md)).

### Graceful shutdown

On shard shutdown:
1. Dispatcher stops accepting new items.
2. Workers drain pending items with a 30 s timeout.
3. Items not drained get audit-row stubs `Skipped(reason: "shutdown drain timeout")` so the post-restart operator sees what was lost.

Brain ships steps 1+2; step 3 (timeout stub-writing) is deferred to avoid touching the shutdown path more than needed.

### Extractor observability

Per [`./00_purpose.md`](./00_purpose.md) (Observability) plus extractor-specific:

Every family below is labelled `shard`, since each shard owns its own `ExtractorWorker`.

- `brain_extractor_tier_runs_total{shard, tier, status}` — per-tier outcome for each memory processed. `tier` ∈ `{pattern, classifier, llm}`, `status` ∈ `{ran, skipped, failed}`.
- `brain_extractor_drops_total{shard}` — encode-side enqueues dropped because the extractor channel was full.
- `brain_extractor_apply_dropped_total{shard, reason}` — real extracted items the apply pass could not persist. **Any nonzero value is signal loss.** `reason` carries both the per-item discard reasons (`subject_unresolved`, `predicate_invalid`, `object_missing`, `nil_endpoint`, `self_reference`, `create_rejected`, `declared_value_type_mismatch`, `relation_type_invalid`, `endpoint_unresolved`, `statement_low_confidence`, `vague_object`, `clause_object`, `relation_value_object`, `artifact_edge_endpoint_unresolved`) and the whole-memory abandonment reasons from the retry ladders (`apply_abandoned_deterministic`, `apply_abandoned_exhausted`, `gate_abandoned`, `llm_abandoned_item`, `llm_abandoned_provider`).
- `brain_extractor_schema_filtered_total{shard, predicate}` — items dropped because their predicate or relation type is not in the active schema.
- `brain_extractor_items_written_total{shard, item_kind}` — typed-graph rows persisted. `item_kind` ∈ `{entity, statement, relation, mention, hype}`.
- `brain_extractor_resolver_outcome_total{shard, tier}` — which resolver tier satisfied each entity mention. `tier` ∈ `{exact, alias, fuzzy, embedding, disambiguated, create}`.
- `brain_extractor_predicate_consolidated_total{shard}` — open-vocab predicate surface forms folded onto an existing near-synonym predicate id.
- `brain_extractor_llm_micro_usd_spent_total{shard}` — LLM-tier spend reported by extractors, in dollar micro-units (1e-6 USD). A counter, not a budget: nothing reads it to refuse a call.
- `brain_extractor_cycle_duration_seconds{shard}` — histogram over worker cycles.

### Extractor tests

Brain's tests verify:

- All three tiers dispatch through the single extractor queue (no in-ENCODE synchronous path).
- Classifier queue overflow drops + writes audit + emits metric.
- `depends_on` chain blocks dequeue until parent's audit row appears.
- Shutdown drains within 30 s timeout.
- A deterministic storage fault abandons on the first failure; a transient one walks the ladder to `max_apply_attempts_per_memory` and then abandons.
- The deterministic/transient split is read off the redb error value itself, not guessed from a message.
- A committed apply clears the memory's ladder entry, so a later failure starts from zero.
- The apply backoff widens and saturates at its cap, and the apply-failure map stays within its bound.
- A `FAILURE` row whose transient budget is unspent is **not** terminal and is due only after its backoff; one that has spent either budget is terminal, and so is a `PERMANENT` row on its first failure.
- A HyPE input recorded as answered closes the refresh gate, and a changed neighborhood reopens it.

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

Input: `IndexableMemory { id: MemoryId, text: String, space_id: SpaceId, kind: MemoryKind, created_at_unix_ms: u64 }`.

Loop:

```
while let Some(item) = queue.recv().await {
    writer.delete_term(memory_id_term(item.id));   // idempotent
    writer.add_document(doc! {
        memory_id => item.id.as_u64(),
        text => item.text,
        space_id => item.space_id.as_bytes(),
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

- `batch_size >= [index] tantivy_commit_n` (default 256), OR
- `now() - last_commit_at >= [index] tantivy_commit_ms` (default 1000 ms).

On `commit()` returning `Err`:
- Retry once after a 10 ms backoff.
- On second failure: **fail the shard** (text indexing is required correctness, per "Two workers, one discipline"). The shard supervisor logs a fatal error, drains other workers, and surfaces the failure to the operator via the shard health endpoint.

This contrasts with the LLM extractor (see [`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md)), which retries a schema-validation failure once and then gives up on that memory, and walks a bounded backoff ladder on a transient provider failure before abandoning it with a terminal audit row. Either way the memory loses its grounding and the shard keeps serving: the LLM tier is best-effort, the text indexer is not.

### WAL integration

The indexer worker sits **downstream of the WAL**. The post-commit pipeline in `brain-ops` emits indexable events only after `wal_record.fsync()` has returned.

Ordering on the shard's post-commit fan-out (deterministic):

1. WAL fsync.
2. redb wtxn commit (memory + typed-graph tables).
3. Extractor enqueue (single queue; non-blocking).
4. **MemoryTextIndexer enqueue** (near-foreground).

Statement creation, entity resolution, and the **StatementTextIndexer enqueue** for extractor-derived statements happen later, inside the ExtractorWorker when it drains the memory (or, for a direct `STATEMENT_CREATE` op, in that op's own post-commit fan-out).

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

Clock-triggered, from the built-in cadence table (`WorkerConfig::defaults_for`). A worker's
cadence is operator-settable only where its own `[workers.<name>]` section
declares a key for it — see the configuration summary at the end of this
file. The scheduler computes the next tick from the previous tick's completion time; a slow tick doesn't accumulate backlog beyond one cycle.

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

**Cadence default:** daily (`[workers.supersession_sweeper] period_seconds`).

**Retention default:** `0` seconds, which sweeps nothing: a zero makes the cycle return immediately. Set `[workers.supersession_sweeper] retention_seconds` to arm it, and `dry_run = true` first to see what a cycle would delete. So the worker has two gates — `enabled` decides whether it is registered at all, and a non-zero retention decides whether a registered cycle does anything.

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

**Cadence default:** daily (built-in, 86 400 s; no config key).

**Retention default:** 90 d (`DEFAULT_AUDIT_RETENTION_SECONDS`). The worker
exposes `with_retention_seconds`, but the server constructs it with
`AuditLogSweeper::new()` and wires nothing to it, so the default is the only
value a deployment can have today. A retention of `0` disables the sweep.

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

**Cadence default:** hourly (`[workers.llm_cache_sweep] interval_secs = 3600`,
with `enabled` alongside it).

**One pass per tick.** `sweep_expired` deletes every row whose expiry is
`<= now`, from the response table and the TTL secondary index, in a single
`WriteTransaction`. Each candidate row carries its own authoritative expiry
and is deleted only when *that* expiry has passed, because re-caching an input
writes a fresh row and leaves the earlier TTL hint behind.

**There is no capacity pass.** Nothing sums the table's bytes and nothing
evicts by recency: the cache is bounded by TTL alone, and a deployment with
many distinct inputs inside one TTL window holds all of them.

**Expiry** is set per row at insert, from the extractor's own `cache_ttl`
declaration in the extractor document; the default when a document omits it is
**7 days**, not 90. It is a schema value, not an operator knob.

**No-op when slot is None.** If `OpsContext.llm_cache` is `None` (schemaless deployment — no LLM extractors configured), the worker returns `Ok(())` immediately.

**Eviction note — `last_used_at`.** Brain does not currently refresh `last_used_at_ns` on cache reads. Eviction order is effectively by `created_at_ns`. Acceptable for the 90 d TTL the cache uses; touch-on-read is a future optimisation.

**No audit row** for cache eviction — the cache is derived state, not a system-of-record.

### Stale extraction detector

**Spec source:** [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) — surface statements whose `schema_version` is behind the current schema.

**Cadence default:** hourly (built-in, 3 600 s; no config key).

**Batch cap default:** 512 rows scanned per tick.

**Predicate:**

```
fn is_stale(stmt, current_version) -> bool {
    stmt.tombstoned == 0 && stmt.schema_version < current_version
}
```

`current_version` is read once per tick as the **maximum** active version across every namespace's `SCHEMA_ACTIVE_VERSIONS_TABLE` row. On a multi-tenant shard that over-counts: one tenant uploading their ninth schema makes every other tenant's v1-era rows read as stale. The count is an operator hint and nothing gates on it, so read it as an upper bound rather than a total. A precise per-`(memory, namespace)` lookup is post-v1. A shard with no active schema at all reads version 0 and the tick is a no-op.

**Effect: this worker counts, and does nothing else.** It writes no per-row flag — that would need a `StatementRow.flags` schema bump — writes no audit row, and does not re-extract. The count is logged and exposed through `sweeper_swept_total{worker="stale_extraction_detector"}`.

**Scope:** active rows only. Tombstoned statements are skipped.

**What re-extracts:** the schema-migration worker, and only when `[workers.extractor] reextract_on_schema_change` is on — it is off by default, because re-extraction is one LLM call per memory. With it off, that worker logs how many memories fell outside the new schema and the operator re-aligns them with `POST /v1/extract/backfill`. See "Schema migration re-extraction gate" below.

### Entity GC sweeper

**Spec source:** [`../02_data_model/06_entity_lifecycle.md`](../02_data_model/06_entity_lifecycle.md) + [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md#retention).

**Cadence default:** daily (built-in, 86 400 s; no config key).

**Off by default**, in two places that agree: `EntityGcWorker::new()` sets
`enabled = false` and `defaults_for(EntityGc)` carries `enabled = false`. There
is no config key — a deployment turns it on by building it enabled, or by the
admin `POST /v1/workers/entity_gc/start`. The worker checks the flag at the top
of each tick; if false, returns `Ok(())` immediately.

**Grace period default:** 30 d (`DEFAULT_ENTITY_GC_GRACE_SECONDS`). Settable
through the worker's builder, which the server does not call, so the default is
the only value a deployment can have today.

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

**Cadence default:** hourly (`[workers.confidence_sweep] interval_secs`, with
`enabled` alongside it; on by default).

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

Brain reads configuration from a TOML file, with environment overrides in the
form `BRAIN__<SECTION>__<FIELD>` (double underscore per level). **No sweeper
knob is a single-underscore `BRAIN_*` variable**; the only ones of that shape
in the tree are test and benchmark switches.

What an operator can actually set:

| Knob | Default | Worker |
|---|---|---|
| `[workers.supersession_sweeper] enabled` | `false` | Supersession sweeper |
| `[workers.supersession_sweeper] retention_seconds` | `0` (sweeps nothing) | Supersession sweeper |
| `[workers.supersession_sweeper] period_seconds` | `86400` | Supersession sweeper |
| `[workers.supersession_sweeper] dry_run` | `false` | Supersession sweeper |
| `[workers.llm_cache_sweep] enabled` | `true` | LLM cache sweeper |
| `[workers.llm_cache_sweep] interval_secs` | `3600` | LLM cache sweeper |
| `[workers.confidence_sweep] enabled` | `true` | Confidence sweep |
| `[workers.confidence_sweep] interval_secs` | `3600` | Confidence sweep |
| `[workers.statement_reclaim] enabled` | `false` | Statement reclaim |
| `[workers.statement_reclaim] grace_seconds` / `period_seconds` | 30 d / `86400` | Statement reclaim |
| `[workers.predicate_gc] enabled` | `false` | Predicate GC |
| `[workers.predicate_gc] grace_seconds` / `period_seconds` | 7 d / `86400` | Predicate GC |

What is a code default with no configuration surface:

| Value | Default | Worker |
|---|---|---|
| Cadence | daily | Audit log sweeper, entity GC |
| Cadence | hourly | Stale extraction detector |
| Batch cap | 1024 | Audit log sweeper, LLM cache sweeper |
| Batch cap | 256 | Supersession sweeper, entity GC |
| Audit retention | 90 d | Audit log sweeper |
| Entity GC grace | 30 d | Entity GC |
| Entity GC enabled | `false` | Entity GC |
| Forget-cascade confidence threshold | `0.2` | Forget cascade |

Response-cache TTL belongs to neither table: it is `cache_ttl` on the
extractor definition, defaulting to 7 days.

A worker can also be paused and resumed at runtime without a restart, through
`POST /v1/workers/{name}/{stop|start|run-now}` on the admin plane. Tests set
small values (seconds, not days) by constructing workers directly.

### Per-statement-kind retention

A predicate declares `retention: <duration>` in the schema DSL. The `statement_reclaim` worker soft-tombstones statements past that window with reason `RetentionExpired`, and they then reclaim through the normal tombstone-grace flow.

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

Completed checkpoints accumulate. Brain keeps them indefinitely for their audit value; no sweeper deletes them.

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

#### Per-item failure accounting

Resilience is per item, not per run: **one bad item never aborts the whole backfill.** There is no aggregate failure-rate abort, no `BackfillAborted` event and no `HighFailureRate` reason — a plan runs to the end of its items or until it is cancelled.

An item is retried on later cycles up to `MAX_ATTEMPTS_PER_ITEM` (3). Once its checkpoint reads `Failed` with the cap reached, it is permanently failed: counted as `failed` in the run's progress — deliberately not as `skipped_already_completed`, so a resume cannot mask a bad item as a skip — and never retried again, while the rest of the run continues.

The operator-facing signal is therefore the run's own `failed` count, not an abort. The extractor worker's apply ladder shares this cap by default, so a memory that cannot be applied gives up at the same point on both paths (see §"Bounded extraction retries").

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
     a. If the FORGET is soft, journal a ForgetUndoRecord to
        forget_undo_log capturing the stripped evidence entry
        and the row's prior confidence / is_current /
        tombstone_reason / overflow id (hard FORGET journals
        nothing — it is irreversible).
     b. Drop `memory_id` from `evidence`.
     c. Recompute `confidence` per ../10_metadata/00_purpose.md.
     d. If evidence.is_empty():
          - confidence >= threshold:
              mark `stale_evidence` flag; keep row.
          - else:
              tombstone with reason=SourceMemoryForgotten;
              audit row.
     e. mark_completed in the same wtxn.
5. Commit. If more than 256 dependents remain, enqueue a
   continuation job for the leftover.
```

#### Soft vs hard cascade

- **Soft FORGET** (Brain's default with a grace window): the `Apply` cascade mutates dependent rows immediately but additively journals each mutation to `forget_undo_log` (see step 4a above and [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) — the additive undo log). If the FORGET is reverted within grace, the worker receives a `CascadeKind::Revert` job and replays the undo log: it re-attaches evidence, re-adds the reverse-index row, recomputes confidence, and un-tombstones each row still carrying reason `SourceMemoryForgotten`, deleting each consumed undo row in the same txn so a re-run is a structural no-op.
- **Hard FORGET**: the cascade hard-tombstones immediately and writes no undo records — it is irreversible.
- **Post-grace**: once the grace window passes, slot reclamation reaps the forgotten memory's undo rows, so a soft FORGET becomes irreversible after grace too.

#### Confidence threshold

`DEFAULT_CASCADE_CONFIDENCE_THRESHOLD = 0.2`, a code default with no config key. Below this, an empty-evidence statement is tombstoned; above this, it survives with the `stale_evidence` flag set (operator can re-extract or accept the staleness).

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

#### Schema migration re-extraction gate

Nothing refuses a migration on cost — spend is tracked, never rationed (see [`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §22). What stands between a `SCHEMA_UPLOAD` and a large provider bill is a deliberate operator decision, not a ceiling.

`[workers.extractor] reextract_on_schema_change` defaults to **off**, and that default is the control. Re-extraction costs one LLM call per affected memory, so a single upload against a large corpus would otherwise turn into thousands of paid calls with no operator in the loop.

With it off, the worker still completes its flag sweep and then logs — at `info`, because staying flagged is the configured behaviour and not a fault — the namespace, the new version, how many memories carry statements outside it, and the instruction to run `POST /v1/extract/backfill` to re-align them. The operator sees the size of the bill before choosing to pay it.

With it on, a namespace becomes self-healing: each stale memory's text is re-enqueued for extraction. A memory that is tombstoned, hard-forgotten or has lost its text row is skipped — there is nothing left to re-extract from. An enqueue that finds the channel full is also skipped rather than retried in place, which would spin against a full channel; the statement keeps its flag, so the next upload or an admin backfill picks it up. Either way the work is bounded by the extractor's own `channel_capacity`: enabling this cannot outrun that ceiling, only fill it.

#### Schema migration cancellation

`AdminCancelSchemaMigration(request_id)`. Same shape as backfill cancellation.

#### Schema migration audit rows

One `AuditOp::SchemaUpgraded` per plan, written once at plan start. Per-item audit rows per the `ReextractOutcome`'s normal extraction-audit semantics (`AuditOp::Extracted` / `AuditOp::Superseded`).

### State-carrying limits

- Backfill and schema-migration process **one item per transaction**, which keeps each item's checkpoint isolated from its neighbours' failures.
- A shard runs **one** backfill or migration plan at a time; plans do not run concurrently within a shard.
- A single migration plan runs its extractors serially — there is no per-extractor parallelism inside one plan.

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
