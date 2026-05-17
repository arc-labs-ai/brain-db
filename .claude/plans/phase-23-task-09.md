# Plan: Phase 23 — Task 09, Wire opcodes 0x0160-0x0163

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Expose the hybrid query pipeline (23.6/23.7/23.8) over the
wire. Adds four opcode pairs in the `0x0160-0x0163` /
`0x01E0-0x01E3` ranges (the knowledge namespace's next free
block after relations 0x015x):

| Req opcode | Resp opcode | Name | Purpose |
|---|---|---|---|
| `0x0160` | `0x01E0` | `Query` | Run hybrid query end-to-end; return fused items + (optional) metadata. |
| `0x0161` | `0x01E1` | `QueryExplain` | Run the planner (23.6); return plan text (23.8 `render_plan`). No execution. |
| `0x0162` | `0x01E2` | `QueryTrace` | Run the executor (23.7); return plan + execution metadata text (23.8 `render_trace`). |
| `0x0163` | `0x01E3` | `RecallHybrid` | RECALL-shaped alias of `Query` that explicitly opts into the hybrid path (vs RECALL's transparent dispatch in 23.11). Returns memory ids + scores only. |

Concrete deliverables:

1. brain-protocol opcodes — 4 new `Req` + 4 new `Resp` entries
   in `crates/brain-protocol/src/opcode.rs`.
2. Wire types in `crates/brain-protocol/src/knowledge/query.rs`
   (new module):
   - `QueryRequest` (wire) with fields covering the planner's
     `QueryRequest` minus `text` representation (wire uses
     `String`).
   - `RetrieverSelectionWire` (`Auto` / `Explicit(Vec<RetrieverWire>)`).
   - `RetrieverWire { Semantic | Lexical | Graph }`.
   - `TimeRangeWire` (`{from_ms, to_ms}`).
   - `FusionConfigWire { k, semantic_w, lexical_w, graph_w }`.
   - `QueryResponse { items: Vec<QueryResultItem>,
     metadata: QueryMetadataWire }`.
   - `QueryResultItem { id: ItemIdWire, fused_score: f64,
     contributing: Vec<RetrieverContributionWire> }`.
   - `ItemIdWire { kind: u8, bytes: [u8; 16] }` — discriminant
     0=Memory, 1=Statement, 2=Entity, 3=Relation.
   - `QueryExplainRequest` (just wraps `QueryRequest`).
   - `QueryExplainResponse { plan_text: String,
     estimated_cost_ms: f32 }`.
   - `QueryTraceRequest` (just wraps `QueryRequest`).
   - `QueryTraceResponse { trace_text: String,
     total_latency_ms: f64 }`.
   - `RecallHybridRequest { text, limit, agent_id_filter }`
     — narrow surface matching RECALL-style memory recall.
   - `RecallHybridResponse { items: Vec<MemoryHit> }` with
     `MemoryHit { memory_id, fused_score }`.
3. Codec — `rkyv`-derived `Archive + Serialize + Deserialize`
   on every wire type (matches the §28 §"Wire encoding" pattern
   used by every other §28 op).
4. brain-ops handler module
   `crates/brain-ops/src/ops/knowledge_query.rs`:
   - `handle_query(req, ctx)` — builds planner `QueryRequest`
     from the wire request, calls `plan + execute`, projects
     `QueryResult` → `QueryResponse`.
   - `handle_query_explain(req, ctx)` — plan only; returns
     `QueryExplainResponse { plan_text: render_plan(&qp),
     estimated_cost_ms: qp.estimated_cost_ms }`.
   - `handle_query_trace(req, ctx)` — full execute; returns
     `QueryTraceResponse { trace_text: render_trace(&qp,
     &result.metadata), total_latency_ms }`.
   - `handle_recall_hybrid(req, ctx)` — narrow projection that
     keeps only Memory ids in the response.
5. Dispatch wiring in
   `crates/brain-server/src/network/dispatch.rs` (or
   wherever the §28 opcodes already route) — match the four
   new opcodes to the handlers.
6. **Reuses existing OpsContext slots**. The handler builds a
   `HybridExecutorContext` from `ctx.semantic_retriever`,
   `ctx.lexical_retriever`, `ctx.graph_retriever`,
   `ctx.executor.metadata`. If any slot is `None` and the
   planner picks that retriever, the executor returns
   `MissingRetriever` → the handler maps to
   `OpError::ServiceUnavailable("retriever not configured")`.
7. Integration tests in
   `crates/brain-server/tests/knowledge_query_wire.rs`:
   - Wire-smoke test for each of the 4 opcodes — encode,
     dispatch, decode response.
   - EXPLAIN-only doesn't invoke retrievers (verify with mock
     retrievers that count calls).
   - QUERY end-to-end against a small fixture (3 memories
     indexed, 1 hit returned).

NOT in scope:
- Streaming responses (limit > 100) — post-v1.
- Wire VERSION bump — adding new opcodes is forward-compatible
  per §28 §"Wire encoding" rules.
- SDK builder (23.10).
- RECALL transparent hybrid (23.11) — that's a separate
  sub-task; this commit only exposes the explicit
  RECALL_HYBRID opcode.

