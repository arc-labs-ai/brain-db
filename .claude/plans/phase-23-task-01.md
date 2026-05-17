# Plan: Phase 23 — Task 01, SemanticRetriever impl

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Materialise the §23/03 surface. Implement `SemanticRetriever`
over the substrate memory HNSW (already wired through
`ExecutorContext.index: SharedHnsw<384>`) plus the statement
HNSW (declared in `brain-index` since phase 17.5 but not yet
plumbed through `OpsContext` — this sub-task adds the slot
and tolerates `None` for v1).

Concrete deliverables:

1. New module `crates/brain-index/src/semantic_retriever.rs`:
   - Trait `SemanticRetriever` (object-safe, `Send + Sync`).
   - Types: `SemanticQuery { Vector | Text }`, `SemanticScope`,
     `SemanticFilters`, `SemanticRetrieverConfig`, `SemanticError`.
   - Impl `BrainSemanticRetriever` that holds:
     - `Arc<dyn Dispatcher>` (the embedder, for the Text path).
     - `SharedHnsw<384>` (memory HNSW reader handle).
     - `Option<Arc<RwLock<StatementHnswIndex>>>` (None until the
       statement-embedding worker is brought up in a later
       sub-task; v1 returns `Ok(vec![])` when the scope is
       `Statement` or `Both` and the slot is None).
     - `Arc<Mutex<MetadataDb>>` for filter-side metadata reads.
2. Push-down filtering for memory scope via the HNSW
   `filter: F where F: Fn(MemoryId) -> bool` callback already
   exposed by `HnswIndex::search`. Filters that need a redb
   read (e.g. agent_id, kind) go through a closure that opens
   a read-txn once and queries the `MemoryMetadata` row per
   candidate. Cheap because the closure is per-candidate, not
   per-iteration.
3. Statement scope: `StatementHnswIndex::search` doesn't take
   a filter callback in v1, so apply filters **post-search**.
   Document the gap in code + reference §23/03 §5.
4. Both scope: fan out the two searches in parallel via
   `futures::join!` (already a workspace dep), merge results
   by descending cosine, re-rank dense 1-based.
5. New `OpsContext.semantic_retriever: Option<Arc<dyn SemanticRetriever>>`
   slot + `with_semantic_retriever` builder; shard spawn
   constructs the retriever after `ExecutorContext` is built.
6. Unit tests over an in-memory `SharedHnsw` + a fresh
   `MetadataDb` for filter joins.

NOT in scope:
- Statement-embedding worker bring-up — wired in a separate
  follow-up; v1 statement scope returns `Ok(vec![])` when the
  handle is `None`.
- Wiring brain-embed's `EmbedderFingerprintMismatch` check
  — needs an additional API on the embedder for "what model
  was the corpus indexed with"; punt until 23.7's planner
  needs it for cost estimation. v1 trusts the operator.
- Schema-change re-embedding (§23/03 §9).
- Snippet generation — always `None` per §23/03 §6.
- Cross-shard fan-out — router scope.

## 2. Spec references

- `spec/23_retrievers/03_semantic_retriever.md` (just landed
  in 23.0) — binding for trait surface, config defaults,
  scope dispatch, filter push-down, errors, v1 limitations.
- `spec/23_retrievers/01_rrf_fusion.md` — fusion consumes
  `RankedItem` shape declared in §23/02 §6, which §23/03 §6
  inherits.
- `spec/06_ann_index/02_parameters.md` — substrate HNSW
  defaults referenced in §23/03 §3 (`ef_search = 64`, cap
  `ef_search_max = 500`).
- `spec/16_benchmarks_acceptance/02_latency_targets.md` §2.10
  — perf targets validated in 23.12.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `HnswIndex::search` signature + filter callback | `crates/brain-index/src/hnsw.rs:224` | `search(query, k, ef, filter)` where `filter: F: Fn(MemoryId) -> bool` — push-down is available for memory scope. ✓ |
