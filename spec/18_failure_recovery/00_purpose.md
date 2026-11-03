# 18. Failure Modes + Recovery

> **TL;DR.** What can go wrong and how Brain handles it. Crash recovery replays the WAL deterministically. CRCs on every WAL record and arena slot catch corruption; mismatches halt rather than return wrong data. Soft failures self-recover, hard failures follow documented runbooks, catastrophic failures use snapshots and external backups. Priority order: data integrity > durability > availability > performance. RPO is zero for committed writes; RTO is 1-30 minutes per shard. Chaos tests exercise kill-during-operation scenarios.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Brain implementers; SRE teams |
| Voice | Hybrid (rationale + normative) |
| Depends on | All earlier architecture specs |
| Referenced by | — |

## What this spec defines

Brain's failure modes — what can go wrong — and the recovery procedures for each. This consolidates the failure-modes sections from earlier specs into one comprehensive reference.

This document specifies Brain's failure modes and recovery procedures — what can go wrong, what Brain does, and what operators do.

## What this document covers

- The taxonomy of failure modes.
- Crash recovery via WAL.
- Corruption detection and recovery.
- Data loss scenarios and bounds.
- Partial failures (some shards down).
- Disaster recovery (DR).
- Chaos testing methodology.

## What this document does not cover

- **The mechanics of WAL, snapshots, etc.** Defined in [08. Storage](../08_storage/00_purpose.md).
- **Per-component failure modes.** Documented in each spec's failure-modes file.

This spec consolidates the cross-cutting failure-mode story.

## 1. The failure-tolerance model

Brain's failure model:

- **Soft failures** (transient, self-recovering): network blips, brief overload, transient I/O errors. Brain handles automatically; operations complete.
- **Hard failures** (require intervention): crashes, corruption, hardware failure. Recovery procedures restore the system.
- **Catastrophic failures** (data loss possible): multiple concurrent failures, attacker corruption. DR procedures mitigate.

## 2. The "no silent corruption" principle

When something goes wrong, Brain prefers to fail loudly:

- Corruption detected → return error, log critical.
- Inconsistency detected → log, alert, may fail-stop.

Silent corruption (where wrong data is returned without indication) is the worst outcome. Brain's checks (CRCs, version checks, invariants) prevent this.

## 3. The "data is sacred" priority

Among the trade-offs, Brain prioritizes:

1. **Data integrity** — never silently corrupt.
2. **Data durability** — committed data survives crashes.
3. **Availability** — be up and answering.
4. **Performance** — be fast.

These are in priority order. Brain sacrifices availability for integrity (if corruption is suspected, fail-stop), and sacrifices performance for durability (sync writes). Nothing is sacrificed for data integrity.

## 4. The "recoverable" guarantee

For every failure mode Brain handles:

- A clear recovery procedure.
- Bounded data loss (preferably zero).
- Bounded recovery time.

Operators should never face "Brain is broken; Brain does not know what to do".

## 5. The "failure budget" framing

Even with strong design, failures happen. The question is the rate:

- Crash recovery: maybe once per month per node (~1 in 10⁷ requests).
- Corruption: very rare (<1 in 10⁹ requests).
- Catastrophic: extremely rare (per-deployment, not per-request).

Brain is designed so that the rare events are recoverable.

## 6. The "RPO" and "RTO"

For DR:

- **RPO (Recovery Point Objective)**: how much data can be lost? Brain's design: zero data loss for committed writes (WAL is durable).
- **RTO (Recovery Time Objective)**: how fast can recovery complete? Per-shard: 1-5 minutes for small shards; 10-30 minutes for large.

For deployments with strict RTO/RPO, snapshots and standby nodes reduce both.

## 7. The classification

The failure modes are classified along axes:

- **Locality**: single shard / multi-shard / cluster-wide.
- **Recoverability**: automatic / operator-assisted / DR.
- **Data impact**: none / transient / lost (within recovery window) / lost permanent.
- **Detection**: automatic / by metrics / by user impact.

## 8. Brain's obligations

When something goes wrong, Brain:

- Detects (via invariants, error codes).
- Logs the issue (structured, with session).
- Acts within its capability (auto-recover, fail-fast).
- Surfaces to operators (metrics, alerts).

## 9. The operator's obligations

When something goes wrong, the operator:

- Identifies the issue (alerts, logs, dashboards).
- Follows the runbook (where one exists).
- Escalates if outside runbook scope.
- Investigates root cause after recovery.

## 10. The "failures Brain does not recover from"

Some failures are unrecoverable without external action:

- Catastrophic disk failure with no backup.
- Successful attack that corrupted data and audit logs.
- Operator error that bypassed safety checks.

Brain provides backups, audit, and safety checks. Beyond that, the operator's process must include external safeguards (off-site backups, security practices).

## 11. The defensive posture

Brain is defensive:

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

Brain's design isn't perfect; post-mortems improve it. Brain's audit logs and metrics provide enough data for thorough post-mortems.

## 13. Pre-production testing

Before production:

- Simulate failures in staging.
- Practice recovery procedures.
- Validate runbooks.

Brain ships with chaos-testing tools (see [`07_chaos_testing.md`](07_chaos_testing.md)) for this.

## 14. The "blast radius" awareness

For each failure mode, what's the blast radius?

- Crash of one shard: affects clients of that shard.
- Crash of Brain process: affects all clients of all shards.
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
