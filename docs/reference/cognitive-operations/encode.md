# ENCODE

Store a memory. Brain embeds the text, allocates an arena slot,
writes the WAL record (fsynced before ack), updates metadata and
indexes, and returns a stable `MemoryId`.

**Opcode:** `EncodeReq = 0x0020` / `EncodeResp = 0x00A0`.
**Spec:** §09/02. **Source:** `crates/brain-ops/src/ops/encode.rs`.

## Request fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `text` | `String` | yes | 1 B – ~1 MB. Embedded for the vector; stored verbatim. |
| `agent_id` | `AgentId` | yes | Owning agent; auth-checked. |
| `context` | `ContextRef` | no | Name or `ContextId`. Auto-created if missing. Defaults to agent's default context. |
| `kind` | `MemoryKind` | no | `Episodic` (default) or `Semantic`. `Consolidated` is worker-only — clients setting it get `BadMemoryKind`. |
| `metadata` | `Map<String, String>` | no | Agent-defined K/V. Stored verbatim, not indexed. Few-KB cap. |
| `edges` | `Vec<EdgeSpec>` | no | Up to 64 edges. Each: `target: MemoryId`, `kind: EdgeKind`, `weight: f32` (default 1.0). |
| `salience` | `f32` | no | `[0.0, 1.0]`. Default 0.5. Affects decay rate and recall weighting. |
| `deduplicate` | `bool` | no | If true, server may dedupe by fingerprint (same text + agent + context → returns existing id). |
| `request_id` | `RequestId` | yes | UUIDv7-shaped. Idempotency key. |

## Response fields

| Field | Type | Notes |
|---|---|---|
| `memory_id` | `MemoryId` | Slot + version. Stable handle. |
| `edge_results` | `Vec<EdgeResult>` | Per-edge success or per-edge error. ENCODE itself succeeds even if some edges fail. |
| `persisted_at` | `u64` | Substrate's monotonic timestamp at WAL fsync. |
| `fingerprint` | `ModelFingerprint` | Embedding-model id + version. |

## Side effects

1. **WAL** — one record appended; fsynced before ack.
2. **Arena** — one slot allocated; vector written; CRC stamped.
3. **redb** — rows added in `memories`, `texts`, and (if applicable) `edges_out` + `edges_in`.
4. **HNSW** — node inserted; the search index sees it after the next maintenance tick (typically < 50 ms).
5. **Knowledge layer** — if a schema is declared, the extractor pipeline runs **best-effort after WAL commit**. Failures are logged, never returned to the client.

## Errors

| Code | When |
|---|---|
| `TextEmpty` | `text` is zero bytes. |
| `TextTooLarge` | `text` exceeded ~1 MB. |
| `BadContextId` | `context` not valid for this agent. |
| `BadMemoryKind` | `kind` was `Consolidated`. |
| `MemoryNotFound` *(in `edge_results[i]`)* | An edge target didn't exist; that edge is skipped, encode still succeeds. |
| `BadEdgeKind` | An edge had an unknown `kind`. |
| `IdempotencyConflict` | Same `request_id` reused with different params. |
| `OutOfSlots` | Arena full; the slot-reclamation worker hasn't freed any. Retry. |
| `EmbeddingError` | Embedder failed (model not loaded / GPU OOM). |
| `StorageError` | WAL or arena write failed. Server emits structured telemetry. |

## Idempotency

Re-sending with the same `request_id` returns the original
response (no duplicate memory, no extra WAL record) for 24 h.
Reuse with different params → `IdempotencyConflict`.

## Performance target

Spec §16/02 §3:

| Percentile | Target |
|---|---|
| p50 | 5–10 ms (warm embedder cache: 2–3 ms) |
| p99 | 25 ms |
| Throughput | ~10 K encodes / sec / shard (batched embedder) |

## Substrate vs knowledge

| Mode | Behaviour |
|---|---|
| No schema declared | Pure substrate: vector + WAL + HNSW only. |
| Schema declared | Substrate writes complete first; then pattern → classifier → LLM extractor pipeline runs in the background and writes any typed entities / statements / relations. ENCODE response is unchanged. |

## See also

- [`recall.md`](recall.md) — the read side.
- [`../wire-protocol/opcodes.md`](../wire-protocol/opcodes.md) — opcode list.
- [`../../architecture/03-arena-and-wal.md`](../../architecture/03-arena-and-wal.md) — what happens under the hood.

**Spec:** §09/02.
