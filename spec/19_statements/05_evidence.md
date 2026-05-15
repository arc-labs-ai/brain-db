# 19.05 Evidence

How statements reference the memories / sources they derive from, with overflow handling for high-evidence statements and FORGET cascade for memory deletion.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Schema" — `evidence: EvidenceRef`.
- [`./04_confidence.md`](./04_confidence.md) — confidence aggregates over evidence.
- [`./03_storage.md`](./03_storage.md) §1.6, §1.8 — `STATEMENTS_BY_EVIDENCE_TABLE`, `EVIDENCE_OVERFLOW_TABLE`.
- [`../28_knowledge_wire_protocol/06_statement_frames.md`](../28_knowledge_wire_protocol/06_statement_frames.md) §2.3 — wire shape.

## 1. The model

A statement's `evidence` is a list of pointers to the memories (and metadata) that the statement was derived from. Two variants:

```rust
pub enum EvidenceRef {
    Inline(SmallVec<EvidenceEntry, 8>),    // up to 8 entries
    Overflow(EvidenceOverflowId),          // pointer to evidence_overflow row
}

pub struct EvidenceEntry {
    pub memory_id: MemoryId,
    pub confidence: f32,                   // [0, 1] — source-supplied
    pub timestamp_unix_nanos: u64,         // when observed
    pub extractor_id: u32,                 // 0 for user-authored
}
```

- **Inline** for the common case (most statements derive from 1-5 memories).
- **Overflow** when > 8 evidence entries — pointer indirection to a separate row.

## 2. Why 8

Cap is small enough to fit in one cache line (8 × 24 bytes ≈ 192 bytes) and covers the long tail of common cases. Phase 22's pattern extractor typically produces 1-3 evidence per statement; LLM extractor typically 2-5. Operator-authored statements typically reference 1.

Statements with > 8 evidence are usually:

- Aggregated claims from many memories ("Priya prefers X" backed by 50 conversations).
- Long-lived Preferences that get reaffirmed over time.

For these, overflow is the right path.

## 3. Inline → Overflow promotion

When `statement_create` is called with ≥ 9 evidence entries:

1. Allocate `EvidenceOverflowId` (UUIDv7).
2. Write `EvidenceOverflow { memory_ids: Vec<...>, extractor_ids: Vec<u32>, confidences: Vec<f32>, timestamps: Vec<u64> }` to `EVIDENCE_OVERFLOW_TABLE`.
3. Set `Statement.evidence = EvidenceRef::Overflow(overflow_id)`.

Subsequent reads dereference the pointer transparently — SDK / handler decodes overflow rows into the same `EvidenceEntry` shape callers see for inline.

### 3.1 Add-evidence promotion

A future `STATEMENT_ADD_EVIDENCE` op (not in v1.0) appends new evidence to an existing statement. When the post-append count crosses 8, promote inline → overflow inside the same redb txn. Tracked in [`./06_open_questions.md`](./06_open_questions.md).

For v1.0: evidence is set at create / supersede time only.

## 4. Overflow row shape

```rust
pub struct EvidenceOverflow {
    pub memory_ids: Vec<[u8; 16]>,
    pub extractor_ids: Vec<u32>,
    pub confidences: Vec<f32>,
    pub timestamps: Vec<u64>,
}
```

4 parallel vectors of the same length (one entry across all = one `EvidenceEntry`). Stored as a single redb value; rkyv-archived; `check_bytes` validates on read.

Cap per overflow row: 1000 evidence entries (~32 KB). Above that, statements use **multiple chained overflow rows** — implementation detail in `brain-metadata::statement_ops` (only the first overflow id is stored on the statement; subsequent rows are chained via `next_chunk_id`).

V1.0 supports a single overflow row per statement (up to 1000 entries). Multi-chunk evidence is a phase-22 extension (when bulk extractor backfills create high-evidence claims).

## 5. Reverse index — `STATEMENTS_BY_EVIDENCE`

Per evidence entry on each statement, one row is written to `STATEMENTS_BY_EVIDENCE_TABLE`:

```
key:   (memory_id_bytes: [u8; 16], statement_id_bytes: [u8; 16])
value: ()
```

Inline + overflow contribute equally — when a statement has 50 overflow entries, 50 rows go into the reverse index.

This is what makes FORGET cascade O(K) where K is the number of dependent statements: range-scan `(memory_id, *)`.

## 6. FORGET cascade

When `FORGET memory_id` (substrate opcode `0x0024`) is called:

