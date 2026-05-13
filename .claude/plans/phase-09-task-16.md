# Sub-task 9.16 — PLAN / REASON tombstone filter

**Reads:**
- `spec/16_benchmarks_acceptance/01_correctness_criteria.md` §12
  (Tombstone correctness MUST: "Visibility: not returned unless
  explicitly requested" + concrete test: "FORGET m. PLAN through a
  chain that includes m. Verify m is excluded.").
- `crates/brain-planner/src/executor/path.rs` — BFS traversal.
- `crates/brain-planner/src/executor/reason.rs` — DAG expansion.
- `crates/brain-index/src/shared.rs` —
  `SharedHnsw::is_tombstoned(memory_id)` already exists, backed by
  the index's tombstone bitmap.

**Phase doc:** orientation §11 sub-task **9.16**.

**Done when:** PLAN's bidirectional BFS and REASON's DAG expansion
both drop tombstoned memories from every neighbour set, not just
when an active txn snapshot is present (current behavior).
Integration tests prove `FORGET m; PLAN through chain incl. m`
excludes `m`.

---

## 1. The bug

PLAN's `path.rs` filters tombstoned neighbours **only inside an
`Option<Arc<TxnSnapshot>>` block** (line 280–281):

```rust
if let Some(snap) = &ctx.txn {
    // ...
    neighbours.retain(|(_, other, _)| !snap.tombstoned.contains(other));
}
```

Outside a txn — i.e., 99% of operations — the BFS happily traverses
through tombstoned nodes. Same shape in REASON's `reason.rs`
line 254.

Reason for the original gap: the txn snapshot is the only place that
*adds* in-flight tombstones during a single op's lifetime. The base
case (committed tombstones) was supposed to be filtered by querying
metadata or the HNSW bitmap — but that lookup was never wired in.
Spec §16/01 §12 acceptance test is therefore failing today.

## 2. The fix

For every neighbour candidate, query
`ctx.index.is_tombstoned(memory_id)` (cheap — atomic-bitmap lookup
inside the RwLock-guarded HnswIndex). Drop tombstoned candidates
unconditionally; layer the txn snapshot on top as before.

Two flavours:

**Option A — per-neighbour filter:**

```rust
neighbours.retain(|(_, other, _)| !ctx.index.is_tombstoned(*other));
```

Roughly O(N · log) where N is neighbour count and log is bitmap
membership cost. Bitmap is a `HashSet<MemoryId>` under the hood
(small ~O(1)).

**Option B — batch query helper:**

Add `SharedHnsw::tombstoned_subset(ids: &[MemoryId]) -> HashSet<MemoryId>`
to acquire the read lock once and look up multiple ids inside it.
Faster under contention; more code.

**Recommendation: Option A.** The lock is parking_lot RwLock; read
contention is unmeasurable at expected PLAN/REASON cadences (<100
calls/sec/shard). `is_tombstoned` acquires + releases the read lock
internally; each call is ~50 ns. A BFS with 1000 neighbours hits
50 µs of lock churn — negligible next to the metadata reads
already happening in the same loop.

Reconsider in v2 if benchmark numbers come in.

## 3. Code changes

`crates/brain-planner/src/executor/path.rs`:

```rust
// After fetching `neighbours` from edges_out / edges_in tables:

// Spec §16/01 §12: drop tombstoned memories from PLAN traversals
// (sub-task 9.16). Outside a txn this is the only filter; inside
// one, the txn snapshot adds its in-flight tombstones below.
neighbours.retain(|(_, other, _)| !ctx.index.is_tombstoned(*other));

if let Some(snap) = &ctx.txn {
    // ... pending_links / pending_unlinks logic stays ...
    neighbours.retain(|(_, other, _)| !snap.tombstoned.contains(other));
}
```

`crates/brain-planner/src/executor/reason.rs`:

Same pattern — add the unconditional `is_tombstoned` retain *before*
the `if let Some(snap)` block.

The two changes are mechanical: 2 LOC per file + the comment.

## 4. Also: terminal/start handling

Spec §16/01 §12 wording is "not returned unless explicitly
requested". PLAN's input is a `start` MemoryId + `goal` MemoryId.
What if the **start** itself is tombstoned?

Two interpretations:

- (a) Reject — `ExecError::MemoryNotFound { what: "start" }`.
- (b) Treat as "client knows what it's asking for; surface the path
  through the tombstone".

Current PLAN behavior: the start is accepted unconditionally; the
BFS spreads outward. It doesn't matter whether the start is
tombstoned — it's the *neighbours* that need filtering.

We pick **(a) at request entry** to match spec §16/01 §12's
"visibility" rule and avoid surprise. Add an explicit guard at the
top of `executor::plan::execute_plan` and `executor::reason::execute_reason`:

```rust
if ctx.index.is_tombstoned(plan.start) {
    return Err(ExecError::MemoryNotFound { what: "start" });
}
if let Some(goal) = plan.goal { // some shapes
    if ctx.index.is_tombstoned(goal) {
        return Err(ExecError::MemoryNotFound { what: "goal" });
    }
}
```

REASON's input is a `seed: MemoryId`. Same guard.

## 5. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-planner/src/executor/path.rs` | per-neighbour `is_tombstoned` filter + start/goal entry guards | ~15 delta |
| `crates/brain-planner/src/executor/reason.rs` | per-neighbour filter + seed entry guard | ~10 delta |
| `crates/brain-planner/tests/path_executor.rs` | new test: `plan_excludes_tombstoned_chain_member` | ~80 |
| `crates/brain-planner/tests/reason_executor.rs` | new test: `reason_excludes_tombstoned_seed_neighbour` | ~80 |

Total: ~185 LOC. Tiny.

## 6. Tests

### 6.1 `plan_excludes_tombstoned_chain_member`

Setup: encode three memories `a → b → c` linked by `Follows` edges.
Tombstone `b` (via the writer's HNSW mark-removed path). Plan from
`a` to `c`. Expect `NoPathFound` because the only path runs through
the tombstoned `b`.

Then encode a fresh detour `a → d → c`. Plan again. Expect a path
that goes `a → d → c` and does *not* include `b`.

### 6.2 `reason_excludes_tombstoned_seed_neighbour`

Setup: encode a seed memory `s` with three `Implies` neighbours
`n1, n2, n3`. Tombstone `n2`. Run REASON from `s` with depth 1.
Expect the result set contains `n1` and `n3` but not `n2`.

### 6.3 `plan_rejects_tombstoned_start`

Encode `a`, tombstone it. Try `plan(start=a, goal=b)`. Expect
`MemoryNotFound`.

(Plus the symmetric `reason_rejects_tombstoned_seed`.)

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `ctx.index.is_tombstoned(...)` acquires the HNSW read lock per call | At PLAN's neighbour cadence (~100 calls/sec/shard, ~10 neighbours each) this is 1000 lock acquisitions/sec/shard. parking_lot RwLock read is ~10 ns uncontended; ~10 µs/sec total. Negligible. |
| Tombstone state changes mid-BFS | The HNSW's tombstone bitmap is updated by the writer; readers see a consistent snapshot per `read()` call. Mid-BFS changes are rare and self-correcting (a stale tombstone that "comes back" only happens via slot reclamation + new encode, which spec §16/01 §13 already requires us to handle via slot-version checks). v1 is correct under spec assumptions. |
| Start-guard breaks the "operate on a tombstoned id mid-flight" use case | None of the current ops do this. The guard surfaces a clear error; the caller can FORGET-and-retry. v2 can add a `bypass_tombstone_filter` flag if a use case shows up. |
| Test fixtures rely on real `RealWriterHandle` paths to tombstone | The writer's forget path is fully wired (Phase 7); we exercise it the same way `forget_end_to_end` already does. |

---

## 8. Done criteria

- [ ] `path.rs::execute_plan` filters tombstoned neighbours via `ctx.index.is_tombstoned`.
- [ ] `reason.rs::execute_reason` does the same.
- [ ] Both add start/seed guards returning `MemoryNotFound` for tombstoned entry points.
- [ ] 4 new tests pass (2 traversal-exclusion + 2 entry-guard).
- [ ] All existing planner tests still pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.16 marked `[x]`.

---

## 9. What 9.16 explicitly defers

- **`include_tombstoned` request flag** — spec §16/01 §12 says "not
  returned unless explicitly requested"; the wire shape doesn't
  carry an `include_tombstoned` knob yet. v2 adds it to PLAN /
  REASON request frames + threads through.
- **Batch-query `tombstoned_subset` API** — single-call retain is
  fine at v1 scale (see Risks §7).
- **Slot-version cross-check** — spec §16/01 §13 is its own
  acceptance criterion; v1 already handles via the slot version
  in `MemoryId`'s high bits. Out of 9.16's scope.

---

*Implement on approval.*
