---
name: brain-wal-audit
description: Audit WAL discipline (WAL-before-ack, O_DIRECT, pwritev2 RWF_DSYNC group commit, recovery idempotency). Fires on diffs in crates/brain-storage/wal/. Spec §05/03 + §05/08.
when-to-use: |
  Triggers:
    - Diff in crates/brain-storage/wal/**/*.rs or near WAL append / fsync paths
    - User says "review WAL" / "fsync correctness" / "recovery"
    - Adding a new WAL record format
    - Touching the segment writer / reader, group-commit batching, or checkpoint logic
trigger-files:
  - crates/brain-storage/**/*.rs
spec-refs:
  - spec/05_storage_arena_wal/04_wal_overview.md
  - spec/05_storage_arena_wal/05_wal_records.md
  - spec/05_storage_arena_wal/06_wal_durability.md
  - spec/05_storage_arena_wal/08_recovery.md
  - spec/16_benchmarks_acceptance/06_durability_criteria.md
---

# WAL Audit

## When to use

Any change to write-ahead-log code: append, fsync, group-commit, checkpoint, recovery, segment rotation. The WAL is the durability backbone — bugs here are silent until the worst possible moment (a crash).

## What this enforces

### From CLAUDE.md §5 (invariants)

- **#1 WAL-before-acknowledge.** No operation returns success until its WAL record is fsynced.
- **#3 CRC everywhere.** Every WAL record has a CRC32C; reads verify; mismatches halt.
- **#7 No silent corruption.** CRC mismatch on replay → halt + alert. Never patch over.

### From spec §05/03 (the WAL format)

- Records have a fixed header + payload + CRC32C trailer.
- Records are appended to **segments** (rotating files); each segment carries an LSN range.
- Writes use **O_DIRECT** to bypass the page cache (alignment-sensitive — buffers must be page-aligned).
- Multiple records are batched into a single **`pwritev2(RWF_DSYNC)`** call ("group commit") so N concurrent writers fsync once.

### From spec §05/08 (recovery)

- Recovery replays from the last checkpoint LSN forward. Records past the in-progress write boundary may be torn; CRC catches and stops at the last good record.
- Replay is **idempotent**: replaying a committed record twice produces the same outcome (the record's effect is keyed by its LSN + RequestId).

## Workflow

1. **Locate WAL touchpoints in the diff.** `grep -nE 'wal::|fsync|RWF_DSYNC|pwritev|append.*record|replay' <files>`.

2. **WAL-before-ack.** For every code path that returns `Ok(...)` for a write op (encode, forget, txn-commit), trace upstream until you find the fsync. The fsync MUST be `RWF_DSYNC` group-commit, NOT a per-record fsync. The ack happens *after* the fsync returns.

3. **CRC.** New record format? Field reorder? CRC32C is computed over the record bytes (header + payload, excluding the CRC field itself). Compute on append; verify on replay. Mismatch → `brain_storage::Error::Corruption(...)` and the shard halts.

4. **O_DIRECT alignment.** Buffers passed to `pwritev2` must be page-aligned (typically 4 KiB). New buffer? It either uses the per-shard scratch arena (already aligned) or is allocated via `posix_memalign` / equivalent. Misaligned writes silently fail or corrupt.

5. **Group commit batching.** If you see a fsync per record, that's wrong — collect pending records, issue one `pwritev2(RWF_DSYNC)` per shard tick.

6. **Recovery idempotency.** Replaying a committed encode must NOT create a duplicate memory. Verify the recovery path consults the idempotency table (RequestId-keyed) before applying. If the record's RequestId is already in the dedupe table, skip. Tested by chaos test in spec §16/06.

7. **Segment rotation.** Segments rotate at a configured size. Verify the new segment is opened *before* the rotate decision, with a fresh CRC chain — not append-then-rotate.

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `fn handle_encode(...) -> Result<...> { wal.append(...)?; Ok(...) }` (no fsync) | Ack before durability | `wal.append(...)?; wal.flush_with_dsync()?; Ok(...)` |
| Per-record fsync in a hot path | Throughput collapse | Group commit batching |
| `Vec::<u8>::with_capacity(N)` for O_DIRECT write | Unaligned | Aligned scratch buffer or `posix_memalign` |
| `if record.crc != computed { record.crc = computed; }` on replay | Silent corruption #7 violation | Halt + emit `tracing::error!` |
| Replay path doesn't check idempotency table | Duplicate effects | Consult dedupe table; skip on hit |
| Segment rotation closes-then-opens, leaving a window | Lost records | Open new segment before closing old |

## Test coverage required

- **Round-trip:** append a record → flush → read back → verify equality.
- **CRC corruption:** flip a byte in a stored record → replay halts at that record, returns `Corruption`.
- **Torn write:** truncate a segment mid-record → replay stops at the last good record, no panic.
- **Group commit batching:** N concurrent writers → 1 fsync (or close to it).
- **Crash injection:** kill the process between `pwritev2` and the response → on restart, replay either includes the record (fsynced before crash) or excludes it (not fsynced); never a half-state.
- **Idempotency:** replay a record twice → identical post-state.

## Cross-references

- `brain-invariants` — invariants #1, #3, #7.
- `brain-arena-audit` — companion for the arena side.
- `brain-chaos-test` — kill-during-operation tests.
- spec §05/03, §05/08, §16/06.

## Source / Adaptations

Project-local. Operationalizes spec §05.
