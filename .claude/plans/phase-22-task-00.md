# Plan: Phase 22 — Task 00, Spec backfill (lexical retrieval / tantivy)

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1 (one `docs(spec): 22.0 — ...` commit)

---

## 1. Scope

Bring §23 / §26 / §27 / §16-02 to phase-22 implementation depth so the remaining 22.1–22.8 sub-tasks can cite concrete §N.M anchors instead of inventing layout, schemas, and perf targets at code time.

This sub-task is **spec + plan only**. No Rust code, no crate edits, no test changes. It mirrors the 20.0 (`73c99fe`) and 21.0 (`96b68ff`) precedent — last touch before implementation starts.

Also writes the phase-22 master plan (`.claude/plans/phase-22.md`) that indexes the eight implementation sub-tasks and records scope cuts up front.

**Not in scope (deferred to per-sub-task plans):**
- Cargo dependency choices and exact tantivy crate version (22.1 plan).
- Tokenizer regex / Porter stemmer crate selection (22.2 plan).
- Worker queue plumbing into `OpsContext` / `brain-workers` (22.3 + 22.4 plans).
- `LexicalRetriever` trait surface inside `brain-core` vs. `brain-planner` (22.5 plan).

## 2. Spec references — current state vs. needed depth

### `spec/23_retrievers/`

| File | Present? | Notes |
|---|---|---|
| `00_purpose.md` | yes (181 LOC) | LexicalRetriever gets ~15 lines — BM25 params, tokenizer one-liner, two-index split. |
| `01_rrf_fusion.md` | yes | Phase 23 territory. Reference only. |
| `02_lexical_retriever.md` | **missing — new** | Full LexicalRetriever mechanics. |

Binding constraints already in `00_purpose.md`:
> "BM25 parameters: `k1 = 1.2`, `b = 0.75` (tantivy defaults; configurable)."
> "Lowercase, English stemming (Porter or Snowball). Sublanguage tokens preserved (URLs, IDs like 'ACME-1247', code identifiers)."

### `spec/26_knowledge_storage/`

| File | Present? | Notes |
|---|---|---|
| `00_purpose.md` | yes (179 LOC) | Per-shard tantivy gets ~40 lines — directory layout + field list per index. No commit cadence, segment-merge policy, recovery contract, or atomic-rebuild rules. |
| `01_tantivy_layout.md` | **missing — new** | Full storage discipline. |

Binding constraints already in `00_purpose.md`:
> `memory_text.tantivy/` fields: `text` (TEXT), `agent_id` (STRING), `kind` (STRING), `created_at` (DATE), `memory_id` (STORED).
> `statements.tantivy/` fields: `subject_name` (TEXT), `predicate_name` (STRING), `object_text` (TEXT), `kind` (STRING), `confidence_bucket` (INT bucketed in 0.1), `extracted_at` (DATE), `statement_id` (STORED).
> Derived; rebuildable from authoritative redb tables.

### `spec/27_knowledge_workers/`

| File | Present? | Notes |
|---|---|---|
| `00_purpose.md` | yes (182 LOC) | Memory + statement text indexers appear in the worker table only (rows 15–16: "Near-foreground", "Bounded queue"). No body. |
| `01_extractor_workers.md` | yes | Phase 20–21 territory. Reference only. |
| `02_text_indexer_workers.md` | **missing — new** | Full text-indexer worker spec. |
| `07_open_questions.md` | yes | Track unresolved items. |

Binding constraints already in `00_purpose.md`:
> Tantivy indexer: deterministic by record content.
> "Near-foreground" priority lane (25% of shard time).
> Persisted queues handle pending tantivy commits.

### `spec/16_benchmarks_acceptance/02_latency_targets.md`

Currently terminates at §2.8 (LLM extractor — phase-21 backfill). Phase doc 22.8 wants `P50 ≤ 10 ms @ 100K memories`; no §2.9 entry exists yet. Phase-gate table (§3) currently lands phase 21 in §2.8 and stops.

## 3. External validation

