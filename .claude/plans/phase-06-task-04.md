# Sub-task 6.4 — Encode planner + executor + `WriterHandle` trait

The first write-path in Phase 6. Maps a wire `EncodeRequest` to an `EncodePlan`, then drives it through embed → context resolve → writer dispatch → response. Introduces the **`WriterHandle` trait** that all future write operations (forget, link, unlink) plug into.

Phase 6 doesn't ship the production writer task (spec §08/08 §10's group-commit channel-fed loop) — that lands in Phase 8/9. We ship the trait + an in-process fake for tests. The fake exercises the *interface*, not the durability story.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/04 §1 | `EncodeRequest { text, kind, salience_hint, edges, context_id, request_id, txn_id, deduplicate }` |
| §08/04 §3 | `EncodePlan { shard, idempotency_check, embedding, context_resolution, allocation, wal_append, apply, edges, response }` |
| §08/04 §4 | Idempotency: brief read txn against the `idempotency` table; cached response short-circuits |
| §08/04 §5 | Embed (cue cache lookup OK) |
| §08/04 §6 | Context resolution — explicit ContextId is fast-path; named contexts get-or-create |
| §08/04 §7 | Slot allocation — fast atomic; arena grows asynchronously |
| §08/04 §8 | WAL append + fsync — durability barrier (CLAUDE.md invariant 1: "WAL-before-acknowledge") |
| §08/04 §9 | Apply: arena write, metadata write, HNSW insert — *after* the durability barrier |
| §08/04 §10 | Edges insert in the same metadata txn as the memory row |
| §08/04 §11 | Response carries the new `MemoryId` |
| §08/04 §12 | Cap at 64 edges per encode |
| §08/04 §15 | Validation: text non-empty + size cap, kind ≠ `Consolidated`, salience ∈ [0, 1], edges valid |
| §08/08 §10 | Writer task: channel-fed, batches encodes, group-commits WAL; sends acks back |
| CLAUDE.md §5 | WAL-before-ack; single-writer-per-shard; idempotency by RequestId; tombstone grace; CRC everywhere |

## 1. Scope

**In scope for 6.4:**
- `crates/brain-planner/src/encode.rs` — planner side. `plan_encode(&EncodeRequest, &PlannerContext) -> Result<ExecutionPlan, PlanError>`.
- `crates/brain-planner/src/executor/writer.rs` — the **`WriterHandle` trait** + the `EncodeOp` / `EncodeAck` payloads it carries. Phase 6 ships the trait; Phase 8/9 wires the real channel-fed writer.
- `crates/brain-planner/src/executor/encode.rs` — executor side. `execute_encode(plan, ctx) -> Result<EncodeResult, ExecError>`.
- `EncodeResult` struct + `EdgeResult` row (new module `executor/result.rs` already has `RecallResult`; extend).
- `ExecutorContext` gains `writer: Arc<dyn WriterHandle>` (constructor + tests updated).
- New `ExecError` variant: `IdempotencyMissing` (we expected a cached response by id but the metadata path returned an error). Plus the writer's failures bubble up via `ExecError::Internal` for now.
- Tests:
  - Pure planner units: validation paths (zero text, oversized text, too-many edges, Consolidated kind, salience out of range), happy-path plan shape, idempotency step always present, context_resolution always `Explicit` (the wire only carries `WireContextId`).
  - Executor integration with a `FakeWriterHandle`: encode round-trips, response carries the right `MemoryId`, idempotency replay returns the cached vector, multiple encodes hand off in order.

**NOT in scope:**
- The real writer task — spec §08 §10's channel-fed group-commit loop. That's Phase 8 (workers) / Phase 9 (server) territory.
- Actual WAL fsync. The `FakeWriterHandle` doesn't write to WAL files; production writer (later phase) does.
- Re-embedding / migration (spec §14.1).
- Bulk `ENCODE_BATCH` (spec §14.2) — separate opcode, separate plan.
- Named-context get-or-create (spec §6) — wire only carries `WireContextId`. When the wire gains a name field, the `ContextResolutionStep::GetOrCreate` branch is wired up.
- `Consistency::ReadAfterWrite` for the recall executor — separate concern. We do honour WAL-before-ack: `execute_encode` only returns `Ok` after the writer acks.
- Cascading edge handling on forget — that's 6.6.

