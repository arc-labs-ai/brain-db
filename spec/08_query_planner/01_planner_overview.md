# 08.01 Planner Overview

The planner converts a typed request into an execution plan. This file describes its architecture and decision-making.

## 1. The planner's input

A typed request from validation:

```rust
enum Request {
    Encode(EncodeRequest),
    Recall(RecallRequest),
    Plan(PlanRequest),
    Reason(ReasonRequest),
    Forget(ForgetRequest),
    Link(LinkRequest),
    Unlink(UnlinkRequest),
    Admin(AdminRequest),
    Subscribe(SubscribeRequest),
    Txn(TxnRequest),
    // ...
}
```

Each variant carries the request's parameters: cue text, K, filters, idempotency key, etc.

## 2. The planner's output

An execution plan:

```rust
enum ExecutionPlan {
    Encode(EncodePlan),
    Recall(RecallPlan),
    // ... one variant per request kind
}

struct RecallPlan {
    embedding: EmbeddingStep,
    shards: Vec<ShardSearchStep>,
    merge: MergeStep,
    response: ResponseStep,
}
```

The plan is a description of what to do, not the doing itself. The executor takes the plan and runs it.

## 3. The planner's logic

For each request kind, the planner has a function:

```rust
fn plan_recall(req: &RecallRequest, ctx: &PlannerContext) -> RecallPlan {
    // ...
}
```

These functions:

1. Resolve the routing — which shards to involve.
2. Pick parameters — ef_search, over_factor, etc.
3. Decide on transformations — pre-filter, post-filter.
4. Determine response shape — text inclusion, score thresholds.

Each function is straightforward; no global optimizer.

## 4. The planner context

The planner has access to:

- The request itself.
- Per-shard statistics (memory count, tombstone ratio, last-rebuild-at).
- Configuration (default ef_search, max-results, etc.).
- The agent's metadata (quotas, configuration overrides).

It doesn't have access to the actual storage layer. Planning is computation only; no I/O.

## 5. The planner's invariants

For every request:

- Plan time < 100 µs.
- Plan size < 4 KB (in memory).
- Deterministic given the same input + context (no randomness).

These let the planner be invoked synchronously without yielding the executor's task.

## 6. Decision-making style

The planner uses fixed rules and lookup tables, not search:

```rust
fn pick_ef_search(filter_selectivity: f32, k: usize) -> usize {
    // Simple heuristic
    let base = 64;
    let selectivity_factor = (1.0 / filter_selectivity).max(1.0).min(8.0);
    let k_factor = (k as f32 / 10.0).max(1.0).min(4.0);
    (base as f32 * selectivity_factor * k_factor) as usize
}
```

No ML, no probing, no cost minimization across alternatives. Just rules that have been hand-tuned to work well for typical workloads.

## 7. The "fast path" idea

For the most common request shapes, the planner has a fast path that bypasses the general logic:

- Single-shard agent-scoped RECALL with default params: just pick ef=64, route to the agent's shard, search.
- Single ENCODE on a healthy shard: just route, embed, store.

The fast path is ~10 µs. The general planner is ~50-100 µs. Most requests hit the fast path.

## 8. The plan as a value

The plan is an immutable value passed from planner to executor. This:

- Lets the planner and executor be tested independently.
- Lets the executor log the plan for observability ("this request used ef_search=128 due to selective filter X").
- Allows future enhancements like plan caching or replay.

## 9. The planner doesn't do work

Embedding, storage I/O, and HNSW search aren't part of planning. The planner only describes them.

This separation matters because:

- Planning is synchronous (no yielding).
- Execution is asynchronous (yields all over).

If planning did I/O, the planner would be async, and request handling would have more layers of indirection.

## 10. The planner's API

```rust
struct Planner {
    config: PlannerConfig,
}

impl Planner {
    fn plan(&self, req: Request, ctx: &PlannerContext) -> Result<ExecutionPlan, PlanError> {
        match req {
            Request::Encode(r) => Ok(ExecutionPlan::Encode(self.plan_encode(r, ctx)?)),
            Request::Recall(r) => Ok(ExecutionPlan::Recall(self.plan_recall(r, ctx)?)),
            // ...
        }
    }
}
```

Synchronous. Pure given input and context. Easy to unit-test.

## 11. The planner doesn't know about networks

The planner doesn't deal with TCP, with retries, with timeouts. It doesn't know whether the executor will run subqueries on the same machine or across the network. The plan is at a level of abstraction above transport.

The executor maps shard references to actual destinations: same-machine (direct call) or cross-machine (RPC). The plan just says "search shard X".

## 12. Static vs dynamic planning

The planner does **static** planning: it decides everything based on the request and pre-computed context. It doesn't:

- Issue test queries to the storage to gauge cost.
- Run a small portion of the work and decide based on early results.
- Adapt during execution.

These would be "dynamic" planning. The simpler static approach is enough for our workloads.

The executor does **runtime adaptation** for some things (e.g., re-querying with higher ef if too few results). This isn't planner re-planning; it's the executor following an alternative branch defined in the plan.

## 13. The planner and observability

Each plan is logged with structured fields:

- Request type.
- Chosen parameters (ef_search, over_factor, etc.).
- Estimated cost.
- Latency of the planner itself.

Operators query these logs to debug latency anomalies ("why did this request take 50 ms? — the planner picked ef=500 due to a very selective filter").

## 14. Future enhancements

Possible enhancements (deferred):

- Plan caching: same request shape → cached plan.
- Cost-based plan selection across alternatives.
- Adaptive learning: track per-shard recall vs ef and tune.

For v1, the simple rules are sufficient.

---

*Continue to [`02_request_lifecycle.md`](02_request_lifecycle.md) for the request lifecycle.*
