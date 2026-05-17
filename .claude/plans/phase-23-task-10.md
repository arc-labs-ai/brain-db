# Plan: Phase 23 — Task 10, SDK fluent query builder

> **Design lens.** The SDK is the *public* face of Brain. Its
> method names must describe what the caller is doing
> (querying, recalling, planning), not which server-side
> engine answers the call (`recall_hybrid`, `query_explain`,
> `query_trace`). Server pipelines / opcodes / "hybrid vs
> substrate" routing decisions are implementation details
> that live behind a single domain verb.

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 0. Operating assumptions

- **No DB / wire versioning.** Brain ships as one monolithic
  artifact. The SDK, wire protocol, server, and on-disk
  layout all advance together. No compatibility shims, no
  `v1` aliases kept "in case", no forward-compat indirection.
- **No `pub use X as Y` aliasing.** Every type exposed by
  the SDK has a proper domain name and its own `struct` /
  `enum` definition. Wire types are an implementation
  detail (`brain_protocol::knowledge::*Wire`); the SDK
  doesn't leak them, doesn't rename them at the `use` site,
  doesn't re-export them.
- **SDK types carry behaviour, not just data.** Each SDK
  type defined here exists because it has real business
  logic to enforce — validating constructors, ergonomic
  helpers, pattern-matchable variants, typed IDs, methods
  that protect the caller from invalid state. A struct
  whose only methods are `pub fn new(...)` and field access
  isn't worth defining; in that case we use the wire
  primitive (`u32`, `f32`, `String`) directly in the
  builder fields.
- **Boundary translation is one hop, one place.** Every
  SDK type defines `impl From<WireType> for SdkType` (and
  the reverse where the SDK constructs requests). The
  translation lives in `brain-sdk-rust/src/knowledge/query.rs`
  next to the SDK types themselves, never sprinkled across
  the codebase.

## 1. Scope

Hand-written fluent query builder for the Rust SDK, sitting on
the 23.9 wire opcodes. Targets §29/00 §"Fluent query builder"
ergonomics. **One public verb — `client.query()`** — covers
the whole hybrid surface; modifier methods on the builder
expose the cost-vs-detail spectrum (`.execute()` for results,
`.explain()` for a plan, `.trace()` for plan + execution
debug info).

```rust
// Run a hybrid query.
let results = client.query()
    .text("budget pushback from leadership")
    .with_entity(priya.id)
    .of_kinds([StatementKind::Fact, StatementKind::Event])
    .where_time(TimeRange::last_days(30))
    .with_min_confidence(0.6)
    .limit(20)
    .execute()
    .await?;

for hit in &results.items {
    match &hit.id {
        ItemRef::Memory(id)    => { /* ... */ }
        ItemRef::Statement(id) => { /* ... */ }
        ItemRef::Entity(id)    => { /* ... */ }
        ItemRef::Relation(id)  => { /* ... */ }
    }
}

// Convenience: callers who only want memories — they filter
// the heterogeneous result rather than calling a separate
// `recall_hybrid` method.
let memories: Vec<_> = results
    .items
    .iter()
    .filter_map(|h| h.id.as_memory().map(|id| (id, h.fused_score)))
    .collect();

// Or use the substrate verb, which transparently uses the
// hybrid path when a schema is declared (delivered in
// sub-task 23.11; out of scope here).
let recalled = client.recall("...").send().await?;

// EXPLAIN / TRACE share the same builder.
let plan    = client.query().text("...").explain().await?;
let traced  = client.query().text("...").trace().await?;
```

### Why no `client.recall_hybrid(...)` SDK verb

The 23.9 wire opcode `RECALL_HYBRID` exists for narrow,
memory-only callers (other-language SDKs, raw wire callers,
the hybrid path's "narrow projection" — useful at the
protocol level). At the **SDK** level the same outcome is one
filter call (`.filter_map(|h| h.id.as_memory())`) on a
`query().execute()` result. Adding a separate Rust-SDK verb
just leaks the implementation name ("hybrid") into the public
API. The existing `client.recall(...)` is the domain verb for
"give me memories by cue"; sub-task 23.11 makes the server
route it through the hybrid engine when a schema is declared.
That keeps the SDK's vocabulary stable across substrate /
knowledge deployments.