## 2. Planner-side design

### 2.1 Function signature

```rust
// crates/brain-planner/src/encode.rs

pub fn plan_encode(
    req: &brain_protocol::request::EncodeRequest,
    ctx: &PlannerContext,
) -> Result<ExecutionPlan, PlanError>;

/// Same shape as plan_recall_inner — gives tests direct access to
/// the inner struct without unwrapping.
pub fn plan_encode_inner(
    req: &EncodeRequest,
    ctx: &PlannerContext,
) -> Result<EncodePlan, PlanError>;
```

### 2.2 Validation (spec §08/04 §15)

- `req.text.is_empty()` → `InvalidParameters { field: "text", reason: "must be non-empty" }`.
- `req.text.len() > MAX_TEXT_BYTES` (1 MiB, hardcoded constant) → `InvalidParameters`.
- `req.kind == Consolidated` → `InvalidParameters { field: "kind", reason: "Consolidated is worker-only" }`.
- `req.salience_hint ∉ [0, 1]` → `InvalidParameters`.
- `req.edges.len() > config.max_edges_per_encode` → `InvalidParameters`.
- Each edge's `weight ∉ [-1, 1]` (spec §08/04 §15 says "valid kinds and weights"; spec §06 says edge weights are normalised; we accept the full bipolar range) — actually, the spec doesn't strictly pin the range. Defer to a permissive check: `weight.is_finite()`.

### 2.3 Cost + budget

```rust
let cache_hit = false;  // pessimistic
let estimated = cost::cost_encode(cache_hit, req.edges.len());
cost::check_budget(estimated, ctx)?;
```

`cost_encode` is already in 6.2.

### 2.4 Plan assembly

```rust
EncodePlan {
    shard: 0,
    idempotency_check: IdempotencyCheckStep {
        request_id: RequestId::from(req.request_id),
    },
    embedding: EmbeddingStep {
        text: req.text.clone(),
        cache_lookup: true,
    },
    context_resolution: ContextResolutionStep::Explicit(
        ContextId::from(req.context_id),
    ),
    allocation: SlotAllocationStep {
        arena_grow_if_needed: true,
    },
    wal_append: WalAppendStep {
        kind: MemoryKind::from(req.kind),
        salience_initial: req.salience_hint,
        fsync: true,
    },
    apply: ApplyStep {
        arena_write: true,
        metadata_write: true,
        hnsw_insert: true,
    },
    edges: req.edges.iter().map(|e| EdgeStep {
        edge: EdgeSpec {
            target: MemoryId::from(e.target),
            kind: EdgeKind::from(e.kind),
            weight: e.weight,
        },
        insert_in_metadata: true,
    }).collect(),
    response: EncodeResponseStep {
        persistent_id: true,
    },
    estimated_cost_ms: estimated,
}
```

The vector + text live transiently in `EmbeddingStep` (text) and the executor (vector after embed). Spec §04 §13 says plan size is ~500 bytes; the cue text dominates if large, and that's already required for the embed step.

## 3. Executor-side design

### 3.1 `WriterHandle` trait

