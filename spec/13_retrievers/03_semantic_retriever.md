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
    pub timeout_ms: u32,                 // config default 50; the plan sets 1 s (§8)
}
```

`ef_search` defaults are per-corpus, and they differ because the corpora differ (see [`../09_indexing/01_hnsw_basics.md`](../09_indexing/01_hnsw_basics.md) §25):

| Scope | `M` | `ef_construction` | Default `ef_search` | Cap |
|---|---|---|---|---|
| `Memory` | 16 | 200 | 64 | 500 |
| `Statement` | 32 | 200 | 128 | 500 |
| `Both` | — | — | 64 for the memory leg, 128 for the statement leg | 500 |

Statement embeddings are a denser, more uniform population than memory text — short predicate-plus-object renderings cluster much more tightly than free prose — so the statement graph carries twice the out-degree and searches with twice the beam to reach comparable recall (`crates/brain-index/src/statement_hnsw.rs:70-80`, `crates/brain-index/src/params.rs:28-92`).

A `Some(n)` in `config.ef_search` overrides every leg's default uniformly. Callers who want per-scope overrides issue two `retrieve` calls.

`retrieve()` is **read-only**. No side effects.

## 2. Embedding semantics

Two input modes:

- **`SemanticQuery::Vector`** — caller supplies the 384-dim
  embedding directly. Dimension must equal
  `brain_core::VECTOR_DIM`; mismatch → `QueryParseFailed`.
- **`SemanticQuery::Text`** — retriever calls into the
  per-shard embedder (`brain-embed`) to encode the
  text. The model fingerprint is the same one Brain
  uses for ingest; mismatch between the corpus's embedding and
  the query's would silently produce poor recall, so the
  retriever rejects queries whose embedder fingerprint differs
  from the indexed corpus's via `SemanticError::EmbedderFingerprintMismatch`.

The Text path adds the embedder's wall-time to the query
budget. §19 perf-target numbers assume Text input.

**Where that wall-time is spent.** The encode is handed to the shard
executor's blocking pool rather than run on the reactor, so a forward
pass cannot stall the shard's other in-flight requests
(`crates/brain-ops/src/embedding.rs:52-61`). It does not thereby become
free or parallel: the blocking thread builds a one-per-thread intra-op
pool whose workers inherit that thread's CPU affinity, which confines
Candle's matmul fan-out to the shard's **own core** instead of letting
one shard's embed take cores from its neighbours
(`crates/brain-ops/src/embedding.rs:16-38`). Sharding is the parallelism;
the intra-op pool is a containment boundary. The offload is a contract
rather than an optimisation — the helper panics outright when no
executor is present, instead of quietly blocking the caller for the
length of a forward pass.

One consequence matters for capacity: a text query's embed cost is a
per-core cost that does not shrink as cores are added to the box. More
cores buy more shards, each with its own embedder lane, not a faster
embed.

## 3. HNSW search params

Defaults match Brain HNSW (§09/02):

- `top_k = 64`.
- `ef_search = 64`. Hard cap `ef_search_max = 500`. Values
  above the cap → `QueryParseFailed`.
- `similarity_threshold = 0.0` (no cutoff). Applied
  post-search: candidates whose cosine `< threshold` are
  dropped before ranks are assigned.
- `timeout_ms = 50` as a config default; the planner plans this lane
  at **1 s**, and the budget is soft — exceeding it records `Timeout`
  and warns without cancelling, and a late result still reaches fusion
  (`crates/brain-planner/src/retrieval/planner/logic.rs:55-65`). The
  wider budget exists for this retriever in particular: the `Text`
  path's embed step is the single most expensive thing any lane does,
  and a hard 50 ms deadline would classify the normal case as a
  timeout.

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

The `+2 ms / +10 ms` text-path row is the embed increment measured as an
index-adjacent cost. It is the one figure in this table that is a
property of the host rather than of the index: the forward pass is
confined to one core (§2), so it scales with that core's throughput and
with input length, and it does not improve on a wider machine. A
deployment reading this table on hardware unlike the reference box
should re-measure this row first — a per-core embed figure is the
quantity most likely to move, and it moves the whole `Text`-path budget
with it.

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
