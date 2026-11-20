# brain-index

> ANN (HNSW) index integration for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The per-shard retrieval index layer. It wraps `hnsw_rs` for approximate-nearest-
neighbour search over `VECTOR_DIM` (384, BGE-small) embeddings, with five index
variants (memory, entity, statement, hypothetical-question, statement-question)
plus their lifecycle (build, search, snapshot, rebuild, persistence) and a
lock-free `arc-swap` two-tier main/pending publication model so reads never
block on writes: a flush publishes a whole new generation through `ArcSwap`, and
the generation it displaced is freed by its own `Arc` refcount once the last
reader holding it is done. It also provides the lexical (BM25) index via
`tantivy` with a Brain-specific analyzer (URL/code-ID preservation, Porter
stemming, NFC normalization), a product-quantization (`pq`) path, tombstone
tracking, and the semantic/lexical/graph retrievers. It is a closed leaf:
vectors in, candidates out — no dependency on `brain-storage` or
`brain-metadata`.

## Key modules

| Module | Purpose |
|---|---|
| `hnsw` | Full-precision memory HNSW (`HnswIndex`), exact-cosine scoring. |
| `entity_hnsw` / `statement_hnsw` | Entity and statement HNSW variants. |
| `hype_hnsw` | HNSW over hypothetical questions generated for a memory; many question points map back to one memory. |
| `statement_question_hnsw` | The per-statement analogue: templated questions whose answer is one statement, resolving to `(StatementId, Slot)`. |
| `shared` | `SharedHnsw` — lock-free main/pending two-tier publication, one `ArcSwap` generation per flush. |
| `params` | HNSW knobs (`M=16, ef_construction=200, ef_search=64`) and `VECTOR_DIM`. |
| `idmap` | Maps internal HNSW ids ↔ Brain ids. |
| `persistence` / `rebuild` | Snapshot, load, and rebuild lifecycle with rebuild reports. |
| `pq` | Product-quantization codebook, encoding, and distance tables. |
| `tantivy_shard` | BM25 lexical index, Brain analyzer/tokenizer, lexical retriever. |
| `semantic_retriever` | Semantic (ANN) retriever with scope/filter validation. |
| `graph_retriever` | Entity-graph proximity retriever. |
| `tombstones` | `TombstoneBitmap` — a bit per id, filtering forgotten entries out of search results rather than deleting their graph nodes. |
| `arena_reader` | `ArenaReader` trait abstracting vector source (incl. null impl). |
| `space_vector_source` | `SpaceVectorSource` trait — by-slot full-precision vectors for the single-space brute-force lane, so this crate needs no `brain-storage` dependency. |

## Where it fits

Depends only on `brain-core` plus index leaves (`hnsw_rs`, `tantivy`, `arc-swap`,
`parking_lot`, stemmer/normalization crates). Cross-crate composition
(rebuilding from arena slots) lives in a higher layer; this crate is consumed by
`brain-planner` / `brain-ops` and the shard runtime.

## Spec

- Indexing overview: [`../../spec/09_indexing/00_purpose.md`](../../spec/09_indexing/00_purpose.md)
- HNSW basics & parameters: [`../../spec/09_indexing/01_hnsw_basics.md`](../../spec/09_indexing/01_hnsw_basics.md)
- HNSW lifecycle: [`../../spec/09_indexing/03_hnsw_lifecycle.md`](../../spec/09_indexing/03_hnsw_lifecycle.md)
- Product quantization: [`../../spec/09_indexing/07_hnsw_pq.md`](../../spec/09_indexing/07_hnsw_pq.md)
- Tantivy (lexical) layout: [`../../spec/10_metadata/06_tantivy_layout.md`](../../spec/10_metadata/06_tantivy_layout.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
