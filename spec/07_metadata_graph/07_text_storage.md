# 07.07 Text Storage

Memory text lives in a dedicated `texts` table, keyed by MemoryId. This file specifies that table.

## 1. The table

```rust
table: texts
key: MemoryId
value: Vec<u8>  // UTF-8 bytes
```

A simple key-value table. Each memory has one entry.

## 2. Why a separate table

Memory text is variable-length and not always read. Putting it in the main `memories` table would:

- Bloat that table: text varies from a few bytes to ~1 MB.
- Slow random-access on `memories` (more bytes per row).

Separating text means:

- `memories` rows are fixed-size (~150 bytes).
- Reading metadata doesn't pay for reading text.
- Text is read on demand (when the response actually needs it).

## 3. Read patterns

Text is read:

- **In RECALL responses** when the client requests text (an option, not the default).
- **By the consolidation worker** when summarizing source memories.
- **By the migration worker** when re-embedding with a new model.
- **Rarely** for debugging or admin tools.

Most queries don't need text. The default `RECALL` returns memory IDs and metadata, not text. Clients explicitly opt in via a flag.

## 4. The text size

Text size varies by application:

- Short messages: ~50-200 bytes.
- Document chunks: ~500-2000 bytes.
- Long content: up to the model's max length (~3000 chars for 512 tokens).

Substrate enforces a max text size (default 1 MB; configurable). Larger texts are rejected at the wire-validation layer.

## 5. Text encoding

Text is stored as UTF-8 bytes. The wire protocol carries UTF-8; the substrate stores it byte-for-byte.

The substrate validates:
- The bytes parse as valid UTF-8.
- The byte length matches the protocol-declared length.

Invalid UTF-8 is rejected at validation.

## 6. Text deduplication

The substrate supports **opt-in fingerprint deduplication** at ENCODE time. When the caller passes `EncodeRequest.deduplicate = true`, the substrate consults a fingerprint index before allocating a new slot; on a hit, the existing `MemoryId` is returned and no new slot, WAL record, or HNSW node is created.

### 6.1. Scope

Fingerprint dedup is scoped per `(shard, agent_id, context_id)`. The same text encoded by:

- the same agent in the same context → **dedup hit** (returns existing `MemoryId`).
- the same agent in a *different* context → **no hit** (different memory, allocated fresh). This preserves the spec's original observation that the same utterance in different episodic contexts is semantically different.
- a different agent → **no hit**. The fingerprint table is partitioned by `agent_id` for both privacy (one agent's encoded text never matches against another's index) and ownership clarity (each agent owns its own dedup index).

Cross-shard dedup is not supported. The fingerprint table is per-shard, and routing already hashes the agent to a single shard, so all of that agent's memories live in one shard's table.

### 6.2. Hash

The fingerprint is `BLAKE3(canonical_utf8(text))[..32]` — the first 32 bytes of BLAKE3 over the UTF-8 byte representation of the text. Canonicalisation in v1 is a no-op (the bytes go in as-is); future spec revisions may add NFC normalisation if cross-platform consistency becomes a real concern.

### 6.3. Tombstone semantics

Dedup only hits **Active** memories. If the matching memory has been tombstoned (soft FORGET, hard FORGET, or worker-reclaimed), the dedup lookup misses and a fresh memory is allocated. Implementations are free to either:

(a) check the memory's state on every lookup (simpler), or
(b) evict the fingerprint entry on every FORGET / reclamation (faster lookup, more write paths to maintain).

v1 chooses **(b)** — eviction on FORGET / reclamation — because the read path is the hot path. `do_forget` removes the matching `(agent_id, context_id, content_hash)` entry in the same write transaction as the tombstone.

### 6.4. Default

Dedup is **off by default**. Callers that want it must opt in explicitly. The default-off reflects the substrate's primitive: "one ENCODE call → one memory" is the simpler, more predictable model, and avoids silently merging memories whose distinct identity might matter to a downstream cognitive operation.

### 6.5. Storage cost

The fingerprint table adds, per Active memory under dedup, one row of `agent_id(16) + context_id(8) + content_hash(32) + memory_id(16) = 72 bytes`. At 1M Active memories per shard with 100% dedup-on, this is ~72 MiB of additional redb storage — comfortably within the spec's metadata-budget envelope.

### 6.6. Refcount?

Earlier drafts considered a refcount table (so a single stored text could back N memories). v1 rejects that: dedup hit means the *same MemoryId is returned*, not "a new MemoryId backed by shared storage." Two callers asking for dedup get the same `MemoryId`; there's no refcount because there's only ever one row.

### 6.7. Use cases

