# 28.13 Query Wire Frames

Request/response body schemas for `0x0160–0x0163` — the hybrid-query opcodes. These are the primary read API of the knowledge layer and accept traffic regardless of schema state.

Cross-references:
- [`../23_retrievers/00_purpose.md`](../23_retrievers/00_purpose.md) — three retrievers (semantic / lexical / graph) + RRF fusion.
- [`../24_hybrid_query/00_purpose.md`](../24_hybrid_query/00_purpose.md) — query model, filters, planner.
- [`../03_wire_protocol/09_streaming.md`](../03_wire_protocol/09_streaming.md) — substrate streaming model reused here.

## 1. Opcode index

| Opcode | Name | Section | Status |
|---|---|---|---|
| `0x0160` | `QUERY` | §3 | spec-only — phase 23 |
| `0x0161` | `QUERY_EXPLAIN` | §4 | spec-only — phase 23 |
| `0x0162` | `QUERY_TRACE` | §5 | spec-only — phase 23 |
| `0x0163` | `RECALL_HYBRID` | §6 | spec-only — phase 23 |

Responses live at `0x01E0–0x01E3`.

`QUERY` (`0x0160`) is the primary structured query opcode. `RECALL_HYBRID` (`0x0163`) is the simple-text fast path used by clients that just want hybrid text-only retrieval without an explicit query language.

The substrate's `RECALL_REQ` (`0x0021`) runs the hybrid path by default in every deployment (see [`./08_schema_optional_mode.md`](./08_schema_optional_mode.md) §5). The wire response carries `contributing_retrievers` and `fused_score` populated whether or not a schema has been declared.

## 2. Shared types

### 2.1 `QueryRequest`

```rust
pub struct QueryRequest {
    pub query_dsl: String,                  // structured query DSL; phase 24 grammar
    pub top_k: u32,                         // 1..=1000
    pub filters: QueryFilters,
    pub retriever_selection: RetrieverSelection,
    pub budget_wall_time_ms: u32,           // 1..=60000
    pub include_provenance: bool,
    pub include_trace: bool,                // used by QUERY_TRACE only
    pub schema_version: u32,                // 0 = current
    pub request_id: WireUuid,               // [0;16] = no idempotency cache
    pub txn_id: WireUuid,                   // [0;16] = no transaction
}

pub struct QueryFilters {
    pub entity_type_id: u32,                // 0 = no filter
    /// Predicate filter as canonical `"namespace:name"` qnames.
    /// Empty vec = no filter. The planner resolves each qname through
    /// the predicate registry per request — unknown qnames produce an
    /// empty result set in open-vocabulary mode and `PredicateNotInSchema`
    /// (0x004B) when a schema is active for the namespace.
    pub predicate_filter: Vec<String>,
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub min_confidence: f32,
    pub context_ids: Vec<u64>,              // empty = no filter
    pub kind_filter: Vec<StatementKindWire>, // empty = no filter
    pub min_salience: f32,                  // for substrate-side filter
}

pub struct RetrieverSelection {
    pub use_semantic: bool,                 // default true
    pub use_lexical: bool,                  // default true
    pub use_graph: bool,                    // default true
    pub semantic_top_k: u32,                // per-retriever top-K; 0 = retriever default
    pub lexical_top_k: u32,
    pub graph_top_k: u32,
    pub rrf_k_constant: u32,                // RRF k parameter; 0 = default 60 per §23
}
```

Semantics:

- The query DSL is structured (phase 24 grammar) — combinations of entity / predicate / time / confidence conditions. For text-only queries the SDK builds the DSL automatically (or use `RECALL_HYBRID` §6).
- `RetrieverSelection` lets clients disable retrievers or override per-retriever depth. Setting all three to false → `INVALID_ARGUMENT`.
- `top_k` is the **final fused** top-K. Per-retriever `top_k`s are typically larger to give RRF a useful candidate pool (default: `4 * top_k`).
- `budget_wall_time_ms` is a soft budget. The server returns whatever it has when exceeded with `QUERY_TIMEOUT` on the final frame.

### 2.2 `QueryResult` — streamed per-frame item

```rust
pub struct QueryResultItem {
    pub kind: ResultKind,                   // Entity / Statement / Relation / Memory
    pub entity: EntityView,                 // populated when kind=Entity
    pub statement: StatementView,           // populated when kind=Statement
    pub relation: RelationView,             // populated when kind=Relation
    pub memory: MemoryResult,               // populated when kind=Memory; substrate shape
    pub fused_score: f32,                   // post-RRF rank score
    pub contributing_retrievers: Vec<u8>,   // bit-flag values; see §2.4
    pub explanation: String,                // human-readable why-this-result; empty when !include_provenance
}

#[repr(u8)]
pub enum ResultKind {
    Entity = 1,
    Statement = 2,
    Relation = 3,
    Memory = 4,
}

pub struct QueryResultTail {
    pub total_returned: u32,
    pub fused_from_candidate_pool_size: u32, // pre-fusion candidate count
    pub retriever_timings_ms: RetrieverTimings,
    pub truncated_by: u8,                   // 0=none, 1=top_k, 2=budget
}

pub struct RetrieverTimings {
    pub semantic_ms: u32,
    pub lexical_ms: u32,
    pub graph_ms: u32,
    pub fusion_ms: u32,
    pub total_ms: u32,
}
```

