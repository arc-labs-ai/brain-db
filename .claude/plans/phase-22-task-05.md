# Plan: Phase 22 — Task 05, LexicalRetriever trait + impl

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Read side of the lexical pipeline. Implements the §23/02 surface:
takes a `LexicalQuery` + scope + config, runs BM25 against the
matching per-shard tantivy index, applies filters, and returns
`Vec<RankedItem>` with dense 1-based ranks.

Concrete deliverables:

1. New module `crates/brain-index/src/tantivy_shard/retriever.rs`:
   - `LexicalQuery`, `LexicalFilters`, `LexicalRetrieverConfig`,
     `RankedItem`, `RankedItemId`, `LexicalError`.
   - `LexicalRetriever` trait — object-safe (`dyn`-able), `Send + Sync`.
   - `TantivyLexicalRetriever` struct holding an
     `Arc<TantivyShard>` plus cached `tantivy::IndexReader`s per
     scope.
2. BM25 dispatch:
   - Parse the brain-side `LexicalQuery` into a `tantivy::Query`
     (`BooleanQuery` of `TermQuery`/`PhraseQuery` over the right
     text field).
   - Apply `BM25 k1 / b` overrides via `Searcher::search_with`
     when non-default.
   - Apply filters as `Occur::Must` clauses against the indexed
     metadata fields.
3. Scope dispatch:
   - `LexicalScope::MemoryText` → query `memory_text.tantivy`,
     emit `RankedItemId::Memory(MemoryId)`.
   - `LexicalScope::StatementText` → query
     `statements.tantivy`, emit `RankedItemId::Statement(StatementId)`.
4. Filter validation — a filter targeting a field absent from
   the chosen scope's schema returns `LexicalError::QueryParseFailed`
   (§23/02 §5 binding).
5. Wire into `OpsContext` via a new
   `lexical_retriever: Option<Arc<dyn LexicalRetriever>>` slot,
   installed at shard spawn alongside the 22.3 / 22.4
   dispatchers.
6. Unit tests against an in-memory index — round-trip Upsert
   then retrieve; filter positive/negative; BM25 ordering; empty
   result; `IndexUnavailable` shape.

NOT in scope:
- RECALL wire-op integration (phase 23 — hybrid query owns the
  client-facing path).
- Snippet generation (§23/02 §6 marks optional; v1 returns
  `None`).
- Cross-shard fan-out (phase 23 router).
- Pagination / cursor (post-v1).
- The 22.6 rebuild path (separate sub-task).

## 2. Spec references

- `spec/23_retrievers/02_lexical_retriever.md` — full surface
  binding. §1 trait + types; §2 BM25 defaults; §3 tokenizer
  (already wired in 22.2); §4 scope dispatch; §5 filter shape;
  §6 idempotency + snippet; §7 error taxonomy; §8 perf bounds.
- `spec/26_knowledge_storage/01_tantivy_layout.md` §2 — schema
  field bindings 22.5 reads filters against.
- `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.9 —
  perf targets validated in 22.8.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `tantivy::query::QueryParser` for multi-field text queries | docs.rs/tantivy/0.26.1 | Yes — `QueryParser::for_index(&index, vec![text_field])`. |
| BM25 k1/b override | docs.rs `tantivy::query::BM25SimilarityProvider` | Yes — pluggable similarity provider; `BM25SimilarityProvider::new(k1, b)`. |
| `BooleanQuery` + filter clauses | docs.rs `tantivy::query::{BooleanQuery, Occur}` | Yes — standard combinator. |
| Bytes-field equality (`Term::from_field_bytes`) for `agent_id` filter | docs.rs `tantivy::Term` | Yes; needs `INDEXED` flag — already on the field after 22.3 fix. |
| u64 range queries on `confidence_bucket` / `created_at` / `extracted_at` | docs.rs `tantivy::query::RangeQuery` | Yes — supports `i64`/`u64`/`f64`/`bytes` bounds. |
| `IndexReader::searcher()` returns a snapshot view between commits | docs.rs | Yes — explicit `ReloadPolicy::OnCommit` (default). |

## 4. Architecture sketch

```rust
// crates/brain-index/src/tantivy_shard/retriever.rs

use std::ops::RangeInclusive;
use std::sync::Arc;

use brain_core::{knowledge::StatementKind, AgentId, MemoryId, MemoryKind, StatementId};
use tantivy::{IndexReader, Searcher};