1. **Hard mode** (the memory is gone, not just marked): for each statement referencing `memory_id`:
   - Look up via `STATEMENTS_BY_EVIDENCE_TABLE` prefix scan.
   - Per dependent statement S:
     - Remove the evidence entry referencing `memory_id` (inline path: rewrite Statement; overflow path: rewrite EvidenceOverflow).
     - Recompute `S.confidence` ([`./04_confidence.md`](./04_confidence.md) §4).
     - If `S.evidence.is_empty()` after removal:
       - Tombstone S with `reason = SourceMemoryForgotten`.
     - Else:
       - Update bucket in `STATEMENTS_BY_PREDICATE_TABLE` if confidence_bucket changed.
   - Remove the reverse-index row from `STATEMENTS_BY_EVIDENCE_TABLE`.

2. **Soft mode** (the memory is tombstoned but not yet reclaimed): same as hard for confidence-recomputation purposes, but the reverse-index row stays so the substrate can replay if the memory is restored within grace.

The cascade runs **inside** the FORGET op's redb txn for atomicity. For memories with > 1000 dependent statements, the cascade batches into multiple txns (rare; tracked in [`./06_open_questions.md`](./06_open_questions.md)).

## 7. Evidence integrity

Every evidence entry's `memory_id` must reference an existing (active or tombstoned) memory at creation time:

```text
For each new evidence at statement_create / supersede:
    if !MEMORY_EXISTS(memory_id):
        return INVALID_ARGUMENT
```

Evidence to forgotten memories is invalid; the cascade in §6 cleans up.

Evidence to memories in OTHER shards is allowed. The cross-shard reverse-index entry lives on the memory's shard (so that shard's FORGET cascade finds the dependency). Phase 17 implements the cross-shard write path via the substrate's existing routing mechanism.

## 8. Evidence vs `extractor_id`

`extractor_id` lives on each `EvidenceEntry` and identifies **which extractor produced this evidence**. Values:

- `0` — user-authored (no extractor; the statement came from an SDK call by an agent or human).
- `≥ 1` — registered extractor id (per [`../22_extractors/`](../22_extractors/)).

The substrate uses `extractor_id` for:

- **Audit** — "which extractor's output drove this claim?"
- **Per-extractor governance** — when an extractor is retracted (`EXTRACTOR_DISABLE`), all its evidence remains but downstream consumers see the `extractor_id` and can filter or down-weight.
- **Confidence calibration** (phase 21+) — different extractors have different reliability profiles; future versions weight by extractor.

## 9. Tests (phase 17 acceptance)

- Inline evidence round-trip: create with 3 evidence; read back 3.
- Overflow promotion: create with 9 evidence; read back 9 (overflow row written + read transparently).
- Mixed: create with 5 evidence, then (future op) add 4 more — must promote.
- Reverse index: after create with N evidence, `STATEMENTS_BY_EVIDENCE` has N rows under each memory_id.
- FORGET cascade: forget M1 referenced by 5 statements; each statement's confidence recomputes; if confidence drops to ε near zero, tombstone fires.
- FORGET cascade with empty-evidence outcome: forget the only-evidence memory; statement tombstones with `SourceMemoryForgotten`.
- Evidence integrity: create with non-existent `memory_id` → `INVALID_ARGUMENT`.
- Cross-shard evidence: statement on shard A, evidence memory on shard B; reverse index row written to shard B.

Test files:

- Unit: `crates/brain-metadata/src/statement_ops.rs::tests` for inline / overflow path.
- Cascade: `crates/brain-server/tests/knowledge_forget_cascade.rs` (Linux-only; needs in-process server for cross-shard).

## 10. Sizing

For a deployment with M statements, average evidence count N:

- Inline path (N ≤ 8): ~24 · N bytes per statement (no overflow row).
- Overflow path (N > 8): ~50 bytes (overflow id + bookkeeping) + 1 row × ~24·N bytes in `EVIDENCE_OVERFLOW_TABLE`.

`STATEMENTS_BY_EVIDENCE_TABLE`: ~32 bytes × N · M total. For 10M statements × 3 avg evidence: ~1 GB.

## 11. Open questions

See [`./06_open_questions.md`](./06_open_questions.md). Notably:

- Multi-chunk overflow (> 1000 evidence per statement).
- `STATEMENT_ADD_EVIDENCE` opcode for post-creation evidence appending.
- Whether the substrate should auto-tombstone statements when evidence count drops to 1 + the remaining evidence has very low confidence.