| Item | URL | Why we need it |
|---|---|---|
| tantivy current version on crates.io | https://crates.io/crates/tantivy | Confirm the major version we're pinning the spec to — phase doc cites "0.21+" but tantivy may have moved. Pin the exact major in §26/01 §2 so 22.1 doesn't bikeshed. |
| tantivy schema / indexing semantics | https://docs.rs/tantivy/latest/tantivy/schema/ | Confirm `TEXT` / `STRING` / `DATE` / `INT` / `STORED` semantics match how `00_purpose.md` describes the field set. |
| tantivy commit/segment policy | https://docs.rs/tantivy/latest/tantivy/struct.IndexWriter.html | Inform §26/01 §3 commit cadence (every N writes or T seconds — match substrate WAL discipline). |
| Porter stemmer (rust-stemmers) | https://crates.io/crates/rust-stemmers | Confirm Porter English availability for §23/02 tokenizer. |

These are confirmations, not architectural decisions — the spec text will pin behavior, not crate versions; crate selection happens in the 22.1 plan.

## 4. Architecture sketch — spec files to be written

### `spec/23_retrievers/02_lexical_retriever.md` (~220 LOC)

```
§1 Surface
  - pub trait LexicalRetriever (object-safe) — retrieve(query, scope, config)
  - LexicalQuery { terms, phrase_clauses, filters }
  - LexicalScope = MemoryText | StatementText
  - LexicalRetrieverConfig { top_k, bm25_k1, bm25_b, min_score, timeout_ms }
  - Returns Vec<RankedItem { id, rank, score, snippet }>

§2 BM25 parameters
  - k1 = 1.2, b = 0.75 (defaults). Configurable per call.
  - Score scale is internal to lexical; rank is the cross-retriever currency.

§3 Tokenizer pipeline (binding for §26/01 + 22.2 impl)
  1. Unicode normalize (NFC).
  2. Lowercase.
  3. Split on whitespace + punctuation, with TWO preserved sublanguages:
     - URL tokens (regex \bhttps?://\S+).
     - Code identifiers (regex matching `[A-Z]+-\d+` like ACME-1247, plus underscore-or-dot-joined idents).
  4. Stop-word removal? — NO in v1 (BM25 idf demotes them naturally; stop words break exact-id queries).
  5. Porter / English stemming.

§4 Scope dispatch
  - MemoryText → memory_text.tantivy. Returns RankedItem { id: MemoryId, ... }.
  - StatementText → statements.tantivy. Returns RankedItem { id: StatementId, ... }.
  - No cross-scope queries in v1; router (§23/00) issues two retriever calls for hybrid.

§5 Filters (binding for §26/01 field set)
  - agent_id (exact, memory scope only)
  - kind (exact, both scopes)
  - created_at / extracted_at range
  - confidence_bucket range (statement scope; bucketed 0.1 increments)

§6 Result shape + idempotency
  - retrieve() is read-only; no side effects.
  - Two calls with identical (query, scope, config) return identical results between commits.

§7 Errors
  - IndexUnavailable (during atomic-swap rebuild — §26/01 §5).
  - QueryParseFailed.
  - Timeout.
  - Empty result = Ok(vec![]), not an error.

§8 Performance bound
  - Pin §16/02 §2.9 references (P50 ≤ 10 ms, P99 ≤ 50 ms @ 100K memories / 1M statements).
```

### `spec/26_knowledge_storage/01_tantivy_layout.md` (~250 LOC)

```
§1 Per-shard directory layout
  data/
    shards/000/
      memory_text.tantivy/    (managed by tantivy IndexWriter)
      statements.tantivy/
      memory_text.tantivy.rebuild/  (atomic-swap staging — §5)
      statements.tantivy.rebuild/

§2 Schemas
  - memory_text.tantivy: full field list with TEXT/STRING/DATE/STORED bindings
    (copies and pins what §26/00 lines 59–66 list).
  - statements.tantivy: full field list with bucketed confidence (§26/00 lines 70–79).
  - Schema version stored in tantivy meta.json; mismatch → rebuild.

§3 Commit cadence (binding for 22.3 / 22.4)
  - Group commit every N writes OR T seconds, whichever first.
  - Defaults: N = 256, T = 1 s.
  - Match substrate WAL group-commit discipline (§05/03): the indexer worker
    receives writes via a Glommio channel from the shard's commit pipeline; it
    drives tantivy IndexWriter.commit() on the cadence above.
  - Loss bound: at most N-1 writes are unindexed at a shard crash (recovered
    via §5 rebuild from authoritative redb).

§4 Segment merge
  - tantivy's default merge policy (LogMergePolicy).
  - Schedule merges in low-traffic windows? — out of scope for v1; rely on
    LogMergePolicy + the substrate's I/O budget.

§5 Rebuild from authoritative state
  - Trigger: admin command, startup with corrupt index, or version mismatch.
  - Build into `<index>.rebuild/` next to the live index.
  - On completion, atomic rename swap. Readers using the old index keep
    operating; new readers pick up the new index via the standard tantivy
    Index::open lookup.
  - Source of truth for memory_text: redb MEMORIES_TABLE (text column).
  - Source of truth for statements: redb STATEMENTS_TABLE + entity canonical_name + predicate name (computed at rebuild time).

§6 Recovery on startup
  - On shard start: try Index::open(memory_text.tantivy).
    - Ok → ready, replay any unflushed WAL writes from the shard's commit log.
    - Err (corrupt / version mismatch / missing) → schedule §5 rebuild;
      reads on this scope return IndexUnavailable until done.

§7 Size budgets (informational)
  - Memory tantivy: ~500 MB @ 1M memories (matches §26/00 line 173).
  - Statement tantivy: ~100 MB @ 1M statements.

§8 No mmap discipline required
  - tantivy manages its own files; the substrate's arena/WAL discipline
    (§05/02) doesn't apply.
```

