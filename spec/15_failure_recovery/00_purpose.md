# 15.00 Purpose

This document specifies Brain's failure modes and recovery procedures — what can go wrong, what the substrate does, and what operators do.

## What this document covers

- The taxonomy of failure modes.
- Crash recovery via WAL.
- Corruption detection and recovery.
- Data loss scenarios and bounds.
- Partial failures (some shards down).
- Disaster recovery (DR).
- Chaos testing methodology.

## What this document does not cover

- **The mechanics of WAL, snapshots, etc.** Defined in [05. Storage](../05_storage_arena_wal/).
- **Per-component failure modes.** Documented in each spec's failure-modes file.

This spec consolidates the cross-cutting failure-mode story.

## 1. The failure-tolerance model

Brain's failure model:

- **Soft failures** (transient, self-recovering): network blips, brief overload, transient I/O errors. The substrate handles automatically; operations complete.
- **Hard failures** (require intervention): crashes, corruption, hardware failure. Recovery procedures restore the system.
- **Catastrophic failures** (data loss possible): multiple concurrent failures, attacker corruption. DR procedures mitigate.

## 2. The "no silent corruption" principle

When something goes wrong, the substrate prefers to fail loudly:

- Corruption detected → return error, log critical.
- Inconsistency detected → log, alert, may fail-stop.

Silent corruption (where wrong data is returned without indication) is the worst outcome. Brain's checks (CRCs, version checks, invariants) prevent this.

## 3. The "data is sacred" priority

Among the trade-offs, Brain prioritizes:

1. **Data integrity** — never silently corrupt.
2. **Data durability** — committed data survives crashes.
3. **Availability** — be up and answering.
4. **Performance** — be fast.

These are in priority order. We sacrifice availability for integrity (if corruption is suspected, fail-stop). We sacrifice performance for durability (sync writes). We sacrifice nothing for data integrity.

## 4. The "recoverable" guarantee

For every failure mode the substrate handles:

- A clear recovery procedure.
- Bounded data loss (preferably zero).
- Bounded recovery time.

Operators should never face "the substrate is broken; we don't know what to do".

## 5. The "failure budget" framing

Even with strong design, failures happen. The question is the rate:

- Crash recovery: maybe once per month per substrate (~1 in 10⁷ requests).
- Corruption: very rare (<1 in 10⁹ requests).
- Catastrophic: extremely rare (per-deployment, not per-request).

The substrate is designed so that the rare events are recoverable.

## 6. The "RPO" and "RTO"

For DR:

- **RPO (Recovery Point Objective)**: how much data can be lost? Brain's design: zero data loss for committed writes (WAL is durable).
- **RTO (Recovery Time Objective)**: how fast can we recover? Per-shard: 1-5 minutes for small shards; 10-30 minutes for large.

For deployments with strict RTO/RPO, snapshots and standby substrates reduce both.

## 7. The classification

The failure modes are classified along axes:

- **Locality**: single shard / multi-shard / cluster-wide.
- **Recoverability**: automatic / operator-assisted / DR.
- **Data impact**: none / transient / lost (within recovery window) / lost permanent.
- **Detection**: automatic / by metrics / by user impact.

## 8. The substrate's obligations

When something goes wrong, the substrate:

- Detects (via invariants, error codes).
- Logs the issue (structured, with context).
- Acts within its capability (auto-recover, fail-fast).
- Surfaces to operators (metrics, alerts).

## 9. The operator's obligations

When something goes wrong, the operator:

- Identifies the issue (alerts, logs, dashboards).
- Follows the runbook (where one exists).
- Escalates if outside runbook scope.
- Investigates root cause after recovery.

## 10. The "failures we don't recover from"

Some failures are unrecoverable without external action:

- Catastrophic disk failure with no backup.
- Successful attack that corrupted data and audit logs.
- Operator error that bypassed safety checks.

Brain provides backups, audit, and safety checks. Beyond that, the operator's process must include external safeguards (off-site backups, security practices).

## 11. The defensive posture

The substrate is defensive:

- Validates inputs.
- Checks invariants.
- Verifies CRCs.
- Asserts pre-/post-conditions.

Each check is a chance to catch a problem before it propagates. The cost (CPU) is low; the benefit (early detection) is high.

## 12. The post-mortem culture

Every significant incident should be post-mortemed:

- What happened.
- Why.
- How was it detected.
- How was it fixed.
- What can be done differently.

Brain's design isn't perfect; post-mortems improve it. The substrate's audit logs and metrics provide enough data for thorough post-mortems.

## 13. Pre-production testing

Before production:

- Simulate failures in staging.
- Practice recovery procedures.
- Validate runbooks.

Brain ships with chaos-testing tools (see [`07_chaos_testing.md`](07_chaos_testing.md)) for this.

## 14. The "blast radius" awareness

For each failure mode, what's the blast radius?

- Crash of one shard: affects clients of that shard.
- Crash of the substrate process: affects all clients of all shards.
- Data corruption on one shard: affects only that shard's data.

Knowing the blast radius helps prioritize.

## 15. The cumulative-failure scenarios

Worst-case failures combine multiple events:

- Disk failure + recent backup not yet replicated → data loss.
- Crash during recovery + corruption → recovery failure.

Brain's design handles single failures well. Cumulative failures are where careful operations matter (frequent backups, redundant replication, etc.).

## 16. The reliability engineering process

Reliability isn't just spec'd; it's engineered:

- Code review focuses on failure-handling.
- Tests include failure injection.
- Observability surfaces failures quickly.
- Runbooks are exercised (game days).

This document is part of that process.

---

*Continue to [`01_failure_taxonomy.md`](01_failure_taxonomy.md) for the failure taxonomy.*