Concrete deliverables (one commit). Everything below is an
SDK-owned type in `brain-sdk-rust/src/knowledge/query.rs`:

### 1.1 Builder

- `QueryBuilder<'a>` — chainable setters for every query
  field; terminal verbs `.execute()` / `.explain()` /
  `.trace()`. Empty text + no entity anchor →
  `.execute()` rejects before a round-trip.
- **No** `RecallHybridBuilder` / `Client::recall_hybrid()`
  in the SDK. The wire `RECALL_HYBRID` opcode stays
  (shipped in 23.9 for narrow callers / other-language
  SDKs); the Rust SDK reaches the same data by filtering
  `QueryResult.items` or by calling `client.recall(...)`
  once 23.11 routes it through the hybrid engine.

### 1.2 Domain enums

- `Retriever { Semantic, Lexical, Graph }`. Methods:
  - `pub fn name(self) -> &'static str` — display name
    ("semantic" / "lexical" / "graph"); used in EXPLAIN /
    log lines.
  - `pub fn needs_text(self) -> bool` — true for Semantic,
    Lexical; false for Graph. Used by the builder to skip
    invalid combinations early.
  - `pub fn needs_anchor(self) -> bool` — true for Graph
    only.
  - `From<RetrieverWire>` + `From<Retriever> for
    RetrieverWire`.

- `RetrieverSelection { Auto, Explicit(Vec<Retriever>) }`.
  Constructors enforce server limits at the boundary:
  - `pub fn auto() -> Self` — sugar.
  - `pub fn explicit(picks: impl IntoIterator<Item = Retriever>)
    -> Result<Self, QueryBuilderError>`:
    - rejects empty list (the server would treat it as
      NoSignal post-routing — fail fast here).
    - rejects > 3 entries (`MAX_EXPLICIT_RETRIEVERS`).
    - dedups while preserving caller order.
  - `From<RetrieverSelectionWire>` etc.

- `RetrieverOutcomeStatus { Success, Skipped { reason: String },
  Timeout, Failure { message: String } }` — collapses the
  wire's `(status: u8, message: String)` pair. Methods:
  - `pub fn is_success(&self) -> bool`.
  - `pub fn is_terminal_failure(&self) -> bool` — true for
    `Failure`, false for `Skipped`/`Timeout` (which still
    let other retrievers contribute).
  - `pub(crate) fn from_wire(status: u8, message: String)
    -> Result<Self, ClientError>` — unknown byte →
    `ClientError::Protocol(MalformedPayload)`.

- `ItemRef { Memory(MemoryId) | Statement(StatementId) |
  Entity(EntityId) | Relation(RelationId) }`. Methods:
  - `pub fn kind(&self) -> ItemKind` — small enum used in
    counters / filters.
  - `pub fn as_memory(&self) -> Option<MemoryId>` (and
    `as_statement`, `as_entity`, `as_relation` siblings)
    — non-panicking accessors.
  - `pub(crate) fn from_wire(w: ItemIdWire)
    -> Result<Self, ClientError>` — unknown discriminant
    → `MalformedPayload`.

- `ItemKind { Memory, Statement, Entity, Relation }` —
  payload-free; exists so callers can `match`
  on `hit.id.kind()` without destructuring the typed id.

### 1.3 Domain structs

- `FusionConfig { pub k: u32, pub semantic_weight: f32,
  pub lexical_weight: f32, pub graph_weight: f32 }`.
  Methods:
  - `pub fn new(k: u32) -> Self` — default 1.0 weights.
  - `pub fn weights(self, sem: f32, lex: f32, graph: f32)
    -> Self` — chainable.
  - `pub fn validate(&self) -> Result<(),
    QueryBuilderError>` — k > 0, weights finite and ≥ 0.
  - `From<FusionConfigWire>` etc.

