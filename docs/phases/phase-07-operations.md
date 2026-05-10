# Phase 7 — Cognitive Operations

## Goal

Implement ENCODE, RECALL, PLAN, REASON, FORGET (plus LINK/UNLINK and TXN_*) on top of the planner. Idempotency is enforced here. After this phase, every wire opcode has a working server-side handler with the spec'd semantics, including the "WAL before acknowledge" durability contract.

## Prerequisites

- [x] Phase 6 complete.
- All five lower crates are usable.

## Reading list

1. [`spec/09_cognitive_operations/00_purpose.md`](../../spec/09_cognitive_operations/00_purpose.md)
2. [`spec/09_cognitive_operations/01_semantics_overview.md`](../../spec/09_cognitive_operations/01_semantics_overview.md)
3. [`spec/09_cognitive_operations/02_encode.md`](../../spec/09_cognitive_operations/02_encode.md)
4. [`spec/09_cognitive_operations/03_recall.md`](../../spec/09_cognitive_operations/03_recall.md) — **the most-read spec section.**
5. [`spec/09_cognitive_operations/04_plan.md`](../../spec/09_cognitive_operations/04_plan.md)
6. [`spec/09_cognitive_operations/05_reason.md`](../../spec/09_cognitive_operations/05_reason.md)
7. [`spec/09_cognitive_operations/06_forget.md`](../../spec/09_cognitive_operations/06_forget.md)
8. [`spec/09_cognitive_operations/07_link_unlink.md`](../../spec/09_cognitive_operations/07_link_unlink.md)
9. [`spec/09_cognitive_operations/08_transactions.md`](../../spec/09_cognitive_operations/08_transactions.md)
10. [`spec/09_cognitive_operations/09_subscribe.md`](../../spec/09_cognitive_operations/09_subscribe.md)
11. [`spec/16_benchmarks_acceptance/01_correctness_criteria.md`](../../spec/16_benchmarks_acceptance/01_correctness_criteria.md)

## Outputs

- `crates/brain-ops` exports per-op handlers + an `Operation` dispatcher.
- Idempotency layer wraps writes.
- Every correctness criterion in spec §16/01 has a passing test.
- Tag: `phase-7-complete`.

## Sub-tasks

### Task 7.1 — `Operation` dispatcher
**Reads:** `spec/09_cognitive_operations/01_semantics_overview.md`
**Writes:** `crates/brain-ops/src/dispatch.rs`
**Done when:** Given a `RequestBody`, dispatcher picks a handler and returns a `ResponseBody` (or error).

### Task 7.2 — Idempotency layer
**Reads:** `spec/09_cognitive_operations/02_encode.md` (idempotency section), `spec/07_metadata_graph/06_idempotency.md`
**Writes:** `crates/brain-ops/src/idempotency.rs`
**What to build:**
- Wrap writes: check idempotency table by RequestId; if hit and params match, return cached response; if hit and params differ, return `Conflict`.
- On success, store the response with insert-time.
**Done when:** Same RequestId returns same response within 24h; different params → `Conflict`; expired entries → re-execute.

### Task 7.3 — ENCODE handler
**Reads:** `spec/09_cognitive_operations/02_encode.md`
**Writes:** `crates/brain-ops/src/encode.rs`
**Done when:** End-to-end test: send EncodeRequest → receive MemoryId → memory queryable via Recall.

### Task 7.4 — RECALL handler
**Reads:** `spec/09_cognitive_operations/03_recall.md`
**Writes:** `crates/brain-ops/src/recall.rs`
**What to build:**
- All filters (agent, context, kind, salience, time, exclude_tombstoned).
- Ranking blend per spec: similarity + salience + recency + access boost.
- K up to 1000.
**Done when:** Tests cover: basic recall, filters, with/without text body, top-1 exact match, sorted descending by score.

### Task 7.5 — PLAN handler
**Reads:** `spec/09_cognitive_operations/04_plan.md`
**Writes:** `crates/brain-ops/src/plan.rs`
**Done when:** Build a known graph; PLAN from a starting memory traverses FollowedBy edges and returns the chain.

### Task 7.6 — REASON handler
**Reads:** `spec/09_cognitive_operations/05_reason.md`
**Writes:** `crates/brain-ops/src/reason.rs`
**Done when:** Multi-hop traversal across multiple edge kinds; depth bound respected; cycles don't loop.

### Task 7.7 — FORGET handler
**Reads:** `spec/09_cognitive_operations/06_forget.md`
**Writes:** `crates/brain-ops/src/forget.rs`
**Done when:**
- Soft FORGET: tombstone, invisible to RECALL, recoverable via UNFORGET.
- Hard FORGET: zero vector + text, irreversible.
- `force_reclaim_now` flag immediately frees the slot.

### Task 7.8 — LINK / UNLINK handlers
**Reads:** `spec/09_cognitive_operations/07_link_unlink.md`
**Writes:** `crates/brain-ops/src/link.rs`
**Done when:** LINK creates the edge (both directions if symmetric); UNLINK removes it.

### Task 7.9 — Transactions
**Reads:** `spec/09_cognitive_operations/08_transactions.md`
**Writes:** `crates/brain-ops/src/txn.rs`
**Done when:** Multi-op transactions commit atomically; aborts roll back; reads within see uncommitted writes.

### Task 7.10 — SUBSCRIBE (streaming)
**Reads:** `spec/09_cognitive_operations/09_subscribe.md`
**Writes:** `crates/brain-ops/src/subscribe.rs`
**Done when:** Subscribe to "new memories matching filter X"; sink receives events; backpressure works.

### Task 7.11 — Correctness suite from §16/01
**Reads:** `spec/16_benchmarks_acceptance/01_correctness_criteria.md`
**Writes:** `crates/brain-ops/tests/correctness.rs`
**Done when:** Every numbered criterion in §16/01 has a passing test.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] Correctness suite (§16/01) all passing.
- [ ] Idempotency tests pass for every write op.
- [ ] Tag `phase-7-complete`.
