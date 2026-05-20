# P3d — HNSW + arena writes for `submit(Write)`

## Context

The apply layer is pure-redb by design: it never touches anything
outside the wtxn. That keeps replay deterministic and the rules simple.
But `Phase::UpsertMemory` and `Phase::UpdateEmbedding` have *non*-redb
side effects too:

- The 384-float vector lives in the per-shard **arena** (an mmap-backed
  slot store managed by `brain-storage::arena::ArenaFile`).
- The vector also gets indexed in **HNSW** (a per-shard in-memory
  k-NN structure managed by `brain-index::hnsw::HnswWriter`).

Without these two writes, the redb row points at a slot with garbage
bytes and RECALL's vector search can't find the memory. Today the
legacy `do_encode` handles the full chain; the unified `submit(Write)`
does not. That makes the unified path unfit to migrate the encode
handler — which means P4 is blocked until P3d lands.

## What needs to happen

For each phase that has non-redb storage effects, do the storage
work in `submit()` after `apply::dispatch` but at the right place in
the durability chain. The phases with side effects are:

| Phase | Side effect |
|---|---|
| `UpsertMemory` | Write vector bytes to arena slot. Insert into HNSW. |
| `UpdateEmbedding` | Overwrite arena slot bytes. Insert into HNSW (replaces old entry). |
| `Tombstone(Memory)` | `HnswWriter::mark_tombstoned(memory_id)`. Arena bytes stay until `Phase::ReclaimSlots` zeroes them later. |

Other phases (Link, UpsertEntity, UpsertStatement, ...) have no
non-redb storage effect — they stay redb-only.

## The ordering question

The legacy `do_encode` orders the durability chain:

```
1. WAL append → fsync     (durability barrier)
2. Arena write             (vector bytes land in mmap; msync deferred)
3. redb commit             (metadata row points at the slot)
4. HNSW insert             (vector becomes searchable)
5. Event publish           (LSN-stamped from WAL append)
```

This ordering means: if the process crashes anywhere between (1) and
(5), recovery (a) replays the WAL onto a fresh metadata DB, (b) the
arena slot is already correct (it was written before the redb commit
that recovery is about to re-do), (c) HNSW gets rebuilt from
MEMORIES_TABLE by `HnswMaintenanceWorker` at steady state.

Three reasonable shapes for inserting arena+HNSW into the unified path:

### Option A — Before commit (matches legacy)

```
submit(Write) {
  idempotency check
  WAL append          ← P3b
  open wtxn
  apply each phase    ← redb mutations
  side effects per phase ← arena + HNSW writes, before commit
  wtxn.commit()
  publish events
  cache stamp
}
```

**Pros:** Matches the legacy ordering exactly. A torn write
(crash between arena write and redb commit) is the same scenario
the existing recovery handles — replay reproduces the same arena
bytes and the same metadata row.

**Cons:** If the side effect fails mid-write (e.g. HNSW OOM), the
wtxn must roll back. The arena bytes were already written; they
become orphaned until `Phase::ReclaimSlots` runs. Same scenario
the legacy path has — not a regression.

**Cons':** The wtxn is held for the duration of arena + HNSW work.
HNSW insert on a hot index is fast (<1ms) but bursts could matter.

### Option B — After commit

```
submit(Write) {
  idempotency check
  WAL append
  open wtxn
  apply each phase
  wtxn.commit()
  side effects per phase   ← arena + HNSW writes, after commit
  publish events
  cache stamp
}
```

**Pros:** wtxn closes early — less lock contention. redb is
authoritative; if side effects fail the metadata row exists but
HNSW/arena lag. `HnswMaintenanceWorker` catches up by scanning
MEMORIES_TABLE and inserting missing entries.

**Cons:** Diverges from legacy ordering. The window between commit
and HNSW insert is observable: a RECALL between those instants sees
the memory in MEMORIES_TABLE but cannot find it via HNSW. Acceptable
because that window is <1ms and HNSW maintenance worker handles
crash recovery, but it's a subtle behaviour change.

### Option C — Side effects in apply (rule break)

Pass the writer's HNSW + arena handles into apply::dispatch so
`apply_upsert_memory` does the full chain. **Rejected:** breaks the
"apply is pure-redb" invariant, which is load-bearing for WAL replay
determinism — recovery must call apply functions with no side effects
beyond redb.

## Recommendation: Option A (before commit)

