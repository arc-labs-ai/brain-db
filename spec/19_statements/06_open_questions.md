# 19.06 Open Questions

Statement-specific open questions. Wire-shape open questions live in [`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md).

## Active

### Q1 — Discrete `STATEMENT_CONTRADICTED` event

[`./02_contradiction.md`](./02_contradiction.md) §5.1: contradictory Facts emit only the per-create `STATEMENT_CREATED` event. Consumers that want contradiction notifications must run their own check after each create.

Should the substrate emit a discrete `STATEMENT_CONTRADICTED` event when `statement_create` detects a new contradiction? Phase 17 doesn't include this — keeps the event surface minimal — but query / monitoring use-cases would benefit.

**Target:** phase 23 (when query routing makes contradictions actionable). **Status:** open. **Likely outcome:** add the event.

---

### Q2 — `STATEMENT_LIST` `only_consistent` filter

Should `STATEMENT_LIST` support a filter that excludes subjects with contradictory Facts? Phase 17 ships without; the wire shape ([§28/06](../28_knowledge_wire_protocol/06_statement_frames.md) §9.1) has no such field.

Use case: "show me Priya's consistent facts; flag the contradictory ones separately."

**Target:** phase 23. **Status:** open. **Likely outcome:** add as part of the query DSL rather than a STATEMENT_LIST filter.

---

### Q3 — Confidence recomputation at read time

[`./04_confidence.md`](./04_confidence.md) §4: stored confidence is a snapshot at last touch. Pure age-based decay is **not** applied at read.

Should reads optionally recompute? A `recompute_at_read: bool` flag on `STATEMENT_LIST` and `QUERY` would let cost-sensitive callers stay with snapshots while accuracy-sensitive callers get fresh confidence.

**Target:** phase 23. **Status:** open. **Trade-off:** read latency vs accuracy.

---

### Q4 — Lazy confidence sweep worker

Phase 21+ may add a periodic worker that recomputes confidence for aging statements. Open: which trigger?

- (a) **Time-based:** every N hours, scan statements with `confidence > 0.1` and `last_touched_at_unix_nanos < now - 30 days`, recompute.
- (b) **Read-driven:** on each `STATEMENT_GET` of a statement older than threshold, queue it for recomputation; worker drains the queue.

**Target:** phase 21. **Status:** open. **Likely outcome:** (a) — simpler ops story.

---

### Q5 — `STATEMENT_ADD_EVIDENCE` opcode

[`./05_evidence.md`](./05_evidence.md) §3.1: evidence is set at create / supersede time only in v1.0. A separate op to append evidence (without superseding) would be useful for extractor pipelines that observe the same claim repeatedly.

**Target:** phase 22 (extractors). **Status:** open. **Likely outcome:** add as `STATEMENT_ADD_EVIDENCE` (0x0147 or similar) and route through `statement_ops`.

---

### Q6 — Multi-chunk evidence overflow

[`./05_evidence.md`](./05_evidence.md) §4: v1.0 supports a single overflow row per statement (up to 1000 evidence entries). Statements with > 1000 evidence need multi-chunk overflow.

Use cases: aggregated long-lived Preferences ("Priya prefers async meetings" with 10,000 supporting conversations).

**Target:** phase 22. **Status:** open. **Likely outcome:** chained `EvidenceOverflow` rows with `next_chunk_id`.

---

### Q7 — Contradiction down-weighting

[`./04_confidence.md`](./04_confidence.md) §"Open questions": should contradictory Facts down-weight each other? Current spec: each carries its own confidence independently; contradictions are surface signal, not confidence input.

Alternative: divide each contradicting Fact's confidence by the number of contradicting peers. Models "agreement reduces credibility."

**Target:** post-v1.0. **Status:** deferred. **Likely outcome:** stay independent; consumers weight at query time.

---

### Q8 — Per-predicate decay overrides

[`./04_confidence.md`](./04_confidence.md) §3.4: kind-level decay defaults only in v1.0. Per-predicate overrides would let `manages` (slowly changing) decay slower than `prefers_format` (fast-changing) within the Preference kind.

**Target:** phase 19 (schema DSL extension). **Status:** open. **Likely outcome:** add as `decay_half_life_seconds` predicate metadata in the schema DSL.

---

### Q9 — Auto-tombstone on critical evidence loss

[`./05_evidence.md`](./05_evidence.md) §11: when FORGET cascade reduces a statement's evidence to a single low-confidence entry, should the substrate auto-tombstone with `SourceMemoryForgotten`?

Trade-off: aggressive cleanup (tidy graph) vs preserving evidence for audit (history matters).

**Target:** phase 22. **Status:** open. **Likely outcome:** make it configurable via deployment config; default conservative.

---

### Q10 — Cross-shard `statements_by_object_entity` consistency

[`./03_storage.md`](./03_storage.md) §9: a statement with cross-shard `subject` and `object` writes its `by_object_entity` index entry to the object's shard. The two writes (primary on subject's shard, reverse-index on object's shard) must coordinate.

Phase 17 ships best-effort: primary writes succeed; reverse index updates asynchronously. Brief inconsistency window during which "what statements have X as object?" may miss a just-created statement.

**Target:** phase 17.4 implementation note; long-term resolution in phase 23. **Status:** open. **Likely outcome:** acceptable for v1.0; document the window.

---

### Q11 — `STATEMENT_LIST` cursor pagination

Per phase 17 plan: ships single-frame snapshot with limit cap 1000. Same deferral as `ENTITY_LIST` (Q13 in §28/09).

**Target:** phase 23. **Status:** open.

---

### Q12 — Statement-on-statement (meta-statements)

[`./00_purpose.md`](./00_purpose.md): `StatementObject::Statement(StatementId)` is allowed by the type system — meta-statements like "Statement S1 was authored by Priya."

Open: when meta-statements form cycles (S1 references S2 which references S1), how do reads handle? Phase 17 stores them as-is; cycle detection is a phase-23 query concern.

**Target:** phase 23. **Status:** open. **Likely outcome:** detect and reject cycles at create time in a future hardening pass; until then, query consumers must guard.

---

### Q13 — Preference de-duplication

When `statement_create` for a Preference matches an existing current Preference exactly (same subject, predicate, object, evidence): is it a duplicate (return existing) or a supersession (chain extends with the new)?

Phase 17 implementation: **supersession with new evidence merged**. This is consistent with re-affirmation as a confidence boost.

Alternative: **silent duplicate suppression** — return existing without new chain entry.

**Target:** phase 17.4 — pick during implementation. **Status:** open. **Likely outcome:** supersession (re-affirmation is real signal).

## Resolved

(none yet — §19 backfill is recent)