| `StatementHnswIndex::search` filter? | `crates/brain-index/src/statement_hnsw.rs:225` | No filter callback. Post-filter only. Documented in §23/03 §5 fallback. |
| `SharedHnsw<384>` reader handle | `crates/brain-index/src/shared.rs` | Cheap-clone, snapshot-style reads via `ArcSwap`. ✓ |
| `Dispatcher` trait for embedder | `crates/brain-embed/src/lib.rs` | Existing trait; used by the substrate's encode path. ✓ |
| `futures::join!` available | workspace deps | `futures = "0.3"` already pulled by other crates. Confirm direct dep for brain-index. |

## 4. Architecture sketch

```rust
// crates/brain-index/src/semantic_retriever.rs

use std::ops::RangeInclusive;
use std::sync::Arc;

use brain_core::{AgentId, MemoryId, MemoryKind, PredicateId, StatementId};
use brain_core::knowledge::StatementKind;
use brain_embed::Dispatcher;
use brain_metadata::MetadataDb;
use parking_lot::{Mutex, RwLock};

use crate::shared::SharedHnsw;
use crate::statement_hnsw::StatementHnswIndex;
use crate::tantivy_shard::{RankedItem, RankedItemId};

pub const VECTOR_DIM: usize = 384;
pub const DEFAULT_TOP_K: usize = 64;
pub const DEFAULT_EF_SEARCH: usize = 64;
pub const EF_SEARCH_MAX: usize = 500;
pub const DEFAULT_TIMEOUT_MS: u32 = 50;

pub trait SemanticRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &SemanticQuery,
        scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError>;
}

#[derive(Debug, Clone)]
pub enum SemanticQuery {
    Vector(Box<[f32; VECTOR_DIM]>),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticScope { Memory, Statement, Both }

#[derive(Debug, Clone, Default)]
pub struct SemanticFilters {
    pub agent_id: Option<AgentId>,
    pub memory_kind: Option<MemoryKind>,
    pub statement_kind: Option<StatementKind>,
    pub predicate_id: Option<PredicateId>,
    pub confidence_bucket: Option<RangeInclusive<u8>>,
    pub created_at_ms: Option<RangeInclusive<u64>>,
    pub extracted_at_ms: Option<RangeInclusive<u64>>,
}

#[derive(Debug, Clone, Copy)]
pub struct SemanticRetrieverConfig {
    pub top_k: usize,
    pub ef_search: usize,
    pub similarity_threshold: f32,
    pub timeout_ms: u32,
}

impl Default for SemanticRetrieverConfig { /* §23/03 §3 defaults */ }

#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("index unavailable (rebuild in progress)")]
    IndexUnavailable,
    #[error("query parse failed: {0}")]
    QueryParseFailed(String),
    #[error("query timed out after {0} ms")]
    Timeout(u32),
    #[error("embedder fingerprint mismatch")]
    EmbedderFingerprintMismatch,
    #[error("embedder failure: {0}")]
    EmbedderFailure(String),
    #[error("internal: {0}")]
    Internal(String),
}

pub struct BrainSemanticRetriever {
    embedder: Arc<dyn Dispatcher>,
    memory_index: SharedHnsw<VECTOR_DIM>,
    statement_index: Option<Arc<RwLock<StatementHnswIndex>>>,
    metadata: Arc<Mutex<MetadataDb>>,
}

impl BrainSemanticRetriever {
    pub fn new(...) -> Self;
}

impl SemanticRetriever for BrainSemanticRetriever {
    fn retrieve(&self, q, scope, config) -> Result<Vec<RankedItem>, SemanticError> {
        validate_scope_filter_combo(scope, &q.filters)?;
        if config.ef_search > EF_SEARCH_MAX {
            return Err(SemanticError::QueryParseFailed(
                format!("ef_search {} > max {}", config.ef_search, EF_SEARCH_MAX),
            ));
        }
        let vector = embed(&q, &self.embedder)?;
        match scope {
            SemanticScope::Memory => self.search_memory(&vector, config, &q.filters),
            SemanticScope::Statement => self.search_statement(&vector, config, &q.filters),
            SemanticScope::Both => {
                let (mem, stmt) = self.search_both(&vector, config, &q.filters)?;
                Ok(merge_and_rerank(mem, stmt, config))
            }
        }
    }
}

fn search_memory(...) -> Result<Vec<RankedItem>, SemanticError> {
    let metadata_snapshot = self.metadata.lock();
    let rtxn = metadata_snapshot.read_txn().map_err(...)?;
    let mem_table = rtxn.open_table(MEMORIES_TABLE).map_err(...)?;
    let filters = q.filters.clone();
    let filter = |id: MemoryId| -> bool {
        let Some(row) = mem_table.get(&id.raw().to_be_bytes()).ok().flatten() else {
            return false;  // missing row = exclude
        };
        let meta = row.value();
        if let Some(agent) = filters.agent_id {
            let bytes: [u8; 16] = agent.into();
            if meta.agent_id_bytes != bytes { return false; }
        }
        if let Some(kind) = filters.memory_kind {
            if meta.kind != kind_to_u8(kind) { return false; }
        }
        if let Some(range) = filters.created_at_ms.as_ref() {
            let ms = meta.created_at_unix_nanos / 1_000_000;
            if !range.contains(&ms) { return false; }
        }
        true
    };
    let hits = self.memory_index.search(&vector, config.top_k, Some(config.ef_search), filter);
    Ok(hits.into_iter()
        .filter(|(_, s)| *s >= config.similarity_threshold)
        .enumerate()
        .map(|(i, (id, score))| RankedItem {
            id: RankedItemId::Memory(id),
            rank: (i as u32) + 1,
            score,
            snippet: None,
        })
        .collect())
}
```

