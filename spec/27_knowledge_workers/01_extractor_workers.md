# 27.01 Extractor Workers

Worker-side scheduling for the three extractor tiers (pattern,
classifier, LLM). Phase 20 implements the pattern + classifier
workers; the LLM worker is phase 21.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — worker overview.
- [`../22_extractors/01_pattern_extractor.md`](../22_extractors/01_pattern_extractor.md)
  — pattern semantics.
- [`../22_extractors/02_classifier_extractor.md`](../22_extractors/02_classifier_extractor.md)
  — classifier semantics.
- [`../22_extractors/05_audit.md`](../22_extractors/05_audit.md) —
  audit row written by each worker.

## 1. Dispatch from ENCODE

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
                                                    (phase 21)
```

Dispatch order:
1. Pattern extractors run **synchronously** inside the ENCODE op
   handler, before the response is returned. Their outputs are
   already persisted when ENCODE acknowledges.
2. Classifier extractors are **enqueued** onto the near-foreground
   queue. ENCODE doesn't wait for them. Their outputs are visible
   1–10 ms later (typical p99).
3. LLM extractors are **enqueued** onto the background queue.
   Phase 21 implements the dispatcher; phase 20 leaves them
   un-dispatched (audit row writes `Skipped(reason: "llm tier
   pending")`).

## 2. Queue shapes

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
    /// Deps already resolved? (§22/03 §6.)
    pub deps_satisfied: bool,
}
```

Per-tier defaults (operator-configurable):

| Tier | Capacity | Overflow |
|---|---|---|
| Pattern | n/a (synchronous, no queue) | n/a |
| Classifier | 1 000 | `Drop` + metric |
| LLM | 10 000 | `Drop` + metric |

Overflow policy `Drop` records a metric counter and emits a
warn-level trace event so the operator sees pressure. The dropped
extraction writes an audit row `Skipped(reason: "queue full")`.

## 3. Scheduling priorities

Inherited from [`./00_purpose.md`](./00_purpose.md) §"Scheduling
priorities":

| Tier | Priority | Budget |
|---|---|---|
| Pattern | Foreground | shares the ENCODE op's allowance |
| Classifier | Near-foreground | 25% of shard time |
| LLM | Background | 20% of shard time (phase 21) |

The per-shard executor's cooperative-yield model (§11) applies —
a long classifier inference yields between memories to let
foreground work proceed.

## 4. Dispatch eligibility check

Before enqueueing, the dispatcher walks the active extractors and
filters:

```rust
fn is_dispatchable(ext: &ExtractorRow, mem: &Memory) -> bool {
    if !ext.enabled { return false; }
    let trigger = compile_trigger(&ext.trigger);
    if !trigger.evaluate(mem) { return false; }
    // depends_on resolution checked at dequeue time (§22/03 §6).
    true
}
```

Pattern dispatch runs this filter synchronously in ENCODE.
Classifier dispatch runs the same filter — if the trigger doesn't
match, no audit row is written (no row, no skip). The condition
"trigger eval error" still writes `Skipped(reason: "trigger eval
error")` because the extractor was eligible but the condition
itself was malformed.

## 5. Worker loop

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

`run_one_extraction` is the same function for any tier; it dispatches
to the right `dyn Extractor` impl based on the registry entry.

## 6. Backpressure

On classifier queue overflow:

1. The dispatcher writes the audit row directly with `Skipped(queue
   full)`.
2. A `worker_queue_overflow_total{worker=classifier}` counter
   increments.
3. The operator sees the metric and either:
   - Lowers the trigger filter (less work).
   - Disables the offending extractor.
   - Scales out (more shards).

Phase 20 doesn't implement adaptive throttling — that's phase 22+.

## 7. Disabled extractors

`extractor.enabled = false` (set via `EXTRACTOR_DISABLE`, §28/05
§7) means dispatch skips the extractor entirely at the §4 filter.
In-flight items already in the queue dequeue and run to completion
(per §28/05 §7 wording: "disabling is non-disruptive").

The audit row a disabled extractor would have written becomes
`Skipped(reason: "disabled")` if the dispatcher caught it; in-flight
items run normally and write `Success` / `Failure`.

## 8. Graceful shutdown

On shard shutdown:
1. Dispatcher stops accepting new items.
2. Workers drain pending items with a 30 s timeout.
3. Items not drained get audit-row stubs `Skipped(reason:
   "shutdown drain timeout")` so the post-restart operator sees
   what was lost.

Phase 20 ships steps 1+2; step 3 (timeout stub-writing) is phase
22+ to avoid touching the substrate shutdown path more than needed.

## 9. Observability

Per spec §00 §"Observability" plus phase-20-specific:

- `extractor_dispatch_total{tier, extractor_id}` — items dispatched.
- `extractor_skipped_total{tier, extractor_id, reason}` —
  filter / disabled / queue-full / dep-not-ready.
- `extractor_run_seconds{tier, extractor_id}` — histogram.
- `extractor_audit_writes_total{status}` — Success / Failure /
  Skipped* / SkippedDuplicate.

## 10. Tests

Phase 20 verifies:

- Pattern dispatch is in-process synchronous (no queue).
- Classifier queue overflow drops + writes audit + emits metric.
- `enabled = false` causes dispatch to skip.
- `depends_on` chain blocks dequeue until parent's audit row
  appears.
- Shutdown drains within 30 s timeout.