## 2. Spec references

- `spec/28_knowledge_wire_protocol/00_purpose.md` — opcode
  allocation conventions.
- `spec/24_hybrid_query/00_purpose.md` §"Query shape" + §"Result
  shape" — the wire types mirror these.
- `spec/03_wire_protocol/03_frame_header.md` — opcode bits.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| Next free opcode block | `crates/brain-protocol/src/opcode.rs` | `0x0156`+ free (relations end at `0x0155`); we take `0x0160-0x0163` to leave room. |
| `rkyv` archive pattern | every existing `§28` request file | Yes — `#[derive(Archive, Serialize, Deserialize)]` + `#[archive(check_bytes)]`. |
| `OpError::ServiceUnavailable` | brain-ops::error | Exists. |

## 4. Architecture sketch

### Opcode allocation

```rust
// crates/brain-protocol/src/opcode.rs (append after relations)

// §28 hybrid query operations (0x0160-0x0163) — phase 23.9.
QueryReq = 0x0160,
QueryResp = 0x01E0,
QueryExplainReq = 0x0161,
QueryExplainResp = 0x01E1,
QueryTraceReq = 0x0162,
QueryTraceResp = 0x01E2,
RecallHybridReq = 0x0163,
RecallHybridResp = 0x01E3,
```

### Wire types (brain-protocol/src/knowledge/query.rs)

```rust
use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryRequest {
    pub text: String,
    pub entity_anchor: Option<[u8; 16]>,
    pub kind_filter: Vec<u8>,           // StatementKind bytes
    pub predicate_filter: Vec<u32>,
    pub time_filter: Option<TimeRangeWire>,
    pub confidence_min: Option<f32>,
    pub include_tombstoned: bool,
    pub include_superseded: bool,
    pub limit: u32,
    pub retrievers: RetrieverSelectionWire,
    pub fusion_config: Option<FusionConfigWire>,
    pub request_id: [u8; 16],
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum RetrieverSelectionWire {
    Auto,
    Explicit(Vec<RetrieverWire>),
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
#[repr(u8)]
pub enum RetrieverWire { Semantic = 0, Lexical = 1, Graph = 2 }

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct TimeRangeWire {
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct FusionConfigWire {
    pub k: u32,
    pub semantic_weight: f32,
    pub lexical_weight: f32,
    pub graph_weight: f32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryResponse {
    pub items: Vec<QueryResultItem>,
    pub total_latency_ms: f64,
    pub retriever_outcomes: Vec<RetrieverOutcomeWire>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryResultItem {
    pub id: ItemIdWire,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContributionWire>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ItemIdWire {
    pub kind: u8,   // 0=Memory, 1=Statement, 2=Entity, 3=Relation
    pub bytes: [u8; 16],
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RetrieverContributionWire {
    pub retriever: RetrieverWire,
    pub rank: u32,
    pub raw_score: f32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RetrieverOutcomeWire {
    pub retriever: RetrieverWire,
    pub status: u8,   // 0=Success, 1=Skipped, 2=Timeout, 3=Failure
    pub message: String,  // Skipped reason or Failure msg; "" for others
    pub latency_ms: f64,
    pub result_count: u32,
}

// EXPLAIN.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryExplainRequest {
    pub query: QueryRequest,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryExplainResponse {
    pub plan_text: String,
    pub estimated_cost_ms: f32,
}

// TRACE.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryTraceRequest {
    pub query: QueryRequest,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct QueryTraceResponse {
    pub trace_text: String,
    pub total_latency_ms: f64,
}

// RECALL_HYBRID.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallHybridRequest {
    pub text: String,
    pub agent_id_filter: Option<[u8; 16]>,
    pub limit: u32,
    pub request_id: [u8; 16],
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RecallHybridResponse {
    pub items: Vec<MemoryHit>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MemoryHit {
    pub memory_id: u128,
    pub fused_score: f64,
}
```

### Handler (brain-ops/src/ops/knowledge_query.rs)

```rust
pub async fn handle_query(
    req: QueryRequest,
    ctx: &OpsContext,
) -> Result<QueryResponse, OpError> {
    let planner_req = wire_to_planner_request(req)?;
    let plan = brain_planner::knowledge::planner::plan(&planner_req)
        .map_err(map_plan_error)?;
    let exec_ctx = build_executor_context(ctx)?;
    let result = brain_planner::knowledge::executor::execute(
        &plan, &planner_req, &exec_ctx,
    ).map_err(map_executor_error)?;
    Ok(project_response(result))
}
```

The wire-to-planner and result-to-wire translations are
mechanical — `wire_to_planner_request` builds the planner's
typed `QueryRequest` from the wire shape (handling
`RetrieverSelectionWire`, `FusionConfigWire`, kind byte →
enum, etc.); `project_response` walks the fused items and
their `RankedItemId` to produce `ItemIdWire`.

### Dispatch wiring