### `spec/27_knowledge_workers/02_text_indexer_workers.md` (~200 LOC)

```
§1 Two text-indexer workers
  - MemoryTextIndexer — trigger: ENCODE commit (memory has text).
  - StatementTextIndexer — trigger: statement_create / supersede / tombstone.
  - Both run near-foreground (25% shard CPU lane per §27/00).
  - Both use bounded queues (default 4096) with overflow = backpressure on
    the calling op (not drop — text indexing isn't best-effort, unlike LLM).

§2 Memory text indexer
  - Receives MemoryId + text snapshot from the post-commit pipeline.
  - Adds doc { text, agent_id, kind, created_at, memory_id } to
    memory_text.tantivy via IndexWriter.add_document.
  - On FORGET: IndexWriter.delete_term(memory_id).
  - Idempotency: re-indexing the same memory_id is a delete-then-add;
    safe at WAL replay.

§3 Statement text indexer
  - Receives StatementId on create / supersede / tombstone.
  - Computes the text repr at index time:
      subject.canonical_name + " " + predicate.name + " " + object_text
  - On supersede / tombstone: delete_term(statement_id); a fresh statement
    is indexed separately if the supersede creates a new statement_id.

§4 Commit policy
  - Indexer worker batches writes; flushes via IndexWriter.commit() on the
    cadence pinned in §26/01 §3.
  - On commit failure: retry once, then fail the shard with a fatal alert
    (text indexing is required correctness, not best-effort).

§5 WAL integration
  - Indexer is downstream of the WAL — the shard's commit pipeline emits
    "indexable" events only after WAL fsync.
  - On shard restart, the pipeline replays any post-WAL/pre-tantivy-commit
    writes by reading the WAL tail; the indexer dedupes via delete-then-add.

§6 Backpressure
  - Queue full → calling op blocks (await on the channel send).
  - This is the only worker class that backpressures the foreground (others
    drop with a metric). Justification: lexical recall is a correctness
    property of hybrid query; silent index drift is unacceptable.

§7 Coordination with extractors (phase 20–21)
  - Text indexer is independent of extractors — it indexes raw memory text,
    not extractor outputs.
  - Order is: WAL fsync → pattern + classifier extractors (phase 20) → LLM
    extractor (phase 21) → memory text indexer → statement text indexer
    (only if extractors created statements).
  - Each is a separate shard-local queue; failures don't cascade.
```

### `spec/16_benchmarks_acceptance/02_latency_targets.md` (+§2.9, ~30 LOC inline)

```
§2.9 LexicalRetriever
  Scope                  | p50    | p99    | Notes
  ---------------------- | ------ | ------ | -------------
  Memory @ 100K          | 10 ms  | 50 ms  | Single-term query
  Memory @ 100K          | 15 ms  | 70 ms  | Multi-term + filter
  Statement @ 1M         | 10 ms  | 50 ms  | Single-term
  Statement @ 1M         | 15 ms  | 70 ms  | Multi-term + filter
  Index commit (256 docs) | 5 ms   | 25 ms  | Group commit cadence

  Backed by the phase-22 §22.8 acceptance test.
```

