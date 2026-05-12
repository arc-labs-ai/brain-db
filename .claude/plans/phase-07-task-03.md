# Sub-task 7.3 — ENCODE handler

Thin glue layer that replaces the 7.1 stub. The planner (6.4) and executor (6.4 + 7.2's `RealWriterHandle`) are already complete — this sub-task wires them through `brain-ops::dispatch` and maps the result into the wire `EncodeResponse`.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/02 §1 | Wire `EncodeRequest`: text + context_id + kind + salience_hint + edges + request_id + txn_id + deduplicate |
| §09/02 §3 | Wire `EncodeResponse`: memory_id + was_deduplicated + salience + auto_edges_added |
| §09/02 §4 | Same RequestId → same response; no duplicate memory; no extra WAL record (already enforced by 7.2's `RealWriterHandle`) |
| §09/02 §7 | Failure modes: EmbeddingFailed, QuotaExceeded, ContextLimitReached, InvalidEdge (per-edge, doesn't fail the encode), TooManyEdges |
| §09/02 §14 | `Consolidated` kind is worker-only; rejected at validation (already enforced by `plan_encode`) |
| §09/02 §15 | Encode is a "single-write commit" — one WAL group commit per encode (7.2 doesn't have WAL yet; Phase 8/9 finishes) |

## 1. Scope

**In scope for 7.3:**
- Replace `crates/brain-ops/src/encode.rs::handle_encode` stub with a real implementation.
- Add `planner_ctx: PlannerContext` to `OpsContext` (additive change; the field defaults to `PlannerContext::default()` if the constructor isn't given one).
- Map `brain_planner::EncodeResult` → wire `EncodeResponse`:
  - `memory_id` → `WireMemoryId` (u128).
  - `was_deduplicated` ← `EncodeResult.replayed` (a replay is the only way to "deduplicate" in our model).
  - `salience` ← the post-encode salience (= the request's `salience_hint` for v1; we don't yet apply decay or boosts here).
  - `auto_edges_added` ← `edge_results.iter().filter(|o| matches!(o, Inserted)).count()` cast to u32.
- Integration test in `crates/brain-ops/tests/encode.rs` that runs the full pipeline (dispatcher → handler → planner → executor → real writer → wire response).
- One unit test pinning the `EncodeResult` → `EncodeResponse` mapping.

**NOT in scope:**
- Salience decay / access boosts (spec §09/02 §11). Phase 11 observability + workers.
- `was_deduplicated = true` for non-replay duplicate text (spec mentions `deduplicate: bool` request flag — fingerprint-based dedupe). Wire field exists; we forward as `false` for non-replay. A future sub-task wires the dedupe path.
- `EncodeVectorDirect` opcode — separate dispatch arm, still `NotYetImplemented`.
- Wire-frame parsing / response framing — Phase 9 server.

## 2. Implementation decisions

### 2.1 `OpsContext` gets a `planner_ctx` field

```rust
pub struct OpsContext {
    pub executor: ExecutorContext,
    pub planner_ctx: PlannerContext,
}

impl OpsContext {
    pub fn new(executor: ExecutorContext) -> Self {
        Self { executor, planner_ctx: PlannerContext::default() }
    }
    pub fn with_planner_context(mut self, planner_ctx: PlannerContext) -> Self {
        self.planner_ctx = planner_ctx;
        self
    }
}
```

Non-breaking: 7.1's existing tests still construct `OpsContext::new(executor)` and get the default planner context.

### 2.2 Handler body

```rust
pub async fn handle_encode(
    req: EncodeRequest,
    ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    // 1. Plan.
    let plan = brain_planner::plan_encode_inner(&req, &ctx.planner_ctx)?;

    // 2. Capture salience for the response (the planner stored it on
    //    the WalAppendStep).
    let salience = plan.wal_append.salience_initial;

    // 3. Execute.
    let result = brain_planner::execute_encode(plan, &ctx.executor).await?;

    // 4. Map to wire.
    let auto_edges_added = result
        .edge_results
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;

    Ok(EncodeResponse {
        memory_id: result.memory_id.raw(),
        was_deduplicated: result.replayed,
        salience,
        auto_edges_added,
    })
}
```

`?` propagates `PlanError` + `ExecError` through `OpError`'s `#[from]` impls; the dispatcher's match arm wraps the success side in `ResponseBody::Encode`.

### 2.3 `was_deduplicated` semantics

The spec field is named `was_deduplicated`. Spec §09/02 §4 calls replay "same RequestId returns same response". Wire field semantics: `true` iff the substrate did NOT do new work. For v1 that's exactly `EncodeResult.replayed`. The fingerprint-based content-dedupe path (spec §09/02 §13: `deduplicate: bool` request flag) is a future sub-task.

### 2.4 `salience` in the response

`EncodeResult` doesn't carry the salience back from the writer (it's not a per-write decision in v1; the planner pins it on the plan). The handler reads it off the plan before the executor consumes it. Same value on replay vs fresh write — replay returns the cached MemoryId; salience came from the original request's hint.

This is technically a small white-lie on replay: if the original write decayed the salience between then and now, the response shows the hint, not the current value. Acceptable — the canonical salience lives in metadata, recallable via RECALL. Document.

### 2.5 No new error variants

Every failure path is already mapped:
- `PlanError::InvalidParameters` → `OpError::PlanError` → wire `InvalidRequest`.
- `PlanError::QueryTooExpensive` → same.
- `ExecError::EmbedFailed` → wire `InternalError` (spec §09/02 §7's `EmbeddingFailed` — wire stable code is `InternalError`).
- `ExecError::WriterFailed(Conflict)` → wire `Conflict` (spec §09/02 §4 idempotency mismatch).
- `ExecError::WriterFailed(Overloaded)` → wire `Overloaded` (retryable).

7.2's `error_code()` mapping table already covers all of these.

### 2.6 Test harness

Reuse the `RealWriterHandle` + `MetadataDb` + `SharedHnsw` fixture from `tests/writer.rs`. Build an `OpsContext` around the executor. Dispatch an `EncodeRequest` through the handler.

For the embedder, we need a real `Dispatcher` — none of the previous brain-ops tests have constructed one. Two options:
- **(A) Use brain-embed's `CpuDispatcher` gated on `BRAIN_EMBED_MODEL_DIR`** — same pattern as brain-planner's integration tests.
- **(B) Use a local mock dispatcher** that returns deterministic vectors. Phase 6 used this pattern.

**Choice: (B) for the main integration test, (A) for one BGE-gated test.** Mock keeps `cargo test` fast and offline; the gated test exercises the real embedder when the env var is set.

### 2.7 Integration tests (5)

- `encode_full_pipeline_returns_memory_id` — dispatcher → handler → real writer → metadata row exists → wire response carries the new MemoryId.
- `encode_replay_sets_was_deduplicated` — same RequestId twice; second response has `was_deduplicated: true`.
- `encode_conflict_returns_error_variant` — same RequestId, different text; handler returns `OpError::ExecError(ExecError::WriterFailed(Conflict(_)))`; `error_code()` → `Conflict`.
- `encode_consolidated_kind_rejected` — kind=Consolidated → `OpError::PlanError(InvalidParameters)`.
- `encode_with_real_embedder_end_to_end` — gated on `BRAIN_EMBED_MODEL_DIR`; uses `CpuDispatcher`.

### 2.8 Unit tests (1)

- `encode_result_to_response_mapping` — given a known `EncodeResult` + `salience` value, the wire response carries the right fields. Pure mapping test.

Actually, since the mapping is inside `handle_encode` (no extracted helper), the integration tests cover it. Skip the unit test.

## 3. Files written / changed

```
crates/brain-ops/src/context.rs     [edit: + planner_ctx field + with_planner_context]
crates/brain-ops/src/encode.rs      [edit: real handler body]
crates/brain-ops/src/lib.rs         [edit: re-export PlannerContext (convenience)]
crates/brain-ops/tests/encode.rs    [new — 5 integration tests]
```

No new external deps.

## 4. Verify checklist

- `cargo build -p brain-ops` clean.
- `cargo test -p brain-ops` — 25 existing + ~4 new (4 mock + 1 BGE-gated which skips).
- `cargo clippy -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-ops` no diff.

## 5. Commit message (draft)

```
feat(brain-ops): ENCODE handler (sub-task 7.3)

Replaces the 7.1 stub with a real implementation that plumbs the
existing planner (6.4) + executor (6.4) + real writer (7.2) through
brain-ops::dispatch and maps the result into the wire EncodeResponse.

- handle_encode: plan_encode_inner → execute_encode → map result.
  PlanError + ExecError propagate via OpError's #[from] impls; the
  dispatcher wraps the success side in ResponseBody::Encode.
- OpsContext gains a planner_ctx: PlannerContext field with a
  with_planner_context builder. Defaults to PlannerContext::default()
  so 7.1's existing tests still construct OpsContext::new(executor)
  unchanged.
- Wire mapping:
  - memory_id ← result.memory_id.raw()
  - was_deduplicated ← result.replayed (spec §09/02 §4 replay = the
    only dedupe path in v1; fingerprint-based content dedupe is
    future)
  - salience ← plan.wal_append.salience_initial (captured from the
    plan before execute consumes it; same value on replay)
  - auto_edges_added ← count of EdgeOutcome::Inserted

Tests: 4 mock-embedder integration tests pinning full pipeline,
replay flag, conflict error, Consolidated-kind rejection. One
BGE-gated end-to-end test using CpuDispatcher.

No new external deps. brain-planner unchanged.
```

## 6. Risks

- **Salience drift on replay**. Documented in §2.4. If a future sub-task adds salience decay between writes, the replay response carries the original hint, not the current value. Wire field is `salience` (the response field); RECALL is the canonical source for the live salience.
- **`OpsContext` constructor signature**. Adding a field via builder pattern is non-breaking. The dispatcher tests in 7.1 already construct `OpsContext::new(executor)`; that still works because `planner_ctx` defaults.

## 7. Out-of-scope flags

- No fingerprint-based content dedupe (spec §09/02 §13's `deduplicate` flag forwarded as-is; logic deferred).
- No salience decay / access boosts.
- No `EncodeVectorDirect` opcode.
- No batch encode.
- No quota enforcement (spec §09/02 §7's QuotaExceeded — Phase 8/9 wires the quota service).

---

PLAN READY.
