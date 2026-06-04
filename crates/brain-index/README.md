# brain-index

> ANN (HNSW) index integration for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

The per-shard retrieval index layer. It wraps `hnsw_rs` for approximate-nearest-
neighbour search over `VECTOR_DIM` (384, BGE-small) embeddings, with three index
variants (memory, entity, statement) plus their lifecycle (build, search,
snapshot, rebuild, persistence) and a lock-free `arc-swap` + `crossbeam-epoch`
two-tier main/pending publication model so reads never block on writes. It also
provides the lexical (BM25) index via `tantivy` with a Brain-specific analyzer
(URL/code-ID preservation, Porter stemming, NFC normalization), a product-
quantization (`pq`) path, tombstone tracking, and the semantic/lexical/graph
retrievers. It is a closed leaf: vectors in, candidates out — no dependency on
`brain-storage` or `brain-metadata`.

## Key modules

| Module | Purpose |
|---|---|
| `hnsw` | Full-precision memory HNSW (`HnswIndex`), exact-cosine scoring. |
| `entity_hnsw` / `statement_hnsw` | Entity and statement HNSW variants. |
| `shared` | `SharedHnsw` — lock-free main/pending two-tier publication. |
| `params` | HNSW knobs (`M=16, ef_construction=200, ef_search=64`) and `VECTOR_DIM`. |
| `idmap` | Maps internal HNSW ids ↔ Brain ids. |
| `persistence` / `rebuild` | Snapshot, load, and rebuild lifecycle with rebuild reports. |
| `pq` | Product-quantization codebook, encoding, and distance tables. |
| `tantivy_shard` | BM25 lexical index, Brain analyzer/tokenizer, lexical retriever. |
| `semantic_retriever` | Semantic (ANN) retriever with scope/filter validation. |
| `graph_retriever` | Entity-graph proximity retriever. |
| `tombstones` | `TombstoneBitmap` for filtering reclaimed slots. |
| `arena_reader` | `ArenaReader` trait abstracting vector source (incl. null impl). |

## Where it fits

Depends only on `brain-core` plus index leaves (`hnsw_rs`, `tantivy`, `arc-swap`,
`crossbeam-epoch`, stemmer/normalization crates). Cross-crate composition
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