Reasoning:
1. **Matches legacy ordering.** Less mental drift, same crash story.
2. **Atomicity within the wtxn lifetime.** If arena or HNSW fail,
   wtxn drops → redb auto-rolls-back. No orphaned redb row.
3. **HNSW maintenance worker still rebuilds** on a torn commit
   (arena write succeeded, then crash before commit) — same as legacy.
4. **Lock-hold time is bounded.** The wtxn-open window grows by the
   arena+HNSW work which is <1ms in steady state. Worst case (HNSW
   level promotion) is bounded by `IndexParams.ef_construction`.

The cost is one slightly longer wtxn-open window per encode-shaped
write. Acceptable.

## What this slice ships

### 1. Writer holds the arena + HNSW handles

`RealWriterHandle` already holds `hnsw_writer: Mutex<HnswWriter<384>>`.
Today's `do_encode` locks it briefly per insert. The unified path
needs the same access.

For the arena: `RealWriterHandle` does NOT hold the arena handle today
— `do_encode` calls into the executor's arena via a different path.
Either:
  (a) Add `arena: Arc<Mutex<ArenaFile>>` to `RealWriterHandle`, or
  (b) Plumb the arena through the apply context via a side-effect
      callback.

(a) is simpler and matches the HNSW handle pattern. The arena is
already per-shard; the writer is per-shard; the ownership fits.

### 2. New side-effects pass in submit()

In `crates/brain-ops/src/ops/writer/submit.rs`:

```rust
pub async fn submit(&self, write: Write) -> Result<WriteAck, WriterError> {
    if let Some(cached) = cache.lookup(write.write_id) { return Ok(cached); }

    // 1. WAL append (P3b — assumed landed before P3d).
    let lsn_range = self.wal_append_for(&write).await?;

    // 2. Open wtxn.
    let mut db = self.metadata().lock();
    let wtxn = db.write_txn()?;

    // 3. Apply each phase (redb-only).
    let phase_acks = apply_all_phases(&wtxn, &write)?;

    // 4. NEW P3d: side effects (arena + HNSW), before commit.
    self.execute_side_effects(&write)?;  // may return Err — wtxn drops → rollback

    // 5. Commit.
    wtxn.commit()?;
    drop(db);

    // 6. Publish events with LSN-stamped envelopes.
    publish_events_with_lsns(self, &write, lsn_range, &phase_acks);

    // 7. Cache stamp.
    let ack = WriteAck { ... };
    cache.stamp(write.write_id, ack.clone());
    Ok(ack)
}
```

`execute_side_effects` is the new function:

```rust
fn execute_side_effects(&self, write: &Write) -> Result<(), WriterError> {
    for phase in &write.phases {
        match phase {
            Phase::UpsertMemory { id, vector, arena_slot, .. } => {
                self.write_arena_slot(*arena_slot, vector)?;
                self.hnsw_insert(*id, vector)?;
            }
            Phase::UpdateEmbedding { id, new_vector } => {
                let slot = self.lookup_slot_for(*id)?;
                self.write_arena_slot(slot, new_vector)?;
                self.hnsw_insert(*id, new_vector)?;  // replaces by id
            }
            Phase::Tombstone { target: TombstoneTarget::Memory { id, .. }, .. } => {
                self.hnsw_tombstone(*id)?;
                // Arena bytes stay until Phase::ReclaimSlots runs.
            }
            _ => {}
        }
    }
    Ok(())
}
```

The lookups inside `execute_side_effects` (for `UpdateEmbedding`'s
slot) happen against the *uncommitted* wtxn since the prior apply
already wrote any prerequisite rows. Alternative: carry `arena_slot`
on `Phase::UpdateEmbedding` so no lookup is needed — the handler
already knows it. Pre-allocating ids and slots is the existing rule;
extend it to `arena_slot` for embedding updates.

### 3. Pre-allocation of arena slot

`Phase::UpsertMemory` already carries `arena_slot: u64`. Handlers
need to ask the writer for one via `reserve_id(IdKind::MemorySlot)`
before submit. Implement that path:

```rust
impl RealWriterHandle {
    pub async fn reserve_id(&self, kind: IdKind) -> Result<AllocatedId, WriterError> {
        match kind {
            IdKind::MemorySlot => {
                let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
                Ok(AllocatedId::MemorySlot(slot))
            }
            IdKind::Memory => {
                let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
                let id = MemoryId::pack(self.shard_id, slot, 0);
                Ok(AllocatedId::Memory(id))
            }
            IdKind::Entity   => Ok(AllocatedId::Entity(EntityId::new())),
            IdKind::Statement => Ok(AllocatedId::Statement(StatementId::new())),
            IdKind::Relation  => Ok(AllocatedId::Relation(RelationId::new())),
        }
    }
}
```

The `next_slot` counter already exists on `RealWriterHandle` for the
legacy `reserve_memory_id` path. `reserve_id` is the universal
equivalent. P4 will migrate `handle_encode` to use it.

### 4. Idempotency of side effects under WAL replay

WAL recovery calls `apply::dispatch` against a fresh wtxn. For
`Phase::UpsertMemory`, the redb row gets re-written — but recovery
does NOT re-do the arena + HNSW side effects (apply is redb-only).

This is fine because:
- **Arena**: the bytes were written before the original commit. On
  restart, mmap loads them; they're already there.
- **HNSW**: rebuilt by `HnswMaintenanceWorker` scanning MEMORIES_TABLE
  on startup or periodically. Recovery doesn't insert into HNSW; the
  worker does.

Document this explicitly: **recovery replays redb-only. Arena bytes
are inherited from disk. HNSW is rebuilt by maintenance.**

This matches the legacy path's recovery story exactly.

### 5. Arena msync timing

Arena writes go into the mmap; the OS flushes them lazily. The legacy
path doesn't msync per encode (would be expensive); instead, the
checkpoint worker periodically msyncs the whole arena. P3d follows
the same pattern — no per-write msync.

### 6. Tests

1. **UpsertMemory writes arena bytes.** Submit `Write { phases: [UpsertMemory] }`.
   Read the arena slot directly. Bytes match the phase's vector.

2. **UpsertMemory inserts into HNSW.** Same submit. Then
   `hnsw.search_active` for the vector — returns the memory_id.

3. **UpdateEmbedding overwrites arena + HNSW.** Submit UpsertMemory,
   then submit UpdateEmbedding with a different vector. Arena bytes
   reflect the new vector. HNSW search for the new vector returns
   the same memory_id.

4. **Tombstone(Memory) marks HNSW tombstoned.** Submit UpsertMemory,
   then Tombstone(Memory). `hnsw.is_tombstoned(id)` returns true.
   `hnsw.search_active` excludes it.

5. **Atomicity on side-effect failure.** Inject an HNSW error
   (saturating index size, etc.). Submit fails with `WriterError`;
   no metadata row, no arena entry, no HNSW entry.

6. **Recovery after crash.** Submit several encodes. `kill -9`
   simulation (drop the metadata DB, keep the WAL + arena). Restart.
   Replay WAL → metadata rebuilt. Arena bytes already in place.
   HnswMaintenanceWorker rebuilds HNSW from metadata. RECALL works.

## Files touched

```
crates/brain-ops/src/ops/writer/
├── mod.rs                  # Add arena field if needed (or plumb
│                           # the existing executor.arena handle).
│                           # Implement reserve_id(IdKind).
├── submit.rs               # Add execute_side_effects between apply
│                           # and commit. Update tests.
└── side_effects.rs         # NEW. Side-effect functions:
                            #   write_arena_slot
                            #   hnsw_insert
                            #   hnsw_tombstone
                            #   lookup_slot_for_memory
                            # (~150 LoC)
```

Nothing in `crates/brain-ops/src/apply/` changes — the purity rule
stays intact.

## Edge cases

1. **UpsertMemory with arena_slot beyond capacity.** ArenaFile auto-
   grows in chunks. Check the existing `grow_to` semantics — does it
   require a separate lock? If grow conflicts with the wtxn the
   write order matters.

2. **HNSW insert race with HnswMaintenanceWorker rebuild.** Both
   take `&mut HnswWriter`. The mutex serialises them. No race.

3. **Tombstone(Memory) for a memory that's not in HNSW yet.** Can
   happen if WAL recovery is mid-replay and `HnswMaintenanceWorker`
   hasn't run yet. `mark_tombstoned` returns an `HnswError::NotFound`
   in that case — surface as `WriterError::Internal` or treat as
   no-op? Treat as no-op: a tombstone is a "don't surface" marker;
   if HNSW doesn't have the entry, the result (not surfacing) is
   already correct.

