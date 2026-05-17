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

### Q9 — Full content-aware memory text rebuild

[`../26_knowledge_storage/01_tantivy_layout.md`](../26_knowledge_storage/01_tantivy_layout.md)
§5 specifies rebuild from authoritative redb tables. Phase 22.6
discovered that `MEMORIES_TABLE` stores `text_size` but not the
text itself (text lives only on the ENCODE wire path + WAL
frames), so v1's `rebuild_memory_text` produces a valid empty
index — operators re-ingest existing memories from their own
source-of-truth. Full content-aware rebuild needs either a WAL
scan or a parallel text-store table.

**Target:** post-v1. **Status:** deferred.

---

### Q10 — Partial WAL replay on shard recovery

[`./02_text_indexer_workers.md`](./02_text_indexer_workers.md)
§6 describes WAL-based replay of unflushed writes at startup.
Phase 22.7 implements only the full-rebuild path on
`IndexStatus::NeedsRebuild`; for `Ready` indexes, the loss
bound is ≤ N-1 writes per indexer at crash (default N=256 per
§26/01 §3). Cursor-tracked partial replay (stamping
`last_indexed_unix_ms` on the tantivy payload, scanning redb
for rows beyond the cursor at startup) is a post-v1 improvement.

**Target:** post-v1. **Status:** deferred.

---

### Q11 — Hot rebuild while live writer is running

[`../26_knowledge_storage/01_tantivy_layout.md`](../26_knowledge_storage/01_tantivy_layout.md)
§5's atomic-rename semantics allow in-flight readers to keep
operating against the old index until they re-open. Phase 22.6
implements startup-only rebuild — no coordination with a live
`IndexWriter`. Hot rebuild (e.g. admin-triggered without
restarting the shard) requires writer pause + drain coordination
that the 22.3 / 22.4 drain loops don't yet support.

**Target:** post-v1. **Status:** deferred.

---

### Q12 — Segment-merge windowing during low-traffic intervals

[`../26_knowledge_storage/01_tantivy_layout.md`](../26_knowledge_storage/01_tantivy_layout.md)
§4 calls out tantivy's segment merge as expensive and notes
v1 relies on `LogMergePolicy` running as part of tantivy's
background merger threads (governed by the shard's I/O budget).
Operators that observe latency hits during merges may want to
window merges into low-traffic intervals.

**Target:** post-v1. **Status:** deferred.

---

### Q13 — Admin rebuild wire op (`ADMIN_TANTIVY_REBUILD`)

Phase 22.6 lands the rebuild functions but the on-demand admin
trigger (operator-facing wire op or CLI subcommand) is admin-
surface scope.

**Target:** §28/05 admin. **Status:** deferred.

## Resolved

- Per-tier dispatch semantics (sync / near-foreground / background)
  — resolved in [`./01_extractor_workers.md`](./01_extractor_workers.md).
- Worker overflow policy — `Drop + audit Skipped(queue full) +
  metric`, resolved in §27/01 §6.
- Text-indexer overflow policy — `Backpressure on foreground` (not
  drop). Resolved in [`./02_text_indexer_workers.md`](./02_text_indexer_workers.md)
  §1 §6 with full justification (lexical recall is correctness, not
  best-effort).
