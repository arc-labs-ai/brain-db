# Plan: Phase 22 — Tantivy / Lexical retrieval

**Status:** awaiting-confirmation (master plan; per-sub-task plans land 22.1+)
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 9 (one per sub-task 22.0–22.8)

---

## 1. Scope

Light up the lexical-retrieval tier of the knowledge layer:

- Per-shard tantivy indexes for memory text and statement text (§26/01).
- Two near-foreground workers that maintain those indexes on
  ENCODE / statement_create / supersede / tombstone / FORGET
  (§27/02).
- A `LexicalRetriever` trait + impl that runs BM25 queries with
  filters and returns ranked items (§23/02).
- Rebuild + recovery paths anchored in the authoritative redb
  tables (§26/01 §5–§6).
- Acceptance benches that hit §16/02 §2.9 targets at 100K
  memories / 1M statements.

Phase 22 does NOT:
- Implement hybrid query (RRF fusion across retrievers) — that's
  phase 23 (§24).
- Implement cross-shard ranking — also phase 23 (router fan-out).
- Add admin wire ops for rebuild trigger — phase 22.6 lands the
  worker; the wire op is an admin concern parked for §28/05.

## 2. Spec anchors (post 22.0)

| Section | Status after 22.0 |
|---|---|
| `spec/23_retrievers/02_lexical_retriever.md` | ✓ written |
| `spec/26_knowledge_storage/01_tantivy_layout.md` | ✓ written |
| `spec/27_knowledge_workers/02_text_indexer_workers.md` | ✓ written |
| `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.9 | ✓ amended |

## 3. Sub-tasks

| # | Title | Plan | Crates |
|---|---|---|---|
| 22.0 | §23/02 + §26/01 + §27/02 + perf-targets spec backfill | `phase-22-task-00.md` (✓ awaiting approval) | spec + plans only |
| 22.1 | tantivy dependency + shard init (open / version-check / schedule rebuild) | `phase-22-task-01.md` (to draft) | `brain-index` |
| 22.2 | Custom tokenizer (URL / code-ID preservation + Porter) | `phase-22-task-02.md` | `brain-index` |
| 22.3 | `MemoryTextIndexer` worker + post-commit hook | `phase-22-task-03.md` | `brain-workers`, `brain-ops` |
| 22.4 | `StatementTextIndexer` worker + statement-op post-commit hook | `phase-22-task-04.md` | `brain-workers`, `brain-ops` |
| 22.5 | `LexicalRetriever` trait + impl (BM25 + filters + scope dispatch) | `phase-22-task-05.md` | `brain-planner` (or `brain-core`, decided in 22.5 plan) |
| 22.6 | Rebuild worker + atomic-swap path | `phase-22-task-06.md` | `brain-workers` |
| 22.7 | Recovery on shard startup (open / WAL replay / fallback to rebuild) | `phase-22-task-07.md` | `brain-server` |
| 22.8 | Integration tests + criterion benches + phase exit + tag `phase-22-complete` | `phase-22-task-08.md` | tests + ROADMAP + tag |

## 4. Scope cuts

| Cut | Where it goes | Reason |
|---|---|---|
| Cross-shard lexical ranking | Phase 23 (§24 router) | Lexical retrieval is per-shard; the router fans out and merges. |
| Stop-word removal in tokenizer | Post-v1 | Breaks exact-ID queries like `ACME-1247`; BM25 idf demotes naturally. |
| Snippet generation | Optional in v1 (§23/02 §6) | tantivy's `SnippetGenerator` is available; phase 22.5 decides whether to populate the field. |
| Segment-merge windowing | Post-v1 | Rely on tantivy's `LogMergePolicy`; revisit if I/O budget gets squeezed. |
| Admin wire ops for rebuild trigger | Phase 28/05 admin | The worker exists in 22.6; CLI / wire surface is admin scope. |
| Schema migration on `brain_schema_version` change | Auto-rebuild (§26/01 §2) | v1 has no schema migration framework (§21/07 Q3 + §27/07 Q8); mismatch = rebuild. |
| `BRAIN_TANTIVY_*` env-var hot reload | Post-v1 | Cadence params read at shard spawn only. |

## 5. Risks

| Risk | Mitigation |
|---|---|
| tantivy major-version churn breaks 22.1 wiring | §26/01 pins behaviors not API; 22.1 plan picks the version. |
| Backpressure-on-foreground for text indexer impacts ENCODE P99 | §16/02 §2.1 ENCODE budget (20 ms) leaves headroom; a 50 µs `add_document` × backpressured wait is negligible at queue depth < 4096. 22.3 measures. |
| Rebuild during heavy write load consumes shard I/O | §26/01 §5 step 4 commits on the standard cadence; the substrate's I/O budget caps the rebuild. Operators can scale shards or rebuild during off-peak. |
| WAL replay creates double-index entries | `delete_term + add_document` is idempotent (§27/02 §2 + §6). |

## 6. Verification gate (phase exit, 22.8)

- All 22.1–22.7 commits land on `feature/phase-22-tantivy-lexical`.
- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- `just docker cargo test --workspace --lib --bins` green.
- `cargo bench -p brain-index --bench lexical_retrieve -- --quick`
  meets §16/02 §2.9 targets (or the gap is explicitly recorded).
- Tag `phase-22-complete` (annotated).

## 7. Tagging discipline

Each sub-task: one commit, descriptive message. No squashing,
no merge commits, no `Co-Authored-By` trailer. Phase-exit tag
is annotated and points to the 22.8 commit.

---

After 22.0 is approved and committed, I draft `phase-22-task-01.md`
for the tantivy dependency / shard init sub-task.
