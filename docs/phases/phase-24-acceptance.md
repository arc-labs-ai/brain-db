# Phase 24: Sweepers and Acceptance

## Goal

Implement backfill jobs, FORGET cascade, supersession sweeper, audit log sweeper, stale extraction detection, entity GC, and schema-migration runner. Wire the full acceptance suite from `spec/31_complete_acceptance/`.

## Prerequisites

- All prior knowledge-layer phases (15 through 23) complete.

## Reading list

- `spec/25_provenance_versioning/00_purpose.md`
- `spec/31_complete_acceptance/00_purpose.md`
- `spec/27_knowledge_workers/00_purpose.md` (sweepers, GC)
- `spec/21_schema_dsl/00_purpose.md` (migration semantics)

## Outputs

- Backfill worker with progress tracking and resume.
- FORGET cascade worker.
- Supersession sweeper.
- Stale extraction detection worker.
- LLM cache sweeper.
- Entity GC worker (off by default).
- Audit log sweeper.
- Schema migration runner.
- Full knowledge-layer acceptance suite passing.

## Sub-tasks

### 24.1 Backfill worker

**Reads:** `spec/21_schema_dsl/00_purpose.md` (schema versioning), `spec/22_extractors/00_purpose.md`.
**Writes:** `crates/brain-workers/src/backfill.rs`.
**Done when:** admin can trigger backfill for a (memory_range, extractor_set); progress reported; resumable on restart; respects priority budget.
**Pitfalls:** Track per-(memory, extractor) completion in a redb table so we don't redo work.

### 24.2 FORGET cascade worker

**Reads:** `spec/25_provenance_versioning/00_purpose.md` (cascade section).
**Writes:** `crates/brain-workers/src/forget_cascade.rs`.
**Done when:** when a memory is forgotten, dependent statements have evidence list updated and re-evaluated for tombstone. Soft FORGET → soft cascade; hard FORGET → hard cascade.

### 24.3 Supersession sweeper

**Reads:** `spec/25_provenance_versioning/00_purpose.md` (retention).
**Writes:** `crates/brain-workers/src/supersession_sweeper.rs`.
**Done when:** periodically hard-deletes supersession-chain-old-entries past their retention period (default never; configurable).
**Pitfalls:** Default to forever-retention; users opt in to sweeping.

### 24.4 Stale extraction detection worker

**Reads:** `spec/25_provenance_versioning/00_purpose.md`.
**Writes:** `crates/brain-workers/src/stale_detector.rs`.
**Done when:** periodically flags statements with old `schema_version` or `extractor_version`; admin can list them.

### 24.5 LLM cache sweeper

**Reads:** `spec/26_knowledge_storage/00_purpose.md` (LLM cache).
**Writes:** `crates/brain-workers/src/llm_cache_sweeper.rs`.
**Done when:** removes expired cache entries; LRU eviction when over capacity.

### 24.6 Entity GC worker (off by default)

**Reads:** `spec/18_entities/00_purpose.md` (GC).
**Writes:** `crates/brain-workers/src/entity_gc.rs`.
**Done when:** if enabled, tombstones entities with no active references after grace period; reversible during the GC grace.

### 24.7 Audit log sweeper

**Reads:** `spec/25_provenance_versioning/00_purpose.md` (retention).
**Writes:** `crates/brain-workers/src/audit_sweeper.rs`.
**Done when:** removes audit entries older than the configured retention (default 90d).

### 24.8 Schema migration runner

**Reads:** `spec/21_schema_dsl/00_purpose.md` (migration semantics).
**Writes:** `crates/brain-workers/src/schema_migration.rs`.
**Done when:** on schema upload, computes migration plan (already done in phase 19); executes plan: re-extract memories under new schema, supersede old statements appropriately.

### 24.9 Schema-toggle runbook

**Reads:** `spec/21_schema_dsl/00_purpose.md`, `spec/31_complete_acceptance/00_purpose.md`.
**Writes:** `docs/runbooks/schema-toggle.md`.
**Done when:** step-by-step ops document for declaring a schema on an existing deployment, running backfill, and (if needed) reverting to schema-off mode without data loss.

### 24.10 Schema-on / schema-off end-to-end test

**Reads:** `spec/31_complete_acceptance/00_purpose.md`.
**Writes:** `tests/schema_toggle_e2e.sh`.
**Done when:** start with deployment + sample memories under no schema → declare schema → backfill → verify hybrid query works → verify substrate primitives still work as before.

### 24.11 Full acceptance suite

**Reads:** `spec/31_complete_acceptance/00_purpose.md`.
**Writes:** `tests/full_acceptance.rs`, `scripts/full-acceptance.sh`.
**Done when:** all functional, performance, storage, operational, and schema-toggle acceptance criteria pass.

### 24.12 Documentation polish

**Reads:** the whole spec.
**Writes:** consolidate cross-refs, fix typos, ensure all open questions tagged.
**Done when:** spec is consistent, all references resolve, acceptance gate items match across spec and phase docs.

## Done-when (phase)

- All workers spawn, run, and respect priorities.
- Schema toggle (declaring or removing a schema) works end-to-end.
- Full acceptance suite passes.

## Pitfalls

- Don't ship workers that are on by default but expensive (entity GC, supersession sweeper). Operators opt in.
- Backfill at scale: estimate cost (especially for LLM extractors) before running on millions of memories.
- Some acceptance tests need real workload patterns; design fixtures carefully.