```rust
// brain-server/src/network/dispatch.rs — extend the §28 match arm.

Opcode::QueryReq => {
    let req: QueryRequest = decode(frame)?;
    let resp = brain_ops::ops::knowledge_query::handle_query(req, ctx).await?;
    encode_response(Opcode::QueryResp, resp)
}
// ... same shape for QueryExplain / QueryTrace / RecallHybrid
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| 4 separate opcodes (this plan) | Each is independent; clients can issue EXPLAIN without paying for execution | More wire surface | ✓ — matches §24/00 |
| Single QUERY opcode with `mode` flag (Run/Explain/Trace) | One opcode | Couples request shape to mode; complicates clients | rejected |
| Make `RecallHybrid` an alias of `Query` server-side | Less code | Loses the narrow surface (just text + limit + agent filter) that simple clients want | rejected — small surface justifies the opcode |
| Wire types in a single file vs split by op | Smaller file count | Per-op files match the existing `brain-protocol::knowledge::*` pattern | this plan uses a single `query.rs` for cohesion since all four share the request DAG |
| Include the full `QueryMetadata` in the QUERY response | Symmetry with TRACE | Bloats every QUERY response with diagnostic data | rejected — QUERY includes a slimmer `retriever_outcomes` summary; TRACE is the verbose path |

## 6. Risks / open questions

- **Risk:** `RetrieverSelectionWire::Explicit(Vec<RetrieverWire>)` — the `Vec` length should be capped to prevent malicious payloads. **Mitigation:** validate at decode (cap at 3 — matches `MAX_RETRIEVERS`).
- **Risk:** `text` length unbounded → memory abuse. **Mitigation:** the wire layer's general payload-size cap covers this; the handler also bounds at 16 KB (matches RECALL's text-bound limit).
- **Risk:** `Option<u128>` for memory_id — rkyv handles fine. **Mitigation:** none needed.
- **Open question:** Should `Query` accept a pre-embedded vector? **Resolution:** post-v1; the wire today is text-only (Text path). Adding Vector path is a future opcode extension.
- **Open question:** Should `RecallHybrid` return Memory-only ids, or pass-through whatever the planner produces? **Resolution:** Memory-only. The narrow surface is explicit — clients expecting memories shouldn't get statements back from RECALL.

## 7. Test plan

Integration tests in
`crates/brain-server/tests/knowledge_query_wire.rs`:

- `query_smoke_round_trips_a_simple_request` — encode QUERY
  for `text="topic"`, send through the dispatcher, decode
  QueryResponse, assert empty `items` (no data in fixture
  shard yet) + non-empty `retriever_outcomes`.
- `query_explain_returns_plan_text_without_execution` —
  EXPLAIN over a request with `text` + an entity anchor;
  response.plan_text contains "RETRIEVERS:" and
  "GraphRetriever".
- `query_trace_returns_execution_block` — TRACE response
  contains "EXECUTION:" + per-retriever latency lines.
- `recall_hybrid_filters_to_memory_ids` — fixture with one
  memory indexed; RECALL_HYBRID response contains exactly
  that memory id.
- `query_no_signal_returns_invalid_request_error` — text +
  anchor both empty + no filters → wire error mapped from
  `PlanError::NoSignal`.
- `query_request_id_round_trips` — request_id echo'd back in
  response metadata (for idempotency tracing).

Unit tests in brain-protocol for wire encode/decode of every
new type — round-trip a value through `rkyv` and assert
equality. Covers `QueryRequest`, `QueryResponse`,
`ItemIdWire`, `RetrieverOutcomeWire`, etc.

## 8. Commit shape

Single commit:

```
feat(protocol,ops,server): 23.9 — QUERY / EXPLAIN / TRACE / RECALL_HYBRID wire

- crates/brain-protocol/src/opcode.rs: add 8 entries
  (Query / QueryExplain / QueryTrace / RecallHybrid +
  matching Resp at 0x01E0-0x01E3). No version bump.
- crates/brain-protocol/src/knowledge/query.rs (new): wire
  types + rkyv codecs (~15 archived structs/enums).
- crates/brain-ops/src/ops/knowledge_query.rs (new): four
  handlers + wire⇄planner translation helpers. Reuses the
  semantic / lexical / graph retriever slots already on
  OpsContext (23.1 / 22.5 / 23.2). Missing retriever →
  ServiceUnavailable. PlanError::NoSignal → InvalidRequest.
- crates/brain-server/src/network/dispatch.rs: route the
  four new opcodes.
- crates/brain-server/tests/knowledge_query_wire.rs (new):
  6 integration tests via the TCP dispatcher.
- crates/brain-protocol/tests/...: wire-roundtrip unit
  tests for new types.
```

## 9. Confirmation

Please confirm:

1. **Opcode range `0x0160-0x0163` / `0x01E0-0x01E3`** — next free §28 block after relations 0x015x.
2. **Four separate opcodes** (vs single QUERY-with-mode) — matches §24/00 split.
3. **`RecallHybrid` returns Memory ids only** — narrow surface; clients of RECALL expect memories.
4. **No wire VERSION bump** — new opcodes are forward-compatible.
5. **Text length cap = 16 KiB** at handler entry (matches RECALL's existing bound).

After approval: implement + tests + commit.
