# 25.07 Open Questions

Provenance / versioning deferrals.

## Active

### Q1 — `ADMIN_GET_EXTRACTION_AUDIT` wire op

[`./01_audit_tables.md`](./01_audit_tables.md) §8 — audit query
is available via `brain-metadata::audit_ops` in phase 20 but
isn't exposed over the wire. Operators have to attach a CLI / SDK
shim. A dedicated wire op lands post-phase 20.

**Target:** phase 22+ admin. **Status:** deferred.

---

### Q2 — Audit-row output overflow

[`../22_extractors/05_audit.md`](../22_extractors/05_audit.md) §1 —
`outputs: Vec<OutputRefRow>` is capped at 64 entries. Overflow
behaviour (follow-on row keyed by `(audit_id, seq)`) deferred to
post-phase-20.

**Target:** phase 22+. **Status:** deferred.

---

### Q3 — Audit-log sweeper

[`../27_knowledge_workers/07_open_questions.md`](../27_knowledge_workers/07_open_questions.md)
Q4 tracks the sweeper that deletes expired rows + indexes. Phase
20 ships the write path; the sweeper itself lands post-phase-20.

**Target:** phase 22+. **Status:** deferred.

---

### Q4 — Re-extraction worker

[`./00_purpose.md`](./00_purpose.md) §"Re-extraction workflow"
describes the admin-triggered re-extraction. Phase 20 supports
re-running via `ExtractorRunOptions { replay: true }` but only as
a direct API call. A worker that runs across many memories lands
post-v1.

**Target:** post-v1. **Status:** deferred.

---

### Q5 — Bitemporal as-of-transaction

[`./00_purpose.md`](./00_purpose.md) §"Version visibility" — `as_of`
is valid-time only. Transaction-time queries (return state as it
existed in the system at time T) is post-v1.

**Target:** post-v1. **Status:** deferred.

---

### Q6 — Stale-extraction detection worker

[`./00_purpose.md`](./00_purpose.md) §"Stale extraction detection"
flags older versions. The detector runs as a periodic worker;
phase 20 doesn't ship it.

**Target:** phase 22+. **Status:** deferred.

---

### Q7 — `model_metadata` shape

[`./01_audit_tables.md`](./01_audit_tables.md) §1 — `model_metadata:
Vec<u8>` is an rkyv-archived blob. Phase 20 doesn't define the
inner shape (it's empty for pattern + classifier). Phase 21 LLM
fills it; the field structure (token counts, cache hit, model
version) lands then.

**Target:** phase 21. **Status:** open.

## Resolved

- Audit row layout (primary + 3 indexes) — resolved in
  [`./01_audit_tables.md`](./01_audit_tables.md).
- Atomicity (audit row + outputs in one wtxn) — resolved in
  [`./01_audit_tables.md`](./01_audit_tables.md) §9.