`search_statement` is identical-shape but uses
`StatementHnswIndex::search_with_ef` (no callback) and applies
filters post-search. Returns `Ok(vec![])` if
`self.statement_index.is_none()`.

`search_both` issues both searches sequentially (no Glommio
context-aware parallelism in the retriever itself; the planner
parallelises across retrievers, not within one), merges by
score, and re-ranks.

### OpsContext wiring

```rust
// crates/brain-ops/src/context.rs (additive)
pub semantic_retriever: Option<Arc<dyn SemanticRetriever>>,
```

Plus `with_semantic_retriever`. Phase 23's planner will read
through this slot (23.6 / 23.7); for v1 nothing else does.

Server-side spawn:

```rust
let semantic_for_ops: Option<Arc<dyn brain_index::SemanticRetriever>> = {
    let retriever = brain_index::BrainSemanticRetriever::new(
        embedder.clone(),
        hnsw_shared.clone(),
        None,                    // statement HNSW handle, v1: None
        metadata.clone(),
    );
    Some(Arc::new(retriever))
};
// ...with_semantic_retriever(semantic_for_ops)
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Module in `brain-index` (this plan) | Co-located with HNSW handles + lexical retriever (22.5 precedent) | brain-index gains a dep on brain-embed | ✓ |
| Module in `brain-planner` | Planner-side feels "right" | brain-planner already has heavy retrieval logic in 23.6+; adding the retriever there too couples impl and orchestration | rejected |
| Statement scope returns `IndexUnavailable` when handle is None | Loud failure | Forces clients to handle a transient error mode that v1 doesn't have a fix for | rejected — silent `Ok(vec![])` matches the spec |
| Filter push-down via per-candidate redb txn (this plan) | Correct; cheap because read-txn is shared per query | ~5-10 µs per candidate metadata lookup adds to single-search latency | ✓ — within budget |
| Filter push-down by pre-loading the filter universe (e.g. all MemoryIds matching the filter) into a bitmap before HNSW search | Constant-time filter lookup | Doesn't scale — corpus may be millions; cost dominates | rejected |
| Open redb read-txn once at retriever construction, reuse forever | Zero per-query setup | redb read-txns snapshot at open; staleness grows; explicit re-open per query is cheap | rejected |

## 6. Risks / open questions

- **Risk:** `MetadataDb::read_txn` is mutex-locked in v1 (the `Arc<Mutex<MetadataDb>>` wrapping on `OpsContext`). Each retrieve acquires the lock for the duration of the search. **Mitigation:** the lock is per-shard, not per-server; ENCODE / FORGET / extractor pipeline also acquire it. v1 latency budget tolerates this; phase-23 perf benchmarks will validate.
- **Risk:** HNSW filter callback may invoke `filter()` many times per candidate (during ef-search escalation). Cost compounds. **Mitigation:** the closure is cheap (one redb get per call); profile in 23.12 if needed.
- **Risk:** `Both` scope sequential search (memory then statement) doubles wall-time. **Mitigation:** §23/03 §4 allows parallel execution; v1 uses sequential because there's no Glommio-friendly join primitive in scope; revisit in 23.7 if the planner wants parallel.
- **Open question:** Where does the `SemanticRetriever` get its `Dispatcher` reference — `OpsContext.executor.embedder`? **Resolution:** yes; the retriever construction at shard spawn pulls the dispatcher from the same place `ExecutorContext` does.

## 7. Test plan

Unit tests in `crates/brain-index/src/semantic_retriever_tests.rs`:

- `memory_scope_returns_ranked_hits` — insert 3 memories with known vectors into a fresh `SharedHnsw`, build retriever, query a vector close to one of them, assert that memory ranks 1.
- `memory_kind_filter_narrows` — two memories with different kinds; filter selects exactly one.
- `agent_id_filter_narrows` — two memories under different agents; filter selects one.
- `created_at_range_filter` — three memories at different timestamps; range filter selects the middle.
- `statement_scope_with_none_handle_returns_empty` — Statement scope, `statement_index = None`, returns `Ok(vec![])` (not an error).
- `both_scope_merges_by_score` — feed memory + statement (after wiring a non-None statement handle) and assert merged ordering.
- `vector_query_dim_mismatch_errors` — `SemanticQuery::Vector` with the wrong dim (compile-time impossible with `Box<[f32; 384]>`, but exercise the runtime equivalent with a `Vec<f32>` adapter if added).
- `ef_search_above_max_errors` — config.ef_search = 1000 → QueryParseFailed.
- `similarity_threshold_drops_low_scores` — set threshold = 0.9, assert hits all have score ≥ 0.9.
- `text_path_routes_through_embedder` — mock Dispatcher returns a known vector; retriever passes the text through; assert the embedded vector is what HNSW searches with.
- `wrong_scope_filter_combination_errors` — `predicate_id` filter on `Memory` scope → QueryParseFailed.

## 8. Commit shape

Single commit:

```
feat(index,ops,server): 23.1 — SemanticRetriever trait + impl

