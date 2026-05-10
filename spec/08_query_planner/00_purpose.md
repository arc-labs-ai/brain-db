# 08.00 Purpose

This document specifies how the substrate plans and executes requests. It bridges between the wire protocol (which defines what comes in and goes out) and the storage layer (which holds the durable state).

## What this document covers

- The lifecycle of a request from receipt to response.
- The planner — how the substrate chooses an execution strategy.
- The executor — how the substrate runs that strategy.
- Cost estimation, fan-out, and merging across shards.
- Concurrency between operations on a shard.
- Failure handling at the planner/executor level.

## What this document does not cover

- **Connection management.** Defined in [03. Wire Protocol](../03_wire_protocol/).
- **The cognitive primitives' semantics.** Defined in [09. Cognitive Operations](../09_cognitive_operations/).
- **Sharding decisions.** Defined in [12. Sharding + Clustering](../12_sharding_clustering/).
- **Background work.** Defined in [11. Background Workers](../11_background_workers/).

## 1. The role of planning

A request like "RECALL with this cue, K=10, kind=Episodic" can be satisfied many ways. The planner picks one:

- Use exact (brute-force) search? Or HNSW?
- ef_search = 64 (fast) or 128 (more recall)?
- Fan out to all shards or just the agent's shard?
- Pre-filter on metadata or post-filter?
- Return text inline or just IDs?

The planner makes these decisions based on:
- The request's parameters.
- The shard's current state (size, recent activity).
- Cost estimates.
- Quality requirements (the client's K and confidence).

## 2. The role of execution

Once the plan is chosen, the executor runs it:

- Issue read transactions on the metadata store.
- Call the embedder to compute query vectors.
- Call HNSW search.
- Read the arena.
- Apply filters.
- Marshal results into a response.

The executor handles concurrency: subqueries to multiple shards run in parallel; embedding and HNSW search overlap; result merging happens as data arrives.

## 3. The pipeline shape

```
[Connection layer]         (03. Wire Protocol)
      │
      ▼ (request frame)
[Validation + parsing]
      │
      ▼ (typed request)
[Planner]                  (this document)
      │
      ▼ (execution plan)
[Executor]                 (this document)
      │
      ├─→ [Embedder]       (04. Embedding Layer)
      ├─→ [Storage layer]  (05–07. Storage)
      └─→ [Result merger]
      │
      ▼ (response)
[Response framing]
      │
      ▼ (wire frame)
[Connection layer]
```

The planner and executor are the substrate's "brain" — where strategy is chosen and orchestrated.

## 4. Latency budget

For a typical RECALL with K=10:

| Stage | Latency budget |
|---|---|
| Validate + parse | < 10 µs |
| Plan | < 50 µs |
| Embed cue | 5-10 ms |
| HNSW search | 1-2 ms |
| Metadata lookup (per result) | 1-5 µs × K |
| Filter and merge | < 100 µs |
| Frame and send | < 50 µs |
| **Total** | **~10-15 ms** |

The embedder dominates. Other stages are a small fraction of the total. The planner aims to not become a meaningful part of the latency.

## 5. Throughput target

Per shard, sustained throughput:

- ENCODE: ~1K-5K/sec (limited by group commit and embedder).
- RECALL: ~5K-20K/sec (limited by embedder and HNSW).
- LINK / FORGET: ~5K-10K/sec (limited by group commit).

Per substrate (multiple shards in parallel): roughly N × shard throughput, where N is the number of cores.

## 6. The simplicity preference

The planner is intentionally simple compared to a SQL query optimizer. We don't have:

- Cost-based optimization with statistics.
- Query rewriting.
- Multi-stage plans with intermediate materialization.

Instead, we have:

- Decision rules: "if the request has X, do Y".
- A small cost estimator for picking ef_search and over_factor.
- Predictable execution.

This keeps planning fast (< 50 µs) and predictable (no surprise latency from the planner).

## 7. The "no surprise" principle

A request's latency should be predictable. Two requests with the same shape should have similar latency. The planner doesn't introduce variance through complex decision-making.

When the planner has multiple options, it picks based on simple, stable rules. Adaptivity is in the executor (e.g., re-querying with higher ef if too few results), not in the planner's strategy choice.

## 8. The relationship to cognitive operations

The 5 cognitive primitives (ENCODE, RECALL, PLAN, REASON, FORGET) are different shapes of requests. Each has its own planning rules. Subsequent files cover each.

The substrate also has admin operations (snapshots, recover, stats). These are more straightforward — typically a single operation against the storage layer with light planning.

## 9. The error path

A request can fail at many stages: validation, embedding, search, storage. The executor handles errors uniformly:

- Catch the error.
- Map it to a wire-protocol error code.
- Build an error response.
- Log details (with trace context).

The error response is sent on the same stream as the request. The client sees a structured error.

## 10. The role in the substrate

The planner+executor are the substrate's request-handling muscle. Most code paths flow through them. They're where:

- Quality vs latency trade-offs are made.
- Cross-shard coordination happens.
- Concurrency is orchestrated.

They're also where most of the substrate's runtime cost is — embedding and search, both initiated and orchestrated here.

---

*Continue to [`01_planner_overview.md`](01_planner_overview.md) for the planner architecture.*
