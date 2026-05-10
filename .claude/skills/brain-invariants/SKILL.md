---
name: brain-invariants
description: Audit a diff against CLAUDE.md §5's seven invariants (WAL-before-ack, single-writer, CRC, slot-version, idempotency, tombstone, fail-stop). Use when storage/ops/workers/server change.
when-to-use: |
  Triggers:
    - Diff touches crates/{brain-storage,brain-ops,brain-workers,brain-server}
    - User says "review invariants" / "is this safe?" / "could this corrupt data?"
    - Adding a new write path (encode, forget, txn-commit)
    - Touching any code that emits an ack to the client
    - Touching slot reclamation, GC, or tombstone reaping
trigger-files:
  - crates/brain-storage/**/*.rs
  - crates/brain-ops/**/*.rs
  - crates/brain-workers/**/*.rs
  - crates/brain-server/**/*.rs
spec-refs:
  - spec/16_benchmarks_acceptance/06_durability_criteria.md
  - spec/05_storage_arena_wal/08_recovery.md
---

# Brain Invariants — Audit

## When to use

Any diff touching storage, ops, workers, or server. The seven invariants are *non-negotiable*: code that violates them is wrong, regardless of test results (CLAUDE.md §5).

## What this enforces

The seven invariants:

| # | Invariant | What to check |
|---|-----------|---------------|
| 1 | **WAL-before-acknowledge** | No operation returns success until its WAL record is fsynced. Trace every code path from the client-facing op to the ack — find the fsync. |
| 2 | **Single writer per shard** | No locks needed; the discipline enforces it. Verify the shard's mutator is a single tokio/glommio task or owned by one struct without `Arc<Mutex>` on the write path. |
| 3 | **CRC everywhere** | Every WAL record AND every arena slot has a CRC32C. Reads verify; mismatches halt and alert. Grep for `crc32c::` near write/read boundaries. |
| 4 | **Slot version on `MemoryId`** | Slot version is encoded in the ID; stale references → `NotFound`, not wrong data. Check `brain_core::ids::MemoryId::pack` callers. |
| 5 | **Idempotency by `RequestId`** | 24h TTL. Same params → cached response. Different params with same id → `Conflict`. Verify the dedupe table is consulted on every write op. |
| 6 | **Tombstone grace before reclamation** | Default 7 days. Hard FORGET zeroes immediately. Soft FORGET leaves data recoverable until the GC worker's grace expires. |
| 7 | **No silent corruption** | Fail-stop and alert. Never return wrong data. CRC mismatches → halt the shard; never repair-and-continue. |

## Workflow

For each invariant, run a focused check on the diff:

1. **WAL-before-ack** —
   `grep -n 'Result.*ok\(\)\|return Ok' <changed-files>` then trace upstream. Every return-Ok path must follow a fsync. If the path doesn't touch storage, skip; if it does, the fsync must happen in this code or a clearly named helper.

2. **Single writer per shard** —
   Look for `Arc<Mutex<…>>` or `Arc<RwLock<…>>` on per-shard data. Reject; the discipline replaces locks. Cross-shard reads use `ArcSwap` + `crossbeam-epoch`.

3. **CRC everywhere** —
   New on-disk byte format? It needs a CRC. Grep for the new struct's `read` / `write` paths; verify CRC computed on write and verified on read. Mismatch handler must `halt` (not `repair`).

4. **Slot version on `MemoryId`** —
   Grep for `MemoryId::pack(`. Verify the `version` arg is the slot's current version, not zero or a constant.

5. **Idempotency** —
   Any new `…_REQ` payload with a `request_id` field? Verify the handler consults the idempotency table before doing work and writes the response into the table.

6. **Tombstone grace** —
   Forget paths: soft mode must leave the slot reclaimable but readable; hard mode must zero immediately. Reaper code must respect the grace window from config.

7. **No silent corruption** —
   On any CRC verification failure: the shard halts, emits an alert (tracing event with `error` level), and the WAL recovery picks up on next start. No "fall back to text", no "use the other replica" (we don't have one).

## Reporting

When this skill fires, emit a per-invariant verdict:

```
✓ #1 WAL-before-ack — fsync at brain-storage/src/wal/segment.rs:142
✓ #2 single-writer  — no shared-mutex writes introduced
✓ #3 CRC            — new SlotHeader.crc32c, read+write covered
✗ #4 slot version   — MemoryId::pack(shard, slot, 0) at brain-ops/src/forget.rs:67 — must use current slot version
✓ #5 idempotency    — request_id consulted at line 23
✓ #6 tombstone      — soft path leaves data; hard path zeroes
✓ #7 no silent corr — halts with tracing::error! on CRC mismatch
```

If any `✗`: STOP and surface (AUTONOMY §3) before merging. Don't autofix invariant violations — they're design-level concerns.

## Examples

### Golden — adding a new write op

User: "Adding `RECLASSIFY` op."

Walk:

- WAL-before-ack: new WAL record `KindChange { memory_id, new_kind, txn_lsn }`; fsync before ack. ✓
- Single-writer: handler owned by the per-shard executor; no Mutex. ✓
- CRC: WAL record's CRC computed at append. Slot's existing CRC unchanged (kind isn't part of slot bytes). ✓
- Slot version: not relevant — we don't reclaim on reclassify.
- Idempotency: request_id present in `AdminReclassifyRequest`; consulted via the dedupe table.
- Tombstone: not relevant.
- No silent corruption: kind validation on the way in; corrupted record at recovery time → halt.

Verdict: ship it.

### Counter — silent CRC repair

```rust
let computed = crc32c::crc32c(&slot.bytes);
if computed != slot.header.crc32c {
    tracing::warn!("slot CRC mismatch; rewriting");
    slot.header.crc32c = computed;        // ← invariant #7 violation
    return Ok(slot);
}
```

Reject. Mismatch halts the shard; we never overwrite a stored CRC with a recomputed one. The mismatch could be silent corruption from disk; the fix is fail-stop, not paper over.

## Source / Adaptations

Project-local. Encodes CLAUDE.md §5.
