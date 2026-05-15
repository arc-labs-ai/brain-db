# Retrievers

## The hybrid retrieval architecture

the knowledge layer introduces a multi-retriever architecture. Three retrievers run in parallel (or selectively, per the query router), each producing a ranked list. Results are fused with Reciprocal Rank Fusion (RRF) into a single ranked output.

```
                   ┌─────────────────────────────────────┐
                   │       Query Router (08)             │
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

Wraps the substrate HNSW (section 06). It operates over multiple corpora:

- **Memory HNSW (section 06): embeddings of memory text. 384-dim.
- **Statement HNSW** (new here): embeddings of statement representations. The representation embedded is `predicate + " " + object + " " + subject.canonical_name`. Captures the semantics of the statement for similarity matching.
- **Entity HNSW** (new here, see Entity section): used for entity resolution, not query retrieval typically.

Configuration:
- Search target: `memory | statement | both`.
- ef_search, top_k, similarity threshold.

Returns: ranked items with cosine scores.

## LexicalRetriever

New here. Uses tantivy for BM25.

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

New here. Operates on the entity graph (Entities + Relations + Statement subjects).

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
- Memory HNSW: substrate worker (section 11), unchanged by the knowledge layer.
- Statement HNSW: new worker. Re-embeds on statement create/supersede.
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

The router (Section 08) classifies and picks. Defaults to Semantic + Lexical for ambiguous text-only queries.

## Failure modes

- **Empty corpus for a retriever**: returns empty list. Fusion degrades gracefully.
- **Retriever times out**: cancelled, returns partial or empty. Fusion proceeds with what arrived. Operator sees a metric.
- **Index out of date**: results may miss recent writes. the worker pipeline keeps indexes within seconds of writes. Eventual consistency is acceptable.
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
