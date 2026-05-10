# 05.05 WAL Record Formats

The byte-level format of WAL records. Implementers MUST produce these layouts; recovery and SUBSCRIBE consume them.

## 1. The segment header

Each segment file begins with a 4 KB header:

| Offset | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | magic | "BWAL" (0x42 0x57 0x41 0x4C) |
| 4 | 4 | format_version | u32 LE |
| 8 | 16 | shard_uuid | UUIDv7 |
| 24 | 8 | segment_seq | u64 LE (matches the file name) |
| 32 | 8 | starting_lsn | u64 LE (first LSN in this segment) |
| 40 | 8 | created_at | u64 LE, unix nanoseconds |
| 48 | 4 | header_crc32c | u32 LE |
| 52 | 4044 | reserved | zero |

After the header (offset 4096), records begin.

## 2. The record header (32 bytes)

Each record starts with a 32-byte header:

| Offset | Size | Field | Type |
|---|---|---|---|
| 0 | 8 | lsn | u64 LE |
| 8 | 1 | record_type | u8 |
| 9 | 1 | flags | u8 |
| 10 | 2 | reserved | zero |
| 12 | 4 | payload_length | u32 LE |
| 16 | 8 | timestamp | u64 LE, unix nanoseconds |
| 24 | 8 | agent_id_lo64 | u64 LE (low 8 bytes of agent UUID) |

The full agent UUID is 16 bytes; we store only the low 64 bits in the header for filtering. The full UUID is in the payload when needed.

After the record header comes the payload (variable length, exactly `payload_length` bytes), followed by an 8-byte footer:

| Offset (relative to footer) | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | payload_crc32c | u32 LE (CRC32C over the entire record header + payload) |
| 4 | 4 | reserved | zero |

Total record size: `32 + payload_length + 8`.

## 3. Record types

The `record_type` byte:

| Value | Type | Description |
|---|---|---|
| 0 | Reserved | Never used |
| 1 | ENCODE | A new memory was created |
| 2 | FORGET | A memory was forgotten |
| 3 | LINK | An edge was added |
| 4 | UNLINK | An edge was removed |
| 5 | UPDATE_SALIENCE | A memory's salience was updated |
| 6 | RECLAIM | A tombstoned slot was reclaimed |
| 7 | CONSOLIDATE | The consolidation worker created a Consolidated memory |
| 8 | UPDATE_KIND | A memory's kind was changed |
| 9 | UPDATE_CONTEXT | A memory's context was changed |
| 10 | CHECKPOINT_BEGIN | A checkpoint started |
| 11 | CHECKPOINT_END | A checkpoint completed |
| 12 | TXN_BEGIN | A transaction started |
| 13 | TXN_COMMIT | A transaction committed |
| 14 | TXN_ABORT | A transaction was aborted |
| 15 | MIGRATE_EMBEDDING | A memory was re-embedded with a new model |
| 16-127 | Reserved for future v1 minor versions | |
| 128-255 | Reserved for v2+ | |

Each type's payload format is specified below.

## 4. Flags byte

The `flags` byte:

| Bit | Meaning |
|---|---|
| 0 | Part of a transaction (see TxnId in payload) |
| 1 | Coalesced (this record represents multiple logical operations of the same type) |
| 2 | Replayed (set during recovery; helps idempotency in re-recovery scenarios) |
| 3-7 | Reserved |

## 5. ENCODE record payload

```
struct EncodeRecord {
    memory_id: MemoryId,             // 16 bytes (slot_id + version assigned by allocator)
    request_id: RequestId,           // 16 bytes (UUIDv7)
    agent_id: AgentId,               // 16 bytes (full UUID; matches header low64)
    context_id: ContextId,           // 8 bytes
    kind: u8,                        // 1 byte (Episodic/Semantic/Consolidated)
    salience_initial: f32,           // 4 bytes
    embedding_model_fp: [u8; 16],    // 16 bytes
    text_length: u32,                // 4 bytes
    text: [u8; text_length],         // UTF-8
    vector: [f32; 384],              // 1536 bytes (only if FLAG_INCLUDE_VECTOR)
    // edges follow only if FLAG_INCLUDE_EDGES
    edge_count: u16,
    edges: [EdgeRecord; edge_count],
}
```

The vector is included in ENCODE records by default. This makes the WAL self-sufficient: replay can reconstruct the arena without consulting the metadata store. The cost is ~1.5 KB per encode in the WAL.

For deployments that prefer smaller WAL records, a configuration option excludes the vector (the WAL just records "this memory was encoded"; the arena and metadata store the vector). Recovery is still correct as long as both the arena and WAL survive together.

Default: include the vector.

## 6. FORGET record payload

```
struct ForgetRecord {
    memory_id: MemoryId,             // 16 bytes
    request_id: RequestId,           // 16 bytes
    mode: u8,                        // 0 = soft, 1 = hard
    reason: u8,                      // 0 = client request, 1 = eviction, ...
}
```

Total payload: 34 bytes. Plus header + footer = 74 bytes per FORGET record.

## 7. LINK record payload

```
struct LinkRecord {
    source: MemoryId,                // 16 bytes
    target: MemoryId,                // 16 bytes
    edge_kind: u8,                   // 1 byte (one of the 8 edge types)
    weight: f32,                     // 4 bytes
    origin: u8,                      // 0 = explicit, 1 = auto-derived
}
```

