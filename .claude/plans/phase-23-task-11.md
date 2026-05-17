# Plan: Phase 23 — Task 11, transparent hybrid RECALL

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

> **Design lens.** RECALL is a domain verb — "give me memories
> by cue". The user shouldn't have to know whether the server
> answered with pure vector search, the hybrid engine, or some
> future engine. Schema-declared deployments transparently
> route through hybrid; substrate-only deployments stay on the
> vector path. Same wire opcode, same response shape, two
> extra fields on `MemoryResult` that carry the hybrid
> metadata when present.

---

## 0. Operating assumptions

- **No DB / wire versioning.** Per
  [[feedback_no_db_wire_versioning]] the new
  `MemoryResult.contributing_retrievers` and
  `MemoryResult.fused_score` fields are part of the wire
  shape unconditionally. Pre-schema callers see them empty /
  zero. No compat shims, no version negotiation.
- **No `pub use X as Y` aliasing.** The new wire
  `RetrieverNameWire` enum is its own definition next to
  `RetrieverWire` (which is `brain-protocol::knowledge::query`'s
  hybrid-query enum). The two have the same variants but
  different namespaces — `MemoryResult` is a substrate type
  and shouldn't import from the knowledge namespace just to
  rename a wire enum.
- **Domain verbs in the public API.** No new SDK or wire
  method gets the word "hybrid" in its name — the routing is
  an implementation detail of `RECALL_REQ`.

## 1. Scope

Make the substrate `RECALL_REQ` (`0x0021`) opcode
transparently route through the hybrid query engine on
schema-declared deployments.

Concrete deliverables (one commit):

### 1.1 Wire shape additions

- `brain-protocol/src/responses/cognitive.rs`:
  - Add two fields to `MemoryResult`:
    - `pub contributing_retrievers: Vec<RetrieverNameWire>`
    - `pub fused_score: f32`
  - Add a new enum `RetrieverNameWire { Semantic, Lexical,
    Graph }` (rkyv-archivable). Lives in `responses::types`
    next to the substrate's other shared wire enums so
    `MemoryResult` doesn't take a build dep on the knowledge
    namespace.
  - Add a `From<brain_protocol::knowledge::RetrieverWire>
    for RetrieverNameWire` conversion (one-hop, in the same
    module). The two enums are deliberately separate types —
    one is a substrate response field, the other names a
    knowledge-query retriever; they happen to share variants.

  Pre-schema servers populate `contributing_retrievers =
  Vec::new()` and `fused_score = 0.0`. The substrate's two
  existing fields (`similarity_score`, `confidence`) stay
  the same.

### 1.2 Per-shard schema-declared gate

- New file `brain-ops/src/schema_gate.rs`:
  - `pub struct SchemaGate(pub Arc<ArcSwap<bool>>)`.
  - `pub fn is_declared(&self) -> bool` — single `.load()`.
  - `pub fn set_declared(&self, declared: bool)` —
    `.store(Arc::new(declared))`.
  - `pub fn initial(metadata: &MetadataDb) -> Result<Self,
    OpError>` — reads `schema_namespaces(&rtxn)` once at
    startup and seeds the boolean.
- Add `schema_gate: Arc<SchemaGate>` to `OpsContext` with
  the existing `Arc<RwLock<...>>` pattern (default: gate
  built from the live metadata).
- `with_schema_gate` builder method.
- `handle_schema_upload` (in `brain-ops::ops::knowledge_schema`)
  flips the gate to `true` inside the success path, right
  after the `wtxn.commit()` returns — matches spec §28/08
  §1 ("The cutover is the redb commit").

### 1.3 `handle_recall` routing

Update `brain-ops/src/ops/recall.rs`:

```rust
pub async fn handle_recall(req, ctx) -> Result<RecallResponseFrame, OpError> {
    if ctx.schema_gate.is_declared() && req.txn_id.is_none() {
        return hybrid_recall(req, ctx).await;
    }
    substrate_recall(req, ctx).await   // existing logic, refactored out
}
```

`hybrid_recall(req, ctx)`:
- Build a planner `QueryRequest` from the RECALL request:
  text = cue_text; entity_anchor = None; kind/predicate
  filters = empty; time_filter = None; confidence_min =
  Some(req.confidence_threshold) if > 0; limit = req.top_k;
  retrievers = Auto; fusion_config = None.
- Validate against substrate-side limits (cue length already
  validated upstream).
- `plan(&q)` + `execute(&plan, &q, &exec_ctx)` reusing the
  23.7 executor (`build_executor_context(ctx)`).
