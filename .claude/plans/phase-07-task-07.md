# Sub-task 7.7 — FORGET handler

The planner (`plan_forget_inner`) and the executor (`execute_forget`)
both exist from Phase 6; this sub-task is pure plumbing through
`brain-ops::dispatch`. UNFORGET is **not** wired here — the spec
§09/06 §13 says it's `ADMIN_RESTORE_FORGOTTEN`, an admin operation,
not a top-level cognitive primitive. v1 wire has no Unforget variant;
the original 7.x plan said "wire variant addition needed" — we defer
that to a future admin-ops sub-task.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/06 §1 | FORGET marks memory(ies) tombstoned; optionally hard-zeros |
| §09/06 §2 | Two modes: Soft (default, grace 7 days) vs Hard (immediate zero) |
| §09/06 §3 | Wire `ForgetResponse { forgotten, not_found, failed, grace_until }` — but the **actual** wire shape is single-memory: `{ memory_id, was_already_forgotten, edges_removed }` |
| §09/06 §4 | Idempotency by RequestId; replay = same response |
| §09/06 §14 | MemoryNotFound is in `not_found` (no-op), not an error; NotOwned + TooManyMemories + Conflict are errors |
| §09/06 §7 | Edges referencing forgotten memory become "stale" — maintenance worker tombstones them (not the handler) |

## 1. Scope

**In scope for 7.7:**

- Replace `crates/brain-ops/src/forget.rs::handle_forget` stub:
  `plan_forget_inner` → `execute_forget` → map `ForgetResult` →
  wire `ForgetResponse`.
- Tests: 5 integration tests.

**NOT in scope:**

- UNFORGET wire variant. Spec §09/06 §13 keeps restore as an admin
  operation; wire never carried Unforget. Add when the admin-ops
  surface is built (Phase 12 or later).
- Multi-target FORGET (`Memories(Vec<MemoryId>)` / `Filter`).
  Wire `ForgetRequest` is single-`memory_id`; spec §09/06 §2's
  multi-target shape isn't on the wire yet. v1 is single-memory only.
- Edge cascade. Spec §09/06 §7 says a maintenance worker tombstones
  dangling edges; the handler reports `edges_removed = 0` for now
  (documented v1 gap). Phase 11 workers wire the cascade.
- Hard-mode arena zeroing. `execute_forget` already sets the
  tombstone via `submit_forget` → `RealWriterHandle::do_forget`,
  which respects the `mode` flag at the metadata layer. Arena
  zeroing is the workers' job once Phase 8 ships WAL-driven
  reclamation.
- Grace-until reporting. Wire field doesn't carry it.

## 2. Implementation decisions

### 2.1 Handler body

```rust
pub async fn handle_forget(
    req: ForgetRequest,
    ctx: &OpsContext,
) -> Result<ForgetResponse, OpError> {
    let memory_id_wire = req.memory_id;
    let plan = plan_forget_inner(&req, &ctx.planner_ctx)?;
    let result = execute_forget(plan, &ctx.executor).await?;

    // Spec §09/06 §14 — MemoryNotFound is NOT an error; it surfaces
    // on the wire as `was_already_forgotten = true` with memory_id
    // echoed back. The wire enum has no "really not found" variant
    // (spec §09/06 §3's `not_found` list is the multi-target wire
    // shape we don't have yet).
    let was_already_forgotten = matches!(
        result.outcome,
        ForgetOutcome::AlreadyTombstoned | ForgetOutcome::MemoryNotFound
    );

    Ok(ForgetResponse {
        memory_id: memory_id_wire,
        was_already_forgotten,
        edges_removed: 0, // v1 gap: edge cascade is a worker job
    })
}
```

### 2.2 Why no error on MemoryNotFound

Spec §09/06 §14 explicitly: "The MemoryId doesn't exist… Returned in
`not_found`; not an error." On the single-memory wire shape there's
no `not_found` list, so we collapse `MemoryNotFound` into the
`was_already_forgotten` flag. That matches "the memory is no longer
visible" — which is the operational meaning the caller cares about.

### 2.3 Idempotent replay

`execute_forget` returns `replayed = true` from `ForgetResult` when
the writer's idempotency table replayed. The wire shape has no
`was_replayed` field, but the response is semantically identical
(same `was_already_forgotten` flag) — replay is invisible at the wire,
correct per spec §09/06 §4.

