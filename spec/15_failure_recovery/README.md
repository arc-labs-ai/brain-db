# 15. Failure Modes + Recovery

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Substrate implementers; SRE teams |
| Voice | Hybrid (rationale + normative) |
| Depends on | All earlier architecture specs |
| Referenced by | — |

## What this spec defines

Brain's failure modes — what can go wrong — and the recovery procedures for each. This consolidates the failure-modes sections from earlier specs into one comprehensive reference.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_failure_taxonomy.md`](01_failure_taxonomy.md) | Categorizing failures |
| [`02_crash_recovery.md`](02_crash_recovery.md) | Crash recovery via WAL |
| [`03_corruption_recovery.md`](03_corruption_recovery.md) | Corruption recovery |
| [`04_data_loss_scenarios.md`](04_data_loss_scenarios.md) | When data loss is possible |
| [`05_partial_failures.md`](05_partial_failures.md) | Partial failure handling |
| [`06_disaster_recovery.md`](06_disaster_recovery.md) | DR procedures |
| [`07_chaos_testing.md`](07_chaos_testing.md) | Testing failure modes |
| [`08_open_questions.md`](08_open_questions.md) | Unresolved questions |
| [`09_references.md`](09_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
