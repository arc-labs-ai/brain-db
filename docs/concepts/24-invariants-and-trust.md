# 24 — Invariants and the trust model

This chapter answers two questions:

1. **What does Brain *promise*?** Specifically — what
   properties does the substrate hold no matter what?
2. **How does Brain *keep* those promises?** What's
   authoritative, what's derived, and what survives various
   kinds of failure?

The answer to (1) is the **seven invariants**. The answer
to (2) is the **trust model**: a small hierarchy of what's
truth and what's reconstructible.

If you trust these, you can build on Brain. If you don't,
the rest of the documentation doesn't help.

---

## The seven invariants

These are the properties Brain holds **always**. Code that
violates them is a bug, regardless of test results.

### 1. WAL-before-acknowledge

> No operation acknowledges success until its write-ahead-log
> record is fsync'd to disk.

The most fundamental durability promise. By the time
`encode` returns OK to the client, the memory is on stable
storage. A power loss in the next millisecond cannot lose
the memory.

This is what makes the client's retry loop sound. If the
client doesn't get an ack, it doesn't know whether the
operation took effect — it retries with the same
`request_id`. If the client *does* get an ack, the
operation is durable; the client can move on.

Chapter 18 covers the WAL mechanism in detail.

### 2. Single writer per shard

> Within a shard, only one task writes to the shard's data
> at a time.

This is what lets the substrate avoid locks on the hot
path. The arena, metadata, WAL, and HNSW writer all
assume serial access — they don't take internal mutexes
because there's no one to take them from.

Enforced not by locks but by construction: each shard
has one Glommio executor, the executor is
single-threaded, so at any moment at most one task is
making progress.

Reads can be concurrent (interleaved at `.await` points
on the same thread). Writes are inherently serialised.

Chapter 22 covers the concurrency model.

### 3. CRC everywhere

> Every WAL record carries a CRC32C checksum. Every arena
> slot carries a CRC32C checksum. Reads that mismatch are
> rejected.

The substrate doesn't assume disk bytes are correct. Every
durable artifact has a checksum that lets recovery (and,
where appropriate, runtime reads) detect bit rot, torn
writes, and post-write disk corruption.

When a CRC mismatch is detected, the substrate's
behaviour depends on context:

- During recovery: the mismatched record marks the
  truncation point; recovery stops there.
- During runtime reads: the substrate logs the error and
  returns a structured error to the client.

Either way, **bad bytes never become good answers**.
Chapter 18 and 19 cover the specifics.

### 4. `MemoryId` carries the slot version

> Every `MemoryId` encodes the version of the slot it was
> issued against. Stale `MemoryId`s find a slot with a
> different version, and the operation returns
> `MemoryNotFound`.

Chapter 19 covered this. The mechanic: every reclamation
(post-grace-period forget) bumps the slot's version
counter. A `MemoryId` from before the reclamation has
the old version; after reclamation, it doesn't match the
slot anymore.

This is what makes `MemoryId`s **safe even when the slot
is reused**. A client holding a stale `MemoryId` gets a
clean `NotFound` rather than silently operating on
someone else's data.

### 5. Idempotency by `request_id`

> Every state-mutating operation carries a client-supplied
> `request_id`. Retrying with the same `request_id` (within
> 24 hours) returns the cached response from the first
> call. Retrying with the same `request_id` and different
> parameters returns `IdempotencyConflict`.

The promise: **safe retries**. If you ever wonder
"did my operation go through?", you retry with the same
`request_id`, and Brain figures out whether to replay the
old result or run the operation for the first time.

Conflict detection: if you reuse a `request_id` for a
*different* operation, Brain notices via a content hash
mismatch and rejects with a structured error. That's a
client bug; the substrate refuses to silently overwrite.

The 24-hour window is the idempotency table TTL; a
background sweeper prunes expired entries. Chapter 25
covers this in detail.

### 6. Tombstone grace before reclamation

> When a memory is forgotten, its storage slot is
> tombstoned but not immediately reclaimed. A configurable
> grace period (default 7 days) must elapse before the
> slot becomes available for reuse.

This bounds the window between "client forgot a memory"
and "another encode might land in the same slot." During
the grace period, any stale `MemoryId` operations on the
forgotten memory get a `NotFound` (because the tombstone
bit is set); after reclamation, they get `NotFound`
because the slot version mismatches.

