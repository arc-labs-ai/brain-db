# 10.09 Open Questions

Concurrency-model questions unresolved as of this spec version.

---

## OQ-CC-1: Multi-writer-per-shard

**Issue.** The single-writer-per-shard discipline is a strong simplification. Could multi-writer give more throughput?

**Options.**

a) **Single-writer (status quo).** Simple; predictable.

b) **Multi-writer with serialization.** Multiple writers, but they serialize commits. Marginal improvement.

c) **Truly parallel writers.** Each commits independently with conflict resolution. Significant complexity.

**Recommendation.** Stay with single-writer. For higher write throughput, scale shards (each with its own writer). The architecture matches the workload pattern.

---

## OQ-CC-2: Adaptive publication interval

**Issue.** The publication interval is fixed (10 ms typical). Could it adapt — slow under heavy write load (better throughput), fast under light load (better read freshness)?

**Options.**

a) **Fixed (status quo).** Simple.

b) **Adaptive.** Adjust based on write rate and read demand.

c) **Per-request control.** Reads with `consistency=ReadAfterWrite` force immediate publication.

**Recommendation.** (c) is already implemented. (b) is a minor optimization; not pursued in v1.

---

## OQ-CC-3: Read priorities

**Issue.** All reads have the same priority. Some applications might want priority for time-sensitive reads.

**Options.**

a) **Equal priority (status quo).**

b) **Per-tenant priority.** Configurable.

c) **Per-request priority.** Set in the request.

**Recommendation.** Defer. For most workloads, equal priority is fine. Per-tenant comes in with multi-tenancy features (v1.x).

---

## OQ-CC-4: Adaptive yield budgets

**Issue.** The yield budget (~100 µs) is fixed. Could it adapt to load — finer granularity under contention, coarser when idle?

**Options.**

a) **Fixed (status quo).**

b) **Load-aware.** Track concurrent task counts; yield more when contended.

**Recommendation.** Defer. Fixed budget is simple and predictable. Adaptive scheduling is hard to get right and easy to misconfigure.

---

## OQ-CC-5: Replacement for crossbeam-epoch

**Issue.** crossbeam-epoch is mature but has rough edges. Hazard pointers (HP) or RCU might be alternatives.

**Options.**

a) **crossbeam-epoch (status quo).**

b) **Hazard pointers.** When a mature Rust impl exists.

c) **Custom epoch protocol.** Tailored to Brain's use cases.

**Recommendation.** Stay with crossbeam-epoch. Alternatives don't have clear advantages for our scale.

---

## OQ-CC-6: Per-shard executor config

**Issue.** All shards use the same Glommio executor configuration. Different shards (e.g., write-heavy vs read-heavy) might benefit from different tuning.

**Options.**

a) **Single config (status quo).**

b) **Per-shard tuning.** Configurable per shard.

**Recommendation.** Defer. The current config works for typical workloads. If specific shards need different config, an operator can add overrides.

---

## OQ-CC-7: Cross-shard transaction isolation

**Issue.** Brain doesn't support cross-shard transactions. Some applications might need them (rare).

**Options.**

a) **No support (status quo).**

b) **Two-phase commit.** Heavy; adds complexity.

c) **Saga pattern via SDK.** Application-level compensating actions.

**Recommendation.** Stay with (a). For applications needing cross-shard atomicity, the SDK's saga pattern is sufficient.

---

## OQ-CC-8: Memory pressure handling

**Issue.** Under memory pressure, the substrate may shed load and degrade gracefully. The current behavior is conservative (reject when CPU > 90% sustained for 5 sec). Could it be smarter?

**Options.**

a) **Threshold-based (status quo).**

b) **Predictive.** Detect rising load earlier and pre-emptively shed.

c) **Per-operation cost.** Higher-cost operations shed first.

**Recommendation.** (c) is reasonable; tracked for v1.1.

---

## OQ-CC-9: Interruptible long-running queries

**Issue.** Some queries (very deep PLAN, large RECALL) take seconds. Currently they run to completion.

**Options.**

a) **Run to completion (status quo).** Cancellable on client disconnect.

b) **Periodic check-in.** The client sends keep-alives; missing them cancels.

c) **Server-side time budget.** Cancel after a configured budget.

**Recommendation.** (c) is partially implemented (request timeout). For v1.x, expose finer-grained controls.

---

## OQ-CC-10: Consistency hint per request

**Issue.** Currently `consistency` has two values (Eventual / ReadAfterWrite). More nuanced options might be useful (e.g., "wait for the writer's queue to drain").

**Options.**

a) **Two-value (status quo).**

b) **Multi-value.** With explicit timing semantics.

**Recommendation.** Stay with two values. The current model handles the common cases.

---

*Continue to [`10_references.md`](10_references.md) for references.*