- Template-based agents that emit the same observation repeatedly.
- Idempotent batch ingestion where the source already has stable content.
- Caching layers that re-encode the same prompt during retries (request-level idempotency handles the explicit retry case; fingerprint dedup handles the case where the same content appears under a different `request_id`).

## 7. Text size limits

The text byte size is configurable:

```
[memory]
max_text_bytes = 1048576    # 1 MB default
```

Practical limits:

- The model's max context (512 tokens ≈ 2000 chars) means content beyond that doesn't influence the vector. So storing > 2000 chars is mostly for reference, not for vector quality.
- Typical agent text is well under 1 KB.

For deployments with longer content, the limit can be raised. Above 1 MB, performance considerations (transaction sizes, network bandwidth) become more relevant.

## 8. Text immutability

Once written, text is immutable. Brain doesn't support "update the text of memory M" — that would invalidate the vector, which depends on the text.

To "update" a memory:
1. Encode the new text as a new memory.
2. Optionally link new and old via a `DERIVED_FROM` or `REFERENCES` edge.
3. Optionally FORGET the old.

This pattern preserves the embedded-vector consistency with the text.

## 9. Hard-forget zeroing

When a memory is hard-forgotten:

1. The slot's vector is zeroed (the arena's bytes for that slot become all zeros).
2. The text in `texts` is overwritten with zeros (same length) before deletion.
3. The metadata is updated.

The zero-then-delete pattern ensures the text isn't recoverable from the file. (An attacker with disk access might still recover from filesystem-level fragments, but the substrate has done its part.)

For paranoid deployments, the substrate can also call `FALLOC_FL_PUNCH_HOLE` on the text region, encouraging the filesystem to release the underlying blocks.

## 10. Text and snapshots

Snapshots include the `texts` table (it's part of the metadata.redb file). Restoring from a snapshot brings back the text.

For deployments that want to retain memories without text (e.g., to honor right-to-be-forgotten requests), the operator can hard-forget specific memories before taking a snapshot.

## 11. Text and consolidation

When the consolidation worker creates a Consolidated memory:

1. Reads the text of source memories (via `texts` table).
2. Generates a summary (via an external LLM call, typically).
3. Encodes the summary as a new memory: writes new vector to arena, new text to `texts`.

The source memories' text is unchanged. The Consolidated memory has its own text — the summary.

## 12. Bulk text retrieval

For workloads needing many memory texts (e.g., bulk export), the substrate doesn't optimize specially. Each lookup is a separate `texts.get(&memory_id)`. With redb's MVCC, a read transaction can iterate efficiently:

```rust
let txn = db.begin_read()?;
let texts = txn.open_table(TEXTS)?;
for memory_id in memory_ids {
    let text = texts.get(&memory_id)?.unwrap();
    process(text);
}
```

For 1000 lookups on a warm cache: ~5 ms total.

## 13. Text storage size

For 1M memories with avg 500 byte text each: ~500 MB.

For 10M memories: ~5 GB.

Text typically dominates the metadata store's disk footprint. Operators planning capacity should size for text at the expected average size.

## 14. The "store-text=no" mode

For deployments that don't want text stored in Brain (e.g., text is in another system; Brain just holds vectors):

```
[memory]
store_text = false
```

In this mode:
- The wire protocol's ENCODE still requires text (so the substrate can embed it).
- After embedding, the text is discarded — not written to `texts`.
- RECALL responses can't return text.
- Migration can't re-embed (it would need the text); migration is unsupported in this mode.

The mode is a niche optimization. Most deployments store text.

## 15. Text vs metadata coupling

The text and the rest of the memory's metadata are written in the same transaction:

```rust
let mut wtxn = db.begin_write()?;
{
    let mut memories = wtxn.open_table(MEMORIES)?;
    let mut texts = wtxn.open_table(TEXTS)?;
    memories.insert(&memory_id, &metadata)?;
    texts.insert(&memory_id, &text_bytes)?;
}
wtxn.commit()?;
```

Atomic. After commit, both the metadata row and the text are durable.

If a crash happens before commit, neither is durable. Recovery from WAL replays the ENCODE record, which contains both.

## 16. Text and the wire protocol

The wire protocol's ENCODE carries text inline. RECALL with `include_text = true` returns text inline. There's no chunked text retrieval; for very large texts (~MB), the response carries them in one frame.

The frame size limit is 16 MiB ([03.03 Frame Header](../03_wire_protocol/03_frame_header.md)). Text up to ~16 MB minus protocol overhead is fine. The default max_text_bytes (1 MB) is well below this.

---

*Continue to [`08_transactions.md`](08_transactions.md) for transactions.*
