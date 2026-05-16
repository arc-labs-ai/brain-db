# 20.05 Evidence

How a relation references the memories it was derived from. Simpler
than statement evidence (§19/05) — flat `Vec<MemoryId>` only, no
per-entry metadata, no overflow.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Schema" — `evidence:
  Vec<MemoryId>`.
- [`./03_storage.md`](./03_storage.md) §1.4 — reverse-index table.
- [`../19_statements/05_evidence.md`](../19_statements/05_evidence.md)
  — richer statement evidence model that relations may adopt
  post-v1.0.

## 1. The model

```rust
struct Relation {
    // ...
    pub evidence_inline: Vec<MemoryId>,  // flat list
    // ...
}
```

No per-entry confidence. No per-entry timestamp. No overflow row.
A relation cites the memories that support it; relevance + recency
are properties of the relation itself, not of individual evidence
entries.

The relation's overall `confidence` (top-level f32) reflects the
caller's certainty across all evidence, computed externally
(extractor combines source confidences; phase 22 path).

## 2. Why simpler than statements

Statements (§19/05) carry per-entry metadata because:

- Multiple extractors may contribute independent evidence for the
  same Fact / Preference / Event over time.
- The noisy-OR aggregation (§19/04) needs per-entry confidences.

Relations are typically single-extraction:

- One extractor sees "Priya manages Bob" in a memory and creates
  the relation.
- Subsequent extractions trigger supersede (a new version), not
  evidence accumulation on the existing row.
- Confidence comes from the relation type's extraction precision,
  not from aggregated votes.

If a relation gains more support over time, the typical pattern is:

- Existing relation continues to be the current version.
- New extractions of the same `(from, type, to)` are dropped at the
  cardinality / dedup gate (ManyToMany may store duplicates, but
  the schema designer typically uses a different cardinality for
  reinforced edges).

If per-entry metadata becomes load-bearing later, the wire shape +
storage shape evolve to the statement-style overflow path. Phase 22
extractor work will revisit; v1.0 ships the flat shape.

## 3. Evidence cap

`evidence_inline` is uncapped at the storage layer but capped at the
wire layer:

- `RelationCreateRequest.evidence` is `Vec<[u8; 16]>` with a soft
  cap of 32 entries (spec §28/07 §3.4). Beyond that, the caller
  splits the relation creation into multiple supersession steps
  (each step gaining a few more evidence entries).
- Realistic relation evidence sets are ≤ 5 memories.

The redb row stores all entries verbatim. The reverse index
(§03 §1.4) writes one row per evidence entry. If evidence ever
needs to scale beyond ~32, phase 22 work introduces an overflow
table analogous to `EVIDENCE_OVERFLOW` for statements.

## 4. Reverse index population

For every memory in `evidence_inline`, `relation_create` writes:

```text
RELATIONS_BY_EVIDENCE_TABLE.insert((mem_id, relation_id_bytes), ())
```

`relation_supersede` follows the same pattern for the new relation;
the old row's reverse-index entries are **preserved** (the old
relation still cites those memories).

`relation_tombstone` preserves reverse-index entries as well —
audit / FORGET-cascade queries need them.

## 5. FORGET cascade

When a memory is forgotten (substrate `FORGET` op runs), the
FORGET worker queries `RELATIONS_BY_EVIDENCE_TABLE` at `(mem, *)`
and finds every relation citing it.

For each affected relation:

```text
if relation.tombstoned:
    // Already gone; FORGET removes the reverse index entry only.
    delete RELATIONS_BY_EVIDENCE row
    continue

remaining_evidence = relation.evidence_inline.filter(|m| m != forgotten_mem)
if remaining_evidence.is_empty():
    // No evidence left → relation has no support.
    // v1.0: tombstone the relation with reason = SourceMemoryForgotten.
    relation_tombstone(wtxn, relation.id, now)
else:
    // Some evidence remains; rewrite the relation with reduced list.
    relation.evidence_inline = remaining_evidence
    rewrite relation.

delete RELATIONS_BY_EVIDENCE row for (forgotten_mem, relation_id)
```

The cascade runs in a single redb txn per shard. Cross-shard
cascade (when the relation lives on shard A and the forgotten
memory on shard B) uses the same routing as substrate cross-shard
edges (phase 9.11+); phase 18 handles same-shard only.

Phase 21+ adds the FORGET cascade worker that calls this path
automatically; phase 18 exposes the entry point but doesn't wire
the worker.

## 6. Auto-tombstone discretion

§5's "no evidence remaining → tombstone" rule is the v1.0 default.
Operators wanting a more conservative policy (e.g., preserve the
relation as a low-confidence claim) can disable via deployment
config — tracked in [`./06_open_questions.md`](./06_open_questions.md)
Q8.

## 7. Tests

Phase 18.4 verifies:

- Reverse index populated on create.
- Reverse index preserved through supersede.
- Reverse index preserved through tombstone.
- FORGET cascade with all-evidence-gone → relation tombstoned.
- FORGET cascade with partial evidence → row rewrites with reduced
  list, `valid_to_unix_nanos` unchanged.
- Cross-shard cascade: documented as same-shard only in v1.
