---
name: brain-chaos-test
description: Design kill-during-operation tests for recovery code. Verify WAL replay idempotency, partial-write resilience, snapshot/restore correctness. Spec §16/06 + §05/08.
when-to-use: |
  Triggers:
    - Diff in WAL recovery, snapshot, restore, or any recovery code
    - User says "chaos test" / "kill during" / "crash safety"
    - Phase exit checklist for brain-storage
    - Investigating a durability incident
spec-refs:
  - spec/05_storage_arena_wal/08_recovery.md
  - spec/16_benchmarks_acceptance/06_durability_criteria.md
  - spec/05_storage_arena_wal/11_failure_modes.md
---

# Chaos Test Design

## When to use

Recovery code (WAL replay, segment rotation, checkpoint, snapshot, restore) needs kill-during-operation tests. These are different from unit tests — they exercise the *invariants* under failure, not the happy path.

## What this enforces

Per spec §16/06 (durability criteria):

- **Atomic ack:** an op that returned `Ok` MUST be visible after recovery; one that didn't return MUST be invisible OR cleanly rolled back.
- **No half-states:** no torn records, no partial writes, no slot with old header + new vector.
- **Idempotent replay:** replaying a fully-fsynced record twice produces the same post-state.
- **Bounded recovery time:** replay from the last checkpoint completes within the spec's recovery RTO.

## Test scaffolding

A chaos test is structured as:

1. Start a fresh shard.
2. Issue N operations.
3. **Kill the shard** at a chosen point: between WAL append and fsync; mid-`pwritev2`; after fsync but before ack; mid-checkpoint; mid-segment-rotation.
4. Restart the shard. Recovery runs.
5. Assert the post-state:
   - Every op that received `Ok` is visible.
   - Every op that did NOT receive `Ok` is invisible (or no longer pending).
   - CRC verifications pass.
   - The shard is healthy (no `Corruption` errors).

Use a `kill_at` injection point — a deterministic counter or named hook the test sets before issuing the op.

```rust
#[test]
fn kill_between_wal_append_and_fsync_loses_op_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let mut shard = Shard::open(dir.path()).unwrap();

    shard.set_kill_hook(KillPoint::AfterWalAppendBeforeFsync);
    let result = shard.encode("hello");                // panics; recovery needed
    assert!(result.is_err());

    // Restart.
    let shard = Shard::open(dir.path()).unwrap();
    assert_eq!(shard.find_text("hello"), None);        // op was lost cleanly
    assert!(shard.is_healthy());
}
```

## Hard rules

- **Tests MUST hit a real fs** (use `tempfile::TempDir`); no in-memory fakes that skip fsync semantics.
- **Tests MUST cover at least:** kill-pre-wal-append, kill-pre-fsync, kill-post-fsync-pre-ack, kill-mid-checkpoint, kill-mid-segment-rotation.
- **Replay idempotency:** issue an op, kill *after* fsync, restart; the op must be visible. Issue the same op (same RequestId) again; no duplicate.
- **No "best-effort" assertions.** If recovery is supposed to be deterministic, assert it is — not "approximately works".

## Common gaps to fill

| Scenario | Test name idea |
|---|---|
| Kill between WAL append and fsync | `op_not_acked_is_invisible_after_restart` |
| Kill after fsync, before ack | `op_fsynced_is_visible_after_restart` |
| Kill mid-`pwritev2` (torn write) | `torn_wal_write_recovers_to_last_good_record` |
| Replay sees same RequestId twice | `replay_is_idempotent_on_request_id` |
| Kill mid-checkpoint | `partial_checkpoint_replays_from_prior_checkpoint` |
| Kill mid-segment-rotation | `segment_rotation_is_atomic` |
| Kill mid-snapshot | `partial_snapshot_does_not_corrupt_origin` |

## Anti-patterns

- **Mocking out fsync.** Defeats the point. Fsync semantics are what we're testing.
- **Single kill point.** Multiple paths reach the kill site; cover them all.
- **Asserting only "no panic".** Recovery must be *correct*, not just non-crashing.
- **Sharing state across tests.** Each chaos test gets a fresh `tempdir`.

## Cross-references

- `brain-wal-audit` — WAL discipline contracts.
- `brain-arena-audit` — slot CRC + version invariants.
- `brain-invariants` — the seven invariants (especially #1, #3, #5, #7).
- spec §05/08, §05/11, §16/06.

## Source / Adaptations

Project-local. Operationalizes spec §16/06.
