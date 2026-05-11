# Sub-task 6.7 — `Executor::execute` dispatch

The top-level entry point that matches an `ExecutionPlan` to its `execute_*` async function. Mostly wiring — every per-variant executor (`execute_recall`, `execute_encode`, `execute_forget`) already exists; 6.7 unifies them behind one signature.

Spec §08/08 §1 names the shape: `async fn execute(plan: ExecutionPlan) -> Result<Response, ExecError>`. Phase 6 has no wire-level `Response`; we ship a Rust-side `ExecutionResult` union that Phase 9's server wraps into `ResponseBody`.

The PLAN / REASON variants return `ExecError::Unsupported` per the 6.5 plan — full execution of those lands with Phase 7 cognitive-ops.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/08 §1 | `Executor::execute(plan) -> async Result<Response, ExecError>` |
| §08/08 §2 | `match plan { … }` dispatch — sequential `await` per variant |
| §08/08 §4 | Errors propagate via `Result` + `?`; on failure, build an error response |
| §08/08 §11 | Cancellation: dropping the future cancels in-flight reads; writes complete in the writer task |
| §08/08 §12 | Timeouts: caller's responsibility (`tokio::time::timeout` or equivalent) — the executor itself doesn't enforce |
| §08/08 §13 | Each execution produces a structured log entry — we instrument with `tracing` |
| §08/08 §14 | Backpressure: writer's `WriterError::Overloaded` already flows through `ExecError::WriterFailed` |

## 1. Scope

**In scope for 6.7:**
- `Executor` struct (or free function) — pick one; the spec uses both shapes interchangeably. We ship a **free function** `execute(plan, ctx) -> Result<ExecutionResult, ExecError>` so the call site reads `execute(plan, &ctx).await?` without ceremony. The `Executor` struct can wrap it later if a stateful executor is wanted.
- `ExecutionResult` enum with one variant per cognitive operation:
  - `Recall(RecallResult)`
  - `Encode(EncodeResult)`
  - `Forget(ForgetResult)`
  - `Plan` / `Reason` variants are *not* added — those paths error out, so there's no result variant to return.
- Wire the existing `execute_recall` / `execute_encode` / `execute_forget` into the dispatch.
- `tracing::info_span!("execute", op = "recall"|"encode"|...)` around each branch (spec §08/08 §13).
- Tests:
  - Round-trip a recall, encode, forget through `execute` (each → correct `ExecutionResult` variant).
  - `Plan` and `Reason` plans → `ExecError::Unsupported`.
  - One "encode → execute → forget → execute → recall → execute" smoke test that exercises three branches with one fixture.

**NOT in scope:**
- Timeouts at the executor — spec §12 says caller handles.
- Cancellation tokens — Rust's drop-on-future-cancel is already in place.
- Cooperative yield points (spec §03 cooperative yields inside long ops) — those are inside the per-variant executors; 6.7 only dispatches.
- A stateful `Executor` struct. The free function is enough; we can wrap later if Phase 9 needs config state.
- An `Executor` trait for testing — the free function is straightforward to call.

## 2. Module surface

```rust
// crates/brain-planner/src/executor/mod.rs (add)

pub use dispatch::{execute, ExecutionResult};

// crates/brain-planner/src/executor/dispatch.rs

use crate::plan::ExecutionPlan;
use super::{
    context::ExecutorContext, encode::execute_encode, error::ExecError,
    forget::execute_forget, recall::execute_recall,
    result::{EncodeResult, ForgetResult, RecallResult},
};

#[derive(Debug, Clone)]
pub enum ExecutionResult {
    Recall(RecallResult),
    Encode(EncodeResult),
    Forget(ForgetResult),
}

/// Top-level dispatch. Spec §08/08 §1.
pub async fn execute(
    plan: ExecutionPlan,
    ctx: &ExecutorContext,
) -> Result<ExecutionResult, ExecError>;
```

Re-export from `lib.rs`: `execute`, `ExecutionResult`.

## 3. Implementation decisions

### 3.1 Function vs struct

Spec §08/08 uses both. Two pragmatic considerations:

- A free function reads better at call sites: `execute(plan, &ctx).await`.
- A struct would let Phase 9 stash a `metrics: MetricsHandle` and `config: ExecutorConfig` if needed.

**Decision: free function.** When Phase 9 (server) adds metrics, they live on `ExecutorContext` — that bag already holds the storage handles. Adding `metrics` to `ExecutorContext` is non-breaking. A struct adds nothing.

### 3.2 The `match` body

