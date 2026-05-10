# 11.10 Open Questions

Worker-related questions unresolved as of this spec version.

---

## OQ-BW-1: Worker prioritization within the low-priority pool

**Issue.** All workers are low-priority. But some are more important than others (idempotency cleanup vs decay).

**Options.**

a) **Equal priority (status quo).**

b) **Sub-priorities within low.** Critical workers (idempotency, WAL retention) get higher within-low priority.

**Recommendation.** Implement (b) in v1.x. The current model treats all workers equally; some workers becoming behind has different consequences.

---

## OQ-BW-2: Adaptive intervals

**Issue.** Worker intervals are fixed. Could they adapt to load and pending work?

**Options.**

a) **Fixed (status quo).**

b) **Adaptive.** Workers run more often when there's pending work, less when idle.

**Recommendation.** Defer. Fixed intervals are simple and predictable. Adaptive scheduling adds complexity.

---

## OQ-BW-3: Cross-shard worker coordination

**Issue.** Each shard's workers operate independently. For deployments wanting global view (e.g., total decay across shards), there's no coordination.

**Options.**

a) **Per-shard only (status quo).**

b) **Global coordination.** A central coordinator schedules across shards.

**Recommendation.** (a). The substrate is per-shard by design; cross-shard coordination is a different system.

---

## OQ-BW-4: Worker resource budgets

**Issue.** Workers don't have explicit CPU/memory budgets per worker. They share the low-priority pool.

**Options.**

a) **Shared pool (status quo).**

b) **Per-worker budgets.** Each worker has a CPU% allocation.

**Recommendation.** Defer. The shared pool is fine for typical workloads.

---

## OQ-BW-5: Manual worker triggering

**Issue.** Operators can stop workers but not manually trigger a cycle. Sometimes they want to force a cycle (e.g., immediate decay after a config change).

**Options.**

a) **No manual trigger (status quo).** Wait for next cycle.

b) **Add `ADMIN_WORKER_RUN_NOW <kind>`.**

**Recommendation.** (b) is useful and simple. Implement in v1.

---

## OQ-BW-6: Worker dependency graph

**Issue.** Some workers logically depend on others (e.g., reclamation depends on slot tombstoning happening first). The substrate doesn't model this; it just relies on independent timing.

**Options.**

a) **Independent (status quo).**

b) **Explicit dependencies.** A worker only runs after its dependencies have run.

**Recommendation.** (a) is fine. The implicit timing works because workers are idempotent.

---

## OQ-BW-7: Distributed-mode workers

**Issue.** In a distributed deployment, some workers (e.g., consolidation requiring an LLM call) might benefit from being centralized.

**Options.**

a) **Per-shard (status quo).** Each shard runs its own.

b) **Centralized for some workers.** A leader-elected coordinator handles certain tasks.

**Recommendation.** Defer until v2 (clustered deployments).

---

## OQ-BW-8: Worker hot-config reload

**Issue.** Worker configs are read at startup. Changes require restart.

**Options.**

a) **Restart on change (status quo).**

b) **SIGHUP reload.**

c) **Live reload via admin command.**

**Recommendation.** (c) is operator-friendly. Implement in v1.x.

---

## OQ-BW-9: Worker progress checkpointing

**Issue.** Some workers (decay, edge scrub) have cursors that track progress through a full pass. The cursor is in memory; lost on restart.

**Options.**

a) **In-memory cursor (status quo).** Restart resets to start.

b) **Persistent cursor.** Saved to redb.

**Recommendation.** (b) for workers with long passes. The cost is small (a few bytes); the benefit (continuing where left off) is meaningful for very large shards.

---

## OQ-BW-10: Telemetry granularity

**Issue.** Workers emit metrics at the cycle level. For debugging, per-record telemetry might be useful.

**Options.**

a) **Cycle-level (status quo).** Cheap; coarse.

b) **Per-record on demand.** A debug flag enables per-record logging.

**Recommendation.** (b). Implement as opt-in via configuration. Default off (volume would be too high).

---

*Continue to [`11_references.md`](11_references.md) for references.*