use super::{IndexHandle, LexicalScope, TantivyShard};

pub trait LexicalRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError>;
}

#[derive(Debug, Clone, Default)]
pub struct LexicalQuery {
    /// Free-text terms — joined with OR semantics by the BM25 parser.
    pub terms: Vec<String>,
    /// Each clause is an exact phrase; AND-ed against the terms.
    pub phrase_clauses: Vec<Vec<String>>,
    pub filters: LexicalFilters,
}

#[derive(Debug, Clone, Default)]
pub struct LexicalFilters {
    pub agent_id: Option<AgentId>,
    pub memory_kind: Option<MemoryKind>,
    pub statement_kind: Option<StatementKind>,
    pub predicate_id: Option<u32>,
    pub confidence_bucket: Option<RangeInclusive<u8>>,
    pub created_at_ms: Option<RangeInclusive<u64>>,
    pub extracted_at_ms: Option<RangeInclusive<u64>>,
}

#[derive(Debug, Clone, Copy)]
pub struct LexicalRetrieverConfig {
    pub top_k: usize,
    pub bm25_k1: f32,
    pub bm25_b: f32,
    pub min_score: Option<f32>,
    pub timeout_ms: u32,
}

impl Default for LexicalRetrieverConfig {
    fn default() -> Self {
        Self {
            top_k: 64,
            bm25_k1: 1.2,
            bm25_b: 0.75,
            min_score: None,
            timeout_ms: 50,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedItem {
    pub id: RankedItemId,
    pub rank: u32,
    pub score: f32,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankedItemId {
    Memory(MemoryId),
    Statement(StatementId),
}

#[derive(Debug, thiserror::Error)]
pub enum LexicalError {
    #[error("index unavailable (rebuild in progress or corrupt)")]
    IndexUnavailable,
    #[error("query parse failed: {0}")]
    QueryParseFailed(String),
    #[error("query timed out after {0} ms")]
    Timeout(u32),
    #[error("internal: {0}")]
    Internal(String),
}

pub struct TantivyLexicalRetriever {
    shard: Arc<TantivyShard>,
    memory_reader: IndexReader,
    statements_reader: IndexReader,
}

impl TantivyLexicalRetriever {
    pub fn new(shard: Arc<TantivyShard>) -> Result<Self, LexicalError> {
        let memory_reader = shard
            .memory_text
            .index
            .reader()
            .map_err(|e| LexicalError::Internal(format!("memory reader: {e}")))?;
        let statements_reader = shard
            .statements
            .index
            .reader()
            .map_err(|e| LexicalError::Internal(format!("statements reader: {e}")))?;
        Ok(Self {
            shard,
            memory_reader,
            statements_reader,
        })
    }
}

impl LexicalRetriever for TantivyLexicalRetriever {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        scope: LexicalScope,
        config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        validate(query, scope)?;
        let (handle, reader) = match scope {
            LexicalScope::MemoryText => (&self.shard.memory_text, &self.memory_reader),
            LexicalScope::StatementText => (&self.shard.statements, &self.statements_reader),
        };
        let searcher = reader.searcher();
        let q = build_query(query, handle, scope)?;
        let collector = TopDocs::with_limit(config.top_k).order_by_score();
        let hits = searcher.search(&q, &collector)
            .map_err(|e| LexicalError::Internal(e.to_string()))?;
        project(hits, &searcher, handle, scope, config)
    }
}
```

### Filter → tantivy::Query translation

Each filter applied as an `Occur::Must` clause inside a top-level
`BooleanQuery` that AND-combines the text query and the filters:

| Filter | Mechanism |
|---|---|
| `agent_id` (memory scope) | `TermQuery::new(Term::from_field_bytes(agent_id, &uuid_bytes), IndexRecordOption::Basic)` |
| `memory_kind` (memory scope) | `TermQuery` against u64 `kind` field |
| `statement_kind` (statement scope) | u64 `kind` field |
| `predicate_id` (statement scope) | `TermQuery` against u64 `predicate_id` |
| `confidence_bucket` range | `RangeQuery::new_u64(field, *r.start() as u64, *r.end() as u64)` |
| `created_at_ms` / `extracted_at_ms` range | `RangeQuery::new_u64` against the matching field |

Wrong-scope filter (e.g. `agent_id` set for `StatementText`) →
`LexicalError::QueryParseFailed("agent_id filter applies only to MemoryText")`.

### BM25 k1/b override

If `config.bm25_k1 != 1.2 || config.bm25_b != 0.75`, build a
`Searcher` with a custom `BM25SimilarityProvider::new(k1, b)`
and use `searcher.search_with(&q, &collector, &similarity)`.
Defaults take the standard `searcher.search` path.

### Projection (hits → `Vec<RankedItem>`)

```rust
fn project(
    hits: Vec<(f32, DocAddress)>,
    searcher: &Searcher,
    handle: &IndexHandle,
    scope: LexicalScope,
    config: &LexicalRetrieverConfig,
) -> Result<Vec<RankedItem>, LexicalError> {
    let id_field_name = match scope {
        LexicalScope::MemoryText => "memory_id",
        LexicalScope::StatementText => "statement_id",
    };
    let id_field = handle.index.schema().get_field(id_field_name)?;

    let mut out = Vec::with_capacity(hits.len());
    for (rank0, (score, addr)) in hits.into_iter().enumerate() {
        if let Some(min) = config.min_score {
            if score < min { continue; }
        }
        let doc: TantivyDocument = searcher.doc(addr)?;
        let bytes = doc.get_first(id_field)
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| LexicalError::Internal("doc missing id field".into()))?;
        let bytes_16: [u8; 16] = bytes.try_into()
            .map_err(|_| LexicalError::Internal("id field not 16 bytes".into()))?;
        let id = match scope {
            LexicalScope::MemoryText => {
                RankedItemId::Memory(MemoryId::from_raw(u128::from_be_bytes(bytes_16)))
            }
            LexicalScope::StatementText => {
                RankedItemId::Statement(StatementId::from(bytes_16))
            }
        };
        out.push(RankedItem {
            id,
            rank: (rank0 as u32) + 1,
            score,
            snippet: None,
        });
    }
    Ok(out)
}
```

### OpsContext wiring

```rust
// crates/brain-ops/src/context.rs
pub lexical_retriever: Option<Arc<dyn LexicalRetriever>>,
```

Plus `with_lexical_retriever`. Phase 23's hybrid query reads
through this slot; for v1 nothing else consumes it.

Server-side spawn:

```rust
let lexical_retriever_for_ops = tantivy_for_ops.as_ref().and_then(|shard| {
    match brain_index::TantivyLexicalRetriever::new(shard.clone()) {
        Ok(r) => Some(Arc::new(r) as Arc<dyn brain_index::LexicalRetriever>),
        Err(err) => {
            tracing::error!(target: "brain_server::shard", error = %err,
                "lexical retriever init failed; reads will return IndexUnavailable");
            None
        }
    }
});
// Then `.with_lexical_retriever(lexical_retriever_for_ops)` on the OpsContext builder.
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Retriever in `brain-index` (this plan) | Co-located with schemas + tokenizer; one crate owns the lexical surface | brain-planner consumes a trait from brain-index (already a dep) | ✓ |
| Retriever in `brain-planner` | Planner-side abstraction "feels right" | Forces brain-planner to take a tantivy dep | rejected |
| Retriever in `brain-core` | Pure types | Impl belongs near tantivy; brain-core stays type-only | rejected |
| One `LexicalFilters` struct (this plan) with wrong-scope validation | Single shape for callers | Some fields ignored per scope | ✓ — explicit `QueryParseFailed` keeps errors visible |
| Per-scope filter type (`MemoryFilters` / `StatementFilters`) | No wrong-scope mistakes possible | Doubles the surface; mismatch with §23/02 §5's single `LexicalFilters` shape | rejected |
| Return `Vec<RankedItem<Id>>` generic | Type-safe id per scope | Trait method can't be object-safe with generic return | rejected |
| Cache `Searcher` (not just `IndexReader`) | Fewer per-query allocs | `Searcher::new()` is cheap; tantivy's reload semantics make a long-lived Searcher stale-prone | rejected |

## 6. Risks / open questions

- **Risk:** tantivy's `QueryParser::parse_query` panics on certain malformed inputs. **Mitigation:** wrap in `parse_query_lenient` if available; else map errors to `QueryParseFailed`.
- **Risk:** `RangeQuery::new_u64` API churn between tantivy minor versions. **Mitigation:** pinned at 0.26; if the type moves we adapt in 22.5's commit.
- **Risk:** `Term::from_field_bytes` for `agent_id` requires the field to be `INDEXED` (done in 22.3 fix) AND that the indexer wrote it with the same byte layout. **Mitigation:** 22.3 writes via `add_bytes(agent_id, &uuid_bytes_16)`; retriever uses the same `[u8; 16]` → bytes round-trip.
- **Risk:** A configured `min_score` filters out otherwise-top-k results, leaving fewer than `top_k` items. **Mitigation:** documented behaviour in §23/02 §6 (`top_k applied after filtering`).
- **Open question:** Should the retriever pre-warm the readers eagerly at construction? **Resolution:** `IndexReader::searcher()` is the canonical hot path; constructing readers up front (this plan) avoids per-query reader-acquire cost.
- **Open question:** Phrase-clause semantics — does §23/02 treat `phrase_clauses` as `AND` against `terms`, or as alternates? **Resolution:** AND. Each phrase is a `PhraseQuery::new(...)` joined with `Occur::Must`. Spec patched in lockstep if it's ambiguous.

## 7. Test plan

Unit tests in `crates/brain-index/src/tantivy_shard/retriever/tests.rs`:

- `terms_query_returns_hits` — write 3 docs to memory_text, query for a stemmed term, assert ranks 1..=3 in score order.
- `phrase_query_requires_adjacency` — write `"the quick brown fox"` and `"quick brown"`, query phrase `["quick", "brown"]`, both match; `["brown", "quick"]` (reverse) returns zero.
- `agent_id_filter_includes_matches` — two docs with different `agent_id`; filter selects exactly one.
- `agent_id_filter_on_statement_scope_errors` — same filter on `StatementText` scope returns `QueryParseFailed`.
- `predicate_id_filter_on_memory_scope_errors` — symmetric: predicate filter on memory scope errors.
- `confidence_bucket_range_filter` — 3 statements with confidence 0.2 / 0.5 / 0.85 → buckets 2/5/8; filter `4..=6` returns exactly the middle.
- `min_score_filter_drops_low_hits` — set `min_score = 1.0`; assert hits ≥ that score.
- `empty_result_is_ok_not_error` — query with no matches → `Ok(vec![])`.
- `ranks_are_dense_and_one_based` — top_k = 3, returned items have rank 1, 2, 3.
- `bm25_overrides_are_honored` — query with `bm25_k1 = 5.0` produces different score than default (sanity check — exact magnitudes not asserted, just non-equality).
- `wrong_id_field_size_is_internal_error` — pathological doc with 8-byte memory_id → `Internal`. (Skip if hard to set up; cover via `try_into` failure in a separate unit test.)

Plus a smoke test in `brain-ops::ops::text_indexer::memory::tests` that drives both writer + retriever end-to-end (Upsert via dispatcher → retrieve via `TantivyLexicalRetriever` → assert hit).

## 8. Commit shape

Single commit:

```
feat(index,ops,server): 22.5 — LexicalRetriever trait + tantivy impl

- crates/brain-index/src/tantivy_shard/retriever.rs (new):
  trait + types + TantivyLexicalRetriever impl.
- crates/brain-index/src/tantivy_shard/retriever/tests.rs (new):
  10 unit tests against in-memory indexes.
- crates/brain-index/src/lib.rs: re-export the new surface.
- crates/brain-ops/src/context.rs: `lexical_retriever:
  Option<Arc<dyn LexicalRetriever>>` field + builder.
- crates/brain-server/src/shard/mod.rs: shard spawn constructs
  the retriever from the `TantivyShard` and installs it on
  `OpsContext`.
- crates/brain-ops/src/ops/text_indexer/memory/tests.rs: one
  end-to-end test that exercises Upsert → retrieve.
```

## 9. Confirmation

Please confirm:

1. **Retriever lives in `brain-index`** (vs. `brain-planner` or `brain-core`).
2. **Single `LexicalFilters` struct** with wrong-scope validation returning `QueryParseFailed` (vs. per-scope filter types).
3. **`RankedItemId` tagged enum** for the id field (vs. raw `[u8; 16]` or generic `RankedItem<Id>`).
4. **No snippet in v1** — `RankedItem.snippet` always `None`. Matches §23/02 §6.
5. **`OpsContext.lexical_retriever`** field installed at spawn; phase 23 hybrid query consumes it. Tests pass `None`.

After approval: implement + tests + commit.