4. **Multi-phase write where phase N succeeds (arena written) but
   phase N+1 fails (HNSW OOM).** Wtxn rolls back. Arena bytes for
   phase N are orphaned. Recovery: when WAL replays the write, the
   apply functions re-write the metadata row, but the arena bytes
   from the original write are still there. The new arena_slot in
   the replayed phase might be a different slot (if id allocation
   isn't deterministic). **Open question — does the handler stamp
   the same arena_slot on replay, or does recovery re-allocate?**

   Answer: the arena_slot field travels inside the Phase. The WAL
   record has it. Recovery uses the recorded value, not re-allocated.
   So the same slot gets the same bytes. The orphan never materialises.

   This is one of the load-bearing reasons for pre-allocated ids /
   slots in the Phase value type.

## What this slice does NOT do

- **Arena msync per write.** That's a separate durability tradeoff
  (msync per write vs per-checkpoint). Keep current behaviour:
  msync is the checkpoint worker's job.
- **HNSW persistence per write.** HNSW snapshots are the snapshot
  worker's job. Live writes don't snapshot.
- **Wire-handler migration.** `handle_encode` still uses the legacy
  `submit_encode`. P4 migrates it after both P3b and P3d are in.

## Sequencing

1. **Add `reserve_id(IdKind)` to RealWriterHandle.** Universal id
   reservation. Pure refactor; no callers yet beyond a unit test.

2. **Add `execute_side_effects` + the helpers.** New module
   `side_effects.rs`. Unit tests against tempfile MetadataDb +
   in-memory HNSW.

3. **Wire `execute_side_effects` into `submit()` between apply and
   commit.** Update existing submit tests to include side-effect
   assertions where the phase has them.

4. **Atomicity test.** Inject a failing HNSW write. Confirm wtxn
   rolls back and no redb row materialises.

5. **Recovery test.** Use a tempfile MetadataDb + ArenaFile. Submit
   a UpsertMemory write. Drop the metadata, reopen, recover from
   WAL (P3b). Confirm arena bytes are still there and the metadata
   row materialises. Trigger HnswMaintenanceWorker.rebuild and
   confirm the vector is searchable.

Approximate sizing: ~250 LoC including tests.

## Open questions for the user

1. **Arena field on RealWriterHandle vs. plumbed through executor.**
   Recommendation: add the field. Matches the HNSW pattern; simpler
   to reason about; per-shard ownership is already correct.

2. **UpdateEmbedding's arena_slot.** Add it as a field on the phase,
   or look it up inside the side-effects pass? Recommendation: add
   it to the phase (consistent with the "pre-allocated everything"
   rule).

3. **Tombstone(Memory) on a non-existent HNSW entry.** Recommendation:
   treat as no-op. The tombstone's semantic is "don't surface"; if
   it's already not surfacing, we're done.

4. **Should recovery skip the side-effects step?** Yes — recovery
   only re-does the redb mutations via apply::dispatch. Arena bytes
   are inherited from disk; HNSW is rebuilt by maintenance. Document
   in the recovery code path comments.

## When this slice is done

- `submit(Write)` for `Phase::UpsertMemory` is functionally
  equivalent to `submit_encode` for vectors and search.
- `submit(Write)` for `Phase::UpdateEmbedding` is functionally
  equivalent to `submit_migrate_embedding` (if such a thing exists)
  or to whatever the migrate-embeddings admin op does today.
- `submit(Write)` for `Phase::Tombstone(Memory)` is functionally
  equivalent to `submit_forget` for HNSW (the arena zeroing is
  separately the ReclaimSlots phase's job).
- The combined surface — P3b (WAL) + P3d (HNSW + arena) — closes
  the feature gap with the legacy substrate path. P4 can begin
  migrating wire handlers.

## Dependencies

- **P3b (WAL framing) lands first.** The side-effects pass relies on
  WAL durability for the recovery story to work. Without P3b, a
  crash between arena write and redb commit has no recovery path.
- **P4 (handler migration) depends on both.** Without HNSW writes,
  encode can't migrate. Without WAL framing, none of the substrate
  handlers can migrate without losing durability.

P3b and P3d are independently shippable. Either order works after
the foundation (P1-P3c, P2b-d) lands. My recommendation: ship P3b
first because its design is more constrained (WAL semantics are
spec'd); P3d's design is mostly pattern-matching the legacy ordering.
