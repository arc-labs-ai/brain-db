# Phase 22: Tantivy / Lexical Retrieval ✓

## Status

**Complete** — tag `phase-22-complete`. Nine sub-tasks (22.0–22.8) landed on `feature/phase-22-tantivy-lexical`. Per-sub-task plans live under [`.claude/plans/phase-22-task-0[0-8].md`](../../.claude/plans/) and capture the trade-offs each one took.

## Goal

Integrate tantivy for BM25 over memory text and statement text. Implement the `LexicalRetriever`. Maintain the per-shard tantivy indexes via the post-commit pipeline. Recover from on-disk corruption / schema-version mismatch at shard startup.

## Prerequisites

- Phase 15 (storage) and 17 (statements) complete.

## Reading list

- [`spec/23_retrievers/02_lexical_retriever.md`](../../spec/23_retrievers/02_lexical_retriever.md) — landed in 22.0.
- [`spec/26_knowledge_storage/01_tantivy_layout.md`](../../spec/26_knowledge_storage/01_tantivy_layout.md) — landed in 22.0.
- [`spec/27_knowledge_workers/02_text_indexer_workers.md`](../../spec/27_knowledge_workers/02_text_indexer_workers.md) — landed in 22.0.
- [`spec/16_benchmarks_acceptance/02_latency_targets.md`](../../spec/16_benchmarks_acceptance/02_latency_targets.md) §2.9 — landed in 22.0.

## Outputs

- [x] tantivy 0.26 integration in `brain-index` (`TantivyShard` per shard).
- [x] Brain tantivy analyzer (URL / code-ID / Porter) registered under `"default"`.
- [x] Memory text indexer worker (`brain-ops::ops::text_indexer::memory`).
- [x] Statement text indexer worker (`brain-ops::ops::text_indexer::statement`).
- [x] `LexicalRetriever` trait + `TantivyLexicalRetriever` impl.
- [x] Atomic-swap rebuild worker (`brain-ops::ops::text_indexer::rebuild`).
- [x] Recovery on shard startup (`brain-server::shard::tantivy_recovery`).
- [ ] **Deferred:** RECALL wire-op integration — phase 23 hybrid query owns the client-facing path.

## Sub-tasks

### 22.0 §23/02 + §26/01 + §27/02 + §16/02 §2.9 spec backfill ✓

**Landed in:** [`.claude/plans/phase-22-task-00.md`](../../.claude/plans/phase-22-task-00.md).
**Done when:** four spec files / amendments at phase-22 implementation depth; phase doc reading list links them.

### 22.1 tantivy dependency + per-shard shard init ✓

**Landed in:** [`.claude/plans/phase-22-task-01.md`](../../.claude/plans/phase-22-task-01.md).
**Done when:** `tantivy = "0.26"` pinned; `TantivyShard::open` opens both indexes, reports `IndexStatus { Ready | NeedsRebuild { reason } }` via the `meta.json` payload check.

### 22.2 Custom tokenizer ✓

**Landed in:** [`.claude/plans/phase-22-task-02.md`](../../.claude/plans/phase-22-task-02.md).
**Done when:** brain analyzer registered under tantivy's `"default"` name; URL / code-ID / dotted-identifier preservation; Porter stemming on residue; NO stop-word filter (§23/02 §3 binding).

### 22.3 MemoryTextIndexer worker ✓

**Landed in:** [`.claude/plans/phase-22-task-03.md`](../../.claude/plans/phase-22-task-03.md).
**Done when:** ENCODE / FORGET dispatch `Upsert` / `Forget` events post-WAL-commit; drain loop owns the `IndexWriter`; group commit cadence (N=256 / T=1 s, env-overridable); retry-once-then-shard-fatal on commit failure.

### 22.4 StatementTextIndexer worker ✓

**Landed in:** [`.claude/plans/phase-22-task-04.md`](../../.claude/plans/phase-22-task-04.md).
**Done when:** STATEMENT_CREATE / SUPERSEDE / TOMBSTONE / RETRACT dispatch through the matching events; text repr `subject.canonical_name + predicate.name + object_text` computed at dispatch site via entity + predicate joins; confidence bucketed to `[0, 9]`.

### 22.5 LexicalRetriever trait + impl ✓

**Landed in:** [`.claude/plans/phase-22-task-05.md`](../../.claude/plans/phase-22-task-05.md).
**Done when:** trait is object-safe, dispatches BM25 via tantivy `QueryParser`; filters AND-joined via `BooleanQuery`; wrong-scope filter returns `QueryParseFailed`; `reader.reload()` per query guarantees §23/02 §6 idempotency.

