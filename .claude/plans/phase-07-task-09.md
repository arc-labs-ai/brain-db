# Sub-task 7.9 — TRUE Transactions (buffer-and-apply)

User chose **Option α** — true txn semantics with buffer-until-commit
and rollback. Scope is bigger than the 7.x sub-tasks before it.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/08 §3 | COMMIT writes WAL marker, fsyncs, applies atomically |
| §09/08 §4 | ABORT discards pending ops |
| §09/08 §5 | ENCODE/LINK/UNLINK/FORGET buffer until COMMIT; RECALL/PLAN/REASON see pending writes |
| §09/08 §6 | WAL records carry txn_id; recovery applies BEGIN→COMMIT, skips BEGIN→ABORT |
| §09/08 §7 | Read-committed isolation + read-your-writes within a txn |
| §09/08 §9 | 30 s default timeout, 5 min max |
| §09/08 §11 | TransactionNotFound, Expired, TooLarge |
| §09/08 §14 | Single-shard only |
| §09/08 §18 | TXN_COMMIT replay by request_id (we use txn_id, since wire has no request_id field on TXN_*) |

## 1. Scope

**In scope:**

- True buffer-until-commit semantics. Operations carrying `txn_id` do
  **not** mutate redb or HNSW until COMMIT.
- Read-your-writes: RECALL/PLAN/REASON carrying `txn_id` see pending
  writes from the buffer in addition to committed state.
- Atomic commit: all buffered ops apply in a **single redb write txn**.
  If any fails the whole commit aborts (writes nothing).
- Atomic rollback: ABORT discards the buffer; no redb / HNSW state
  changes from the txn.
- Bounded timeout (1–300 s). Expired txns reject further ops.
- Idempotent COMMIT / ABORT replay by `txn_id`.
- Sweep of expired txns on the next TXN_BEGIN.

**Out of scope (still Phase 9 / later):**

- WAL records (TXN_BEGIN / TXN_COMMIT / TXN_ABORT markers + per-op
  records). `RealWriterHandle` currently has no WAL; we don't add one
  in 7.9. The durability gap is documented: if the process crashes
  between **ack** and the operation being on disk, the client never
  got an ack so there's no "lost durable write" — but a crash
  mid-commit (after some redb work but before commit) is recovered
  by redb's own MVCC. Single big wtxn means atomicity holds.
- Cross-shard txns.
- Nested-txn detection at the substrate (no client identity at this
  layer yet; SDK enforces).
- WAL-backed durable txn registry (in-memory only; restart drops
  active txns).

## 2. Architecture

### 2.1 Pre-commit `MemoryId` allocation

Read-your-writes requires `m1 = encode(txn=T); recall(txn=T)` to find
`m1`. So `ENCODE(txn=T)` must return a stable `MemoryId` immediately,
**before** COMMIT.

Approach: `WriterHandle::reserve_memory_id() -> MemoryId` — bumps the
shard's `next_slot` atomic, packs a `MemoryId`, returns. The slot is
not written to redb / HNSW until COMMIT. If ABORT, the slot is
wasted (skipped — `next_slot` keeps marching). Spec §09/08 §10 caps
txn size at 1000 ops so the leak is bounded.

### 2.2 `TxnStore` + `TxnContext`

```rust
pub type TxnId = [u8; 16];

pub struct TxnStore {
    entries: parking_lot::Mutex<std::collections::HashMap<TxnId, TxnEntry>>,
}

pub enum TxnState { Active, Committed, Aborted, Expired }

pub struct TxnEntry {
    pub state: TxnState,
    pub started_at_unix_nanos: u64,
    pub expires_at_unix_nanos: u64,
    pub ops_applied: u32,
    pub final_response: Option<TxnFinalResponse>,
    /// `None` once committed/aborted. Holds the in-flight buffer
    /// while Active.
    pub buffer: Option<TxnBuffer>,
}

pub struct TxnBuffer {
    pub pending_memories: Vec<PendingMemory>,
    pub pending_tombstones: std::collections::HashSet<brain_core::MemoryId>,
    pub pending_links: Vec<PendingLink>,
    pub pending_unlinks: std::collections::HashSet<(brain_core::MemoryId, brain_core::EdgeKind, brain_core::MemoryId)>,
    /// Idempotency keys consumed by ops inside the txn so we can
    /// dedup intra-txn replays and pre-write the entries on commit.
    pub request_id_index: std::collections::HashMap<[u8; 16], BufferedOpRef>,
}

pub struct PendingMemory {
    pub memory_id: brain_core::MemoryId,
    pub metadata: brain_metadata::tables::memory::MemoryMetadata,
    pub vector: [f32; brain_embed::VECTOR_DIM],
    pub text: String,
    pub edges: Vec<PendingLink>,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at: u64,
}

pub struct PendingLink {
    pub source: brain_core::MemoryId,
    pub target: brain_core::MemoryId,
    pub kind: brain_core::EdgeKind,
    pub weight: f32,
    pub request_id: [u8; 16],
    pub request_hash: [u8; 32],
    pub created_at: u64,
}

// ...similar PendingForget, PendingUnlink.
```

