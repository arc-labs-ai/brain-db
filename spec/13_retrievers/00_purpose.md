# 13. Retrievers

> **TL;DR.** Three retrievers — Semantic (HNSW over memory/statement embeddings), Lexical (tantivy BM25), Graph (entity-joined traversal) — run in parallel and produce ranked lists. Reciprocal Rank Fusion with `k=60` and per-retriever weights merges them into a single ranking. A filter chain (type, time, confidence, tombstone) trims the result. The query router classifies each query and picks which lanes to run; defaults to Semantic + Lexical for ambiguous text-only queries.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of the retrieval engine; schema authors tuning query behavior |
| Voice | Hybrid (rationale + normative) |
| Depends on | [07. Embedding](../07_embedding/00_purpose.md), [09. Indexing](../09_indexing/00_purpose.md), [10. Metadata](../10_metadata/00_purpose.md), [11. Extractors](../11_extractors/00_purpose.md), [12. Query Optimizer](../12_query_optimizer/00_purpose.md) |
| Referenced by | [05. Operations](../05_operations/00_purpose.md), [06. Client Interface](../06_sdk/00_purpose.md), [19. Benchmarks](../19_benchmarks/00_purpose.md) |

## What this spec defines

The retrieval surface activated when a schema is declared. Three retrievers (semantic, lexical, graph) run in parallel; their ranked outputs are fused with weighted Reciprocal Rank Fusion (RRF, k=60). A cross-encoder reranker (bge-reranker-base) then reorders the top-K — always-on whenever the model is loaded, gated only by the deploy-time `config.rerank.enabled` switch, with no per-request flag. A rule-based query router decides per-query which retrievers to invoke and with what weights.

`RECALL` transparently uses this path when a schema is active; the response shape is identical to the schemaless path with extra `contributing_retrievers` and `fused_score` metadata.

## The hybrid retrieval architecture

The typed-graph retrieval surface is a multi-retriever architecture. Three retrievers run in parallel (or selectively, per the query router), each producing a ranked list. Results are fused with Reciprocal Rank Fusion (RRF) into a single ranked output.

```
                   ┌─────────────────────────────────────┐
                   │       Query Router (§13/05)         │
                   │   classifies query, picks lanes      │
                   └──────────────┬──────────────────────┘
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
    ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
    │ SemanticRetr.   │ │ LexicalRetr.    │ │ GraphRetr.      │
    │ HNSW + embed    │ │ tantivy BM25    │ │ entity-joined   │
    │   (memories     │ │  (memories      │ │   (statements   │
    │    or           │ │    or           │ │    or           │
    │    statements   │ │    statements)  │ │    relations)   │
    │    or both)     │ │                 │ │                 │
    └────────┬────────┘ └────────┬────────┘ └────────┬────────┘
             │                   │                   │
             └───────────────────┼───────────────────┘
                                 ▼
                   ┌─────────────────────────────────────┐
                   │       RRF Fusion                    │
                   │  k=60, weighted per-retriever       │
                   └──────────────┬──────────────────────┘
                                  │
                                  ▼
                   ┌─────────────────────────────────────┐
                   │       Filter chain (Type, Time,     │
                   │       Confidence, Tombstone)        │
                   └──────────────┬──────────────────────┘
                                  │
                                  ▼
                   ┌─────────────────────────────────────┐
                   │       Final ranked result            │
                   └─────────────────────────────────────┘
```

## Retriever interface

```rust
trait Retriever {
    type Item;                         // Memory | Statement | Relation | Entity
    type Score;                        // f32 or domain-specific

    fn retrieve(
        &self,
        query: &RetrievalQuery,
        scope: &Scope,
        config: &RetrieverConfig,
    ) -> Vec<RankedItem<Self::Item, Self::Score>>;
    
    fn name(&self) -> &'static str;
}

struct RankedItem<I, S> {
    item: I,
    score: S,
    rank: usize,                       // 1-indexed
    retriever: &'static str,
}
```

All retrievers produce items with ranks. Scores are retriever-internal (cosine for semantic, BM25 for lexical, graph proximity for graph) and are not directly comparable across retrievers — fusion uses rank, not score.

## SemanticRetriever

Wraps Brain HNSW (§09 Indexing). It operates over multiple corpora:

- **Memory HNSW** (§09): embeddings of memory text. 384-dim.
- **Statement HNSW**: embeddings of statement representations. The representation embedded is `predicate + " " + object + " " + subject.canonical_name`. Captures the semantics of the statement for similarity matching.
- **Entity HNSW**: used for entity resolution (§11 Extractors), not query retrieval typically.

