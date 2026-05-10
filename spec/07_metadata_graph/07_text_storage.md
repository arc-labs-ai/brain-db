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

## 6. Text deduplication?

Could the substrate deduplicate identical texts (multiple memories with the same content)?

In v1, no:
- Per-memory storage means a single point of truth per memory.
- Deduplication would require a refcount table, complicating reclaim.
- Text deduplication is rare in practice (different memories tend to have different text, even if similar).

If a deployment has highly repetitive text (e.g., template-based agent outputs), an external dedup layer could be added before encoding.

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
