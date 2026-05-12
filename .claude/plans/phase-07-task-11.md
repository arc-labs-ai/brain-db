# Sub-task 7.11 — Correctness suite from §16/01

**Spec:** `spec/16_benchmarks_acceptance/01_correctness_criteria.md`
**Phase doc:** `docs/phases/phase-07-operations.md` §7.11
**Done when:** "Every numbered criterion in §16/01 has a passing test."

---

## 1. Why one consolidated file

Most of these criteria already have tests scattered across `tests/{encode,recall,plan,reason,forget,link,txn,subscribe}.rs`. 7.11's job is **not** to duplicate them — it's to produce **one auditable file** that maps spec § numbers 1-to-1 onto executable tests so a reviewer can read §16/01 with one hand and `correctness.rs` with the other and confirm coverage.

`crates/brain-ops/tests/correctness.rs` is therefore organised by **spec section number**, each section a `mod § §X_short_name { ... }` (Rust path: `criterion_N_short_name`) with one or more `#[tokio::test]` functions that exercise the MUST clauses from that section.

The existing per-handler test files stay — they're the deep coverage; this file is the **acceptance gate** for Phase 7.

---

## 2. Scope mapping — what passes vs what is `#[ignore]`'d

| §  | Topic                  | v1 status                                | Test |
| -- | ---------------------- | ---------------------------------------- | ---- |
| 1  | Wire-protocol          | brain-protocol tests already exhaustive  | Smoke: round-trip every `RequestBody` variant we use in 7.x through `dispatch` |
| 2  | ENCODE                 | Sub-task 7.3 done                        | Encode N memories; verify each round-trips via RECALL |
| 3  | RECALL                 | Sub-task 7.4 done                        | Encode known set; RECALL top-K returns the closest; filters honoured |
| 4  | PLAN                   | Sub-task 7.5 done                        | Build a chain; PLAN returns it |
| 5  | REASON                 | Sub-task 7.6 done                        | Build CAUSED/SUPPORTS graph; REASON returns expected paths; cycle → no loop |
| 6  | FORGET                 | Soft only; Hard is Phase 8 worker         | Soft FORGET removes from RECALL; **Hard FORGET → `#[ignore]`** (Phase 8) |
| 7  | LINK / UNLINK          | Sub-task 7.8 done                        | LINK creates; UNLINK removes; query reflects both |
| 8  | Idempotency            | Sub-task 7.7 done                        | Same request_id → same MemoryId; cache hit returns same response |
| 9  | Transactions           | Sub-task 7.9 done                        | BEGIN+ENCODE+ABORT → invisible; BEGIN+ENCODE+COMMIT → visible |
| 10 | Filters                | Sub-task 7.4 done                        | RECALL with context/kind/min_salience returns only matches |
| 11 | Edge-traversal         | Sub-task 7.5/7.6 done                    | Multi-kind graph; only requested edge types traversed; direction honoured |
| 12 | Tombstone visibility    | Sub-task 7.7 done                        | FORGET m; RECALL/PLAN/REASON exclude m |
| 13 | Slot version            | **No hard-reclaim yet → `#[ignore]`**     | Stub test referring to Phase 8 GC worker |
| 14 | Audit log               | **No audit impl yet → `#[ignore]`**       | Stub referring to Phase 8 |
| 15 | Recovery                | **No WAL hookup → `#[ignore]`**           | Stub referring to Phase 9 |
| 16 | Configuration           | **Limited plumbing → `#[ignore]`**        | Stub referring to Phase 9 |
| 17 | Error codes             | Sub-task 7.1 done                         | Trigger each error condition; verify code |
| 18 | Schema versioning       | **Single version only → `#[ignore]`**     | Stub |
| 19 | Determinism             | brain-embed deterministic for mock        | Embed same text 5x → identical vector |
| 20 | "No surprises"          | Hard to test directly                    | A targeted "no partial state" test: forced-error mid-op leaves no orphans |

**Concrete count:** 14 sections get **passing** tests; 6 sections get **documented `#[ignore]`** with a comment pointing to the phase that closes them. The phase doc's exit checklist says "Correctness suite all passing" — we interpret that as "every non-ignored test passes, and every ignored test is documented as Phase-N work." The user can override this interpretation.

---

## 3. File layout

```rust
//! Spec §16/01 correctness gate. One numbered section per criterion.

mod common { /* shared fixture */ }

mod criterion_01_wire { #[tokio::test] async fn frames_roundtrip_through_dispatch() { ... } }
mod criterion_02_encode { ... }
mod criterion_03_recall { ... }
mod criterion_04_plan { ... }
mod criterion_05_reason { ... }
mod criterion_06_forget {
    #[tokio::test] async fn soft_forget_hides_from_recall() { ... }
    #[ignore = "hard FORGET reclamation — Phase 8 GC worker"]
    #[tokio::test] async fn hard_forget_zeroes_arena() { ... }
}
mod criterion_07_link_unlink { ... }
mod criterion_08_idempotency { ... }
mod criterion_09_txn { ... }
mod criterion_10_filters { ... }
mod criterion_11_edge_traversal { ... }
mod criterion_12_tombstones { ... }
mod criterion_13_slot_version {
    #[ignore = "hard-reclaim + slot-version mismatch — Phase 8 GC worker"]
    #[tokio::test] async fn stale_memory_id_returns_not_found_after_reclaim() { ... }
}
mod criterion_14_audit_log {
    #[ignore = "audit log — Phase 8 worker (spec §14)"]
    #[tokio::test] async fn every_mutating_op_is_audit_logged() { ... }
}
mod criterion_15_recovery {
    #[ignore = "crash recovery — Phase 9 (WAL writer hookup)"]
    #[tokio::test] async fn restart_preserves_committed_writes() { ... }
}
mod criterion_16_config {
    #[ignore = "configuration plumbing — Phase 9 server"]
    #[tokio::test] async fn config_overrides_are_honoured() { ... }
}
mod criterion_17_error_codes { ... }
mod criterion_18_schema {
    #[ignore = "schema versioning — beyond v1"]
    #[tokio::test] async fn schema_v1_data_reads_with_v1_code() { ... }
}
mod criterion_19_determinism { ... }
mod criterion_20_no_surprises { ... }
```

