# 08.03 The Full Write Path

This file specifies the exact sequence of steps for the most common state-mutating operation: ENCODE. Other operations (FORGET, LINK, etc.) follow analogous patterns.

The write path is where the storage layer's pieces — arena, WAL, metadata store, HNSW index — coordinate to deliver atomic, durable, isolated writes.

## 1. The ENCODE flow at high level

```
Client --[ENCODE]--> Connection layer
                          |
                          v
                   Embedding layer (text → vector)
                          |
                          v
                   Routing layer (space_id → shard)
                          |
                          v
                   Per-shard writer handle
                          |
                          v
                   1. Allocate slot
                   2. Write WAL record
                   3. fsync WAL (durability barrier)
                   4. Insert into HNSW
                   5. Commit metadata in redb
                   6. Resolve the durable watermark
                   7. Publish events; enqueue derivation
                          |
                          v
                   Response back to client
```

Steps 1–7 happen in a strict order *for one request*. The fsync at step 3 is the durability barrier: after it, the operation is durable; before it, the operation is treated as never having happened.

Requests do not take these steps one at a time. A shard dispatches a bounded number of them concurrently onto its executor, so one request's step 3 overlaps another's step 2. Step 5 is the exception: the block that opens the redb transaction, applies every phase and commits holds no await point, so on a single-threaded executor only one request can be inside it. See [14.02 Writer Model](../14_concurrency/02_writer_model.md).

## 2. Slot allocation

There are two slot structures, used by different phases.

