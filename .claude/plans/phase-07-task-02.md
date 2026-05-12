# Sub-task 7.2 — `RealWriterHandle` + idempotency layer

Two things land together because they're inseparable per spec §08/04 §4 + §07/06 §3: the writer owns both directions of the idempotency table (lookup on read, insert in the same write txn as the memory row). Building one without the other would force a redesign.

**What 7.2 ships**: a real `WriterHandle` implementation backed by `Arc<Mutex<MetadataDb>>` + `parking_lot::Mutex<HnswWriter>`, with metadata-table-backed idempotency replay and conflict detection.

**What 7.2 doesn't ship**: WAL fsync. The spec §08/08 §10 group-commit-channel writer is Phase 8 / 9 territory. 7.2's writer is "real-shaped, no-WAL" — production replaces this with a WAL-backed version later without changing the trait surface.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §07/06 §1 | RequestId is UUIDv7; client-generated; unique per logical op |
| §07/06 §2 | `idempotency` table: key = `RequestId`, value = `IdempotencyEntry { response_kind, memory_id, response_payload, request_hash, created_at }` |
| §07/06 §3 | Lookup-then-act protocol: read txn → check → if hit, decode + return; if miss, do work + insert in same write txn as the data |
| §07/06 §4 | Replay returns the original response verbatim — same MemoryId, same metadata |
| §07/06 §5 | Conflict detection: if RequestId is reused with different params (different `request_hash`), return `IdempotencyConflict` |
| §07/06 §6 | TTL = 24h; background worker prunes. Lookup does NOT check expiry (worker's job) |
| §09/02 §4 | ENCODE specifically: same RequestId → same response; no duplicate memory; no extra WAL record |
| §08/04 §4 | Idempotency check is "Phase 1 of encode" — collapsed into the writer |
| §08/08 §10 | Writer task design — Phase 7 ships in-process synchronous version; group-commit-channel is Phase 8 / 9 |
| Orientation plan §4.1 + §4.2 | Real writer + real idempotency land together in `brain-ops/src/writer.rs` + `brain-ops/src/idempotency.rs` |

## 1. Scope

**In scope for 7.2:**
- `crates/brain-ops/src/writer.rs` — `RealWriterHandle` struct + `impl WriterHandle`. Implements `submit_encode` + `submit_forget` against real `MetadataDb` + `HnswWriter`. No WAL.
- `crates/brain-ops/src/idempotency.rs` — helpers:
  - `hash_encode_request(op: &EncodeOp) -> [u8; 32]` — BLAKE3 over canonical fields (text, kind, salience, context, edges; excludes request_id).
  - `hash_forget_request(op: &ForgetOp) -> [u8; 32]` — BLAKE3 over (memory_id, mode).
  - `encode_response_payload(memory_id, edge_outcomes) -> Vec<u8>` and `decode_response_payload(bytes) -> (MemoryId, Vec<EdgeOutcome>)` — minimal bincode-style hand-rolled binary format (rkyv requires deriving; avoids the dep complication for these small payloads).
  - Same pair for forget responses.
- **`brain-planner` extension**: add `WriterError::Conflict(String)` variant. Required because `submit_encode` / `submit_forget` now return errors for idempotency mismatch. `OpError::error_code()` already maps `WriterError` → mostly Internal; we add a `Conflict → Conflict` mapping arm.
- `RealWriterHandle::new(metadata: SharedMetadataDb, hnsw_writer: HnswWriter<384>)` constructor.
- `RealWriterHandle` is `Send + Sync` (compile-asserted) so it sits behind `Arc<dyn WriterHandle>`.
- Tests:
  - Integration tests in `crates/brain-ops/tests/writer.rs` against a tempdir `MetadataDb` + in-memory `SharedHnsw`:
    - `encode_round_trips_and_writes_metadata` — submit encode, look up memory in metadata + index.
    - `idempotent_replay_returns_cached_ack` — same RequestId twice, second has `replayed: true`, same MemoryId.
    - `idempotency_conflict_on_different_params` — same RequestId, different text → `WriterError::Conflict`.
    - `distinct_request_ids_produce_distinct_memory_ids`.
    - `forget_round_trips_via_real_writer`.
    - `forget_idempotent_replay`.
    - `forget_already_tombstoned` — second submit with different RequestId → `ForgetOutcome::AlreadyTombstoned`.
    - `forget_memory_not_found` — unknown id → `ForgetOutcome::MemoryNotFound`.
  - One concurrency test: 8 threads submit distinct encodes; all get unique MemoryIds, no duplicates.

**NOT in scope:**
- WAL fsync — Phase 8 / 9.
- The background pruning worker — Phase 8.
- Idempotency expiry check at lookup time — spec §07/06 §6 says it's the worker's job.
- LINK / UNLINK writer methods — Phase 7.8 adds them when the wire shape gains the variants.
- TXN_*-aware writer — Phase 7.9.
- Embed-step caching on idempotency hit — the planner's `EmbeddingStep.cache_lookup` already handles the embed-cache concern.

## 2. Module surface

```rust
// crates/brain-ops/src/idempotency.rs

use brain_core::{EdgeKind, MemoryId};
use brain_planner::{EdgeOutcome, EncodeOp, ForgetOp};
use brain_protocol::request::ForgetMode;

/// 32-byte BLAKE3 of the canonical encode-request fields. Excludes
/// `request_id` (that's the key) but includes everything else that
/// would make this a *different* request: text, kind, salience,
/// context, edges, fingerprint. Spec §07/06 §5.
pub fn hash_encode_request(op: &EncodeOp) -> [u8; 32];
pub fn hash_forget_request(op: &ForgetOp) -> [u8; 32];

/// 1-byte response_kind tags. Spec §07/06 §2 + §07/04.
pub(crate) const RESPONSE_KIND_ENCODE: u8 = 1;
pub(crate) const RESPONSE_KIND_FORGET: u8 = 2;
// 3 = LINK (Phase 7.8), 4 = UNLINK, etc.

/// Encode a successful ENCODE outcome into bytes for storage in
/// `IdempotencyEntry::response_payload`. Format is hand-rolled
/// (compact + simple): 16-byte MemoryId + varint edge_count + N×u8
/// outcome discriminants.
pub fn encode_encode_payload(memory_id: MemoryId, edge_outcomes: &[EdgeOutcome]) -> Vec<u8>;
pub fn decode_encode_payload(bytes: &[u8]) -> Result<(MemoryId, Vec<EdgeOutcome>), DecodeError>;

/// Same for FORGET. 16-byte MemoryId + 1-byte outcome discriminant.
pub fn encode_forget_payload(memory_id: MemoryId, outcome: ForgetOutcome) -> Vec<u8>;
pub fn decode_forget_payload(bytes: &[u8]) -> Result<(MemoryId, ForgetOutcome), DecodeError>;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("malformed idempotency payload: {0}")]
    Malformed(&'static str),
}
```

```rust
// crates/brain-ops/src/writer.rs

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use brain_core::MemoryId;
use brain_index::Writer as HnswWriter;
use brain_planner::{
    EdgeOutcome, EncodeAck, EncodeOp, ForgetAck, ForgetOp, ForgetOutcome, SharedMetadataDb,
    WriterError, WriterHandle,
};
use brain_metadata::tables::{idempotency::IDEMPOTENCY_TABLE, memory::MEMORIES_TABLE};
use parking_lot::Mutex;

pub struct RealWriterHandle {
    metadata: SharedMetadataDb,
    hnsw_writer: Mutex<HnswWriter<384>>,
    /// Local slot counter. v1 doesn't yet share this with the arena
    /// allocator (Phase 8 / 9 wiring); for now we mint slots in-
    /// memory and trust the writer is the only path that mints them.
    next_slot: Mutex<u64>,
    /// In-process tracking of already-tombstoned memories so a second
    /// FORGET on the same id returns `AlreadyTombstoned` even when the
    /// idempotency table doesn't catch it (different RequestId).
    tombstoned: Mutex<HashSet<MemoryId>>,
}

impl RealWriterHandle {
    pub fn new(metadata: SharedMetadataDb, hnsw_writer: HnswWriter<384>) -> Self;
}

impl WriterHandle for RealWriterHandle {
    fn submit_encode<'a>(/* ... */) -> Pin<Box<...>>;
    fn submit_forget<'a>(/* ... */) -> Pin<Box<...>>;
}
```

## 3. Implementation decisions

### 3.1 `WriterError::Conflict` — extending brain-planner

`brain-planner::WriterError` currently has `Overloaded` + `Internal`. We add:

```rust
#[error("idempotency conflict: {0}")]
Conflict(String),
```

Then `OpError::error_code()`'s `WriterError` arm gains:
```rust
ExecError::WriterFailed(WriterError::Conflict(_)) => ErrorCode::Conflict,
```

This is a non-additive change to `WriterError` (new variant). Existing matches on `WriterError` in `brain-ops/src/error.rs` need to handle the new variant — fortunately the only place is the `error_code()` match, which we update.

### 3.2 Encode submit flow

```
1. Compute request_hash = hash_encode_request(&op).
2. Acquire metadata mutex → read txn:
     idem.get(op.request_id)?:
       Some(prior):
         if prior.request_hash != request_hash → Err(Conflict).
         else: decode prior.response_payload → (memory_id, edge_outcomes)
                → return Ok(EncodeAck { memory_id, edge_outcomes, replayed: true })
       None: proceed.
   drop the read txn + the mutex.

3. Allocate a slot id (next_slot.fetch_add(1)).

4. Build MemoryId::pack(0, slot, 1).

5. Reacquire mutex → write txn:
     a. memories.insert(memory_id, MemoryMetadata::new_active(...))
     b. compute edge_outcomes (existence check per edge target via the same write txn's table)
     c. idempotency.insert(request_id, IdempotencyEntry::new(
            response_kind=1, memory_id_bytes, payload, request_hash, now))
   commit. drop mutex.

6. hnsw_writer.lock().insert(memory_id, &op.vector)?

7. Return Ok(EncodeAck { memory_id, edge_outcomes, replayed: false }).
```

The metadata write + idempotency write are in **the same redb transaction** — atomic per spec §07/06 §3. The HNSW insert is *after* the txn commits (matches Phase 6's `FakeWriterHandle` ordering and spec §08/04 §9's "apply after durability barrier"). If the HNSW insert fails, the metadata + idempotency rows are already committed — the memory exists but isn't searchable. Phase 8 / 9's real writer will sequence WAL → HNSW more carefully; for 7.2 this is acceptable.

### 3.3 Forget submit flow

```
1. Compute request_hash = hash_forget_request(&op).
2. Read txn → idempotency lookup (same shape as encode).
3. Check tombstoned HashSet:
     contains(op.memory_id) → return Ack{ outcome: AlreadyTombstoned }.
4. Read txn → memories.get(op.memory_id):
     None → return Ack{ outcome: MemoryNotFound }.
     Some → proceed.
5. hnsw_writer.lock().mark_tombstoned(op.memory_id)?
6. Write txn: idempotency.insert(...)
7. tombstoned.lock().insert(op.memory_id).
8. Return Ack{ outcome: Tombstoned, replayed: false }.
```

The HNSW tombstone happens **before** the idempotency write so a crash between steps would surface as "tombstone exists but no idempotency record" → next FORGET with the same RequestId would re-tombstone (idempotent) without WAL-recovery needed. Spec-correct.

### 3.4 Payload encoding format

Hand-rolled, version-prefixed:

```
encode_payload:
  [u8; 16]   memory_id (big-endian)
  u8         payload_version (= 1 for v1)
  u16-le     edge_count
  u8 × N     edge outcomes (0 = Inserted, 1 = TargetMissing)
forget_payload:
  [u8; 16]   memory_id
  u8         payload_version (= 1)
  u8         outcome (0 = Tombstoned, 1 = AlreadyTombstoned, 2 = MemoryNotFound)
```

19+N bytes for encode (16+1+2+N), 18 bytes for forget. Compact. Version byte means future schema changes don't break replay (decoder checks the version).

We avoid rkyv here because `EncodeAck` / `ForgetAck` don't currently derive rkyv and pulling them in is out of scope; hand-rolled is 30 lines per kind.

### 3.5 Request hashing

```
hash_encode_request(op):
  BLAKE3(
    b"encode:"
    | op.text.as_bytes()
    | "\0" | (op.context_id as u64 LE bytes)
    | "\0" | [op.kind as u8]
    | "\0" | op.salience_initial.to_le_bytes()
    | "\0" | op.fingerprint
    | "\0" | edge_count LE
    | for each edge: target.to_be_bytes() | [kind as u8] | weight.to_le_bytes()
  )

hash_forget_request(op):
  BLAKE3(
    b"forget:"
    | op.memory_id.to_be_bytes()
    | "\0" | [op.mode as u8]
  )
```

NUL separators between fields prevent canonicalisation ambiguity (e.g. "text=foo, kind=1" vs "text=foo,kind=1"). `blake3` is already a workspace dep via brain-embed.

### 3.6 Concurrency

`RealWriterHandle` is `Send + Sync` because all interior state is `Mutex`/`Arc<Mutex>`-wrapped. The locks serialise per-submit. Spec §08/08 §10 wants a single-writer-per-shard discipline — runtime-enforced by the metadata mutex + slot mutex. Multiple concurrent submits queue at the lock; throughput is bounded by the slowest write but no race conditions are possible.

### 3.7 The `next_slot` counter — temporary

Production needs to allocate slots from the arena (Phase 8 / 9). 7.2's writer mints them in-memory. We document this as a known limitation; the storage hand-off happens in Phase 8 when the real allocator lands.

For now: `next_slot.fetch_add(1)` starting at 1. On restart, this would collide with previously-written slots — but 7.2 runs from a fresh tempdir per test, so no issue.

### 3.8 Idempotency-conflict error message

The conflict message includes the request kind + a short identifier: `"encode request_id={hex} hash mismatch"`. Operators see this in logs and can match to client retry-with-different-params bugs.

### 3.9 Tests use the **real writer**, not the fake

Phase 7.2 tests construct `RealWriterHandle` directly. Phase 6's `FakeWriterHandle` stays in the brain-planner tests (no migration). Both implementations coexist; production will use the real one, brain-planner's tests stay using their fake for isolation.

## 4. Files written / changed

```
crates/brain-planner/src/executor/writer.rs    [edit: + WriterError::Conflict variant]
crates/brain-ops/Cargo.toml                    [edit: + blake3 workspace dep]
crates/brain-ops/src/lib.rs                    [edit: + pub mod idempotency + writer; re-exports]
crates/brain-ops/src/error.rs                  [edit: + WriterError::Conflict mapping arm]
crates/brain-ops/src/idempotency.rs            [new]
crates/brain-ops/src/writer.rs                 [new]
crates/brain-ops/tests/writer.rs               [new — 9 integration tests]
```

No new external deps. `blake3` is already a workspace dep.

## 5. Verify checklist

- `cargo build -p brain-ops` clean (dev container).
- `cargo build -p brain-planner` clean (the WriterError change).
- `cargo test -p brain-ops` — 6 existing + 9 new integration tests.
- `cargo test -p brain-planner` — 101 existing should still pass (the new variant is additive in their `match`es; the test fakes don't construct `WriterError::Conflict` so their exhaustive matches don't need updating).
- `cargo clippy -p brain-ops --all-targets -- -D warnings` clean.
- `cargo clippy -p brain-planner --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-ops` no diff.

## 6. Commit message (draft)

```
feat(brain-ops): RealWriterHandle + idempotency layer (sub-task 7.2)

Ships the real per-shard write surface backed by Arc<Mutex<MetadataDb>>
+ Mutex<HnswWriter>. Idempotency lives at the writer level (spec
§08/04 §4 + §07/06 §3) because the lookup-then-act protocol writes
the response payload in the same redb txn as the memory row.

No WAL — Phase 8 / 9 swaps to the channel-fed group-commit writer
without changing the WriterHandle trait surface.

- writer.rs: RealWriterHandle. submit_encode follows the spec §07/06
  §3 lookup-then-act flow: read txn idempotency check → if hit and
  hash matches, decode + return replayed=true; if hit and hash
  mismatch, return Conflict. On miss, mint MemoryId, write metadata
  + idempotency in one write txn, then HNSW insert. submit_forget
  parallels with a per-MemoryId tombstoned HashSet to surface
  AlreadyTombstoned.
- idempotency.rs: BLAKE3 hashing over canonical encode/forget
  fields (NUL-separated, excludes request_id). Hand-rolled compact
  payload format with a version byte (19+N bytes for encode, 18 for
  forget) so future schema changes are recoverable.
- brain-planner: WriterError gains Conflict(String) variant. The
  brain-ops error_code() mapping handles it. Existing brain-planner
  tests still pass (no exhaustive match on WriterError outside the
  test fakes).

Tests (9 integration in brain-ops): encode round-trip, idempotent
replay, idempotency conflict on different params, distinct
RequestIds, forget round-trip + replay + AlreadyTombstoned +
MemoryNotFound, 8-thread concurrent encode produces 8 distinct
MemoryIds.

New brain-ops dep: blake3 (workspace).

Built/tested in the Linux dev container.
```

## 7. Risks

- **`WriterError::Conflict` is a small breaking change to a Phase 6 trait error type**. The existing FakeWriterHandle implementations in brain-planner's tests construct only `WriterError::Internal` / `Overloaded`; their matches are non-exhaustive (most use `WriterError::Internal(_)`). Should not regress.
- **Slot allocation collides with future arena allocator**. The in-memory `next_slot` counter doesn't survive restarts. Documented as a Phase 8 follow-up.
- **HNSW insert after metadata commit can fail mid-flight**. The memory becomes findable in metadata but not in the index. Recovery (when WAL is wired in Phase 8 / 9) replays from the WAL. For 7.2 there's no WAL — the half-state is observable. Mitigation: an integration test asserts the HNSW insert succeeds before claiming the test passes. Real implementation in Phase 8/9 handles this via the recovery sink (already shipped in Phase 3.11).
- **Payload format versioning**. The version byte lets us evolve the format without breaking replay. If we ever change the encode_encode_payload layout, bump the version and add a decoder branch.

## 8. Out-of-scope flags

- No WAL.
- No expiry check at lookup time.
- No LINK / UNLINK / UNFORGET methods on the trait.
- No TXN buffering.
- No background pruning worker.
- No cross-shard writer.

---

PLAN READY.
