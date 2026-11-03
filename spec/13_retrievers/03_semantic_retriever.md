# 13.03 SemanticRetriever

Normative spec for the semantic retriever. Sits
beside `00_purpose.md`, `01_rrf_fusion.md`, and `02_lexical_retriever.md`.

The SemanticRetriever is one of the three retrievers in §13/00.
It wraps Brain HNSW index (§09) plus the
typed-graph statement HNSW (§09 "Per-shard HNSW additions").

## 1. Surface

Trait shape:

```rust
pub trait SemanticRetriever: Send + Sync {
    fn retrieve(
        &self,
        query: &SemanticQuery,
        scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError>;
}

pub enum SemanticQuery {
    /// Pre-embedded query — caller has the 384-dim vector.
    Vector(Box<[f32; 384]>),
    /// Text query — the retriever asks `brain-embed` to
    /// encode it before searching. The model is the same
    /// `EmbedderConfig` Brain uses (§07).
    Text(String),
}

pub enum SemanticScope {
    /// Memory HNSW (§09). `RankedItem.id = MemoryId`.
    Memory,
    /// Statement HNSW (§09). `RankedItem.id = StatementId`.
    Statement,
    /// Both corpora; results merged by descending cosine.
    Both,
}

pub struct SemanticFilters {
    pub space_id: Option<SpaceId>,
    pub memory_kind: Option<MemoryKind>,
    pub statement_kind: Option<StatementKind>,
    pub predicate_id: Option<PredicateId>,
    pub confidence_bucket: Option<RangeInclusive<u8>>,
    pub created_at_ms: Option<RangeInclusive<u64>>,
    pub extracted_at_ms: Option<RangeInclusive<u64>>,
}

pub struct SemanticRetrieverConfig {
    pub top_k: usize,                    // default 64
    pub ef_search: Option<usize>,        // None ⇒ per-scope default; cap 500 (§09/02)
    pub similarity_threshold: f32,       // default 0.0 (no cutoff)
    pub timeout_ms: u32,                 // default 50
}
```

`ef_search` defaults are per-corpus (see [`../09_indexing/01_hnsw_basics.md`](../09_indexing/01_hnsw_basics.md) §25):

| Scope | Default `ef_search` |
|---|---|
| `Memory` | 64 |
| `Statement` | 128 |
| `Both` | 64 for the memory leg, 128 for the statement leg |

A `Some(n)` in `config.ef_search` overrides every leg's default uniformly. Callers who want per-scope overrides issue two `retrieve` calls.

`retrieve()` is **read-only**. No side effects.

## 2. Embedding semantics

Two input modes:

- **`SemanticQuery::Vector`** — caller supplies the 384-dim
  embedding directly. Dimension must equal
  `brain_core::VECTOR_DIM`; mismatch → `QueryParseFailed`.
- **`SemanticQuery::Text`** — retriever calls into the
  per-shard `EmbedderHandle` (`brain-embed`) to encode the
  text. The model fingerprint is the same one Brain
  uses for ingest; mismatch between the corpus's embedding and
  the query's would silently produce poor recall, so the
  retriever rejects queries whose embedder fingerprint differs
  from the indexed corpus's via `SemanticError::EmbedderFingerprintMismatch`.

The Text path adds the embedder's wall-time to the query
budget. §19 perf-target numbers assume Text input.

## 3. HNSW search params

Defaults match Brain HNSW (§09/02):

- `top_k = 64`.
- `ef_search = 64`. Hard cap `ef_search_max = 500`. Values
  above the cap → `QueryParseFailed`.
- `similarity_threshold = 0.0` (no cutoff). Applied
  post-search: candidates whose cosine `< threshold` are
  dropped before ranks are assigned.
- `timeout_ms = 50`. Exceeded → `Timeout`.

`top_k` is enforced after `similarity_threshold` filtering,
matching §13/02 §6. If fewer matches survive than `top_k`, the
returned slice is shorter.

## 4. Scope dispatch

| Scope | HNSW | `RankedItem.id` |
|---|---|---|
| `Memory` | memory HNSW (§09) | `RankedItemId::Memory(MemoryId)` |
| `Statement` | statement HNSW (§09) | `RankedItemId::Statement(StatementId)` |
| `Both` | both, fanned out concurrently | mixed |

`Both` merges results by descending cosine. Ranks are dense
1-based across the merged slice. Implementations may issue the
two searches in parallel.