Hard-forget (the opt-in mode) zeros the slot's bytes
immediately for privacy / compliance, then proceeds with
the normal reclamation grace period for the slot.

### 7. Fail-stop, never silent corruption

> If the substrate detects internal inconsistency or
> corruption, it refuses to operate rather than returning
> potentially-wrong data.

Concrete examples:

- Arena header CRC mismatch → shard refuses to spawn.
- WAL torn-write mid-segment → recovery stops at the
  truncation point.
- Metadata file fails redb's own integrity check → shard
  refuses to spawn.
- HNSW index detects a stale node version → reject the
  query rather than return potentially-wrong results.

The substrate is **fail-stop**: better to be down loudly
than to be wrong silently. This is the consequence of
"trust the data we serve" being more important than "stay
up at all costs."

For deployments that need higher uptime, the answer is
external HA wrapping (chapter 23), not a more permissive
substrate.

---

## Why these seven

You could imagine a longer list. Why exactly these
seven?

Because they're **load-bearing**. Each invariant directly
enables a guarantee a client relies on:

- WAL-before-ack → "if you got an ack, the memory
  survived a crash."
- Single-writer → "no race conditions inside a shard;
  no need for client-side locking."
- CRC → "we'll never tell you a wrong vector."
- Slot version → "stale `MemoryId`s are safe."
- Idempotency → "retries are safe."
- Tombstone grace → "your `MemoryId`s are stable across
  forgets."
- Fail-stop → "if we tell you something, you can
  believe it."

A subset of seven would weaken specific guarantees. A
larger set would dilute the focus. Seven is the working
count.

These invariants are also **testable**. Brain's
chaos-test suite (architecture tier covers this in
detail) deliberately injects faults during operations —
kills the process, corrupts files, fills the disk —
and verifies that the durable state matches what each
invariant predicts. If a chaos test fails, an invariant
is being violated, and that's a release blocker.

---

## The trust model: authoritative vs derived

Brain's data falls into two categories:

### Authoritative

> The bytes that *define* the substrate's state. If these
> are lost, the data is lost.

- **The write-ahead log** (`wal/*.wal`). Every state-
  changing operation is logged here before being
  applied anywhere else. The WAL is the *truth*.
- **The arena** (`arena.bin`). The vectors that
  memories' embeddings hold.
- **The metadata store** (`metadata.redb`). The rows
  that hold each memory's text, kind, salience,
  timestamps, edges, etc.

Note: the arena and metadata are technically *redundant*
with the WAL — recovery can rebuild them from a fresh
WAL replay. But in practice the arena and metadata are
treated as authoritative too, because the WAL is
truncated periodically (after a checkpoint) and full
replay from time zero would be impractical.

### Derived

> The bytes that the substrate *generates* from
> authoritative state. If these are lost, they can be
> rebuilt.

- **The HNSW index.** Lives only in RAM (rebuilt on
  cold start from the arena + metadata).
- **The tantivy indexes** (knowledge-active mode). On
  disk, but rebuildable from `statements` and the
  memory text.
- **The entity HNSW** (knowledge-active mode). On disk
  but rebuildable from the entity name embeddings.
- **The LLM extractor cache** (knowledge-active mode).
  Recomputable by re-running the extractors.
- **Salience scores** for memories. Computed from
  creation time + access history.
- **Statistics, audit-trail-aggregated views, etc.**

The line matters because **a disaster scenario looks
different for each category**:

- Losing authoritative data → restore from snapshot.
- Losing derived data → automatic rebuild on next boot.
- Losing both → restore from snapshot (which captures
  authoritative + derived bytes; the derived bytes are
  faster to use than rebuilding).

---

## What survives what

A practical table of failures and outcomes:

| Failure | Authoritative state | Derived state | Recovery |
|---|---|---|---|
| Process crash (panic, SIGSEGV) | Last fsync'd WAL intact | All RAM lost | Recovery replays WAL; rebuilds indexes |
| OS crash (kernel panic) | Same | Same | Same |
| Power loss (with FUA-supporting SSD) | Last fsync'd WAL intact | Same | Same |
| Power loss (without FUA) | Last fsync'd WAL *may* be lost (drive cache was volatile) | Same | Restore from snapshot if WAL truncated mid-record |
| Single arena/metadata corruption | Lost on that file | Same | Restore from snapshot |
| Full disk corruption | Lost | Lost | Restore from off-site snapshot |
| Shard executor panics during request | WAL still up to that point | Index entries up to that point | Recovery rebuilds; shard re-spawns |
| Long-running shard fills disk | New writes fail | Index up to disk-full point | Operator clears disk, restarts; recovery proceeds |
| Forgotten `MemoryId` referenced | n/a | n/a | Returns `NotFound` immediately |

