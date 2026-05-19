# Phase 24: Sweepers and Acceptance ✓

## Status

**Complete** — tags `phase-24-complete` and `v1.0.0`. Thirteen sub-tasks (24.0–24.12) landed on `feature/phase-24-acceptance`. Per-sub-task plans live under [`.claude/plans/phase-24-task-0[0-9].md`](../../.claude/plans/) + `phase-24-task-1[0-2].md`.

## Goal

Implement backfill jobs, FORGET cascade, supersession sweeper, audit log sweeper, stale extraction detection, entity GC, and schema-migration runner. Ship the schema-toggle runbook + end-to-end test + full acceptance suite. Cut `v1.0.0`.

## Prerequisites

- All prior knowledge-layer phases (15 through 23) complete.

## Reading list

- [`spec/25_provenance_versioning/00_purpose.md`](../../spec/25_provenance_versioning/00_purpose.md) — provenance, cascade, retention.
- [`spec/27_knowledge_workers/00_purpose.md`](../../spec/27_knowledge_workers/00_purpose.md) — worker table + scheduling.
- [`spec/27_knowledge_workers/03_sweeper_workers.md`](../../spec/27_knowledge_workers/03_sweeper_workers.md) — landed in 24.0.
- [`spec/27_knowledge_workers/04_state_carrying_workers.md`](../../spec/27_knowledge_workers/04_state_carrying_workers.md) — landed in 24.0.
- [`spec/31_complete_acceptance/00_purpose.md`](../../spec/31_complete_acceptance/00_purpose.md) — acceptance gate.
- [`spec/21_schema_dsl/00_purpose.md`](../../spec/21_schema_dsl/00_purpose.md) — migration semantics.

## Sub-tasks

| # | Title | Landed in |
|---|---|---|
| 24.0 | §27/03 + §27/04 spec backfill | [`phase-24-task-00.md`](../../.claude/plans/phase-24-task-00.md) |
| 24.1 | Backfill worker | [`phase-24-task-01.md`](../../.claude/plans/phase-24-task-01.md) |
| 24.2 | FORGET cascade worker | [`phase-24-task-02.md`](../../.claude/plans/phase-24-task-02.md) |
| 24.3 | Supersession sweeper | [`phase-24-task-03.md`](../../.claude/plans/phase-24-task-03.md) |
| 24.4 | Stale extraction detector | [`phase-24-task-04.md`](../../.claude/plans/phase-24-task-04.md) |
| 24.5 | LLM cache sweeper | [`phase-24-task-05.md`](../../.claude/plans/phase-24-task-05.md) |
| 24.6 | Entity GC worker | [`phase-24-task-06.md`](../../.claude/plans/phase-24-task-06.md) |
| 24.7 | Audit log sweeper | [`phase-24-task-07.md`](../../.claude/plans/phase-24-task-07.md) |
| 24.8 | Schema migration runner | [`phase-24-task-08.md`](../../.claude/plans/phase-24-task-08.md) |
| 24.9 | Schema-toggle runbook | [`phase-24-task-09.md`](../../.claude/plans/phase-24-task-09.md) |
| 24.10 | Schema-toggle e2e | [`phase-24-task-10.md`](../../.claude/plans/phase-24-task-10.md) |
| 24.11 | Full acceptance suite | [`phase-24-task-11.md`](../../.claude/plans/phase-24-task-11.md) |
| 24.12 | Phase exit + v1.0.0 | [`phase-24-task-12.md`](../../.claude/plans/phase-24-task-12.md) |

## Outputs delivered

- [x] `WORKER_CHECKPOINTS_TABLE` (shared across state-carrying workers).
- [x] `BackfillWorker` with submit/cancel/progress + per-(memory, extractor) checkpoint walk.
- [x] `ForgetCascadeWorker` + `cascade_ops::cascade_forget_to_statements` (evidence drop + confidence recompute + conditional tombstone).
- [x] Five Low-priority sweeper workers (supersession, audit, LLM cache, stale detector, entity GC).
- [x] `SchemaMigrationWorker` + `MigrationPlan` shape in `brain-core`.
- [x] `docs/runbooks/schema-toggle.md` RB-11.
- [x] `scripts/schema-toggle-e2e.sh` runbook-mirroring bash driver.
- [x] `scripts/full-acceptance.sh` orchestrator.
- [x] `scripts/spec-link-check.sh` cross-ref validator.
- [x] `docs/tutorials/02-first-substrate-app.md` (originally landed as `01-getting-started.md`; renumbered when `01-quickstart-docker.md` was added) 15-minute walkthrough.
- [x] `CHANGELOG.md` v1.0.0 release notes.

## Scope cuts (v1)

| Cut | Reason |
|---|---|
| Live backfill / migration mark items `Failed` — memory text only in WAL | Same scope cut as phase 22's memory text rebuild. Post-v1 memory-text store enables the path. |
| Cascade audit rows + soft-cascade revert | Cascade core logic ships; audit-side + revert deferred. |
| Per-row stale-extraction flag | Needs `StatementRow.flags` schema bump. Count surfaces via metric. |
| Entity GC inbound-reference counting | Workers + env-flag ship; eligibility scan is a stub. |
| LLM cache full sweep | TTL-on-read suffices for v1. |
| handle_forget → cascade enqueue hook | Workers reachable via direct enqueue; §25/00 contract preserved. |
| ADMIN_BACKFILL / cancel wire opcodes | Typed request shapes ship; CLI / HTTP surface follow-up. |
| SCHEMA_DROP opcode | Manual revert via the runbook. |

## Done-when (phase)

- [x] WORKER_CHECKPOINTS_TABLE shared across state-carrying workers.
- [x] All 8 phase-24 workers land on the existing `Worker` trait + scheduler.
- [x] Schema-toggle runbook + e2e bash driver + full-acceptance orchestrator in tree.
- [x] `v1.0.0` tagged.

## Phase exit

- [x] Sub-tasks 24.0–24.12 landed on `feature/phase-24-acceptance`.
- [x] All scope cuts documented in this file + ROADMAP + CHANGELOG.
- [x] Workspace `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` green.
- [x] `cargo clippy --target x86_64-unknown-linux-gnu -p brain-core -p brain-metadata -p brain-workers --all-targets -- -D warnings` clean.
- [x] Tags `phase-24-complete` + `v1.0.0` cut.

## Pitfalls

- Operators triggering backfill / migration against millions of memories that include LLM extractors can incur real LLM cost. Always `--dry-run` first.
- Periodic sweepers default to retention values that opt operators OUT of destructive sweeps (supersession retention = 0 = disabled; entity GC enabled = false). This is the spec §25/00 binding.
- The `worker_checkpoints` table grows monotonically in v1; a post-v1 sweeper handles completed-row retention.