Configuration:
- Search target: `memory | statement | both`.
- ef_search, top_k, similarity threshold.

Returns: ranked items with cosine scores.

## LexicalRetriever

Uses tantivy for BM25.

Two indexes:
- **Memory text index**: every memory's text is indexed. Fields: `text`, `agent_id`, `kind`, `created_at`.
- **Statement text index**: statements' textual representation indexed (predicate + object value + subject canonical_name). Fields: `predicate`, `object_text`, `subject_name`, `kind`.

BM25 parameters: `k1 = 1.2`, `b = 0.75` (tantivy defaults; configurable).

Tokenization:
- Lowercase, English stemming (Porter or Snowball).
- Sublanguage tokens preserved (URLs, IDs like "ACME-1247", code identifiers).
- Configurable per-field.

Returns: ranked items with BM25 scores.

## GraphRetriever

Operates on the entity graph (Entities + Relations + Statement subjects).

Inputs:
- An "anchor" entity (the query's referenced entity).
- Traversal spec: relation types, max depth, direction.

Returns: items at each hop, ranked by graph proximity.

Three modes:

1. **Star**: entity → outgoing/incoming relations → other entities. Returns statements about those entities or memories mentioning them.

2. **Path**: from entity A to entity B, find connecting paths up to depth N. Returns relations and entities along the paths.

3. **Subgraph**: entity → entire k-hop neighborhood. Returns set of entities, relations, statements.

Configuration:
- max_depth (default 3, capped at 5).
- direction (outgoing, incoming, both).
- relation_type_filter (optional whitelist).

Performance: 1-2 hops are fast (O(log N) per hop). 3+ hops can be expensive if branching factor is high; the planner caps result count and depth.

Returns: items with proximity scores (e.g., 1/(hop_distance + 1)).

## Per-retriever indexes

The retrievers depend on these indexes (all maintained automatically):

| Retriever | Index | Built from |
|---|---|---|
| Semantic | Memory HNSW | Memory embeddings |
| Semantic | Statement HNSW | Statement embeddings |
| Lexical | Memory text (tantivy) | Memory text |
| Lexical | Statement text (tantivy) | Statement textual repr |
| Graph | Entity adjacency (redb) | Relations |
| Graph | Statements-by-subject (redb) | Statements |

Maintenance:
- Memory HNSW: maintained by the memory-HNSW worker (§15 Background Workers), unchanged by typed-graph activation.
- Statement HNSW: maintained by the statement-embedding worker. Re-embeds on statement create/supersede.
- Tantivy indexes: incremental on writes, periodic compaction.
- Graph adjacency: incremental on relation create/tombstone, no periodic work.

## When each retriever wins

| Query pattern | Best retriever(s) |
|---|---|
| "What does Priya prefer?" | Graph (Priya) + Type filter (Preference) |
| "Find memories about budget pushback" | Semantic + Lexical |
| "Show me ticket ACME-1247" | Lexical (exact ID match) |
| "What did Priya say last week?" | Graph + Temporal filter |
| "Find similar concepts to X" | Semantic |
| "Anyone connected to Priya through projects" | Graph |
| "All Facts about Project Foo" | Graph (Foo) + Type filter (Fact) |
| "Memories that mention people on the engineering team" | Graph (team) + multi-hop |

The router (§13/05) classifies and picks. Defaults to Semantic + Lexical for ambiguous text-only queries.

## Failure modes

- **Empty corpus for a retriever**: returns empty list. Fusion degrades gracefully.
- **Retriever times out**: cancelled, returns partial or empty. Fusion proceeds with what arrived. Operator sees a metric.
- **Index out of date**: results may miss recent writes. The worker pipeline keeps indexes within seconds of writes. Eventual consistency is acceptable.
- **All retrievers empty**: query returns empty. The router can suggest "did you mean..." based on related entities.

## Cost model

Each retriever has a documented cost:

| Retriever | Cost per query |
|---|---|
| Semantic | O(log N) HNSW lookup; ~1-10 ms |
| Lexical | O(query terms × idf-list-length); ~1-50 ms |
| Graph (1-2 hop) | O(adjacency size); ~1-10 ms |
| Graph (3+ hop) | O(branching^depth); can be slow |
| LLM resolution (if invoked) | ~500 ms - 5 s |

The planner uses these to estimate query cost and surface slow queries.
