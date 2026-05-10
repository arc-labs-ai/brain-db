# 08.11 Open Questions

Planner & executor questions unresolved as of this spec version.

---

## OQ-PE-1: Plan caching

**Issue.** Repeated identical request shapes invoke the planner each time. Caching plans would save ~50 µs per request.

**Options.**

a) **No caching (status quo).** Plan each request.

b) **Plan cache by request shape.** Hash the structural shape of the request; cache the plan.

c) **Lazy plan compilation.** First call compiles; subsequent calls reuse.

**Recommendation.** Defer. Plan time is small relative to total request latency. Caching is justified only if planning becomes a bottleneck.

---

## OQ-PE-2: Cost-based ef adaptation

**Issue.** The planner's ef_search picking is rule-based. Could it be cost-based, picking the ef that meets a target latency given current load?

**Options.**

a) **Rule-based (status quo).** Predictable; simple.

b) **Cost-based.** Estimate cost as a function of ef; pick ef to hit target latency.

c) **Adaptive.** Track per-shard latency vs ef and learn.

**Recommendation.** Defer. The rules are good enough; cost-based adds complexity without clear gain.

---

## OQ-PE-3: Query rewriting

**Issue.** Some requests can be rewritten for efficiency. Example: a complex filter could be split into multiple simpler queries with merged results.

**Options.**

a) **No rewriting (status quo).** Plan as written.

b) **Rule-based rewriting.** A small library of rewrites.

c) **General optimizer.** Like a SQL query optimizer.

**Recommendation.** Stay with (a). Rewrites are a slippery slope; we want predictable execution.

---

## OQ-PE-4: Streaming RECALL responses

**Issue.** For very large K, the response is sent as one frame. Streaming results as they're computed would let clients start processing earlier.

**Options.**

a) **Single-frame response (status quo).**

b) **Streaming**: send results in batches as they're available.

**Recommendation.** Defer. Most clients use small K. For large-K cases, the wire protocol's stream support is available; we can revisit if a real workload demands it.

---

## OQ-PE-5: Idempotency fail-open vs fail-closed

**Issue.** When the idempotency table is unavailable (corrupt, etc.), should the substrate fail open (process without check, risking duplicates) or fail closed (reject the request)?

**Options.**

a) **Fail open (current).** Log warning, proceed.

b) **Fail closed.** Reject with `IdempotencyUnavailable`; client retries.

**Recommendation.** Fail closed — duplicate memories are worse than retry-able errors. Implement before v1 release.

---

## OQ-PE-6: Plan re-execution after partial failure

**Issue.** If a shard fails mid-execution, can we re-route to a healthy shard?

**Options.**

a) **No re-routing (status quo).** Fail with partial results.

b) **Re-routing for read-only operations.** If a shard is unreachable, try a replica.

c) **Full HA failover.** Multi-shard replication with automatic failover.

**Recommendation.** Need replication first. Without replicas, there's nowhere to re-route. v2 priority.

---

## OQ-PE-7: Per-tenant prioritization

**Issue.** All requests are first-come-first-served. Some tenants may need priority over others.

**Options.**

a) **FIFO (status quo).**

b) **Priority queues.** Per-tenant or per-request priority.

c) **Quotas + token-bucket.** Each tenant gets a rate; over-rate requests get deprioritized.

**Recommendation.** (c) is right for multi-tenant deployments. Implement in v1.x.

---

## OQ-PE-8: Compiled plans

**Issue.** Plans are interpreted (the executor matches on plan variants). A compiled plan (executable code) would be faster.

**Options.**

a) **Interpreted (status quo).**

b) **Compiled.** Generate Rust code at startup or runtime.

**Recommendation.** Stay with interpreted. The interpretive overhead is negligible compared to the actual storage operations.

---

## OQ-PE-9: Plan introspection in production

**Issue.** Currently, plans are logged but not visible to clients. Should clients be able to see plans (similar to SQL `EXPLAIN`)?

**Options.**

a) **Operator-only (current).** Plans visible via admin commands; not to typical clients.

b) **Per-request explain flag.** Clients can request the plan in the response.

c) **Always include.** Every response includes the plan.

**Recommendation.** (b) for v1. Explicit opt-in.

---

## OQ-PE-10: Subexpression deduplication

**Issue.** PLAN and REASON involve multiple RECALLs. If two of those RECALLs are similar, work is duplicated.

**Options.**

a) **No deduplication (status quo).**

b) **Detect identical sub-queries within a plan.** Run once; reuse.

**Recommendation.** Defer. Cases where this matters are rare; complexity isn't justified.

---

## OQ-PE-11: Speculative execution

**Issue.** For some queries, the planner could speculatively start work before knowing the full plan. Example: start embedding the cue while finalizing other plan parameters.

**Options.**

a) **Sequential (status quo).** Plan, then execute.

b) **Speculative.** Plan and execute concurrently; cancel if speculation was wrong.

**Recommendation.** Defer. Plan time is < 50 µs; speculation saves at most that.

---

*Continue to [`12_references.md`](12_references.md) for references.*
