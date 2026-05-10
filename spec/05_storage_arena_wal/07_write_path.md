# 05.07 The Full Write Path

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
                   Routing layer (agent_id → shard)
                          |
                          v
                   Per-shard writer task
                          |
                          v
                   1. Allocate slot
                   2. Write WAL record
                   3. fsync WAL (durability barrier)
                   4. Write vector to arena
                   5. Update metadata in redb
                   6. Insert into HNSW
                   7. Publish (epoch advance)
                          |
                          v
                   Response back to client
```

Steps 1–7 happen in a strict order. The fsync at step 3 is the durability barrier: after it, the operation is durable; before it, the operation is treated as never having happened.

## 2. Slot allocation

The slot allocator is a per-shard structure. It maintains:

- A free-list of slot IDs that are free (after FORGET reclamation).
- A "next new slot ID" counter for never-used slots.

When allocation is requested:

1. If free-list is non-empty: pop the head; check the slot's flags to confirm it's still free; if so, use it.
2. Otherwise: take the next new slot ID. If this exceeds the arena's capacity, trigger arena growth (see [`03_arena_growth.md`](03_arena_growth.md)).

The allocated slot is marked as "pending-write" (flags bit 2) before any data is written. This bit is transient — cleared once the slot is fully populated.

The MemoryId is constructed:

```
MemoryId = pack(shard_id_runtime, slot_id, slot_version_new, reserved=0)
```

`slot_version_new` is `current_version + 1` for reclaimed slots, or 1 for never-used slots.

## 3. WAL record construction

The substrate constructs an ENCODE record:

```rust
WalRecord {
    header: WalRecordHeader {
        lsn: next_lsn,
        record_type: ENCODE,
        flags: 0,
        payload_length: ...,
        timestamp: now(),
        agent_id_lo64: agent_id.low64(),
    },
    payload: EncodeRecord {
        memory_id: allocated_id,
        request_id: request.request_id,
        agent_id: request.agent_id,
        context_id: resolved_context_id,
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

If the substrate crashes between record append and step 5, the operation is lost. The client retry (with the same `request_id`) will succeed normally as a new operation; idempotency only kicks in if the original made it past step 5.

## 5. Arena write

After WAL fsync confirms durability, the substrate writes the vector to the arena slot:

```rust
let slot_offset = HEADER_SIZE + slot_id * SLOT_SIZE;
let vector_ptr = arena_base.add(slot_offset);

// Write vector
unsafe {
    std::ptr::copy_nonoverlapping(
        vector.as_ptr() as *const u8,
        vector_ptr,
        VECTOR_BYTE_SIZE,
    );
}

// Write metadata
let metadata_offset = slot_offset + VECTOR_BYTE_SIZE;
write_slot_metadata(arena_base.add(metadata_offset), &metadata);

// Set occupied bit (and clear pending-write bit)
let flags_offset = metadata_offset + 4;
let flags_ptr = arena_base.add(flags_offset) as *mut u32;
unsafe {
    std::ptr::write_volatile(flags_ptr, OCCUPIED_FLAG);
}
```

The arena writes are not fsync'd. They go to the page cache; the kernel writes them back lazily. If the substrate crashes after WAL fsync but before the arena write completes:

- WAL is durable (record is fsync'd).
- Arena write may be partial.
- Recovery replays the WAL record, re-writing the vector to the arena. The arena ends up correct.

This is the "log is truth" invariant in action.

## 6. Metadata update

After the arena write, the substrate updates the metadata store:

```rust
let txn = metadata_db.begin_write()?;
{
    let mut memories = txn.open_table(MEMORIES_TABLE)?;
    memories.insert(
        memory_id,
        MemoryMetadata {
            agent_id: ...,
            context_id: ...,
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

redb handles its own atomicity: the entire transaction either commits or aborts. If redb's commit fails (e.g., disk full), the substrate logs and proceeds to step 7 with a degraded state — the WAL record is still durable, but the metadata didn't update.

A subsequent recovery would replay the WAL record, retry the metadata update, and complete it. Until then, the memory's existence in the WAL is the authoritative source.

In practice, redb commit failures are rare. The metadata store is sized to accommodate normal growth; out-of-space at the metadata layer is detected before this point.

## 7. HNSW insertion

After metadata commits, the substrate inserts the vector into the HNSW index:

```rust
hnsw_index.insert(memory_id, &vector, &options)?;
```

HNSW insertion is in-memory and does not have its own persistence. The HNSW index is reconstructible from the arena and metadata; on restart, it's rebuilt.

In v1, the HNSW index is rebuilt at startup from a checkpoint plus replay of post-checkpoint changes. See [06. ANN Index](../06_ann_index/) §Recovery.

## 8. Publication

After all the above, the new memory is "published":

- The slot's `pending-write` flag is cleared (the slot is fully written).
- The shard's epoch is advanced (or scheduled to advance), so reads can see the new memory.
- Any clients waiting for `request_id` notification are notified.

Publication is the moment the new memory becomes visible to other operations. A `RECALL` issued just before publication sees the previous state; a `RECALL` issued after sees the new memory.

## 9. The acknowledgment

The response is sent back to the client:

```rust
SendResponse {
    request_id: request.request_id,
    memory_id: allocated_id,
    salience: computed_initial_salience,
    // ...
}
```

The acknowledgment can be sent any time after step 4 (WAL fsync). The substrate optimizes for low latency by acknowledging as soon as durability is confirmed; steps 5–8 happen in the background after the ack.

This is a deliberate design choice. The client knows the operation is durable; what's left is bookkeeping that the client doesn't need to wait for.

## 10. Failure handling per step

| Step | Failure modes | Response |
|---|---|---|
| 1. Slot allocation | Out of free slots, growth fails | Return `OutOfStorage` to client |
| 2. WAL append | Buffer full, transient failure | Wait briefly; if persistent, return `WalUnavailable` |
| 3. WAL fsync | Disk error | Mark WAL broken; return `WalUnavailable` |
| 4. Arena write | Should not fail (memory write) | If page-cache OOM, log and continue (write will succeed eventually) |
| 5. Metadata update | redb commit fails | Log; ack to client (WAL is durable); recovery will fix |
| 6. HNSW insertion | OOM | Log; mark for HNSW rebuild on next checkpoint cycle |
| 7. Publication | Should not fail | (Atomic operation) |

Note that steps 4–7 happen after the durability barrier. If they fail, the operation is still durable from the client's perspective; recovery (or background workers) will reconcile.

## 11. The FORGET write path

Analogous to ENCODE:

1. Validate the MemoryId belongs to the agent.
2. Construct a FORGET WAL record.
3. fsync the WAL.
4. Update the slot's flags (set tombstone bit).
5. Update metadata (mark memory as tombstoned, set forgot_at).
6. Remove from HNSW.
7. Publish.
8. For hard mode: zero the slot's vector and text.

For soft mode, steps 4–6 are sufficient; the slot's data is preserved until reclamation. For hard mode, an additional WAL record (HARD_FORGET) is written and step 8 happens, ensuring the data is unrecoverable.

## 12. The LINK write path

For adding edges to existing memories:

1. Validate source and target MemoryIds belong to the agent.
2. Construct a LINK WAL record.
3. fsync.
4. Update the metadata's edge table.
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

Application-side: this is exposed via TXN_BEGIN / TXN_COMMIT opcodes ([03. Wire Protocol](../03_wire_protocol/) §07).

## 14. End-to-end latency

For a typical ENCODE on commodity NVMe:

| Step | Time |
|---|---|
| Embedding (cache hit) | <0.001 ms |
| Embedding (cache miss; CPU inference) | 5-10 ms |
| Slot allocation | <0.001 ms |
| WAL append | <0.01 ms |
| Group-commit wait + fsync | 0.1-0.3 ms |
| Arena write | <0.001 ms |
| Metadata commit (redb) | 0.1-0.5 ms |
| HNSW insert | 0.5-2 ms |
| Publication | <0.001 ms |
| **Total (cache hit)** | **~1 ms** |
| **Total (cache miss)** | **6-13 ms** |

The bottleneck is embedding. The storage layer's contribution is minor.

## 15. Concurrent ENCODEs

Multiple ENCODE operations from different agents (different shards) run independently. Within a shard, the writer task serializes them — single-writer-per-shard.

For high-throughput single-shard workloads, the writer task's throughput is bounded by:

- WAL fsync throughput (with group commit, ~50K-200K records/sec).
- redb commit throughput (~10K-50K commits/sec for small transactions).
- HNSW insertion throughput (~5K-20K inserts/sec).

The HNSW insertion is typically the bottleneck. For workloads that exceed HNSW's per-shard insertion rate, the substrate batches HNSW inserts (multiple memories from a recent window are inserted together; see [06. ANN Index](../06_ann_index/) §Batching).

---

*Continue to [`08_recovery.md`](08_recovery.md) for crash recovery.*