The pattern: **as long as the WAL is intact and recent,
the data is recoverable**. Snapshots ensure recent
authoritative state; the WAL covers everything since.

---

## "Verify, don't trust"

The substrate doesn't take shortcuts on integrity:

- Every WAL record's CRC is checked on read.
- Every arena slot has a CRC.
- Every redb operation runs through redb's own
  transaction-and-checksum infrastructure.
- The model fingerprint is checked on cache reads so
  stale embeddings don't get returned.
- Slot versions are checked on every `MemoryId`
  lookup.

This isn't paranoia; it's the only way to maintain the
fail-stop invariant. The substrate has to *be able to
tell* when something is wrong. Without checksums, "wrong"
just looks like "data."

---

## The honest trade-off

Strong invariants aren't free. The cost shows up in:

- **Latency on writes.** WAL fsync adds 50–200 µs per
  group-commit batch. Single-writer serialisation
  limits per-shard write throughput.
- **Storage overhead.** Every record carries a CRC. The
  WAL roughly doubles disk usage versus a no-WAL system.
- **RAM overhead.** Idempotency table holds 24 hours of
  request_ids. Snapshot worker keeps an extra copy of
  files briefly.
- **Operational complexity.** You have to think about
  snapshots, retention, backups, recovery drills.

Brain pays these costs because the alternatives are
worse:

- A database without WAL can lose committed writes on a
  crash.
- A database without CRC can silently serve wrong data
  for years until someone notices.
- A database without idempotency can double-count
  retries.

These aren't theoretical failures; they happen
regularly in real systems. The substrate's discipline
is *not* over-engineering — it's the floor of "you can
build on this."

---

## What the invariants do *not* promise

Equally important: a list of what Brain doesn't claim.

- **Not a probability of recall.** A recall returns
  the top-K hits the substrate has; whether any
  particular memory is among them depends on indexing
  quality. The substrate doesn't promise "if you asked
  about X, you'll find X."
- **Not perfect deduplication.** Two encodes of the
  *same text* produce two memories (with the same
  embedding, but different `MemoryId`s). The substrate
  doesn't auto-dedup; clients can use the
  `idempotency_key` to make retries safe but can't
  use it for semantic dedup across distinct requests.
- **Not real-time freshness.** Memories are visible to
  recall immediately after the encode ack (the WAL +
  index update is part of the ack). But salience
  updates, consolidations, and extractor outputs are
  *eventually consistent* — they run in the
  background and the change might not be visible for
  seconds to minutes.
- **Not magic understanding.** The substrate stores and
  retrieves. It doesn't reason. LLM extractors call
  external models; the substrate caches and audits
  but doesn't generate text itself.
- **Not horizontally distributed.** v1 is single-server
  by design. Multi-server clustering is a future
  iteration.

Honesty about non-guarantees is the flip side of strong
invariants. The substrate keeps its promises *exactly*;
it doesn't promise more than it can.

---

## Recap

Seven invariants, each load-bearing:

1. WAL-before-acknowledge.
2. Single writer per shard.
3. CRC everywhere.
4. `MemoryId` carries slot version.
5. Idempotency by `request_id`.
6. Tombstone grace before reclamation.
7. Fail-stop, never silent corruption.

The trust model: the WAL + arena + metadata are
*authoritative*; everything else is *derived*. Disaster
recovery restores authoritative state; derived state
rebuilds.

Strong invariants cost latency, storage, and
operational complexity. The alternatives — silently
wrong data, lost writes, double-counted retries — are
worse.

---

## Where to go next

- **What durability means concretely:**
  [chapter 18](18-storage-and-durability.md).
- **Determinism and idempotency:**
  [chapter 25](25-determinism-idempotency-replay.md).
- **The architecture-tier statement of these
  invariants:**
  [`../architecture/03-arena-and-wal.md`](../architecture/03-arena-and-wal.md)
  and the durability-flow chapters.