## 8. UNLINK record payload

```
struct UnlinkRecord {
    source: MemoryId,
    target: MemoryId,
    edge_kind: u8,
    edge_seq: u32,                   // For multi-edges
}
```

## 9. UPDATE_SALIENCE record payload

```
struct UpdateSalienceRecord {
    memory_id: MemoryId,
    new_salience: f32,
    reason: u8,                      // 0 = access, 1 = decay, 2 = explicit
}
```

These records are common (every access boost generates one). They're typically coalesced — one UPDATE_SALIENCE record may cover multiple logical updates within a small time window. The `flags` bit 1 (coalesced) is set when this happens; the payload then carries multiple `(memory_id, new_salience, reason)` tuples.

For typical workloads, salience updates are the most numerous WAL records. Coalescing brings them under control.

## 10. RECLAIM record payload

```
struct ReclaimRecord {
    slot_id: u64,                    // 6 effective bytes
    old_version: u32,
    new_version: u32,
}
```

A RECLAIM record indicates a slot was reused. It's the WAL signal that the old MemoryId becomes invalid.

## 11. CONSOLIDATE record payload

```
struct ConsolidateRecord {
    new_memory_id: MemoryId,         // The new Consolidated memory
    source_memory_ids: Vec<MemoryId>, // The episodic memories consolidated
    text_length: u32,
    text: [u8; text_length],
    vector: [f32; 384],
    embedding_model_fp: [u8; 16],
}
```

Plus the implied edges: `DERIVED_FROM` from the new memory to each source. These edges get their own LINK records, written as part of the same transaction (TXN_BEGIN/TXN_COMMIT bracket).

## 12. UPDATE_KIND record payload

```
struct UpdateKindRecord {
    memory_id: MemoryId,
    new_kind: u8,
}
```

## 13. UPDATE_CONTEXT record payload

```
struct UpdateContextRecord {
    memory_id: MemoryId,
    new_context_id: ContextId,
}
```

## 14. Transaction records

```
struct TxnBeginRecord {
    txn_id: TxnId,                   // 16 bytes
    expected_record_count: u32,      // Hint for recovery; not strict
}

struct TxnCommitRecord {
    txn_id: TxnId,
}

struct TxnAbortRecord {
    txn_id: TxnId,
    reason_code: u32,
}
```

Records within a transaction carry the `txn_id` in their payload (in addition to having `flags` bit 0 set).

Recovery treats transactional records as a unit:
- TXN_BEGIN seen, TXN_COMMIT seen: apply all records in between.
- TXN_BEGIN seen, TXN_ABORT seen: discard all records in between.
- TXN_BEGIN seen, neither commit nor abort (partial transaction at end of WAL): discard.

## 15. CHECKPOINT records

Checkpoint records are detailed in [`09_checkpointing.md`](09_checkpointing.md). Briefly:

```
struct CheckpointBeginRecord {
    checkpoint_id: u64,
    started_at: u64,
}

struct CheckpointEndRecord {
    checkpoint_id: u64,
    durable_lsn: u64,                // All records up to this LSN are reflected in the checkpoint
    arena_capacity: u64,
}
```

## 16. MIGRATE_EMBEDDING record payload

```
struct MigrateEmbeddingRecord {
    memory_id: MemoryId,
    old_fingerprint: [u8; 16],
    new_fingerprint: [u8; 16],
    new_vector: [f32; 384],
}
```

This is what the migration worker writes when it re-embeds a memory.

## 17. Record alignment

Records are not padded to any alignment within a segment. They're packed back-to-back. The segment grows by appending records; the file's tail is the next free byte.

For O_DIRECT writes (see [`06_wal_durability.md`](06_wal_durability.md)), the writes themselves must be aligned to the device's block size (typically 4 KB). The substrate buffers records in an aligned page-sized buffer until full, then writes the buffer.

## 18. CRC32C semantics

The footer's `payload_crc32c` covers:
- The 32-byte record header.
- The variable-length payload.

It does **not** cover itself. The CRC is computed last; it's the receiver's check on the rest.

CRC mismatches during recovery indicate truncation (last record was being written when the crash happened) or corruption (rare). Recovery treats any CRC failure as "truncate here; everything after this point is lost" — which is correct for the truncation case.

## 19. Maximum record size

A record's `payload_length` field is u32 — supports up to 4 GiB payloads. In practice:

- ENCODE records are typically 2-3 KiB (with the included vector).
- CONSOLIDATE records may be 5-10 KiB (multiple source IDs, text, vector).
- Other record types are much smaller.

The substrate enforces a configurable max record size (default: 16 MiB) to prevent pathologically-large records from causing problems. Records larger than the limit are rejected at the request validation layer.

## 20. Record indexing

The WAL is sequential; finding a specific LSN requires reading from the segment that contains it. The starting LSN of each segment is in the segment header, so binary-searching across segments is fast.

Within a segment, records are scanned linearly. There's no per-record index; for SUBSCRIBE clients consuming forward, this is the natural access pattern.

For random-access patterns (recovery only needs sequential), no index is needed.

---

*Continue to [`06_wal_durability.md`](06_wal_durability.md) for the durability mechanism.*