**Cross-shard semantics:** out of scope — retrieval is per-shard.
Multi-shard fan-out is the §13/05 router's responsibility.

## 5. Filter push-down

§13/05 §"Filter as retriever vs filter" specifies that
selective filters push down into retrievers as pre-filters.
For SemanticRetriever, push-down uses the HNSW filter callback
where the underlying library exposes one.

Push-down per filter:

| Filter | Push-down mechanism |
|---|---|
| `space_id` | HNSW filter callback (cheap metadata lookup). |
| `memory_kind` / `statement_kind` | HNSW filter callback. |
| `predicate_id` | HNSW filter callback (statement scope only). |
| `confidence_bucket` range | Post-search filter (range cardinality is typically large; cheaper than per-candidate callback). |
| `created_at_ms` range | Post-search. |
| `extracted_at_ms` range | Post-search. |

If `hnsw_rs`'s filter API isn't available, the retriever falls
back to post-search filtering for all filters. Correctness holds
either way; only push-down impacts latency.

A filter targeting a field absent from the scope's schema
(e.g. `predicate_id` with `SemanticScope::Memory`) returns
`SemanticError::QueryParseFailed`.

## 6. Returns + idempotency

Same `RankedItem` shape as §13/02 §6:

- `Vec<RankedItem>` ordered by descending cosine.
- `rank` 1-based, dense.
- `score` is the cosine similarity in `[-1.0, 1.0]`; cross-
  retriever fusion uses rank, not score.
- `snippet` always `None`.

**Idempotency:** two calls with identical `(query, scope,
config)` between commits return identical `Vec<RankedItem>`.
The HNSW indexes commit on background-worker cadence; reads
see a consistent snapshot between worker commits.

## 7. Errors

`SemanticError` taxonomy (binding for §04/07 error code map):

| Variant | Trigger | Visible to clients |
|---|---|---|
| `IndexUnavailable` | HNSW index mid-rebuild (§09/03) or missing. | Yes — clients retry. |
| `QueryParseFailed` | Vector dim mismatch, scope+filter mismatch, `ef_search` above cap. | Yes — client bug. |
| `Timeout` | Query exceeded `config.timeout_ms` (combined embed + search). | Yes — degraded response. |
| `EmbedderFingerprintMismatch` | `SemanticQuery::Text` path; encoder model fingerprint differs from corpus's. | Yes — operator misconfiguration. |
| `EmbedderFailure` | `SemanticQuery::Text` path; brain-embed returned an error. | Yes — degraded. |

An **empty result** (`Ok(vec![])`) is NOT an error. The
retriever does not interpret zero matches.

## 8. Performance

Pinned in §19 (perf targets):

| Operation | p50 | p99 |
|---|---|---|
| Single-corpus retrieve (Memory or Statement) | 5 ms | 25 ms |
| Both corpora retrieve | 8 ms | 35 ms |
| Text path (adds embed cost) | +2 ms | +10 ms |

Push-down filtering reduces ef_search escalations; the numbers
above assume push-down available. Fall-back to post-filter
adds ~30% under typical filter selectivity.

## 9. Limitations

- **Statement HNSW corpus may be empty.** If a deployment starts
  using semantic retrieval over `SemanticScope::Statement`
  before the statement-embedding worker has caught up with the
  existing corpus, the retriever returns `Ok(vec![])` for those
  statements (no candidates in the index). Operators trigger a
  one-time background re-embedding pass to backfill.
- **Cross-shard fan-out lives in the router**, not the
  retriever. A multi-shard query issues one
  `SemanticRetriever::retrieve` per shard and merges in the
  router.
- **No statement re-embedding on schema change** — if a
  predicate's `name` changes via SCHEMA_UPLOAD, the indexed
  statement HNSW row keeps the old embedding until the next
  full rebuild. The filter chain ignores stale embeddings via
  the `schema_version` field; full re-embedding on schema
  change is deferred.

## 10. Boundaries

- SemanticRetriever does NOT write to the HNSW index — that's
  the embedding workers (§15).
- SemanticRetriever does NOT choose which scope to use — that's
  the §13/05 router.
- SemanticRetriever does NOT fuse with other retrievers — RRF
  (§13/01) does, in the planner.
- SemanticRetriever does NOT follow `EntityMerge.merged_into`
  redirects — the router resolves the anchor entity before
  invoking.