Plus the existing §3 phase-gate table grows one row: "Phase 22 (sub-task 22.8) — §2.9 LexicalRetriever targets at 100K / 1M scale."

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Backfill before impl (this plan) | Implementation tasks cite concrete §N.M; review surface is small; matches 20.0 / 21.0 precedent | One extra commit before code lands | ✓ |
| Skip backfill, decide layout in 22.1 | One less commit; "fast start" | Re-litigates 4 design questions per sub-task; tantivy schema gets argued in PR review; future readers can't tell what's normative | rejected — repeated this pattern in 20 + 21 and it paid off |
| Combine §23/02 + §26/01 + §27/02 into one super-file | Cross-references stay local | Breaks the per-section convention; readers can't find "tantivy storage" without context | rejected |
| Defer §16/02 §2.9 to 22.8's plan | Less churn here | The Done-when in phase doc 22.8 cites a target that doesn't yet exist in the spec | rejected — the gate must be in the spec at the start of phase, not invented at exit |

## 6. Risks / open questions

- **Risk:** tantivy major-version change (0.21 → 0.22+) breaks the API surface we pin in §26/01. **Mitigation:** §26/01 pins behaviors (commit cadence, schema field types), not the crate API; the 22.1 plan picks the version and is responsible for adapting.
- **Risk:** Stop-word policy debate. v1 says NO stop-word removal — but BM25 over high-frequency terms can be slow. **Resolution:** §23/02 §3 records the decision and the reason (preserves exact-ID queries); revisit post-v1 if perf is a problem.
- **Risk:** Backpressure-on-foreground for text indexer differs from every other knowledge-layer worker (which drops on overflow). **Resolution:** §27/02 §6 calls this out explicitly and justifies; reviewers see it can't be missed.
- **Open question:** Cross-shard ranking (lexical retrieval is per-shard; hybrid query plans across shards). Deferred to phase 23 (§24 hybrid query); §23/02 §4 notes "v1 is per-shard, fan-out done by the router."
- **Open question:** Snippet generation in `RankedItem.snippet` — tantivy supports it, but is it required for v1? **Resolution:** §23/02 §1 marks it optional; concrete answer in 22.5 plan.

## 7. Test plan

This sub-task ships spec only — no executable tests. The verification gate is editorial:

- [ ] Each new spec file builds the cross-references it claims (every `§N/M §X` link resolves).
- [ ] The phase-22 doc (`docs/phases/phase-22-tantivy-lexical.md`) reading list updated to cite the new files so 22.1–22.8 readers land on the right section.
- [ ] §22/07, §23/07 (if new), §26/07 (if new), §27/07 open-question files updated with the questions §22.0 declared resolved or explicitly deferred.
- [ ] Phase-22 master plan (`.claude/plans/phase-22.md`) exists and indexes 22.0–22.8 (or 22.0–22.N with scope cuts marked).

The actual proof-of-correctness lives in the per-sub-task plans that follow and in the implementation sub-tasks themselves.

## 8. Commit shape

Single commit:

```
docs(spec): §23/02 + §26/01 + §27/02 + perf targets + plan (22.0)

Phase 22.0 spec backfill — brings the lexical retriever / tantivy
storage / text-indexer workers to phase-22 implementation depth
alongside the §23/01 RRF fusion file.

spec/23_retrievers/02_lexical_retriever.md (new):
  ...

spec/26_knowledge_storage/01_tantivy_layout.md (new):
  ...

spec/27_knowledge_workers/02_text_indexer_workers.md (new):
  ...

spec/16_benchmarks_acceptance/02_latency_targets.md:
  §2.9 LexicalRetriever targets at 100K / 1M. Phase gate row added.

.claude/plans/phase-22.md (new):
  Master plan for phase 22 — 9 sub-tasks (22.0 spec backfill →
  22.8 phase exit), scope cuts (cross-shard ranking deferred to
  §24, no stop-word removal in v1, segment-merge windowing
  post-v1).
```

## 9. Confirmation

Please confirm:
1. The four spec files (§23/02, §26/01, §27/02, §16/02 §2.9) are the right surface to backfill. Want anything added (e.g., a §28 wire-op file for admin rebuild) or removed?
2. The trade-off resolutions in §6 (no stop-word removal; backpressure-on-foreground for text indexer) are the calls you want me to make in the spec.
3. The single-commit shape matches your preference (20.0 and 21.0 were each single commits).

After approval, I write the four spec files + phase-22 master plan, run a final spec-link sanity pass, and commit on `feature/phase-22-tantivy-lexical`.
