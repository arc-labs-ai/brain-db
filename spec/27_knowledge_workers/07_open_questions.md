# 27.07 Open Questions

Worker-specific deferrals. Wire-shape questions live in
[`../28_knowledge_wire_protocol/09_open_questions.md`](../28_knowledge_wire_protocol/09_open_questions.md);
extractor-specific in
[`../22_extractors/07_open_questions.md`](../22_extractors/07_open_questions.md).

## Active

### Q1 — Decay sweeper

[`./00_purpose.md`](./00_purpose.md) lists a "Supersession sweeper"
running periodically at low priority. Statement / relation decay
under §17/04's noisy-OR aggregation needs a sweeper that recomputes
confidence on long-stale rows. Phase 20 doesn't implement this.

**Target:** phase 22+. **Status:** deferred.

---

### Q2 — Resolution workers

Tier-2 (alias) and tier-3 (trigram) entity resolution are
synchronous in v1 (phase 16.5). Heavy-load deployments may want
near-foreground tiers. Workers would queue mention-resolution work
behind a backpressure gate.

**Target:** post-v1. **Status:** deferred.

---

### Q3 — FORGET cascade worker

§25/00 §"Cascading effects of FORGET" describes the cascade. v1
substrate (phase 7) implemented the substrate-side cascade; the
knowledge-layer cascade (statements / relations / entity_mentions
referencing the forgotten memory) lands in a phase 22+ worker.

**Target:** phase 22+. **Status:** deferred.

---

### Q4 — Audit log sweeper

[`../22_extractors/05_audit.md`](../22_extractors/05_audit.md) §5
specifies 90-day default retention. The sweeper itself (periodic
worker that deletes rows + index entries older than the cutoff)
lands post-phase-20.

**Target:** phase 22+. **Status:** deferred.

---

### Q5 — Adaptive throttling

[`./01_extractor_workers.md`](./01_extractor_workers.md) §6 — phase
20 ships static queue capacities. Adaptive throttling that lowers
the dispatch rate when queue depth crosses a threshold (rather
than dropping) is a possible v2 improvement.

**Target:** post-v1. **Status:** deferred.

---

### Q6 — Cross-shard worker coordination

Each shard runs its own worker queues. A noisy classifier on shard
0 doesn't push back against shard 5's load. Some workloads might
want cluster-wide work-stealing.

**Target:** post-v1. **Status:** deferred.

---

### Q7 — Queue persistence across restart

[`./00_purpose.md`](./00_purpose.md) §"Graceful shutdown" mentions
persisting queue state to disk on shutdown. Phase 20 doesn't
implement persistence — restarts lose in-flight items (which then
trigger fresh extraction on the next ENCODE because the audit
probe misses).

**Target:** phase 22+. **Status:** deferred.

---

### Q8 — Schema migration worker

§00 lists a "Schema migration" worker triggered on schema update.
v1 has no migration (phase 19 explicit scope cut, §21/07 Q3); this
worker stays as a 1-line placeholder.

**Target:** post-v1. **Status:** deferred.

## Resolved

- Per-tier dispatch semantics (sync / near-foreground / background)
  — resolved in [`./01_extractor_workers.md`](./01_extractor_workers.md).
- Worker overflow policy — `Drop + audit Skipped(queue full) +
  metric`, resolved in §27/01 §6.