### 2.4 Tombstoned vs AlreadyTombstoned

`ForgetOutcome::Tombstoned` → fresh forget → `was_already_forgotten =
false`. `AlreadyTombstoned` → flag true. The flag captures the spec's
"was this a no-op or did work happen" question.

### 2.5 No new error variants

All failure paths already mapped:
- `PlanError::InvalidParameters` → `OpError::PlanError` → wire
  `InvalidRequest`.
- `ExecError::WriterFailed(Conflict)` → wire `Conflict` (idempotency
  mismatch on the RequestId).
- `ExecError::WriterFailed(Overloaded)` → wire `Overloaded`.

## 3. Files written / changed

```
crates/brain-ops/src/forget.rs       [edit: real handler body]
crates/brain-ops/tests/forget.rs     [new — 5 integration tests]
```

No new external deps. No `lib.rs` re-export changes.

## 4. Test plan

### 4.1 Integration tests (5)

1. `forget_full_pipeline_tombstones_memory` — ENCODE a memory →
   FORGET → assert `was_already_forgotten=false`, `memory_id` matches.
2. `forget_already_tombstoned_returns_flag` — ENCODE → FORGET → FORGET
   with a different RequestId → second call has
   `was_already_forgotten=true`.
3. `forget_memory_not_found_returns_flag_not_error` — FORGET on a
   memory id that was never encoded → succeeds with
   `was_already_forgotten=true`, no error (spec §09/06 §14).
4. `forget_idempotent_replay_returns_cached_response` — same
   RequestId twice → second response identical to first, no
   additional work (transparent at the wire).
5. `forget_idempotency_conflict_returns_error` — same RequestId,
   different `memory_id` → `OpError::ExecError(WriterFailed(Conflict))`
   → wire `Conflict` code.

## 5. Verify checklist

- `cargo build -p brain-ops` clean.
- `cargo test -p brain-ops` — old totals + 5 new.
- `cargo clippy -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-ops -- --check` no diff.

## 6. Commit message (draft)

```
feat(brain-ops): FORGET handler (sub-task 7.7)

Replaces the 7.1 stub with a real implementation that plumbs the
existing planner (6.6) + executor (6.6) + real writer (7.2) through
brain-ops::dispatch.

- handle_forget: plan_forget_inner → execute_forget → map outcome.
  PlanError + ExecError propagate via OpError's #[from] impls.
- Wire mapping (ForgetResult → ForgetResponse):
  - memory_id ← request.memory_id (echoed back)
  - was_already_forgotten ← outcome ∈ {AlreadyTombstoned,
    MemoryNotFound} (spec §09/06 §14 collapses both into a no-op
    flag on the single-memory wire shape)
  - edges_removed ← 0 (v1 gap: edge cascade is a Phase-11 worker
    job per spec §09/06 §7)
- Replay is transparent on the wire — same was_already_forgotten
  value, no additional work.

Tests: 5 integration tests pinning fresh forget, second-forget,
phantom-memory tolerance, idempotent replay, and idempotency
conflict.

UNFORGET intentionally not wired here. Spec §09/06 §13 keeps
restore as an admin operation (ADMIN_RESTORE_FORGOTTEN); the wire
has no Unforget variant. Future admin-ops sub-task adds it.

No new external deps. brain-planner unchanged.
```

## 7. Risks

- **`MemoryNotFound` collapsing into "was_already_forgotten".** A
  caller can't distinguish "I just forgot this" from "this never
  existed." Spec §09/06 §14 explicitly accepts this — both are
  no-ops from the agent's perspective. Documented.
- **`edges_removed = 0` lie.** The wire field reports zero today but
  cascade is supposed to happen eventually. Phase 11's edge-cascade
  worker fills this in. Until then, agents shouldn't rely on the
  field's accuracy for cleanup decisions.
- **Hard-mode arena zeroing is metadata-only today.** The executor
  flips the tombstone but doesn't zero the arena bytes (Phase 8 WAL
  + arena work owns this). Privacy guarantees per spec §09/06 §15
  are not yet honoured by Hard mode.

## 8. Out-of-scope flags

- No UNFORGET / restore.
- No multi-target forget.
- No edge cascade.
- No arena zeroing on Hard.
- No grace_until reporting.

---

PLAN READY.