- crates/brain-index/src/semantic_retriever.rs (new):
  trait + types + BrainSemanticRetriever impl. Memory scope
  uses HnswIndex::search filter callback for push-down;
  Statement scope applies filters post-search (no callback
  in v1 StatementHnswIndex). Both scope sequential; results
  merged by score then re-ranked dense 1-based.
- crates/brain-index/src/semantic_retriever_tests.rs (new):
  ~11 unit tests covering scope dispatch, push-down filters,
  ef_search cap, similarity threshold, none-handle behaviour.
- crates/brain-index/src/lib.rs: re-export the new surface.
- crates/brain-index/Cargo.toml: brain-embed dep added.
- crates/brain-ops/src/context.rs: `semantic_retriever:
  Option<Arc<dyn SemanticRetriever>>` field + builder.
- crates/brain-server/src/shard/mod.rs: shard spawn constructs
  the retriever from the shared HNSW + None statement handle
  + metadata + embedder; installs on OpsContext alongside the
  22.5 lexical retriever.
```

## 9. Confirmation

Please confirm:

1. **Module lives in `brain-index`** (vs. `brain-planner`).
2. **Statement HNSW handle is `Option<...>` and starts at `None` in v1.** Statement scope returns `Ok(vec![])` silently when the handle is None; doesn't surface as an error.
3. **`Both` scope is sequential** (memory then statement) — the planner parallelises across retrievers, not within one.
4. **Per-candidate redb metadata lookup** for filter push-down (vs. pre-loaded bitmaps).
5. **No `EmbedderFingerprintMismatch` check in 23.1** — v1 trusts the operator; defer to 23.7 if cost estimation needs it.

After approval: implement + tests + commit.