### 22.6 Index rebuild worker ✓

**Landed in:** [`.claude/plans/phase-22-task-06.md`](../../.claude/plans/phase-22-task-06.md).
**Done when:** atomic-swap rebuild via `std::fs::rename` per §26/01 §5; memory rebuild produces empty valid index (v1 simplification — see "Scope cuts" below); statement rebuild is content-complete via `STATEMENTS_TABLE` + entity / predicate joins; tombstoned + pending-subject + orphan rows skipped.

### 22.7 Recovery on shard startup ✓

**Landed in:** [`.claude/plans/phase-22-task-07.md`](../../.claude/plans/phase-22-task-07.md).
**Done when:** shard spawn detects `IndexStatus::NeedsRebuild` per scope and runs the matching 22.6 fn before the indexer workers spawn; post-rebuild re-open returns `Ready`; failures log + leave the `lexical_retriever` slot empty.

### 22.8 Phase exit ✓

**Landed in:** [`.claude/plans/phase-22-task-08.md`](../../.claude/plans/phase-22-task-08.md).
**Done when:** 2 phase-exit integration tests (ENCODE → retrieve; FORGET → no hit) via the TCP wire path; 3 criterion benches at 10K corpus scale against §16/02 §2.9; ROADMAP + this phase doc updated; `phase-22-complete` tag cut.

## Done-when (phase)

- [x] tantivy indexes built on ENCODE / STATEMENT_CREATE; removed on FORGET / TOMBSTONE / SUPERSEDE.
- [x] `LexicalRetriever::retrieve` returns BM25-ranked items per-scope.
- [x] Restart recovery rebuilds corrupt / version-mismatched indexes.
- [x] Index rebuild path works (atomic-swap + content-complete for statements).
- [ ] **Performance budgets validated at production scale** — phase 14 acceptance suite. v1 phase-22 benches operate at 10K scale (regression detection).

## Scope cuts

| Cut | Where it goes | Reason |
|---|---|---|
| Memory text rebuild full content reconstruction | Post-v1 (§27/07) | `MEMORIES_TABLE` stores `text_size` but not text itself (text only on ENCODE wire path + WAL frames). v1 rebuild produces a valid empty index; operators re-ingest. |
| Partial WAL replay on shard recovery | Post-v1 (§27/07) | Loss bound ≤ N-1 writes per indexer at crash (default N=256) accepted; cursor-tracked replay is a post-v1 improvement. |
| Hot rebuild while live writer is running | Post-v1 | v1 rebuild is startup-only — no live-writer coordination needed. |
| Stop-word removal in tokenizer | v1 explicit cut | Breaks exact-ID queries like `ACME-1247`; BM25's idf demotes high-frequency naturally. |
| BM25 k1 / b custom similarity | Phase 23 if needed | `LexicalRetrieverConfig` exposes fields but retriever uses tantivy defaults. |
| Snippet generation | Post-v1 | `RankedItem.snippet` always `None` in v1. |
| Cross-shard lexical ranking | Phase 23 router | Per-shard retrieval; router fan-out. |
| Segment-merge windowing | Post-v1 | Rely on `LogMergePolicy`. |
| `ADMIN_TANTIVY_REBUILD` wire op | §28/05 admin | Operator-facing rebuild is admin scope. |
| RECALL wire-op integration | Phase 23 hybrid query | Lexical retriever is one of three retrievers in §23/00; phase 23 wires RRF fusion + the client-facing path. |

## Phase exit

- [x] Sub-tasks 22.0–22.8 landed on `feature/phase-22-tantivy-lexical`.
- [x] All scope cuts documented in this file + ROADMAP.
- [x] Workspace `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` green at tag time.
- [x] `cargo clippy --target x86_64-unknown-linux-gnu -p brain-index -p brain-ops -p brain-server --tests -- -D warnings` clean.
- [x] criterion bench harness in place (`crates/brain-index/benches/lexical_retrieve.rs`); wall-time capture deferred to phase-14 acceptance suite (matching the 21.7 precedent).
- [x] Tag `phase-22-complete` cut.

## Pitfalls

- tantivy's commit semantics: too-frequent commits are slow; too-rare loses recent docs on crash. The §26/01 §3 cadence (N=256 OR T=1 s) is the documented v1 trade-off.
- Index size grows; segment merge is expensive. v1 uses `LogMergePolicy`; windowing post-v1.
- Test with non-English characters; the analyzer must not crash (covered by the NFC normalisation test in 22.2).
