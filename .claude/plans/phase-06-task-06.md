# Sub-task 6.6 — Forget planner + executor

The last write-path in Phase 6. Maps a wire `ForgetRequest` to a `ForgetPlan`, then drives it through idempotency check → writer dispatch. Adds `submit_forget` to the `WriterHandle` trait we set up in 6.4.

Phase 6 ships single-shard, single-memory FORGET. Spec §08/06 §1 names three target shapes (`Memory(id)`, `Memories(Vec<id>)`, `Filter(...)`); the wire `ForgetRequest` carries only the single-`MemoryId` shape (`memory_id: WireMemoryId`). Batch + filter modes are wire-level upgrades for a later phase. v1 is enough.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §08/06 §1 | Wire shape `{ memory_id, mode, request_id, txn_id }` (Phase 1 simplified to single-id) |
| §08/06 §2.1 | Forget-by-id plan: route by shard; per-shard sub-plan |
| §08/06 §3 | Per-shard step: WAL → arena tombstone → metadata commit → HNSW mark removed |
| §08/06 §4 | Hard forget zeros vector + text |
| §08/06 §7 | Idempotency check on `request_id` (same as encode) |
| §08/06 §10 | Per-memory error tolerance: missing → log + no-op (response says so) |
| §08/06 §11 | v1 cascade: outgoing + incoming edges tombstoned along with the memory |
| §08/06 §14 | Strict step ordering: WAL fsync → arena tombstone → metadata commit → HNSW mark |
| §08/06 §16 | Single-memory FORGET: ~1 ms total |
| §08/06 §18 | Validation: memory_id well-formed, request_id set |
| CLAUDE.md §5 inv. 1 | WAL-before-acknowledge |
| CLAUDE.md §5 inv. 6 | Tombstone grace before reclamation (default 7 days). Hard FORGET zeroes immediately |

## 1. Scope

**In scope for 6.6:**
- `crates/brain-planner/src/forget.rs` — planner side. `plan_forget(&ForgetRequest, &PlannerContext) -> Result<ExecutionPlan, PlanError>`.
- Extend `plan/forget.rs` (6.1 shell) with idempotency + apply steps.
- Extend `executor/writer.rs`: add `submit_forget(op: ForgetOp) -> Future<ForgetAck, WriterError>` to `WriterHandle` (default-impl behind `unimplemented!` is anti-spec; we widen the trait, then update `FakeWriterHandle` + `NoopWriter`).
- `executor/forget.rs` — `execute_forget(plan, ctx) -> Result<ForgetResult, ExecError>`.
- `ForgetResult` struct.
- New error variants in `ExecError`? Probably not — `WriterFailed` covers it.
- Tests:
  - Pure planner units: validation paths, both modes (Soft / Hard), happy-path shape.
  - Executor integration: encode-then-forget round-trip; recall-after-forget returns nothing (the tombstone bitmap in `SharedHnsw::search_active` already filters); soft + hard variants behave equivalently from the read path's POV.

**NOT in scope:**
- Batch `Forget(Vec<MemoryId>)` — wire shape only has single-id (Phase 1 simplified). Spec §08/06 §2.1 names it; we ship the single path; the batch path is a future wire bump.
- Forget-by-filter — wire shape doesn't carry a `ForgetFilter`. Same reason.
- Cascade-forget / restrict-forget — spec §08/06 §12 explicitly defers to v2.
- The reclaim worker (spec §08/06 §15). Phase 8 territory.
- Cross-shard fan-out — single-shard in v1 (orientation §4.7).

## 2. Planner-side design

### 2.1 Function signature

```rust
// crates/brain-planner/src/forget.rs

pub fn plan_forget(
    req: &brain_protocol::request::ForgetRequest,
    ctx: &PlannerContext,
) -> Result<ExecutionPlan, PlanError>;

pub fn plan_forget_inner(
    req: &ForgetRequest,
    ctx: &PlannerContext,
) -> Result<ForgetPlan, PlanError>;
```

### 2.2 Validation (spec §08/06 §18)

- `req.memory_id != 0` — `MemoryId::NULL` is the reserved sentinel; rejecting it would be defensive. Actually the spec says "memory IDs are well-formed". `MemoryId::from(0u128)` is technically well-formed; we don't reject. Skip this check.
- `req.request_id != [0; 16]` — the wire allows nil here, but spec §07 says "request_id is set". Reject `[0u8; 16]` as `InvalidParameters { field: "request_id", reason: "must be set" }`.

That's the only validation worth doing at v1. Mode + memory_id are typed and self-validating.

### 2.3 Cost + budget

```rust
let estimated = cost::cost_forget(req.mode == ForgetMode::Hard);
cost::check_budget(estimated, ctx)?;
```

`cost_forget` already exists in 6.2.

### 2.4 Plan assembly