- `TimeRange { pub from_unix_ms: Option<u64>,
  pub to_unix_ms: Option<u64> }`. Methods:
  - `pub fn from_to(from_ms: u64, to_ms: u64)
    -> Result<Self, QueryBuilderError>` — rejects
    `from > to`.
  - `pub fn since(from_ms: u64) -> Self`.
  - `pub fn until(to_ms: u64) -> Self`.
  - `pub fn last_days(n: u32) -> Self` — uses
    `SystemTime::now()`.
  - `pub fn last_hours(n: u32) -> Self`.
  - `pub fn open_ended() -> Self`.
  - `pub fn contains(&self, unix_ms: u64) -> bool` —
    helpful for client-side filtering when needed.
  - `From<TimeRangeWire>` etc.

- `RetrieverContribution { pub retriever: Retriever,
  pub rank: u32, pub raw_score: f32 }`. Replaces
  `RetrieverContributionWire`; same shape but the
  `retriever` field is the SDK enum. No methods beyond
  derives.

- `RetrieverOutcome { pub retriever: Retriever,
  pub status: RetrieverOutcomeStatus,
  pub latency_ms: f64, pub result_count: u32 }`.

- `QueryHit { pub id: ItemRef, pub fused_score: f64,
  pub contributing: Vec<RetrieverContribution> }`.
  Methods:
  - `pub fn contributed_by(&self, r: Retriever) -> bool`
    — convenience for "did the graph retriever surface this".
  - `pub fn rank_in(&self, r: Retriever) -> Option<u32>`.

- `QueryResult { pub items: Vec<QueryHit>,
  pub total_latency_ms: f64,
  pub retriever_outcomes: Vec<RetrieverOutcome> }`.
  Methods:
  - `pub fn outcome(&self, r: Retriever)
    -> Option<&RetrieverOutcome>` — look up by retriever
    without iterating in user code.
  - `pub fn any_failure(&self) -> bool`.

- `ExplainResult { pub plan_text: String,
  pub estimated_cost_ms: f32 }` — separate SDK type even
  though it mirrors the wire response, so the wire type
  doesn't appear in the public SDK surface.

- `TraceResult { pub trace_text: String,
  pub total_latency_ms: f64 }` — same rationale.

### 1.4 Errors

- `QueryBuilderError` — local enum for builder-side
  validation failures (empty explicit list, too many
  retrievers, `from > to` in `TimeRange`, invalid `k`,
  empty text + no anchor). Composed into the existing
  `ClientError` via `From` so all builder verbs return
  `Result<_, ClientError>`.

### 1.5 Wiring

- Re-exports in `brain-sdk-rust/src/knowledge/mod.rs` and
  `brain-sdk-rust/src/lib.rs` for everything in §1.2-§1.4.
- One new `Client` method in
  `brain-sdk-rust/src/client/mod.rs`:
  - `pub fn query(&self) -> QueryBuilder<'_>`

  `client.recall(...)` already exists for memory-only
  retrieval; 23.11 (a later sub-task) routes it through
  the hybrid path on schema-declared deployments. Nothing
  to add here.

2. **Client entry points** added to
   `brain-sdk-rust/src/client/mod.rs`:
   ```rust
   pub fn query(&self) -> QueryBuilder<'_> { ... }
   pub fn recall_hybrid(&self, text: impl Into<String>)
       -> RecallHybridBuilder<'_> { ... }
   ```

3. **Re-exports** in `brain-sdk-rust/src/knowledge/mod.rs`
   and `brain-sdk-rust/src/lib.rs`.

4. **Unit tests** colocated:
   - Builder field accumulation: build, then inspect the
     wire request struct it produces (via a `pub(crate)
     into_wire()` test hook).
   - Retriever selection translation (Auto vs Explicit).
   - Fusion config translation.
   - TimeRange last_days computation.
   - ItemRef projection from `ItemIdWire` for all 4 kinds.