- Filter the fused items for `RankedItemId::Memory(_)` and
  project to `MemoryResult` with:
  - `similarity_score = fused_score` (so clients without
    knowledge of the new field still see a comparable
    score),
  - `fused_score = item.fused_score`,
  - `contributing_retrievers = item.contributing.iter().map(|c| c.retriever.into())`.
- Fetch memory metadata (kind, context_id, created_at,
  salience) for each surviving memory id via the existing
  `MEMORIES_TABLE` lookup. Records that fail the lookup
  (e.g. tombstoned during fusion) are dropped.
- Apply the existing `req.confidence_threshold` /
  `req.salience_floor` / `req.context_filter` /
  `req.kind_filter` / `req.age_bound_unix_nanos` filters
  on top — the hybrid post-filter chain (23.5) covers these
  for statements/relations, but the substrate RECALL shape
  applies them to memories with substrate semantics (kind
  filter = MemoryKind, salience floor, etc.). Re-applying
  on the substrate side is cheaper and keeps the contract.

**Txn fallback.** When `req.txn_id` is set, route the
substrate path. Hybrid + transactional read-your-writes is
out of scope for v1 (lens layering across statements +
relations is significant work); spec §09/08 §5 only defines
substrate txn semantics. Document inline.

### 1.4 Server-side wiring

- `brain-server/src/shard/mod.rs`: build the initial
  `SchemaGate` from the per-shard `MetadataDb` and install
  it on the `OpsContext` via `.with_schema_gate(...)`.
- Nothing else in the server changes — `handle_recall` is
  already routed.

### 1.5 Tests

- **Unit tests** in `brain-ops/src/schema_gate.rs`:
  - `initial_returns_false_on_empty_metadata`.
  - `initial_returns_true_after_namespace_present`.
  - `set_declared_round_trips`.
- **Unit test** in `brain-ops/src/ops/recall.rs` (or its
  test module): route selection — `handle_recall` returns
  hybrid path when gate is true and no txn, substrate path
  when gate is false or txn is set.
- **Wire round-trip test** in `brain-protocol::responses::cognitive`
  tests: `MemoryResult` round-trips with populated
  `contributing_retrievers` + `fused_score`.
- **Server integration test** at
  `crates/brain-server/tests/recall_hybrid_routing.rs`
  (new):
  - Connect → upload trivial schema → RECALL with empty
    fixture → assert response shape; semantic outcome present
    (no observable from RECALL_RESP — but we can assert that
    items are empty and that contributing_retrievers slot
    exists in the encoded body).
  - Connect → no schema → RECALL → contributing_retrievers
    is empty, fused_score = 0.0 (substrate path).

### 1.6 SDK surface

No SDK changes required. `RecallBuilder` already produces
a `MemoryResult` view; the new fields appear on the
existing `Memory` SDK type via a regenerated mapping
helper. We update the substrate SDK's `Memory` projection
(in `brain-sdk-rust/src/ops/recall.rs`) to surface the new
fields:

- New SDK fields on `Memory` (or whatever the projected
  struct is called):
  - `contributing_retrievers: Vec<Retriever>` — reusing the
    SDK enum from 23.10 (`brain-sdk-rust::Retriever`).
  - `fused_score: f32`.

  We do **not** introduce a new SDK type for retriever
  names; the existing `Retriever` enum already has the right
  shape (and the right business methods —
  `needs_text`/`needs_anchor`/`name`).

The mapping helper translates `RetrieverNameWire ->
Retriever` via a one-hop `From`.

## 2. Spec references

- `spec/28_knowledge_wire_protocol/08_schema_optional_mode.md`
  §5 — RECALL routing definition.
- `spec/28_knowledge_wire_protocol/00_purpose.md` §"Substrate
  RECALL" — the same transparent-routing contract.
- `spec/24_hybrid_query/00_purpose.md` — what the hybrid path
  does.
- `spec/09_cognitive_operations/03_recall.md` — substrate
  semantics; unchanged.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| Hybrid plan/execute API | `brain_planner::knowledge::{plan, execute}` | shipped 23.6/23.7 |
| `HybridExecutorContext` build | `OpsContext.semantic_retriever / lexical_retriever / graph_retriever / executor.metadata` | shipped 22.5/23.1/23.2 |
| `schema_active_versions` table | `brain-metadata::schema_store::schema_namespaces` | shipped 19.x |
| `arc-swap` crate | already in workspace | yes |
| `MemoryResult` shape | `brain-protocol::responses::cognitive` | yes; will extend with two fields |
| Memory metadata lookup | `brain-metadata::tables::memory::MEMORIES_TABLE` | yes |

## 4. Architecture sketch