Extend `plan/forget.rs` to carry the apply + idempotency steps. The 6.1 shell has just `{ shard, memory_id, mode, estimated_cost_ms }`. Add:

```rust
pub struct ForgetPlan {
    pub shard: ShardId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
    pub idempotency_check: IdempotencyCheckStep,   // reuse encode.rs's struct
    pub wal_append: ForgetWalStep,                  // new
    pub apply: ForgetApplyStep,                     // new
    pub response: ForgetResponseStep,               // new
    pub estimated_cost_ms: f32,
}
```

The new step structs are small:

```rust
pub struct ForgetWalStep {
    pub fsync: bool,
    pub mode: ForgetMode,    // carried so the WAL record can distinguish soft/hard
}

pub struct ForgetApplyStep {
    /// Spec §08/06 §3.
    pub arena_tombstone: bool,
    pub metadata_commit: bool,
    pub hnsw_mark_removed: bool,
    /// Spec §08/06 §4. Only set on hard forget.
    pub arena_zero_vector: bool,
    pub text_zero: bool,
}

pub struct ForgetResponseStep {
    /// Whether the response includes per-memory outcomes (always
    /// true in v1; batch mode will toggle).
    pub include_outcomes: bool,
}
```

## 3. Executor-side design

### 3.1 `WriterHandle::submit_forget`

```rust
pub trait WriterHandle: Send + Sync {
    fn submit_encode<'a>(/*…*/) -> Pin<Box<dyn Future<Output = Result<EncodeAck, WriterError>> + Send + 'a>>;

    fn submit_forget<'a>(
        &'a self,
        op: ForgetOp,
    ) -> Pin<Box<dyn Future<Output = Result<ForgetAck, WriterError>> + Send + 'a>>;
}
```

Adding a method to the trait is a breaking change for impls. The existing `FakeWriterHandle` (in `tests/encode_end_to_end.rs`) and `NoopWriter` (in `tests/recall_end_to_end.rs`) both need to grow `submit_forget` impls. Acceptable — both are test-only.

### 3.2 `ForgetOp` / `ForgetAck`

```rust
#[derive(Debug, Clone, Copy)]
pub struct ForgetOp {
    pub request_id: RequestId,
    pub memory_id: MemoryId,
    pub mode: ForgetMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetOutcome {
    Tombstoned,
    AlreadyTombstoned,    // spec §08/06 §10 — log + no-op
    MemoryNotFound,        // spec §08/06 §10
}

#[derive(Debug, Clone, Copy)]
pub struct ForgetAck {
    pub memory_id: MemoryId,
    pub outcome: ForgetOutcome,
    pub replayed: bool,
}
```

### 3.3 `execute_forget`

```rust
pub async fn execute_forget(
    plan: ForgetPlan,
    ctx: &ExecutorContext,
) -> Result<ForgetResult, ExecError> {
    let op = ForgetOp {
        request_id: plan.idempotency_check.request_id,
        memory_id: plan.memory_id,
        mode: plan.mode,
    };
    let ack = ctx.writer.submit_forget(op).await?;
    Ok(ForgetResult { ... })
}
```

Same shape as `execute_encode`, just no embed step.

### 3.4 `ForgetResult`

```rust
#[derive(Debug, Clone, Copy)]
pub struct ForgetResult {
    pub memory_id: MemoryId,
    pub outcome: ForgetOutcome,
    pub replayed: bool,
}
```

### 3.5 `FakeWriterHandle::submit_forget` (test side)

