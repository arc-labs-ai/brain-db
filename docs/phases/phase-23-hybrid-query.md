# Phase 23: Hybrid Query Engine

## Goal

Implement the query router, RRF fusion, filter chain, and full hybrid query execution. EXPLAIN and TRACE work. RECALL transparently uses hybrid retrieval when a schema is declared.

## Prerequisites

- Phases 17, 21, and 22 complete.

## Reading list

- `23_retrievers/00_purpose.md` (full)
- `23_retrievers/01_rrf_fusion.md`
- `24_hybrid_query/00_purpose.md`

## Outputs

- SemanticRetriever extended (memory + statement HNSW).
- GraphRetriever implementation.
- Query router (rule-based, 5 rules).
- RRF fusion module.
- Filter chain (type, temporal, confidence, tombstone, supersession).
- Query planner with EXPLAIN / TRACE.
- Wire opcodes 0x60-0x63.
- SDK query builder (`client.query()`).

## Sub-tasks

### 23.1 SemanticRetriever extended

**Reads:** `23_retrievers/00_purpose.md` (semantic section).
**Writes:** `crates/brain-core/src/retriever/semantic.rs`.
**Done when:** retriever can target memory, statement, or both. Returns RankedItems with cosine scores.

### 23.2 GraphRetriever

**Reads:** `23_retrievers/00_purpose.md` (graph section).
**Writes:** `crates/brain-core/src/retriever/graph.rs`.
**Done when:** star, path, subgraph modes; returns RankedItems with proximity scores.
**Pitfalls:** Reuse 15 traversal. Bound branching factor; respect depth cap.

### 23.3 Query router (rule-based)

**Reads:** `24_hybrid_query/00_purpose.md` (router rules).
**Writes:** `crates/brain-planner/src/knowledge/router.rs`.
**Done when:** 5 routing rules implemented; produces RoutingDecision with retriever selection + weights.
**Pitfalls:** NER on query text for entity recognition; simple regex for temporal expressions; document the heuristics.

### 23.4 RRF fusion

**Reads:** `23_retrievers/01_rrf_fusion.md`.
**Writes:** `crates/brain-planner/src/knowledge/fusion.rs`.
**Done when:** `fuse_rrf(retriever_outputs, k, weights) -> Vec<FusedItem>` works with the formula.
**Pitfalls:** Stable sort for deterministic ordering at score ties.

### 23.5 Filter chain

**Reads:** `24_hybrid_query/00_purpose.md` (filter section).
**Writes:** `crates/brain-planner/src/knowledge/filters.rs`.
**Done when:** type, temporal, confidence, tombstone, supersession filters; applied in documented order; push-down to retrievers where applicable.

### 23.6 Query planner

**Reads:** `24_hybrid_query/00_purpose.md` (plan structure).
**Writes:** `crates/brain-planner/src/knowledge/planner.rs`.
**Done when:** `plan(request) -> QueryPlan`; produces DAG with retrievers, fusion, filters, cost estimate.

### 23.7 Query executor

**Reads:** `24_hybrid_query/00_purpose.md` (execution).
**Writes:** `crates/brain-planner/src/knowledge/executor.rs`.
**Done when:** parallel retriever invocation, timeout handling, fusion, filtering. Returns QueryResult with debug metadata.

### 23.8 EXPLAIN and TRACE

**Reads:** `24_hybrid_query/00_purpose.md` (plan structure).
**Writes:** add to planner/executor.
**Done when:** EXPLAIN returns plan without execution; TRACE returns plan + per-retriever traces.

### 23.9 Wire opcodes 0x60-0x63

**Reads:** `28_knowledge_wire_protocol/00_purpose.md`.
**Writes:** `crates/brain-server/src/handlers/knowledge/query.rs`.
**Done when:** QUERY / QUERY_EXPLAIN / QUERY_TRACE / RECALL_HYBRID all work.

### 23.10 SDK query builder

**Reads:** `29_knowledge_sdk/00_purpose.md` (query builder).
**Writes:** `crates/brain-sdk-rust/src/knowledge/query.rs`.
**Done when:** fluent API works for all combinations in spec examples.

### 23.11 RECALL hybrid mode integration

**Reads:** `/* no migration; see below *//00_purpose.md` (stage 5).
**Writes:** `crates/brain-server/src/handlers/substrate/recall.rs` (extended).
**Done when:** substrate RECALL transparently uses hybrid when schema exists; falls back to substrate cosine recall when no schema is declared.

### 23.12 Tests

**Writes:** `tests/knowledge_hybrid_query.rs`.
**Done when:** end-to-end tests for each routing rule; EXPLAIN/TRACE outputs make sense; performance budgets (P50 ≤ 10 ms hybrid, P99 ≤ 50 ms).

## Done-when (phase)

- Hybrid query end-to-end works.
- Router picks reasonable retrievers per query class.
- RRF fusion correct.
- EXPLAIN / TRACE useful for debugging.
- substrate RECALL goes through hybrid when schema is present.

## Pitfalls

- Don't over-tune router; the rule-based router is meant to be sensible-default, not optimal.
- Fusion `k=60` default; document how to tune.
- Push-down filter optimization is a later polish; the initial release can apply all filters post-fusion if push-down is complex.