```
brain-ops/src/
├── schema_gate.rs                            (new)
│   pub struct SchemaGate(Arc<ArcSwap<bool>>)
│   pub fn initial(&MetadataDb) -> Result<Self, OpError>
│   pub fn is_declared(&self) -> bool
│   pub fn set_declared(&self, declared: bool)
│
├── context.rs
│   pub struct OpsContext {
│       …existing…
│       pub schema_gate: Arc<SchemaGate>,     (new)
│   }
│   impl OpsContext {
│       pub fn with_schema_gate(self, g: Arc<SchemaGate>) -> Self
│   }
│
├── ops/knowledge_schema.rs
│   handle_schema_upload(...): after commit → ctx.schema_gate.set_declared(true)
│
└── ops/recall.rs
    pub async fn handle_recall(req, ctx) -> Result<…, …> {
        if ctx.schema_gate.is_declared() && req.txn_id.is_none() {
            return hybrid_recall(req, ctx).await;
        }
        substrate_recall(req, ctx).await   // refactored old body
    }
    async fn hybrid_recall(req, ctx) -> Result<…, …> { … }
    async fn substrate_recall(req, ctx) -> Result<…, …> { … existing logic … }
    fn project_fused_memory(item: FusedItem, md_view) -> MemoryResult { … }


brain-protocol/src/responses/
├── types.rs
│   pub enum RetrieverNameWire { Semantic = 0, Lexical = 1, Graph = 2 }
│   impl From<brain_protocol::knowledge::RetrieverWire> for RetrieverNameWire { … }
│
└── cognitive.rs
    pub struct MemoryResult {
        …existing fields…
        pub contributing_retrievers: Vec<RetrieverNameWire>,
        pub fused_score: f32,
    }


brain-server/src/shard/mod.rs
    let schema_gate = Arc::new(brain_ops::schema_gate::SchemaGate::initial(
        &metadata.lock(),
    )?);
    OpsContext::new(executor_ctx)
        …
        .with_schema_gate(schema_gate)


brain-sdk-rust/src/ops/recall.rs        (small update)
    struct Memory {
        …existing…
        contributing_retrievers: Vec<Retriever>,  // reuses 23.10 enum
        fused_score: f32,
    }
```

### Behaviour matrix

| State | `req.txn_id` | Path |
|---|---|---|
| schema declared | none | hybrid (this PR) |
| schema declared | some | substrate (txn fallback; spec §09/08) |
| no schema | none | substrate |
| no schema | some | substrate |

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Add hybrid behind a request flag on RECALL | Explicit caller control | Leaks "hybrid" into the public API; conflicts with spec §28/08 §5 "transparent" contract | rejected |
| New opcode `RECALL_V2` | Clean separation | Two opcodes for the same domain action; violates spec; we'd also have to update every SDK | rejected |
| Reuse `RetrieverWire` from the knowledge namespace inside `MemoryResult` | Less code | Substrate response type takes a dep on the knowledge namespace; smells | use a separate `RetrieverNameWire` in `responses::types`, with a `From` from the knowledge enum |
| Per-request planner build vs cached plan | Faster | Plans are cheap; caching adds invalidation surface | per-request |
| Push the hybrid txn semantics into v1 | Symmetric | Lens layering across statements + relations is multi-week work; not in 23 scope | defer; substrate path when `txn_id.is_some()` |
| Use `parking_lot::RwLock<bool>` instead of `ArcSwap<bool>` | Familiar | RECALL reads on the hot path; ArcSwap is lock-free reads (the spec §28/08 §1 explicitly calls for ArcSwap) | ArcSwap |
| Drop the cheap re-application of substrate filters (kind, context, salience floor) on top of the hybrid path | Less code | RECALL clients have a documented substrate-filter contract that the hybrid post-filter chain doesn't exactly match (e.g., `salience_floor` semantics) | keep |

## 6. Risks / open questions

- **Risk:** the hybrid engine doesn't currently fetch
  `MemoryResult.text`. Existing `handle_recall` doesn't
  either (`text: hit.text.unwrap_or_default()` →
  `String::new()` when not included), so we match that.
  Spec §09/03 §15 covers `include_text` as a cost; v1
  hybrid keeps the same default (no text inline). **Mitigation:**
  document inline; matches existing behaviour.
- **Risk:** memory-only filter on `FusedItem.id` drops
  statement/entity/relation hits that the hybrid engine
  surfaces. RECALL is a memory-only verb; we want this.
  **Mitigation:** assert in code: the projection skips
  non-Memory ids; counters bump for observability.