5. **Integration test**
   `crates/brain-sdk-rust/tests/knowledge_query.rs`:
   - End-to-end against the support harness shipped in the
     SDK test crate (similar pattern to
     `tests/knowledge_statement.rs`):
     - `query_executes_without_error_on_empty_shard` —
       smoke.
     - `query_explain_returns_non_empty_plan_text`.
     - `query_trace_returns_execution_block`.
     - `recall_hybrid_smoke`.

NOT in scope:
- Derive-macro contribution to schema (phase 19 already
  shipped; nothing new for query).
- Streaming results (post-v1 per spec §24/00 §"Streaming
  results").
- Subscribe extensions (separate spec section, not phase
  23).
- The "RECALL transparently dispatches hybrid when schema
  declared" behaviour — that's task 23.11, distinct from
  this commit's explicit `recall_hybrid` SDK entry.

## 2. Spec references

- `spec/29_knowledge_sdk/00_purpose.md` — §"Fluent query
  builder" target ergonomics.
- `spec/24_hybrid_query/00_purpose.md` — query shape +
  classification + filter chain (already implemented in
  23.3–23.7; this task just exposes it).
- `spec/13_sdk_design/02_core_api.md` — fluent-builder
  conventions (`.send().await` / `.execute().await`).

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `QueryRequest` wire shape | `brain-protocol::knowledge::query` | exists (shipped 23.9). |
| Existing builder pattern | `brain-sdk-rust::knowledge::statement::FactBuilder` | yes — `.subject(...).predicate(...).create().await` form. |
| `send_knowledge_request` helper | `brain-sdk-rust::knowledge::builder` | exists — borrow via `Client`. |
| `Opcode` re-exports | `brain-protocol::opcode` | exists — QueryReq, QueryExplainReq, QueryTraceReq, RecallHybridReq. |

## 4. Architecture sketch

### Module shape

`brain-sdk-rust/src/knowledge/query.rs` owns every SDK type
listed in §1. Wire types (`brain_protocol::knowledge::*Wire`
and `Query*Response`) are imported into this module only;
they never appear in the public SDK surface or in the
signatures of any other SDK module.

```rust
// Public surface (re-exported one level up at module / crate root):

pub struct QueryBuilder<'a> { client: &'a Client, /* fields */ }

pub enum   Retriever                  { Semantic, Lexical, Graph }
pub enum   RetrieverSelection         { Auto, Explicit(Vec<Retriever>) }
pub enum   RetrieverOutcomeStatus     { Success, Skipped { reason }, Timeout, Failure { message } }
pub enum   ItemRef                    { Memory(MemoryId), Statement(StatementId),
                                        Entity(EntityId), Relation(RelationId) }
pub enum   ItemKind                   { Memory, Statement, Entity, Relation }

pub struct FusionConfig               { k, semantic_weight, lexical_weight, graph_weight }
pub struct TimeRange                  { from_unix_ms, to_unix_ms }
pub struct RetrieverContribution      { retriever, rank, raw_score }
pub struct RetrieverOutcome           { retriever, status, latency_ms, result_count }
pub struct QueryHit                   { id, fused_score, contributing }
pub struct QueryResult                { items, total_latency_ms, retriever_outcomes }
pub struct ExplainResult              { plan_text, estimated_cost_ms }
pub struct TraceResult                { trace_text, total_latency_ms }

pub enum   QueryBuilderError          { ... }

// Wire interop lives here, never escapes this module:

impl From<RetrieverWire>                 for Retriever                { ... }
impl From<Retriever>                     for RetrieverWire            { ... }
impl From<RetrieverSelectionWire>        for RetrieverSelection       { ... }
impl From<&RetrieverSelection>           for RetrieverSelectionWire   { ... }
impl From<FusionConfigWire>              for FusionConfig             { ... }
impl From<FusionConfig>                  for FusionConfigWire         { ... }
impl From<TimeRangeWire>                 for TimeRange                { ... }
impl From<TimeRange>                     for TimeRangeWire            { ... }
impl From<RetrieverContributionWire>     for RetrieverContribution    { ... }
impl From<QueryExplainResponse>          for ExplainResult            { ... }
impl From<QueryTraceResponse>            for TraceResult              { ... }

// Fallible (unknown discriminant byte → MalformedPayload):
impl TryFrom<ItemIdWire>                 for ItemRef                  { ... }
impl RetrieverOutcomeStatus              { fn from_wire(...) -> Result<...> }
fn project_query_response(QueryResponse) -> Result<QueryResult, ClientError>
fn build_wire_query(&QueryBuilder<'_>)   -> Result<QueryRequest /*wire*/, ClientError>
```

### `.execute()` flow

```rust
impl<'a> QueryBuilder<'a> {
    pub async fn execute(self) -> Result<QueryResult, ClientError> {
        let wire = build_wire_query(&self)?;
        let body = RequestBody::Query(wire);
        let resp = self.client.send_knowledge_request(
            body, Opcode::QueryReq, Opcode::QueryResp,
        ).await?;
        let ResponseBody::Query(r) = resp else {
            return Err(unexpected_response("QueryResp", resp));
        };
        project_query_response(r)
    }

    pub async fn explain(self) -> Result<ExplainResult, ClientError> { ... }
    pub async fn trace(self)   -> Result<TraceResult,   ClientError> { ... }
}
```

### Client entry point

```rust
impl Client {
    /// Start a fluent query builder. Spec §29 §"Fluent
    /// query builder". The builder validates inputs at
    /// `.execute()` time; invalid combinations (empty text
    /// + no anchor, > 3 explicit retrievers, etc.) fail
    /// before the round-trip.
    ///
    /// Note: this is a richer surface than `client.recall(...)`.
    /// Use `recall` when you only want similar memories by
    /// cue text. Use `query` when you need filters, an
    /// entity anchor, or heterogeneous results
    /// (memories + statements + entities + relations).
    #[must_use]
    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }
}
```

### Error semantics

- Builder-side validation: `QueryBuilderError` enum,
  surfaced through `ClientError`'s existing error chain
  via `From<QueryBuilderError> for ClientError`.
- Server-side errors flow through the existing
  `send_knowledge_request` path; no new plumbing.
- Malformed responses (unknown `ItemIdWire.kind` byte,
  unknown `RetrieverOutcomeWire.status` byte) →
  `ClientError::Protocol(MalformedPayload)`. The server
  should never produce these, but the SDK doesn't panic if
  it does.

### `.execute()` flow

```rust
pub async fn execute(self) -> Result<QueryResult, ClientError> {
    let wire = self.into_wire();
    let body = RequestBody::Query(wire);
    let resp = self.client.send_knowledge_request(
        body, Opcode::QueryReq, Opcode::QueryResp,
    ).await?;
    let ResponseBody::Query(r) = resp else {
        return Err(ClientError::Protocol(ProtocolError::UnexpectedResponseOpcode));
    };
    project_query_response(r)
}
```

`explain()` / `trace()` are the same shape with the
`QueryExplain` / `QueryTrace` variants and their narrower
response projections.

### Client entry points

```rust
// brain-sdk-rust/src/client/mod.rs

impl Client {
    /// Start a hybrid query builder. Spec §29 §"Fluent query
    /// builder".
    #[must_use]
    pub fn query(&self) -> QueryBuilder<'_> {
        QueryBuilder::new(self)
    }

    /// Narrow RECALL-shape entry over the hybrid engine. Returns
    /// memory ids + fused scores only.
    #[must_use]
    pub fn recall_hybrid(&self, text: impl Into<String>)
        -> RecallHybridBuilder<'_>
    {
        RecallHybridBuilder::new(self, text)
    }
}
```

### Error semantics

- All builder verbs return `Result<_, ClientError>`. Re-use
  existing `ClientError` variants.
- Server `Error` frames flow through the existing
  `send_knowledge_request` plumbing — no new mapping.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Hand-written builder (this plan) | Matches existing 16/17/18 SDK builders; no macro complexity | Per-field setter boilerplate | ✓ |
| Generate setters from a macro | Less boilerplate | Query is untyped (heterogeneous results); derive macro buys little | rejected |
| `pub use RetrieverWire as Retriever` etc. (aliasing) | Zero code | Leaks `*Wire` naming into the SDK module path; no place to attach `name()` / `needs_text()` / validating constructors / business logic | **rejected — every SDK type gets its own definition with real methods** |
| Plain `pub struct FusionConfig { k, sem_w, lex_w, gr_w }` with no methods | Less code | Pure data carrier is just a wire-shape rename; we either need real validation (k>0, finite weights) or we drop it and pass primitives directly to the builder | use it **only if** it carries `validate()` + ergonomic constructors (this plan: yes) |
| Make `recall_hybrid` an inherent method on `RecallBuilder` (so `client.recall("x").hybrid()`) | Single recall surface | Conflates substrate RECALL semantics with hybrid path; 23.11 is where existing `client.recall()` transparently goes hybrid | rejected |
| Expose `build_wire_query()` publicly | Maximal flexibility | Anyone who needs wire shape can use `brain-protocol::knowledge` directly; the SDK is the abstraction | `pub(crate)` only |
| `&[StatementKind]` slices in setters vs owning `Vec` | Borrow-friendly | Builder owns state long-term; can't borrow forever | accept `IntoIterator<Item = StatementKind>` in setters; store as `Vec` |
| One `QueryResult.items` Vec for all kinds vs per-`ItemRef` vecs | Mirrors fused output order | Clients usually want grouped views | keep one Vec — clients pattern-match on `hit.id.kind()` |
| Validate at `.setter()` time vs `.execute()` time | Early failure | Builder methods would have to return `Result`, breaking the chain shape | validate **on `.execute()`** so setters stay infallible/chainable |

## 6. Risks / open questions

- **Risk:** `ItemRef::Memory` carries `MemoryId` which is a
  packed u128 (substrate); we must reconstruct it from BE
  bytes the wire emits. **Mitigation:** call
  `MemoryId::from_raw(u128::from_be_bytes(bytes))`; covered by
  unit test.
- **Risk:** `TimeRange::last_days(n)` calls
  `SystemTime::now()`, which makes the builder construction
  observable. **Mitigation:** the helper is opt-in; users
  who need determinism call `TimeRange::from_to(...)`.
- **Risk:** Auto-routing always picks Semantic for text-only
  queries; clients who installed only the lexical retriever
  on the server will get `RetrieverInvocationError::Missing`
  surfaced as a server `Internal` error. **Mitigation:**
  that's a server-side config problem, not an SDK one.
  Document in the `Retrievers` enum rustdoc.
- **Open question:** Should `explain()` and `trace()` also
  accept the same setters as `execute()`? **Resolution:**
  yes — they're terminal verbs on the same builder. The
  three methods share all setters.
- **Open question:** Should we add a `recall_hybrid()`
  helper that *only* takes the four narrow fields, or
  promote it to a full builder? **Resolution:** full
  builder with `.limit()` / `.agent_id()` / `.request_id()`
  to match the rest of the SDK.

## 7. Test plan

Unit tests inside `query.rs`:

- `query_builder_into_wire_round_trips_basic_fields` —
  set text/limit/anchor, assert the wire struct's fields
  match.
- `query_builder_translates_explicit_retrievers` — set
  `Retrievers::Explicit(vec![Semantic, Graph])`, assert wire
  `RetrieverSelectionWire::Explicit(vec![Semantic, Graph])`.
- `query_builder_translates_fusion_config` — k=30,
  weights=(1.5, 0.5, 2.0).
- `time_range_last_days_uses_now` — verify
  `from_unix_ms <= now <= to_unix_ms` (allow slack).
- `item_ref_from_wire_round_trips_each_kind` — kind bytes
  0/1/2/3 → `ItemRef::Memory/Statement/Entity/Relation`.
- `item_ref_from_wire_unknown_kind_errors`.
- `outcome_status_from_wire_translates_all_four_codes`.

Integration tests
`crates/brain-sdk-rust/tests/knowledge_query.rs` (new):
- `query_smoke_runs_on_empty_shard` — builder → execute →
  assert empty items + non-empty outcomes.
- `query_explain_returns_plan_text`.
- `query_trace_returns_execution_block`.
- `query_filtered_to_memories_via_item_ref` — confirms
  the documented "filter for memories" pattern (replaces
  the missing `recall_hybrid` SDK verb).

## 8. Commit shape

Single commit:

```
feat(sdk): 23.10 — fluent query builder

- crates/brain-sdk-rust/src/knowledge/query.rs (new):
  QueryBuilder + RecallHybridBuilder + result/view types +
  ItemRef enum + wire translation helpers. ~700 LOC
  including unit tests.
- crates/brain-sdk-rust/src/knowledge/mod.rs: register the
  new module, re-export QueryBuilder, RecallHybridBuilder,
  Retrievers, RetrieverChoice, FusionWeights, TimeRange,
  QueryResult, ExplainResult, TraceResult, ItemRef,
  RecallHybridHit, and the per-outcome view types.
- crates/brain-sdk-rust/src/lib.rs: top-level re-exports
  (Client::query, Client::recall_hybrid surface).
- crates/brain-sdk-rust/src/client/mod.rs: add
  `pub fn query(&self) -> QueryBuilder<'_>` and
  `pub fn recall_hybrid(...) -> RecallHybridBuilder<'_>`.
- crates/brain-sdk-rust/tests/knowledge_query.rs (new):
  4 integration smoke tests over the live TCP dispatcher.

Verified: cargo zigbuild --target x86_64-unknown-linux-gnu
--workspace --tests; cargo clippy -- -D warnings; cargo
test -p brain-sdk-rust --lib.
```

## 9. Confirmation

Please confirm:

1. **Module location** — `brain-sdk-rust/src/knowledge/query.rs` (per spec §29/00 §"Crate structure").
2. **One public verb only** — `client.query()`. **No `client.recall_hybrid()`** in the SDK; the wire opcode stays for narrow / non-Rust callers, but the Rust SDK leaves it unreachable from the public surface. Users who want memory-only results filter `QueryResult.items` via `ItemRef::as_memory()`, or call `client.recall(...)` (which 23.11 routes through the hybrid path on schema-declared deployments).
3. **Three modifier verbs on the builder** — `.execute()` returns results, `.explain()` returns a plan, `.trace()` returns plan + execution debug. No separate top-level methods for explain/trace.
4. **No `pub use X as Y` aliasing.** Every SDK type gets a proper domain name and its own definition. Wire types stay in `brain-protocol::knowledge::*Wire` and never appear in the SDK public surface.
5. **Every SDK type carries real behaviour** — `Retriever::needs_text/needs_anchor`, `RetrieverSelection::explicit()` validating constructor, `RetrieverOutcomeStatus::is_terminal_failure()`, `TimeRange::from_to/since/until/last_days/last_hours/open_ended/contains`, `FusionConfig::new/weights/validate`, `QueryResult::outcome/any_failure`, `QueryHit::contributed_by/rank_in`, `ItemRef::kind/as_memory/...`. If a type can't justify methods, we use the primitive instead.
6. **Validation happens at `.execute()`**, not in setters — setters stay infallible and chainable. Builder-side errors funnel through `QueryBuilderError → ClientError`.

After approval: implement + tests + commit.