```rust
match plan {
    ExecutionPlan::Recall(p) => {
        let _span = tracing::info_span!("execute", op = "recall").entered();
        execute_recall(p, ctx).await.map(ExecutionResult::Recall)
    }
    ExecutionPlan::Encode(p) => {
        let _span = tracing::info_span!("execute", op = "encode").entered();
        execute_encode(p, ctx).await.map(ExecutionResult::Encode)
    }
    ExecutionPlan::Forget(p) => {
        let _span = tracing::info_span!("execute", op = "forget").entered();
        execute_forget(p, ctx).await.map(ExecutionResult::Forget)
    }
    ExecutionPlan::Plan(_) => Err(ExecError::Unsupported(
        "PLAN execution — Phase 7",
    )),
    ExecutionPlan::Reason(_) => Err(ExecError::Unsupported(
        "REASON execution — Phase 7",
    )),
}
```

`info_span!` is fine for now — Phase 11 observability tightens the structured-fields story. The span enters at branch-start, exits at branch-end; this gives operators a per-request timing baseline.

### 3.3 `ExecutionResult` doesn't carry `Plan` / `Reason` variants

Those dispatch arms return `Err` before producing a result. Adding empty `Plan(())` / `Reason(())` variants would be misleading. When Phase 7 lands the PLAN/REASON executors, they'll add `PathResult` / `ReasonResult` structs to `result.rs` and the corresponding enum variants here.

### 3.4 No new `ExecError` variants

`Unsupported(&'static str)` already exists. The two new arms use it. No churn.

### 3.5 Test pattern

Use the existing `FakeWriterHandle` from `encode_end_to_end.rs` / `forget_end_to_end.rs`. Pull it into a small helper module so `tests/dispatch.rs` can compose `ExecutionPlan::*` and call `execute(plan, &ctx).await`. The fakes are test-only; copying ~50 lines into a third integration file is fine.

Alternative: factor the FakeWriterHandle into a `tests/common/mod.rs` module shared across integration tests. Cleaner. Decision: do it, since `tests/dispatch.rs` is the third place we'd copy.

Actually `cargo test` doesn't auto-discover `tests/common/mod.rs`; each integration test file is its own crate. The standard pattern is `tests/common/mod.rs` + `mod common;` at the top of each test file. Slight overhead — not worth it for one more usage. **Decision: copy the helper into the dispatch test file**, accept the duplication. If a fourth integration test ever needs it, factor then.

### 3.6 Tests to write

`tests/dispatch.rs`:
- `dispatch_recall_returns_recall_variant` — build a `RecallPlan` via `plan_recall`, run `execute`, assert `ExecutionResult::Recall(_)`.
- `dispatch_encode_returns_encode_variant`.
- `dispatch_forget_returns_forget_variant`.
- `dispatch_plan_variant_is_unsupported` — `plan_path` then `execute` → `ExecError::Unsupported`.
- `dispatch_reason_variant_is_unsupported` — `plan_reason` then `execute` → `ExecError::Unsupported`.
- `dispatch_encode_then_recall_then_forget_through_execute` — three operations through the unified entry point.

## 4. Files written / changed

```
crates/brain-planner/src/executor/dispatch.rs           [new]
crates/brain-planner/src/executor/mod.rs                [edit: + pub mod dispatch; re-exports]
crates/brain-planner/src/lib.rs                         [edit: + execute, ExecutionResult]
crates/brain-planner/tests/dispatch.rs                  [new — integration tests]
```

No new external deps. No `Cargo.toml` change.

## 5. Verify checklist

- `cargo build -p brain-planner` clean (dev container).
- `cargo test -p brain-planner` — 87 existing + ~6 new.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 6. Commit message (draft)

```
feat(brain-planner): Executor::execute dispatch (sub-task 6.7)

Top-level entry point that matches an ExecutionPlan to its execute_*
async function. Spec §08/08 §1's `async fn execute(plan) -> Result<
Response, ExecError>` shape.

- ExecutionResult enum: Recall(RecallResult) / Encode(EncodeResult) /
  Forget(ForgetResult). PLAN + REASON have no result variants
  because those arms return ExecError::Unsupported — full execution
  is Phase 7 cognitive-ops territory.
- execute(plan, &ctx).await is the free-function entry; a stateful
  Executor struct is unnecessary at this layer (Phase 9 metrics can
  live on ExecutorContext if needed).
- tracing::info_span!("execute", op = …) wraps each branch so
  operators get a per-request timing baseline.
- PLAN + REASON return ExecError::Unsupported with a "Phase 7"
  message. No new ExecError variants needed.

Tests: ~6 dispatch integration tests using the FakeWriterHandle
pattern from earlier sub-tasks; round-trips for the three supported
ops + the two unsupported-error arms.

No new external deps. Total 93 tests passing in dev container.
```

## 7. Risks

- **Copy-pasted FakeWriterHandle.** Third file with the same fake. If 6.8 also needs it, factor to `tests/common/mod.rs`. For now, ~80 lines × 3 is acceptable.
- **`tracing` span overhead.** `info_span!` allocates a small string and pushes/pops a frame; ~ns. Irrelevant.
- **PLAN / REASON unsupported message.** Returns a `&'static str` "Phase 7"; a future operator reading logs will see "unsupported at execute-time: PLAN execution — Phase 7" and know where to look.

---

PLAN READY.