The fake needs to:
1. Idempotency replay: same `request_id` → cached `ForgetAck`.
2. Look up the memory in metadata.
   - Missing → `ForgetOutcome::MemoryNotFound`.
   - Already tombstoned (we don't track this in the fake yet; trivially treat as `Tombstoned` on first call, `AlreadyTombstoned` on second… but spec §08/06 §10 says per-id idempotence, which our per-request_id replay covers. For v1 we treat all non-NotFound as `Tombstoned`).
3. Call `hnsw_writer.mark_tombstoned(memory_id)` to make recall skip it.
4. Optionally zero the metadata row (hard mode) — actually we just leave the row in place; the HNSW tombstone bitmap is what removes it from recall.

Total: ~30 lines in the fake.

## 4. Test plan

### 4.1 Pure planner tests (forget.rs)

- `happy_path_soft_plan` — both modes produce well-formed plans; check fields.
- `happy_path_hard_plan` — `apply.arena_zero_vector == true` for hard; `false` for soft.
- `nil_request_id_rejected` — `InvalidParameters { field: "request_id" }`.
- `estimated_cost_hard_greater_than_soft`.
- `idempotency_check_carries_request_id`.

### 4.2 Executor integration tests (tests/forget_end_to_end.rs)

Harness: shares the encode fixture (FakeWriterHandle + tempdir MetadataDb + SharedHnsw).

- `forget_round_trips` — encode, then forget, then `outcome == Tombstoned`.
- `recall_after_forget_skips_memory` — encode 2 memories, forget the first, recall returns only the second (HNSW tombstone bitmap).
- `forget_nonexistent_memory_returns_not_found` — pass a `MemoryId` that was never encoded.
- `idempotent_replay_of_forget` — same `request_id` twice → second has `replayed: true`.
- `hard_forget_round_trips` — same shape, different mode.

## 5. Files written / changed

```
crates/brain-planner/src/forget.rs                      [new — planner side]
crates/brain-planner/src/executor/forget.rs             [new — executor side]
crates/brain-planner/src/executor/writer.rs             [edit: + submit_forget, ForgetOp, ForgetAck, ForgetOutcome]
crates/brain-planner/src/executor/mod.rs                [edit: re-exports]
crates/brain-planner/src/executor/result.rs             [edit: + ForgetResult]
crates/brain-planner/src/plan/forget.rs                 [edit: full ForgetPlan + step structs]
crates/brain-planner/src/plan/mod.rs                    [edit: re-export new step types]
crates/brain-planner/src/lib.rs                         [edit: re-exports]
crates/brain-planner/tests/encode_end_to_end.rs         [edit: FakeWriterHandle gains submit_forget]
crates/brain-planner/tests/recall_end_to_end.rs         [edit: NoopWriter gains submit_forget]
crates/brain-planner/tests/forget_end_to_end.rs         [new — integration tests]
```

No new external deps.

## 6. Verify checklist

- `cargo build -p brain-planner` clean (dev container).
- `cargo test -p brain-planner` — 77 existing + ~5 planner + ~5 integration.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-planner` no diff.

## 7. Commit message (draft)

```
feat(brain-planner): Forget planner + executor (sub-task 6.6)

Last write-path in Phase 6. Maps wire ForgetRequest → ForgetPlan,
then drives it through writer dispatch. Single-shard, single-memory
v1 — the wire ForgetRequest only carries one MemoryId (Phase 1
simplified; spec §08/06 §1's batch + filter modes need a wire bump).

Planner (forget.rs):
- plan_forget validates request_id ≠ nil. memory_id + mode are
  typed and self-validating.
- Builds ForgetPlan with idempotency_check + wal_append + apply +
  response steps. Hard mode flips apply.arena_zero_vector +
  apply.text_zero per spec §08/06 §4.
- Cost via cost_forget; budget check.

Executor (executor/forget.rs):
- execute_forget: build ForgetOp, submit to writer, return
  ForgetResult.
- WriterHandle gains submit_forget(ForgetOp) → ForgetAck.
- ForgetOp: { request_id, memory_id, mode }.
- ForgetAck: { memory_id, outcome, replayed }.
- ForgetOutcome: { Tombstoned, AlreadyTombstoned, MemoryNotFound }
  per spec §08/06 §10's per-memory error tolerance.
- ForgetResult mirrors the ack.

Plan struct extended: ForgetPlan now carries
{shard, memory_id, mode, idempotency_check, wal_append, apply,
response, estimated_cost_ms}. New step types: ForgetWalStep,
ForgetApplyStep, ForgetResponseStep.

Tests (~10 new):
- 5 pure planner units pinning validation + mode shape.
- 5 integration tests using the FakeWriterHandle from 6.4 (extended
  with submit_forget): encode→forget round-trip, recall-after-forget
  skips via HNSW tombstone bitmap, NotFound on unknown memory, soft
  vs hard, idempotent replay.

NoopWriter (recall test) and FakeWriterHandle (encode test) updated
for the new trait method.

No new external deps. No new PlanError. WriterError covers writer
failures via the existing ExecError::WriterFailed.

Verify: cargo build/test/clippy -p brain-planner in dev container.
```

## 8. Risks

- **Trait expansion is a breaking change.** Every `WriterHandle` impl needs `submit_forget`. Only two exist (both test-only); both updated in the same commit.
- **Spec's per-id idempotency vs per-request idempotency.** Spec §08/06 §10 says "re-forgetting a tombstoned memory is a no-op", which our `AlreadyTombstoned` outcome captures. Our `replayed` flag separately captures per-request_id idempotency. Both are honoured.
- **HNSW tombstone bitmap interaction.** The fake writer calls `mark_tombstoned` (which 4.x shipped). Our recall integration test relies on `search_active` filtering tombstoned memories — already covered by Phase 4's tests; we just exercise the integration.

## 9. Out-of-scope flags

- No batch / filter target shapes.
- No cascade options (spec §12 defers to v2).
- No reclaim worker (Phase 8).
- No cross-shard fan-out.
- No new PlanError variants.

---

PLAN READY.