- **Risk:** Test harness for the gate flip — committing a
  schema then immediately recalling needs the gate to have
  flipped. Since the same `OpsContext` instance handles
  both, `set_declared(true)` is visible before the response
  frame returns. **Mitigation:** integration test asserts
  this sequencing.
- **Open question:** when the gate is `true` but all three
  retriever slots are `None` (broken config), should we
  fall back to substrate or error? **Resolution:** fall
  back to substrate (`tracing::warn!` once), so a
  misconfigured deployment is still functional. Update
  `hybrid_recall` to fall through on `ExecutionError::MissingRetriever`.

## 7. Test plan

- Unit tests in `brain-ops::schema_gate`:
  - empty metadata → `is_declared() == false`.
  - after a `SchemaUpload`-style commit → `is_declared() ==
    true`.
  - explicit set/flip.
- Unit test in `brain-ops::ops::recall`:
  - mocked `OpsContext` with gate=true, no txn → hybrid path
    invoked (assert via observation that retriever traits
    are called; or assert that the function dispatches via
    `tracing` span name).
  - gate=false → substrate path.
  - gate=true + txn → substrate path.
- Wire test in `brain-protocol`:
  - `MemoryResult` round-trips with non-empty
    `contributing_retrievers` and non-zero `fused_score`.
- Server integration test
  `crates/brain-server/tests/recall_hybrid_routing.rs`
  (new):
  - **Pre-schema RECALL**: `contributing_retrievers ==
    []`, `fused_score == 0.0`.
  - **Post-schema RECALL**: upload a trivial schema, then
    RECALL — assert that the response carries
    `contributing_retrievers` mirroring the auto-router's
    decision for text-only queries (`[Semantic]` on an
    empty fixture).
  - **Post-schema RECALL inside a txn**: opens a txn, runs
    RECALL — assert `contributing_retrievers == []` (txn
    fallback).
- SDK unit test (in `brain-sdk-rust::ops::recall`): mock-
  server round-trip of the new fields.

## 8. Commit shape

Single commit:

```
feat(protocol,ops,server,sdk): 23.11 — RECALL transparent hybrid

- brain-protocol: extend MemoryResult with
  `contributing_retrievers: Vec<RetrieverNameWire>` and
  `fused_score: f32`. Add RetrieverNameWire enum in
  responses::types. Pre-schema servers populate empty/zero;
  post-schema servers populate from the hybrid pipeline.
- brain-ops/schema_gate.rs (new): SchemaGate(ArcSwap<bool>)
  with initial() / is_declared() / set_declared(). Wired
  through OpsContext::with_schema_gate.
- brain-ops/ops/knowledge_schema.rs: flip the gate on
  successful (dry_run=false) SCHEMA_UPLOAD commit.
- brain-ops/ops/recall.rs: route through hybrid_recall when
  the gate is set AND no txn_id is present; otherwise the
  existing substrate path. Falls back to substrate on
  MissingRetriever.
- brain-server/src/shard/mod.rs: build SchemaGate from
  per-shard metadata at spawn; install on OpsContext.
- brain-sdk-rust/src/ops/recall.rs: surface the two new
  fields on the SDK Memory view, reusing the 23.10
  `Retriever` enum.
- brain-server/tests/recall_hybrid_routing.rs (new):
  pre-schema vs post-schema vs txn matrix.

Verified: cargo zigbuild --target x86_64-unknown-linux-gnu
--workspace --tests; cargo clippy -- -D warnings;
cargo test -p brain-protocol --lib; cargo test
-p brain-sdk-rust --lib; cargo test -p brain-ops --lib.

Spec: §28/08 §5 + §09/03.
```

## 9. Confirmation

Please confirm:

1. **Two new `MemoryResult` fields** added unconditionally to the wire shape (no versioning shims): `contributing_retrievers: Vec<RetrieverNameWire>` and `fused_score: f32`.
2. **Separate `RetrieverNameWire` enum** in `responses::types`, not a re-use of the knowledge namespace's `RetrieverWire` — substrate types stay free of the knowledge namespace.
3. **Per-shard `SchemaGate(Arc<ArcSwap<bool>>)`** lives in `brain-ops`; flipped by `handle_schema_upload` and seeded from metadata at startup. Spec §28/08 §1 specifies ArcSwap.
4. **Txn fallback** — when `req.txn_id.is_some()`, RECALL stays on the substrate path even if the gate is declared. Documented in code; deferred to a later phase.
5. **`MissingRetriever` fallback** — if the gate says "declared" but the executor reports a missing retriever, log once at `warn` and fall back to the substrate path rather than failing the call.
6. **SDK reuses the 23.10 `Retriever` enum** — no new SDK type for retriever names on `MemoryResult`.

After approval: implement + tests + commit.