```rust
// crates/brain-planner/src/executor/writer.rs

use std::future::Future;
use std::pin::Pin;

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};

/// Per-shard write surface. Sub-task 6.4 ships the trait + an
/// in-process fake. Phase 8/9 wires the real channel-fed
/// group-commit writer per spec §08/08 §10.
///
/// Returns `Pin<Box<dyn Future>>` so the trait is object-safe —
/// `Arc<dyn WriterHandle>` is what the executor holds.
pub trait WriterHandle: Send + Sync {
    fn submit_encode<'a>(
        &'a self,
        op: EncodeOp,
    ) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + Send + 'a>>;
}

/// Encode operation payload submitted to the writer. Contains
/// everything the writer needs to: alloc a slot, append a WAL record,
/// apply to arena/metadata/HNSW, insert edges, cache the response in
/// the idempotency table.
#[derive(Debug, Clone)]
pub struct EncodeOp {
    pub request_id: RequestId,
    pub context_id: ContextId,
    pub kind: MemoryKind,
    pub text: String,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub salience_initial: f32,
    pub fingerprint: [u8; 16],
    pub edges: Vec<EncodeOpEdge>,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeOpEdge {
    pub target: MemoryId,
    pub kind: EdgeKind,
    pub weight: f32,
}

/// Writer's ack. Spec §08/04 §11 — the response carries the new
/// `MemoryId`. The writer also reports per-edge insertion outcomes so
/// the executor can surface rejected edges (spec §08/04 §10).
#[derive(Debug, Clone)]
pub struct EncodeAck {
    pub memory_id: MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    /// `true` iff this ack came from a replayed idempotency entry;
    /// `false` for a fresh write.
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum EdgeOutcome {
    Inserted,
    /// Spec §08/04 §10: edges whose target doesn't exist are
    /// rejected; the encode proceeds without them.
    TargetMissing,
}

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("writer queue overloaded")]
    Overloaded,
    #[error("writer internal error: {0}")]
    Internal(String),
}
```

Object-safety: bare `async fn` in traits in Rust 1.95 can't be used through `dyn`. We hand-roll the `Pin<Box<dyn Future>>` return so `Arc<dyn WriterHandle>` works at the executor.

### 3.2 `execute_encode`

```rust
// crates/brain-planner/src/executor/encode.rs

pub async fn execute_encode(
    plan: EncodePlan,
    ctx: &ExecutorContext,
) -> Result<EncodeResult, ExecError>;
```

Stages:

1. **Idempotency check** (spec §08/04 §4). Brief read txn on the `idempotency` table. If a cached entry exists for `plan.idempotency_check.request_id` AND the request hash matches, return the cached `(MemoryId, edge_results)` immediately. Spec §08/04 §4's cached-response replay.

   For Phase 6 we hand the idempotency *check* to the writer too (it owns both directions of the table). The executor's `submit_encode` carries the `request_id`, and the writer either replays the cached row (ack with `replayed: true`) or executes the fresh write. This is cleaner than splitting idempotency between executor and writer.

2. **Embed**. `ctx.embedder.embed(&plan.embedding.text)?`. Same flow as recall.

3. **Context resolution**. Wire shape only ever produces `ContextResolutionStep::Explicit(id)`; just unwrap.

4. **Submit to writer**. Build `EncodeOp` from plan + embed result + ctx; call `ctx.writer.submit_encode(op).await`. Translate `WriterError` to `ExecError`.

5. **Build `EncodeResult`** from the ack.

### 3.3 `EncodeResult`

```rust
// crates/brain-planner/src/executor/result.rs (extend)

#[derive(Debug, Clone)]
pub struct EncodeResult {
    pub memory_id: brain_core::MemoryId,
    pub edge_results: Vec<EdgeOutcome>,
    pub replayed: bool,
}
```

`EdgeOutcome` is re-exported from `executor::writer` to keep one home for the type.

### 3.4 `ExecError` additions

```rust
#[error("writer rejected: {0}")]
WriterFailed(#[from] WriterError),
```

`WriterError` already covers `Overloaded` + `Internal`. We `#[from]`-wrap so `?` propagates cleanly.

### 3.5 `ExecutorContext` gains `writer`

```rust
#[derive(Clone)]
pub struct ExecutorContext {
    pub embedder: Arc<dyn Dispatcher>,
    pub index: SharedHnsw<384>,
    pub metadata: Arc<MetadataDb>,
    pub writer: Arc<dyn WriterHandle>,
}
```