### 2.3 `OpsContext` gains `txn_store`

```rust
pub struct OpsContext {
    pub executor: ExecutorContext,
    pub planner_ctx: PlannerContext,
    pub txn_store: std::sync::Arc<txn::TxnStore>,
}
```

### 2.4 Handler routing

Each mutator handler (`encode` / `forget` / `link` / `unlink`) gains
a branch:

```rust
match req.txn_id {
    None => /* existing immediate-execute path */,
    Some(txn_id) => {
        validate_txn_active(ctx.txn_store, txn_id)?;
        let preview = build_preview_ack(...)?;          // allocates MemoryId, computes embedding, etc.
        push_to_buffer(ctx.txn_store, txn_id, preview)?; // also caches request_id → result
        emit_wire_response(preview)
    }
}
```

The preview response is what the client sees on the wire — the same
shape that COMMIT will later produce.

### 2.5 Read-your-writes lens

A `TxnLens` view layered atop the executor's read paths:

```rust
pub struct TxnLens<'a> {
    pub buffer: &'a TxnBuffer,
}

impl<'a> TxnLens<'a> {
    pub fn scan_pending_memories<F>(&self, mut f: F)
    where F: FnMut(&PendingMemory);

    pub fn is_tombstoned(&self, id: MemoryId) -> bool;

    /// Returns pending outgoing edges from `source` filtered by
    /// `edge_kinds`, minus any pending unlinks. Used by PLAN/REASON.
    pub fn edges_out(&self, source: MemoryId, kinds: &HashSet<EdgeKind>) -> Vec<(EdgeKind, MemoryId, f32)>;

    /// Same for incoming.
    pub fn edges_in(&self, target: MemoryId, kinds: &HashSet<EdgeKind>) -> Vec<(EdgeKind, MemoryId, f32)>;
}
```

**RECALL**:
- HNSW search → committed candidates.
- `lens.scan_pending_memories` → linear cosine over the buffer; add
  hits.
- `lens.is_tombstoned(id)` → drop those.
- Filter + sort + truncate (existing logic).

**PLAN**: BFS neighbour lookup wraps `list_edges_from/to` with
`lens.edges_out/in`. Pending unlinks subtract from the result.

**REASON**: same wrapping for the outward walks. Direct-similarity
supporting items include the txn's pending memories.

### 2.6 TXN_COMMIT — atomic apply

```rust
fn handle_txn_commit(...) -> Result<TxnCommitResponse, OpError> {
    let buffer = take_buffer_and_mark_committing(store, txn_id)?;
    
    // Single redb write txn. All-or-nothing.
    apply_buffer_atomically(&buffer, &ctx.executor)?;
    
    // HNSW inserts (post-wtxn; if these fail we log but the redb
    // state is already durable so we don't roll back the txn).
    for pending in &buffer.pending_memories {
        hnsw.insert(pending.memory_id, &pending.vector)?;
    }
    // HNSW tombstones similarly.
    
    finalise_commit(store, txn_id, ops_applied);
    Ok(TxnCommitResponse { ... })
}
```

`apply_buffer_atomically` opens **one** `MetadataDb::write_txn`, then:

1. Inserts all `pending_memories` rows (with their pre-allocated ids
   and pre-computed edge counts).
2. Inserts all `pending_links` (including those bundled inside
   pending_memories.edges); bumps in/out counts on existing rows.