Field discipline: only the field matching `kind` is populated; the others carry zero-filled shapes. This is the rkyv equivalent of a tagged union (avoids the archive-cost of true enums for the per-item path).

### 2.3 `MemoryResult` reuse

The `MemoryResult` substrate type ([`../03_wire_protocol/08_response_frames.md`](../03_wire_protocol/08_response_frames.md)) is reused unchanged. Knowledge-layer hybrid-retrieval results that include memories carry the substrate type's existing fields; the post-schema additions (`contributing_retrievers`, `fused_score`) live on the **outer** `QueryResultItem` rather than mutating `MemoryResult`. Forward-compatible with substrate-only clients.

### 2.4 `contributing_retrievers` bit-flag values

```rust
pub const RETRIEVER_SEMANTIC: u8 = 0b001;
pub const RETRIEVER_LEXICAL: u8  = 0b010;
pub const RETRIEVER_GRAPH: u8    = 0b100;
```

A `QueryResultItem.contributing_retrievers = vec![0b011]` means semantic + lexical ranked this result; graph did not. Each contributor contributes one entry to the vector with a separate flag — `vec![0b001, 0b010]` is also valid encoding meaning "two separate retriever hits" with provenance.

## 3. QUERY (0x0160)

### 3.1 Request

`QueryRequest` (§2.1) directly.

### 3.2 Response — streaming

Multiple `QueryResultItem` frames sharing `stream_id`, followed by a tail frame:

```text
S → C  frame: opcode=0x01E0 stream_id=N        body: QueryResultItem  (intermediate)
S → C  frame: opcode=0x01E0 stream_id=N        body: QueryResultItem  (intermediate)
...
S → C  frame: opcode=0x01E0 stream_id=N EOS    body: QueryResultTail  (tail)
```

The substrate streaming model: per-frame, `EOS` on the tail. The tail body is a different rkyv struct than the per-item bodies — clients dispatch on `is_final` (set when EOS is set) and decode accordingly.

### 3.3 Errors

- `QUERY_TIMEOUT` (substrate `Unavailable`) — wall budget exceeded. Tail frame carries whatever results were ready; clients see a partial result with `QueryResultTail.truncated_by = 2`.
- `QUERY_OVER_BUDGET` (substrate `ResourceExhausted`) — per-shard memory or candidate-pool cap blown. Frame stream ends without an EOS tail; an `ERROR` frame closes the stream.
- `PredicateNotInSchema` (0x004B) — strict mode only; `filters.predicate_filter` contains a qname not declared in the active schema.
- `RelationTypeNotInSchema` (0x004C) — strict mode only; the DSL or graph step referenced an unknown relation type.
- `HybridUnavailable` (0x0083) — a required retriever component is not currently servable (e.g. inside a transaction, during index rebuild).
- `INVALID_ARGUMENT` — DSL parse failure, `top_k > 1000`, all retrievers disabled.

### 3.4 Cancellation

Clients send `CANCEL_STREAM` (`0x0050`) with the offending `stream_id`. Server emits a `CANCEL_STREAM_ACK` (`0x00D0`) on a different stream; the query's frame stream ends with EOS-flagged empty tail.

## 4. QUERY_EXPLAIN (0x0161)

Returns the planner's execution plan **without running it**. Useful for debugging and cost-bounded clients.

### 4.1 Request

`QueryRequest` (§2.1). `include_trace` and `top_k` are ignored.

### 4.2 Response — `QueryExplainResponse`

```rust
pub struct QueryExplainResponse {
    pub plan: QueryPlan,
    pub estimated_cost: PlanCost,
    pub estimated_wall_time_ms: u32,
    pub warnings: Vec<String>,              // schema deprecation, etc.
}

pub struct QueryPlan {
    pub steps: Vec<QueryPlanStep>,
}

pub struct QueryPlanStep {
    pub step_index: u32,
    pub operation: String,                  // e.g. "SemanticRetrieve(entity_hnsw, k=40)"
    pub input_cardinality_estimate: u32,
    pub output_cardinality_estimate: u32,
    pub cost: f32,                          // matched §07 cost model
}

pub struct PlanCost {
    pub vector_search_ms: u32,
    pub tantivy_query_ms: u32,
    pub graph_walk_ms: u32,
    pub fusion_ms: u32,
    pub total_ms: u32,
}
```