Constructor `ExecutorContext::new(embedder, index, metadata, writer)` is the 6.4 signature. Existing 6.3 callers update to pass a writer; tests use `FakeWriterHandle`.

This is a non-additive change to a public API. The lib has shipped exactly one tag (`phase-5-complete`), but the planner is unreleased — we're free to break. Document in commit.

### 3.6 `FakeWriterHandle` for tests

Lives in the integration test, *not* in the lib crate. Spec §08/04 §3 says the plan is at the boundary; the test fake implements `WriterHandle` by:

- Maintaining an internal `slot_counter: AtomicU64`.
- For each encode, packing a `MemoryId::pack(shard=0, slot=counter, version=1)`.
- Writing the `MemoryMetadata` row into the test `MetadataDb`.
- Writing the vector into the test `SharedHnsw` via its `Writer`.
- For now: NO idempotency replay logic — Phase 6 tests don't exercise replay. (Cached-response path covered by a planner unit test only; executor-level replay test deferred to a follow-up sub-task.)

Actually let me revisit — spec §04 §4 mandates idempotency replay. Re-evaluation: the `FakeWriterHandle` *should* simulate idempotency or we leave it out and test the planner shape only.

Pragmatic call: the FakeWriterHandle implements *basic* idempotency — keeps a `HashMap<RequestId, EncodeAck>`. On second submit with the same RequestId, return the cached ack with `replayed: true`. Five lines of code; mirrors spec semantics.

## 4. Test plan

### 4.1 Pure planner tests (encode.rs)

- `default_request_yields_full_plan` — happy path; the 8 steps are populated.
- `empty_text_is_rejected` → `InvalidParameters { field: "text" }`.
- `oversize_text_is_rejected` — text > 1 MiB.
- `consolidated_kind_is_rejected` — spec §08/04 §15.
- `salience_out_of_range_is_rejected` — both negative and > 1.
- `too_many_edges_is_rejected` — > max_edges_per_encode (64).
- `each_edge_is_translated` — wire `EdgeRequest` → planner `EdgeSpec`.
- `idempotency_check_carries_request_id` — `IdempotencyCheckStep.request_id` matches the wire `request_id`.

### 4.2 Executor integration tests (tests/encode_end_to_end.rs)

Harness:
- Real `MetadataDb` (tempdir).
- Real `SharedHnsw`.
- `MockDispatcher` (deterministic vectors per text).
- `FakeWriterHandle` that drives both the `MetadataDb` writer and the HNSW writer.

Tests:
- `encode_round_trips_and_returns_memory_id` — encode "hello", get back a `MemoryId`, look it up in metadata, vector is searchable in the index.
- `encode_with_edges_records_them` — 3 edges → `EncodeAck::edge_results` has 3 outcomes.
- `idempotent_replay_returns_cached_ack` — submit the same request twice; second returns `replayed: true` with the same MemoryId.
- `concurrent_encodes_each_get_unique_memory_ids` — 4 concurrent encodes; all distinct MemoryIds.

### 4.3 Recall-after-encode integration test

Bonus: `encode_then_recall_finds_it` — encode a memory, then run recall with the cue text → the encoded memory is the top hit. Validates the planner stack end-to-end.

## 5. Files written / changed

```
crates/brain-planner/src/encode.rs                      [new — planner side]
crates/brain-planner/src/executor/writer.rs             [new — trait + payloads]
crates/brain-planner/src/executor/encode.rs             [new — executor side]
crates/brain-planner/src/executor/mod.rs                [edit: add modules + re-exports]
crates/brain-planner/src/executor/context.rs            [edit: add writer field]
crates/brain-planner/src/executor/error.rs              [edit: + WriterFailed]
crates/brain-planner/src/executor/result.rs             [edit: + EncodeResult]
crates/brain-planner/src/lib.rs                         [edit: re-exports]
crates/brain-planner/tests/encode_end_to_end.rs         [new — integration tests]
crates/brain-planner/tests/recall_end_to_end.rs         [edit: ExecutorContext::new signature change — pass a no-op WriterHandle]
```

