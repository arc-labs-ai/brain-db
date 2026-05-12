# Phase 7 — Cognitive Operations

Orientation plan. Surfaces the spec-grounded decisions before sub-task 7.1 lands. Implementation lives in `crates/brain-ops/` (currently a 25-line stub).

## 0. Goal

`brain-ops` is the **wire-bound** layer that takes a typed `RequestBody` (from `brain-protocol`) and returns a `ResponseBody`, with full cognitive-operation semantics: idempotency, WAL-before-acknowledge durability, PLAN / REASON edge traversal, LINK / UNLINK, transactions, and SUBSCRIBE.

After Phase 7 lands:
- Every wire opcode has a working handler.
- Every correctness criterion in `spec/16_benchmarks_acceptance/01_correctness_criteria.md` has a passing test.
- The substrate is feature-complete at the cognitive-operations layer; Phase 8 (workers) and Phase 9 (server) wire the network surface.

Tag: `phase-7-complete`.

## 1. Spec grounding (11 files)

| Spec § | Topic | Sub-task anchor |
|---|---|---|
| 00 Purpose | the five primitives + LINK/UNLINK/TXN/SUBSCRIBE/ADMIN | read first |
| **01 Semantics overview** | **agent-substrate contract, identity, time, consistency** | 7.1 |
| **02 ENCODE** | **wire shape, idempotency, latency promise** | 7.2, 7.3 |
| **03 RECALL** | **filters, ranking blend (similarity + salience + recency + access)** | 7.4 |
| 04 PLAN | bidirectional BFS along graph; edge-kind filters; depth bounds | 7.5 |
| 05 REASON | supports / contradicts traversal; aggregation | 7.6 |
| **06 FORGET** | **soft tombstone vs hard zero; UNFORGET; force_reclaim_now** | 7.7 |
| **07 LINK / UNLINK** | **direct edge manipulation; weights; symmetry rules** | 7.8 |
| 08 Transactions | TXN_BEGIN/COMMIT/ABORT; single-shard; bounded duration | 7.9 |
| 09 Subscribe | change-stream; filter; backpressure | 7.10 |
| **§16/01 Correctness** | **20 numbered criteria; test surface** | 7.11 |

## 2. Crate-level structure

```
crates/brain-ops/
├── Cargo.toml          (+ brain-protocol, brain-planner, brain-embed,
│                         brain-index, brain-metadata, brain-core;
│                         tracing, thiserror; dev: tokio, tempfile,
│                         parking_lot, uuid)
└── src/
    ├── lib.rs              (re-exports + module wiring)
    ├── error.rs            (OpError: wraps PlanError + ExecError + new variants)
    ├── context.rs          (OpsContext: bag of handles for handlers)
    ├── idempotency.rs      (read-table check + write-table store)
    ├── writer.rs           (RealWriterHandle: implements brain-planner's
    │                        WriterHandle trait, backed by metadata + index)
    ├── dispatch.rs         (Operation::dispatch(req) -> ResponseBody)
    ├── encode.rs           (handle_encode: idempotency + plan + execute)
    ├── recall.rs           (handle_recall)
    ├── plan.rs             (handle_plan: full BFS executor, not Phase 6's stub)
    ├── reason.rs           (handle_reason: supports + contradicts traversal)
    ├── forget.rs           (handle_forget + UNFORGET)
    ├── link.rs             (handle_link + handle_unlink)
    ├── txn.rs              (TXN_BEGIN/COMMIT/ABORT — single-shard buffer)
    └── subscribe.rs        (change stream; filter; backpressure)
```

Plus `tests/correctness.rs` for the §16/01 sweep (sub-task 7.11).

## 3. Cross-crate boundaries

`brain-ops` is the **integration crate** for the whole substrate. It depends on:

- **`brain-protocol`** — `RequestBody`, `ResponseBody` wire types.
- **`brain-planner`** — `plan_*`, `execute_*`, `WriterHandle`, `ExecutorContext`, `ExecutionPlan`, `ExecutionResult`.
- **`brain-embed`** — `Dispatcher` (forwarded into `ExecutorContext`).
- **`brain-index`** — `SharedHnsw` (forwarded).
- **`brain-metadata`** — `MetadataDb`, idempotency table, edge tables.
- **`brain-core`** — IDs, error taxonomy.

It is **consumed by** `brain-server` (Phase 9). Phase 8 workers also consume it for `ADMIN_*` operations.

The crate is where the planner's runtime-agnostic shape gets bound to actual wire types, real idempotency, real edge traversal, and stream framing.

## 4. Design decisions to surface before 7.1

### 4.1 Where does the real `WriterHandle` live?

Phase 6's orientation plan §4.4 said the writer task lands in Phase 8 / 9. But Phase 7 needs a working write path for end-to-end tests. Two options:

- **(A) Phase 7 ships a real `WriterHandle` impl** in `brain-ops/src/writer.rs` that owns `Arc<Mutex<MetadataDb>>` and a `SharedHnsw<384>::Writer`. No WAL yet — the actual durability path is Phase 8 / 9. Tests use this impl directly; production server can swap to a WAL-backed version later.
- **(B) Keep using Phase 6's `FakeWriterHandle`** pattern. Each integration test copies the fake. Production gets the real writer in Phase 8 / 9.

**Recommendation: (A).** `brain-ops` is the integration crate; having a "real-shaped, no-WAL" writer here matches its job. Phase 8 / 9 will replace the in-process call with channel-fed group-commit per spec §08/08 §10. The trait surface (already pinned in Phase 6.4) doesn't change.

### 4.2 Idempotency layer — where and how?

Spec §08/04 §4 says the executor's first step is the idempotency check (read txn on `idempotency` table). Phase 6's `FakeWriterHandle` did per-RequestId replay via a `HashMap` — the in-memory equivalent. Phase 7 implements the real check against `brain-metadata`'s `idempotency` table (which Phase 3.5 shipped).

Spec §07/06 (idempotency, in metadata) gives the table shape. Lookup: `(request_id, request_hash) → cached_response`. On hit and matching hash → return cached. On hit and mismatched hash → `Conflict`. On miss → proceed; store the response after the write commits.

**Decision:** the idempotency layer is a *function* `idempotency::check_and_replay` called by each write handler before invoking the planner. The check is a brief read txn; the store is part of the same write txn the writer commits. This collapses cleanly into our `WriterHandle` impl from §4.1 (writer owns both sides of the table).

### 4.3 PLAN / REASON executors

Phase 6.5 deferred these. Phase 7 must land them. The bulk is in two areas:

- **Edge traversal across `brain-metadata`'s `edges_out` / `edges_in` tables.** Phase 3.7 (`edge.rs`) shipped the storage; we read it. Iteration over a memory's outgoing edges is a range scan on the prefix `(source_memory_id, …)`.
- **Bidirectional BFS for PLAN; supports/contradicts BFS for REASON.** Spec §08/05 §4 has the pseudocode; we transliterate.

The PLAN executor's depth bound + branching factor → ~b^(d/2) memory peak; we cap explored paths at `traversal.max_paths` (set by planner from `req.budget.max_branches_explored`) to prevent runaway.

REASON's `confidence_threshold` filters evidence; aggregation produces a `confidence: f32` per spec §08/05 §10.

### 4.4 LINK / UNLINK