3. Removes `pending_unlinks` rows; decrements counts.
4. For each `pending_tombstone`: HNSW.mark_tombstoned (deferred); no
   redb mutation needed for soft forget in v1 (the `MemoryMetadata`
   `flags` field could carry the tombstone but the existing FORGET
   path doesn't update the row either — see writer.rs `do_forget`).
   For now we record tombstones in HNSW only, matching the
   non-txn FORGET behaviour.
5. Inserts idempotency entries for every op in the buffer (one row
   per `request_id`).
6. Commits the wtxn.

If any step errors → return the wtxn (drop, redb auto-aborts);
mark the txn `Aborted`. Buffer is dropped.

### 2.7 TXN_ABORT

```rust
fn handle_txn_abort(...) -> Result<TxnAbortResponse, OpError> {
    let (ops_count, _buffer) = take_buffer_and_mark_aborted(store, txn_id)?;
    Ok(TxnAbortResponse { txn_id, operations_discarded: ops_count })
}
```

Buffer is dropped on the floor. Reserved `MemoryIds` are wasted but
bounded. Nothing was written to redb or HNSW.

### 2.8 Idempotency within a txn

- TXN_COMMIT / TXN_ABORT replay by `txn_id`: same id → same response.
- Mutator-op `request_id` replay **within the same txn**: caught via
  `buffer.request_id_index`. Returns the previewed ack from the
  buffer.
- Mutator-op `request_id` replay **across txns**: caught at COMMIT
  time when the idempotency row is inserted; same `request_id` with
  different hash → COMMIT fails with Conflict (txn rolls back).

### 2.9 WriterHandle additions

```rust
pub trait WriterHandle: Send + Sync {
    // existing: submit_encode / submit_forget / submit_link / submit_unlink

    /// Reserve a fresh `MemoryId` without writing anything. The
    /// returned id may be used by the caller (e.g., a transaction's
    /// pending buffer); if the caller never commits, the slot is
    /// silently wasted (skipped in `next_slot`). Bounded leakage by
    /// spec §09/08 §10 (max 1000 ops/txn).
    fn reserve_memory_id<'a>(&'a self)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<MemoryId, WriterError>> + Send + 'a>>;

    /// Apply a pre-built batch of buffered operations atomically.
    /// Returns the per-op acks in order. Used by TXN_COMMIT.
    fn submit_batch<'a>(&'a self, batch: TxnBatch)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TxnBatchAck, WriterError>> + Send + 'a>>;
}
```

`RealWriterHandle::reserve_memory_id` bumps `next_slot` and packs.
`RealWriterHandle::submit_batch` opens one wtxn and runs the apply
logic above.

Test writers (NoopWriter / FakeWriterHandle) get stub impls.

## 3. Files written / changed

```
crates/brain-planner/src/executor/writer.rs    [edit: + reserve_memory_id + submit_batch trait methods + types]
crates/brain-planner/src/lib.rs                [edit: re-export new types]
crates/brain-ops/src/txn.rs                    [REWRITE: real handlers, TxnStore, TxnContext, TxnBuffer]
crates/brain-ops/src/txn_lens.rs               [NEW: TxnLens + lens helpers]
crates/brain-ops/src/context.rs                [edit: + txn_store field]
crates/brain-ops/src/lib.rs                    [edit: + pub mod txn_lens, re-exports]
crates/brain-ops/src/encode.rs                 [edit: txn buffer path]
crates/brain-ops/src/forget.rs                 [edit: txn buffer path]
crates/brain-ops/src/link.rs                   [edit: txn buffer path]
crates/brain-ops/src/recall.rs                 [edit: txn lens for read-your-writes]
crates/brain-ops/src/plan.rs                   [edit: txn lens through executor]
crates/brain-ops/src/reason.rs                 [edit: txn lens through executor]
crates/brain-ops/src/writer.rs                 [edit: implement reserve_memory_id + submit_batch on RealWriterHandle]
crates/brain-planner/src/executor/path.rs      [edit: accept optional TxnLens for edge traversal]
crates/brain-planner/src/executor/reason.rs    [edit: accept optional TxnLens for edge traversal]
crates/brain-planner/src/executor/recall.rs    [edit: accept optional TxnLens for candidate scan]

# Test writers — add reserve_memory_id + submit_batch stubs:
crates/brain-ops/src/lib.rs                    [NopWriter impl]
crates/brain-planner/tests/{dispatch,encode_end_to_end,forget_end_to_end,recall_end_to_end,path_executor,reason_executor}.rs

crates/brain-ops/tests/txn.rs                  [NEW — 15 integration tests]
```

## 4. Test plan (15)

### Lifecycle (3)
1. `txn_begin_returns_handle_with_bounded_timeout` — clamp to [1, 300].
2. `txn_begin_replay_returns_cached_response` — same txn_id twice.
3. `expired_txn_swept_on_next_begin`.

### Buffering & rollback (4)
4. `encode_in_txn_returns_memory_id_but_not_visible_outside` — encode
   carrying txn_id; non-txn RECALL doesn't find it.
5. `commit_makes_buffered_writes_visible` — encode + commit; non-txn
   RECALL now finds.
6. `abort_discards_buffered_writes` — encode + abort; nothing in redb
   or HNSW.
7. `commit_is_atomic_all_or_nothing` — engineered failure mid-commit
   (e.g., LINK to a phantom target after a successful encode in same
   txn); whole commit aborts; redb state unchanged.

### Read-your-writes (5)
8. `recall_in_txn_sees_pending_encode` — encode + recall both with
   txn_id; pending memory in results.
9. `recall_in_txn_drops_pending_tombstone` — pre-commit memory exists
   committed; FORGET it in-txn; in-txn recall doesn't return it; non-
   txn recall still returns it (committed).
10. `plan_in_txn_traverses_pending_link` — A and B committed; LINK
    A→B in-txn; in-txn PLAN finds the path; non-txn PLAN doesn't.
11. `reason_in_txn_picks_up_pending_supports_edge`.
12. `unlink_in_txn_hides_committed_edge` — LINK A→B committed; UNLINK
    in-txn; in-txn PLAN doesn't see it.

### Validation & error paths (3)
13. `op_with_unknown_txn_id_returns_txn_expired`.
14. `op_with_committed_txn_id_returns_txn_expired`.
15. `commit_replay_returns_cached_response` — commit twice → same
    ack with identical timestamps/counts.

## 5. Verify checklist

- `cargo build -p brain-planner -p brain-ops` clean.
- `cargo test -p brain-planner -p brain-ops` — old totals + 15 new.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` no diff.

## 6. Commit message (draft)

```
feat(brain-planner,brain-ops): true transactional semantics (sub-task 7.9)

Ships buffer-until-commit transactions with read-your-writes and
atomic apply/rollback. Operations carrying txn_id buffer in memory;
TXN_COMMIT applies the entire buffer in a single redb write txn
(atomicity holds); TXN_ABORT drops the buffer.

brain-planner:
- WriterHandle gains reserve_memory_id() and submit_batch().
- reserve_memory_id bumps next_slot without writing; used by the
  in-txn ENCODE path so a stable MemoryId is available for
  read-your-writes before commit.
- submit_batch opens one redb write txn, applies pending memories,
  edges, tombstones, and idempotency entries together. All-or-nothing.

brain-ops:
- TxnStore + TxnContext + TxnBuffer in src/txn.rs.
- TxnLens in src/txn_lens.rs — overlay view of pending memories,
  tombstones, and edge changes. RECALL/PLAN/REASON consult it when
  a request carries txn_id.
- handle_txn_begin: clamp timeout to [1, 300]s, allocate, sweep
  expired entries. Replay-safe.
- handle_txn_commit: take buffer → submit_batch → HNSW inserts/
  tombstones → finalise. Cache response for replay.
- handle_txn_abort: drop buffer; report operations_discarded.
- Mutator handlers (encode/forget/link/unlink) carry a buffer
  branch when txn_id is Some: validate the txn is Active, build a
  preview ack (allocating MemoryId for encode), push to the buffer,
  return the preview to the client.
- Read handlers (recall/plan/reason) carry a lens branch when txn_id
  is set: layer pending writes on top of the committed view.

Out of scope: WAL TXN_BEGIN/COMMIT/ABORT markers (no WAL hookup in
brain-ops yet; that's Phase 9). The durability story: a crash
before COMMIT loses the buffer (client never got an ack → no lost
durable write). A crash mid-COMMIT is handled by redb's MVCC —
the wtxn is atomic.

Tests: 15 integration tests across lifecycle, buffering, rollback,
read-your-writes (RECALL/PLAN/REASON cross-product), validation,
and replay.

No new external deps.
```

## 7. Risks

- **Read-lens correctness across all executor paths.** The lens
  touches RECALL, PLAN, REASON. Each must be tested with both
  txn-on and txn-off paths. Plan §4 covers the cross-product;
  more bug surface than any previous 7.x sub-task.
- **Idempotency cross-txn.** A `request_id` reused across two
  different txns hits the in-buffer index in one txn but the redb
  idempotency table after commit. We treat the second use as a
  fresh op (different buffer), but on commit if redb already has
  the row → Conflict bubbles up. Tested in #15.
- **Slot leak on ABORT / expiry.** Bounded by spec §09/08 §10
  (max 1000 ops/txn). With 30 s default timeout + 1000 ops/txn,
  worst-case leak is small. Documented.
- **HNSW post-commit failure.** If `submit_batch` succeeds but
  HNSW.insert fails for a pending memory, the redb row exists but
  isn't searchable. We log + fail the COMMIT. The redb state is
  inconsistent until Phase 8's HNSW maintenance worker rebuilds.
  Same hazard as today's non-txn ENCODE path (which has the same
  post-wtxn HNSW step).
- **No nested-txn check.** Spec wants this; we punt to the SDK.
  Documented.

## 8. Out-of-scope flags

- No WAL records (Phase 9 wires the shard executor's Wal).
- No cross-shard txns.
- No nested-txn detection at the substrate.
- No durable txn registry (in-memory only; lost on restart, but
  there's nothing to recover anyway since uncommitted buffers are
  by definition not on disk).

---

PLAN READY (full buffer-and-apply). Awaiting `go`.