No new external deps. `brain-core::EdgeKind` already exists. `EncodeOp` carries `[f32; brain_embed::VECTOR_DIM]` so the existing `brain-embed` dep covers the vector dim.

## 6. Verify checklist

- `cargo build -p brain-planner` clean (in dev container).
- `cargo test -p brain-planner` — existing 46 + ~8 planner unit + ~5 executor integration.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 7. Commit message (draft)

```
feat(brain-planner): Encode planner + executor + WriterHandle trait (sub-task 6.4)

First write-path in Phase 6. Maps wire EncodeRequest → EncodePlan,
then drives it through embed → context resolve → writer dispatch →
response. Introduces the WriterHandle trait that all future write
operations (forget, link, unlink) plug into.

Planner (encode.rs):
- plan_encode validates per spec §08/04 §15: text non-empty + ≤1 MiB,
  kind ≠ Consolidated, salience ∈ [0, 1], edges ≤ max_edges_per_encode,
  edge weights finite.
- Builds EncodePlan with explicit ContextResolutionStep (wire only
  carries WireContextId in v1; GetOrCreate branch reserved).
- Cost via cost_encode + budget check.

Executor (executor/{writer,encode,context,error,result}.rs):
- WriterHandle trait — per-shard write surface, object-safe via
  Pin<Box<dyn Future>>. Phase 8/9 will wire the real channel-fed
  group-commit writer per spec §08/08 §10.
- EncodeOp payload carries request_id (for idempotency at the writer
  side), context_id, kind, text, vector, fingerprint, edges.
- EncodeAck: memory_id + per-edge outcomes + replayed flag.
- WriterError { Overloaded, Internal } — ExecError::WriterFailed
  #[from]-wraps.
- ExecutorContext gains writer: Arc<dyn WriterHandle> (signature
  break; the only existing caller — recall integration test —
  updated to pass a no-op stub).
- execute_encode: idempotency lives at the writer (spec §04 §4
  brief read txn collapses into the write side); embed → submit →
  return EncodeResult.

Tests:
- ~8 pure planner units pinning validation + plan shape.
- ~5 executor integration tests using a FakeWriterHandle that drives
  the test MetadataDb + SharedHnsw + an internal idempotency
  HashMap.
- One encode_then_recall_finds_it that exercises both planners
  through the same stack.

No new deps. Built/tested inside the dev container.
```

## 8. Risks

- **Plan-time idempotency vs runtime idempotency.** Spec §04 §4 reads as a planner-stage check, but practically it has to consult `brain-metadata`'s idempotency table — which is *write*-side state (the writer caches responses there). We collapse the check into the writer. This is a small deviation from spec wording; not a behavioural change.
- **`Pin<Box<dyn Future>>` ergonomics.** Calling `submit_encode(...).await` works but the trait definition is verbose. Acceptable; the alternative (async-trait crate) adds a dep + macro overhead for one trait.
- **Vector lifetime in `EncodeOp`.** The 384 × 4 = 1536-byte vector copy from executor → writer is fine on CPU; if profiling later shows it's hot, we wrap in `Arc<[f32; 384]>`. Not now.
- **Integration test depending on FakeWriterHandle + real MetadataDb + real SharedHnsw.** Three moving parts; if any errors are flaky we tighten the fake. The recall fixture works the same way and is stable, so we expect this to work.

## 9. Out-of-scope flags (re-confirm)

- No real writer task. `submit_encode` is synchronous-async in the fake.
- No bulk encode opcode.
- No re-embedding migration.
- No cascading edge handling on forget — 6.6.
- No transaction handling (`txn_id` on the wire is recorded in `EncodeOp` but the writer treats every encode as its own txn for v1).

---

PLAN READY.