Spec §09/07 — direct edge manipulation. Single-shard, no embed step, no HNSW interaction. Just write to `edges_out` / `edges_in` (symmetric mirroring; brain-metadata's `edge.rs` already handles this in Phase 3.7).

LINK is idempotent at the per-edge level (same `(source, kind, target)` → no-op). Wire response carries the edge id (composite of `(source, kind, target)` is the natural key).

UNLINK rejects unknown edges with `NotFound`, per spec §07 §10.

### 4.5 Transactions (TXN_BEGIN / COMMIT / ABORT)

Spec §09/08. Single-shard only (§08/03). Single-txn-at-a-time per connection. Bounded duration (default 30 s).

Implementation shape:
- `TXN_BEGIN` returns a `TxnId`. State: `BTreeMap<TxnId, TxnState>` keyed by id. `TxnState` is a buffer of pending ops + a snapshot LSN.
- Each subsequent op with `txn_id: Some(tid)` is appended to the buffer instead of executed.
- `TXN_COMMIT(tid)` applies the buffered ops atomically (one writer batch). On any inner failure, abort.
- `TXN_ABORT(tid)` drops the buffer.

Risks: complex. Spec §08 has the full shape but Phase 7 must build it.

### 4.6 SUBSCRIBE

Spec §09/09 — change stream. A subscription holds an open stream that the substrate writes to as new memories / edges arrive matching the filter. Backpressure: bounded `max_inflight` per spec.

This is the deepest cross-cutting piece. It needs:
- A change-feed source (WAL tail, or a broadcast channel from the writer).
- A filter applied per event.
- Stream framing — but actual wire-level streaming is Phase 9 (server).

**Decision for 7.10:** ship the in-process change-feed (`tokio::sync::broadcast` or similar) + filter + `Stream<Item = Event>` shape. Phase 9 wires this to the network. Tests assert events arrive in WAL order with filter applied.

### 4.7 The dispatcher (7.1)

Top-level `Operation::dispatch(req: RequestBody, ctx: &OpsContext) -> Result<ResponseBody, OpError>`. Matches the request variant to its handler.

Replaces / wraps Phase 6's `execute(plan, &ctx)`. The flow becomes:

```
RequestBody
  → handle_*(req, ctx)        // brain-ops: idempotency check + plan + execute + map
  → ResponseBody
```

`OpError` wraps `PlanError` + `ExecError` + new variants (`Conflict`, `NotFound`, `TooManyMemories`, `TxnExpired`, `Overloaded`).

### 4.8 Sub-task 7.11 — correctness suite

§16/01 has 20 sections, each with N numbered criteria. Spec is the test plan.

**Decision:** incremental — each sub-task 7.3–7.10 adds the §16/01 criteria relevant to its op. 7.11 is the **final sweep** that fills gaps (cross-op criteria like §16/01 §08 idempotency, §09 transactions). Avoids one giant test crate at the end.

### 4.9 Brain-storage usage

Phase 6 carefully avoided `brain-storage` (Linux-only syscalls) in the planner crate proper. Phase 7's `RealWriterHandle` *could* depend on it for the WAL path — but we decided in §4.1 to ship without WAL for v1 of brain-ops. So Phase 7 *doesn't* take a new transitive dep on brain-storage beyond what brain-metadata already pulls in.

This means Phase 7 development still requires the Linux dev container (same as Phase 6) but doesn't add new Linux-only surface.

### 4.10 Test harness reuse

Phase 6's `FakeWriterHandle` lives in three test files (`encode_end_to_end.rs`, `forget_end_to_end.rs`, `dispatch.rs`). Phase 7 introduces a *real* writer; the fakes become obsolete for the new crate. But we still want tests to be fast — no full WAL fsync. Two paths:

- **(A)** Phase 7's `RealWriterHandle` writes through brain-metadata + brain-index synchronously. Tests use it directly. Same speed as the fakes.
- **(B)** Keep the trait-based fakes for unit tests; reserve the real handle for integration tests.

**Decision: (A).** No WAL means the real handle is already fast (single tempdir txn per write). Tests use it directly. The `FakeWriterHandle` pattern can be retired from new code; Phase 6's tests stay using it (no migration needed).

### 4.11 Scope cuts — what we ship vs defer

11 sub-tasks is heavy. Options to consider:

- **All 11**: full Phase 7 as specified.
- **Defer 7.9 + 7.10**: ship a "core" Phase 7 (1–8 + 11). Transactions and Subscribe are the most complex; they could land in Phase 8 (workers) or a Phase 7.5. The substrate would still be functionally complete for single-op writes + reads.
- **Defer 7.10 only**: SUBSCRIBE involves stream framing that crosses into Phase 9 territory anyway.

**Recommendation: ship all 11**, but mark 7.9 (Transactions) and 7.10 (Subscribe) as "minimum viable" — buffered single-shard txn, in-process broadcast channel. Anything more sophisticated lives in a future phase.

Worth surfacing as an `AskUserQuestion` before sub-task 7.1.

## 5. The 11 sub-tasks (re-confirmed against spec)

| # | Title | Spec anchor | Notes |
|---|---|---|---|
| 7.1 | `Operation` dispatcher | §09/01 | Wire RequestBody → handler → ResponseBody. Replaces planner's `execute()` |
| 7.2 | Idempotency layer | §09/02 + §07/06 | Real metadata table; check + store. Used by every write handler |
| 7.3 | ENCODE handler | §09/02 | Wraps plan_encode + execute_encode + idempotency + wire mapping |
| 7.4 | RECALL handler | §09/03 | Ranking blend (similarity + salience + recency + access boost) |
| 7.5 | PLAN handler | §09/04 | Bidirectional BFS over edges; the executor Phase 6 deferred |
| 7.6 | REASON handler | §09/05 | Supports/contradicts BFS + aggregation; Phase 6 deferred |
| 7.7 | FORGET handler + UNFORGET | §09/06 | Soft/Hard already shipped; UNFORGET is new |
| 7.8 | LINK / UNLINK handlers | §09/07 | Direct edge writes; idempotent per edge |
| 7.9 | Transactions | §09/08 | TXN_BEGIN/COMMIT/ABORT; single-shard; buffer until commit |
| 7.10 | SUBSCRIBE | §09/09 | Change-feed + filter + Stream<Event>; wire framing is Phase 9 |
| 7.11 | Correctness sweep | §16/01 | Fill any remaining criteria from §16/01 |

## 6. Expected new dependencies

All already in workspace `[workspace.dependencies]`:
- `brain-protocol`, `brain-planner`, `brain-embed`, `brain-index`, `brain-metadata`, `brain-core` (paths)
- `thiserror`, `tracing`, `parking_lot` (workspace)
- Dev: `tempfile`, `tokio`, `uuid` (workspace)

Possibly new:
- `tokio = { features = ["sync", "macros", "rt"] }` for `broadcast` channel (SUBSCRIBE). Already a dev-dep in brain-planner; might need promotion.

That's the only net-new dep risk. No new external crates.

## 7. Phase exit criteria

- [ ] Sub-tasks 7.1–7.11 ✅.
- [ ] `cargo test -p brain-ops` green (dev container).
- [ ] Every numbered criterion in `spec/16_benchmarks_acceptance/01_correctness_criteria.md` has a passing test (incremental across 7.3–7.10, swept up in 7.11).
- [ ] Idempotency tests pass for every write op (ENCODE, FORGET, LINK, UNLINK, TXN_*).
- [ ] PLAN / REASON traversal tests against hand-built graphs.
- [ ] SUBSCRIBE delivers events in WAL order with filter applied.
- [ ] Tag `phase-7-complete`.

## 8. Open items for the user before 7.1

Three calls worth confirming up front:

1. **`RealWriterHandle` home**: ship the no-WAL real impl in `brain-ops/src/writer.rs` (recommended) vs keep the `FakeWriterHandle` pattern from Phase 6 vs defer to Phase 9?
2. **Scope**: ship all 11 sub-tasks including TXN_* and SUBSCRIBE (recommended, MVP-shape) vs defer 7.9 + 7.10 to a later phase?
3. **Correctness suite (7.11)**: incremental across 7.3–7.10 + final sweep (recommended) vs one-shot test crate at the end?

After confirmation, sub-task 7.1's plan goes in next.

---

PLAN READY.