### 4.3 Errors

Same as `QUERY` minus the timeout / over-budget set (no execution happens).

## 5. QUERY_TRACE (0x0162)

Identical to `QUERY` but the response carries **per-retriever debug info**. Treats `include_trace = true` internally regardless of the request's setting.

### 5.1 Response — streaming with extended tail

Per-item frames are `QueryResultItem` (§2.2) plus a `trace` field. The tail body is extended:

```rust
pub struct QueryTraceTail {
    pub base: QueryResultTail,
    pub per_retriever_traces: Vec<RetrieverTrace>,
    pub planner_log: Vec<String>,
}

pub struct RetrieverTrace {
    pub retriever: u8,                      // bit-flag value from §2.4
    pub candidate_count: u32,
    pub timing_ms: u32,
    pub top_5_summaries: Vec<String>,       // pre-fusion ranked summaries
    pub debug_notes: Vec<String>,
}
```

### 5.2 Performance note

`QUERY_TRACE` is **noticeably slower** than `QUERY` because of the trace bookkeeping. SDKs should expose it as a debug-only operation. Production hot paths use `QUERY` (`0x0160`).

## 6. RECALL_HYBRID (0x0163)

Text-only fast path. The server's planner builds a default `QueryRequest` from the text + minimal filters and runs it.

### 6.1 Request — `RecallHybridRequest`

```rust
pub struct RecallHybridRequest {
    pub text: String,                       // non-empty; ≤ 4 KiB
    pub top_k: u32,                         // 1..=1000
    pub min_confidence: f32,
    pub context_ids: Vec<u64>,              // empty = no filter
    pub time_range_start_unix_nanos: u64,
    pub time_range_end_unix_nanos: u64,
    pub budget_wall_time_ms: u32,
    pub request_id: WireUuid,
    pub txn_id: WireUuid,
}
```

### 6.2 Response

Same shape as `QUERY` (streamed `QueryResultItem` frames + `QueryResultTail`). Clients that want both substrate-style memory results and knowledge-layer entity / statement / relation results use this opcode.

### 6.3 Relationship to substrate `RECALL_REQ`

`RECALL_REQ` (`0x0021`) returns **only** `MemoryResult`s — its substrate contract.

`RECALL_HYBRID` (`0x0163`) returns a mix of `MemoryResult`, `EntityView`, `StatementView`, `RelationView` — leveraging the entity / statement / relation indexes alongside the memory HNSW.

Both deployment postures may use either: the substrate `RECALL_REQ` returns memory results from the hybrid path; `RECALL_HYBRID` additionally surfaces typed entity / statement / relation results that have been populated from prior knowledge writes (open-vocabulary or schema-declared).

### 6.4 Errors

Same as `QUERY`, plus `INVALID_ARGUMENT` for empty `text`.

## 7. Idempotency for queries

Queries with `request_id != [0;16]` populate the idempotency cache (same shape as substrate; 24h TTL). Cached responses are byte-identical, **including** the full streamed result sequence. Re-issuing the same `request_id` replays the entire stream from cache.

Idempotency for queries is unusual but useful for:

- Retries after transient network failure on long-running queries.
- Reproducibility in test suites.

Clients that want **fresh** results every time pass `request_id = [0;16]`.

## 8. Transactions

`QueryRequest.txn_id != [0;16]` makes the query observe a read snapshot that includes the transaction's pending writes — same semantics as substrate `RecallRequest.txn_id` ([substrate §09/08](../09_cognitive_operations/08_transactions.md)).

Reads inside a transaction are visible to the same transaction's subsequent writes (read-your-writes).

## 9. Multi-shard fan-out

A `QUERY` typically fans out to **all** shards unless filters scope it to a specific shard (e.g. `filters.entity_type_id` + a subject filter that the planner can route).

Per-shard results are streamed to the coordinator (the agent's bound shard); the coordinator runs RRF fusion across the union and streams the final fused result to the client. The per-shard streaming back-pressure handling applies — if one shard is slow, the coordinator buffers within its budget.

The wire shape doesn't expose which shard contributed which result — that's an internal detail. `QUERY_TRACE`'s `RetrieverTrace.debug_notes` may include per-shard breakdown when run on multi-shard deployments.

## 10. Open questions

See [`./09_open_questions.md`](./09_open_questions.md) Q8 (streaming back-pressure) and Q-future entries on:

- Cross-shard error aggregation: what if 1 of 8 shards fails? Currently the planner returns a partial result with a warning. Should there be a strict-mode flag for "all-or-nothing"?
- Planner cost model exposure: should `QueryPlanStep.cost` be `f32` (current) or a richer structured cost?
- Stable cursor semantics for `QUERY` streaming pagination (current spec assumes single-shot streaming; resumable queries are post-v1.0).