Each non-ignored test is **self-contained** and **fast** (< 100 ms). The full file should finish in under 2 seconds.

---

## 4. Test details (the in-scope 14)

### §1 — wire correctness (smoke)
Build one of each `RequestBody` variant used by handlers (Encode/Recall/Plan/Reason/Forget/Link/Unlink/Txn*/Subscribe/Unsubscribe), serialise to rkyv, deserialise, and assert equality. The full fuzz/CRC suite lives in `brain-protocol/tests`; this is a smoke test that ensures the ops-layer wire types survive a round-trip.

### §2 — ENCODE
Encode 5 memories; for each, RECALL with the original text and assert top-1 = its memory_id with similarity ≥ 0.99.

### §3 — RECALL
Encode 10 memories with deterministic mock vectors. RECALL top-3 with one cue; assert ordering matches cosine. Then RECALL with context filter; assert off-context drops.

### §4 — PLAN
Build a 4-hop FOLLOWED_BY chain. PLAN from the first memory; assert returned path contains all 4 in order.

### §5 — REASON
Build a CAUSED + SUPPORTS graph with one cycle. REASON from the seed at depth 3; assert returned paths cover both reasoning edges and that depth bound is enforced (no path with > 3 hops; no infinite loop).

### §6 — FORGET (soft only)
Soft FORGET m; RECALL with the original cue; assert m is not in results. Hard FORGET is `#[ignore]`'d.

### §7 — LINK / UNLINK
LINK m1 → m2 (CAUSED). RECALL→PLAN/REASON path through m1 finds m2. UNLINK. PLAN no longer includes the edge.

### §8 — Idempotency
ENCODE with request_id X → memory_id Y. ENCODE same request → Y, `was_deduplicated=true`. ENCODE with X but different text → Conflict.

### §9 — Transactions
`TXN_BEGIN` + `ENCODE(txn)` + `ABORT` → external RECALL doesn't see the memory. Repeat with COMMIT → visible.

### §10 — Filters
Encode memories with contexts {1, 2, 3} and kinds {Episodic, Semantic}. RECALL with `contexts=[2]` returns only context=2. RECALL with `kinds=[Semantic]` returns only Semantic. Combined returns the intersection.

### §11 — Edge traversal
Build a graph with mixed CAUSED + SUPPORTS edges. PLAN restricted to FOLLOWED_BY doesn't return the CAUSED-only chain. REASON restricted to SUPPORTS doesn't traverse CAUSED.

### §12 — Tombstone visibility
FORGET m. Subsequent RECALL/PLAN/REASON exclude m. Asserting via three handler calls.

### §17 — Error codes
Drive five distinct errors and verify each maps to the right `ErrorCode`:
- Encode w/ duplicate request_id but different params → `Conflict`
- Forget unknown memory → `NotFound` (via the response payload, not an Err)
- Recall with similarity > 1.0 → `InvalidRequest` (via planner)
- Unsubscribe unknown stream → `NotFound`
- Subscribe with `similar_to` → `InternalError` (NotYetImplemented family)

### §19 — Determinism
Embed the same text 5x via `MockDispatcher::embed`; assert all returned vectors are bitwise-equal.

### §20 — No surprises
Trigger a writer failure (`force_reclaim_now` is N/A; instead: hit the LINK source-not-found error) and verify the response is the structured error AND no partial state exists (the EDGES_OUT table count for the would-be source remains unchanged).

---

## 5. Shared fixture

Same shape as the `tests/encode.rs` fixture: tempdir, MetadataDb, SharedHnsw, RealWriterHandle with the writer's event-bus left disconnected (we're not testing SUBSCRIBE here, that has its own file). MockDispatcher provides deterministic embeddings.

---

## 6. Done criteria

- [ ] `crates/brain-ops/tests/correctness.rs` exists with 20 modules (one per spec §).
- [ ] Each module has at least one `#[tokio::test]`.
- [ ] In-scope tests (14 sections) pass.
- [ ] Out-of-scope tests (6 sections) are `#[ignore]` with a one-line reason comment pointing to the phase that closes the gap.
- [ ] `cargo test --workspace` green.
- [ ] Commit subject: `test(brain-ops): §16/01 correctness gate (sub-task 7.11)`.

---

## 7. Estimated effort

~700-900 LOC, single file. One container session. No spec changes, no wire bump, no impl-side changes expected (this is a test-only sub-task).

If during implementation a test surfaces a real bug, surface it via plan-update before fixing (per CLAUDE.md §13 + AUTONOMY §3).