**On the live path**, a slot index is reserved from a monotonic in-process counter — a single `fetch_add`, no free-list consultation and no arena access. The counter is seeded at boot by `fetch_max` from the persisted high-water mark in `SLOT_VERSIONS_TABLE`, so a restart on a non-empty shard never re-issues a live slot index. (Without that seed the counter would restart at 1 and collide `memory_id`s, silently overwriting rows and tripping the extractor's `has_extracted` gate so new writes never extract.) `fetch_max` keeps it monotonic: it only ever moves forward, never below 1.

The slot's version comes from a read of `SLOT_VERSIONS_TABLE`: a fresh index has no row and mints version 1, while an index whose prior occupant was reclaimed carries the post-reclaim bumped version, so stale references to that occupant mismatch on every read path.

```
MemoryId = pack(shard_id_runtime, slot_id, slot_version)
```

**The arena's `SlotAllocator`** — the free-list plus next-fresh counter, whose `alloc` stamps `PENDING_WRITE` and whose `free` clears flags and pushes the index back — is rebuilt from the post-replay arena during recovery and serves the replay path. The live write path does not go through it, which is consistent with §5: the live path writes nothing into the arena at all.

Arena growth is likewise a recovery-time and capacity concern rather than a step in a client write; see [`01_arena.md`](01_arena.md).

## 3. WAL record construction

Brain constructs an ENCODE record:

```rust
WalRecord {
    header: WalRecordHeader {
        lsn: next_lsn,
        record_type: ENCODE,
        flags: 0,
        payload_length: ...,
        timestamp: now(),
        agent_id_lo64: space_id.low64(),
    },
    payload: EncodeRecord {
        memory_id: allocated_id,
        request_id: request.request_id,
        space_id: request.space_id,
        session_id: resolved_session_id,
        kind: request.kind,
        salience_initial: computed_initial_salience,
        embedding_model_fp: current_fingerprint,
        text_length: request.text.len() as u32,
        text: request.text,
        vector: embedded_vector,
        // edges if any
    },
    footer: WalRecordFooter {
        payload_crc32c: compute_crc(...),
    }
}
```

The full record is serialized into the WAL's append buffer.

## 4. Group commit and fsync

The append:

1. The record is copied into the WAL writer's aligned buffer.
2. The writer joins the current group-commit window (or starts a new one).
3. When the window closes (100 µs default) or the buffer fills, the writer issues `pwritev2` with `RWF_DSYNC`.
4. The kernel performs the write and signals completion.
5. The writer's coroutine is awakened.

Up to step 5, the operation is **not durable**. Only after step 5 has the write reached stable storage.

If Brain crashes between record append and step 5, the operation is lost. The client retry (with the same `request_id`) will succeed normally as a new operation; idempotency only kicks in if the original made it past step 5.

## 5. The arena on the live path

The live ENCODE path does not write the vector into the arena slot. The slot is allocated (which stamps `PENDING_WRITE`, the new slot version and the slot CRC), and that is the whole of the arena's involvement in a successful write.

The at-rest homes of a memory encoded in the current run are **redb** — which holds the durable write-time vector for the complete live set — and, for its text, the lexical index. The arena is filled by WAL replay: `write_encoded_slot` copies the vector out of the `EncodePayload`, sets `OCCUPIED`, stamps the slot version, the model fingerprint and the record's timestamps, and refreshes the slot CRC.

So the arena is the **boot-time image** of every durable memory, not a live mirror. Two consequences follow, and both are load-bearing:

- A rebuild of the ANN index at runtime must read redb, not the arena. A runtime rebuild from the arena would silently drop every memory encoded in the current run. Only the boot-recovery rebuild may use the arena, because at that moment the arena has just been replayed and *is* the durable image.
- A hard FORGET of a same-run memory is a no-op against the arena: there is no slot to zero, and the data's at-rest homes are purged elsewhere. `hard_forget_slot` therefore checks occupancy and slot version, and leaves a slot alone if it is free or has been reclaimed by a newer memory.

The arena is not fsync'd on the write path either. Its pages go to the page cache and the kernel writes them back lazily; `msync(MS_SYNC)` happens only inside the checkpoint sequence. Durability belongs to the WAL.

This is the "log is truth" invariant in action: the WAL record carries the vector, so the arena is always reconstructible and never has to be correct ahead of recovery.

## 6. Metadata update

Brain then updates the metadata store:

```rust
let txn = metadata_db.begin_write()?;
{
    let mut memories = txn.open_table(MEMORIES_TABLE)?;
    memories.insert(
        memory_id,
        MemoryMetadata {
            space_id: ...,
            session_id: ...,
            kind: ...,
            text_offset: ...,
            text_length: ...,
            salience: ...,
            embedding_model_fp: ...,
            created_at: ...,
            // ...
        },
    )?;

    let mut idem = txn.open_table(IDEMPOTENCY_TABLE)?;
    idem.insert(request_id, (memory_id, response_payload))?;

    if !edges.is_empty() {
        let mut edge_table = txn.open_table(EDGES_TABLE)?;
        for edge in edges {
            edge_table.insert(edge.key(), edge.value())?;
        }
    }
}
txn.commit()?;
```

redb handles its own atomicity: the entire transaction either commits or aborts. If redb's commit fails (e.g., disk full), Brain logs and proceeds to step 7 with a degraded state — the WAL record is still durable, but the metadata didn't update.

A subsequent recovery would replay the WAL record, retry the metadata update, and complete it. Until then, the memory's existence in the WAL is the authoritative source.

In practice, redb commit failures are rare. The metadata store is sized to accommodate normal growth; out-of-space at the metadata layer is detected before this point.

## 7. HNSW insertion

After metadata commits, Brain inserts the vector into the HNSW index:

```rust
hnsw_index.insert(memory_id, &vector, &options)?;
```

HNSW insertion is in-memory and does not have its own persistence. It runs **before** the redb transaction opens, so the transaction's lifetime stays minimal and an HNSW failure abandons the write before any metadata commits.

The HNSW index is rebuilt at startup from a snapshot plus replay of post-snapshot changes, falling back to a full rebuild. See [09. Indexing](../09_indexing/00_purpose.md) §Recovery and [`04_recovery.md`](04_recovery.md) §8.

## 8. Resolving the durable watermark

Immediately after `commit()` returns, the write resolves the LSN range the WAL drain reserved for it. The shard publishes the highest *contiguous* prefix of committed LSNs; see [14.02 §13a](../14_concurrency/02_writer_model.md).

This matters here because concurrent requests do not commit in LSN order. A write that appended earlier may commit later, and until it does, the frontier stays below its first LSN no matter how many later writes have committed.

## 9. Publication and the acknowledgment

After the commit, the write publishes one event per phase that has a wire surface and enqueues its post-commit derivation stages (auto-edge, temporal-edge, extractors). Then the response goes back to the client, carrying the memory id, the durable LSN, and the list of pending background stages.

A new memory is visible to RECALL as soon as its redb row commits; the HNSW point is already in place by then. Publication of the ANN pending buffer into the main graph is a structure-and-cost concern, not a visibility gate — a search merges the pending buffer with the published main graph, so a freshly-inserted vector is found before any publication happens. See [14.02 §17](../14_concurrency/02_writer_model.md) under Atomic Publication.

The ack is sent after the redb commit, not after the WAL fsync. The client's success response therefore means all of: the record is fsync'd, the memory row is committed, and the HNSW point is live. Only the derivation stages are left, and the response names them so a client that wants to wait can.

## 10. Failure handling per step

| Step | Failure modes | Response |
|---|---|---|
| 1. Slot allocation | Out of free slots, growth fails | Return `OutOfStorage` to client |
| 2. WAL append | Channel full, transient failure | Await channel capacity; if persistent, return a WAL-sink error |
| 3. WAL fsync | Disk error | Latch WAL broken (terminal for the process); the write fails non-retryably |
| 4. HNSW insertion | OOM | Abandon the write before the transaction opens; the reserved LSN range is abandoned by its `Drop` guard |
| 5. Metadata commit | redb commit fails | Fail the write; the reserved LSN range is abandoned, so the frontier closes across it on a later commit |
| 6. Watermark resolve | Cannot fail | (Atomic publish under a short mutex) |
| 7. Publish / enqueue | Channel full | Best-effort: the stage is dropped and counted; the write still succeeded |

Note that only step 7 happens after the client's ack. A failure at step 4 or 5 is reported to the client as a failure even though the write's WAL records are already durable, and that is consistent: abandoning the reserved range lets a later real commit close the contiguous prefix across it, so recovery skips those records rather than applying an operation the client was told had failed. If the failure was the last thing to happen before a crash, no later commit closed the prefix, so recovery does replay the record — and the apply re-derives the same refusal. See [`04_recovery.md`](04_recovery.md) §5a: the log records attempts, not only outcomes.

## 11. The FORGET write path

Analogous to ENCODE:

1. Validate the MemoryId belongs to the agent.
2. Construct a FORGET WAL record.
3. fsync the WAL.
4. Mark the memory tombstoned in HNSW.
5. Commit metadata (mark memory as tombstoned, set forgot_at).
6. Publish.
7. For hard mode on a memory that has an arena slot: zero the slot's vector and set `HARD_FORGOTTEN`.

For soft mode, steps 4–5 are sufficient; the data is preserved until reclamation.

Hard mode promises the plaintext-derived embedding is no longer recoverable from the file. Step 7 runs at the shard boundary rather than inside the apply, because the apply layer holds only the redb transaction while the arena lives on the shard. It is guarded on occupancy and slot version: a memory encoded in the current run has no arena slot to zero (its at-rest homes are redb and the lexical index, purged in step 5), and if a newer memory has reclaimed the slot index, the version mismatch leaves that memory's vector alone.

A hard FORGET inside a transaction is buffered until `COMMIT_TXN`, and a rollback must not leave a zeroed slot, so transactional hard forgets are covered by recovery's replay — which re-zeroes hard-forgotten slots — rather than at the shard boundary.

## 12. The LINK write path

For adding edges to existing memories:

1. Validate source and target MemoryIds belong to the agent.
2. Construct a LINK WAL record.
3. fsync.
4. Commit the metadata's edge table.
5. (No arena change.)
6. Publish.

LINK is lighter than ENCODE; no arena work, no HNSW work.

## 13. Transactional grouping

For multi-record transactions (e.g., a batch of edges in a single ENCODE):

1. Allocate a TxnId.
2. Write a TXN_BEGIN WAL record.
3. Write each operation's WAL record (with `flags` bit 0 set, carrying the txn_id).
4. Write a TXN_COMMIT WAL record.
5. fsync (single fsync covers all the records in the transaction).

Recovery treats the records atomically: all-or-nothing. If TXN_COMMIT is missing (due to a crash mid-transaction), all the records since TXN_BEGIN are discarded.

Application-side: this is exposed via TXN_BEGIN / TXN_COMMIT opcodes ([04. Wire Protocol](../04_wire_protocol/00_purpose.md) §07).

## 14. End-to-end latency

For a typical ENCODE on commodity NVMe:

| Step | Time |
|---|---|
| Embedding (cache hit) | <0.001 ms |
| Embedding (cache miss; CPU inference) | CPU- and model-dependent; see the reference measurement below |
| Slot allocation | <0.001 ms |
| WAL append | <0.01 ms |
| Group-commit wait + fsync | 0.1-0.3 ms |
| HNSW insert | 0.5-2 ms |
| Metadata commit (redb) | 0.1-0.5 ms |
| Watermark resolve | <0.001 ms |
| Publish + stage enqueue | <0.001 ms |
| **Total excluding CPU embedding** | **measure on the deployment** |

These are per-request step costs, not a serial budget. Concurrent requests overlap everything except the metadata commit, so a shard's *throughput* is not the reciprocal of the column's sum; the commit row is the only one that every request must have to itself.

The embedding row and the total are not latency contracts. As a reference
measurement, `bge-small-en-v1.5` (BERT, 33M parameters, f32, 127 MB, 384
dimensions) took 16.8 ms for a 9-word text at an intra-op width of one, and
about 51 ms for the same text at a width of 10, on aarch64 Linux in the
devcontainer with 10 CPUs exposed. A roughly 500-token text took about 652 ms.

The forward pass does not run on the shard's reactor. The embed helper leaves
the reactor through `glommio::executor().spawn_blocking`, and that executor's
blocking pool is a single thread whose CPU set holds the shard's own pinned
CPU. The reactor becomes schedulable again while the model runs, but the two
share one core: going off-reactor buys a thread, not a core.

Widening the intra-op split therefore costs throughput rather than buying it —
the extra pieces run on cores other shards own. Measured in isolation on a
10-CPU box, 4 shards sustained 266, 258, 235 and 203 embeds per second at
intra-op widths of 1, 2, 4 and 8; 9 shards sustained 586, 573 and 551 at widths
of 1, 2 and 4. The width is derived from the machine; it is not an operator
knob.

Shard-side latency is unaffected by a concurrent embed. The same harness
measured a probe p95 inflation of 1.00-1.02 across every arm, which is what the
offload exists to deliver.

Measure on the deployment hardware when sizing the write path.

## 15. Concurrent ENCODEs

Multiple ENCODE operations from different agents (different shards) run independently.

Within a shard they overlap. The shard admits up to 15 concurrent dispatches at the default WAL batch budget, so while one ENCODE waits on its group-commit fsync, others are embedding, appending, or inserting into HNSW. What does *not* overlap is the redb transaction: its body holds no await, so on the shard's single-threaded executor the commits run one after another with nothing interleaved. Concurrency between requests, serialization of the transaction.

The ordering consequence is worth stating plainly: two concurrent ENCODEs on one shard may be assigned LSNs in one order and commit in the other. Nothing downstream may assume LSN order implies commit order — which is why the shard's durable watermark tracks a contiguous committed prefix instead of a maximum ([14.02 §13a](../14_concurrency/02_writer_model.md)).

For high-throughput single-shard workloads, throughput is bounded by:

- WAL fsync throughput (with group commit, ~50K-200K records/sec).
- redb commit throughput (~10K-50K commits/sec for small transactions).
- HNSW insertion throughput (~5K-20K inserts/sec).

The HNSW insertion is typically the bottleneck. For workloads that exceed HNSW's per-shard insertion rate, Brain batches HNSW inserts (multiple memories from a recent window are inserted together; see [09. Indexing](../09_indexing/00_purpose.md) §Batching).

---

*Continue to [`04_recovery.md`](04_recovery.md) for crash recovery.*
