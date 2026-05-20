# RECALL

Retrieve memories by similarity. Brain embeds the cue, searches
HNSW, applies filters, and returns up to `k` ranked results.

**Opcode:** `RecallReq = 0x0021` / `RecallResp = 0x00A1` (streaming).
**Spec:** §09/03. **Source:** `crates/brain-ops/src/ops/recall.rs`.

## Request fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `cue_text` | `String` | yes | Query text. Any length. Embedded with the **same model** as stored memories. |
| `agent_id` | `AgentId` | yes | Restricts results to this agent's memories. |
| `k` | `u32` | no | Top-K. Default 10. Max 1 000 (`TopKOutOfRange` beyond). |
| `filter` | `RecallFilter` | no | See below. |
| `include_text` | `bool` | no | Include memory text in results. Default `false`. ~50 µs/result. |
| `include_metadata` | `bool` | no | Include K/V metadata. Default `false`. |
| `consistency` | `Consistency` | no | `Eventual` (default) or `ReadAfterWrite`. The latter waits for the HNSW maintenance tick to apply pending inserts (adds ~50 ms). |
| `confidence_min` | `f32` | no | Drop results with score below this. |

### `RecallFilter`

| Field | Type | Notes |
|---|---|---|
| `kind` | `Option<MemoryKind>` | Filter by `Episodic` / `Semantic` / `Consolidated`. |
| `contexts` | `Option<Vec<ContextRef>>` | Limit to specific contexts. |
| `min_salience` | `Option<f32>` | Drop low-salience memories. |
| `max_age` | `Option<Duration>` | Drop memories older than this. |
| `fingerprint_match` | `bool` | Default `true` — only memories embedded with the current model. |
| `tags` | `Option<Vec<String>>` | AND-combined custom metadata tags. |
| `custom` | `Vec<FilterRule>` | Arbitrary metadata filters (key + op + value). |

## Response fields

Streaming — multiple frames share one `stream_id`, terminated by an `EOS` frame.

| Field | Type | Notes |
|---|---|---|
| `results` | `Vec<RecallResult>` | Sorted by `score`, descending. |
| `partial` | `bool` | `true` if some shards failed (multi-shard deployments). |
| `total_candidates` | `usize` | Pre-filter candidate count. Diagnostic. |

### `RecallResult`

| Field | Type | Notes |
|---|---|---|
| `memory_id` | `MemoryId` | Stable handle. |
| `score` | `f32` | `1 - cosine_distance(cue, memory)`. Range `[-1, 1]`; 1 = identical. |
| `text` | `Option<String>` | Present when `include_text` was true. |
| `metadata` | `Option<MemoryMetadata>` | Present when `include_metadata` was true. |
| `context_id` | `ContextId` | Memory's context. |
| `kind` | `MemoryKind` | Episodic / Semantic / Consolidated. |
| `contributing_retrievers` | `Vec<RetrieverId>` *(knowledge mode only)* | Which retrievers (semantic, lexical, graph) contributed. Empty in substrate mode. |
| `fused_score` | `f32` *(knowledge mode only)* | RRF-fused rank. `0.0` in substrate mode. |

### Score interpretation

| Score range | Reading |
|---|---|
| `> 0.9` | Very similar — near-duplicate or paraphrase. |
| `0.7 – 0.9` | Related, same general topic. |
| `0.5 – 0.7` | Loosely related. |
| `< 0.5` | Weak — tune `confidence_min` to drop. |

These are heuristics; agents should tune `confidence_min` to
their workload.

## Side effects

None. RECALL is read-only.

## Errors

| Code | When |
|---|---|
| `TopKOutOfRange` | `k > 1000` or `k == 0`. |
| `BadStrategyHint` | (knowledge mode) Unknown strategy hint in filter. |
| `EmbeddingError` | Embedder failed on the cue. |
| `IndexError` | HNSW query failed. |
| `MetadataError` | redb read failed. |

Empty results are normal, not errors.

## Idempotency

N/A — RECALL is read-only. The same cue may return different
results over time as the index changes.

## Performance target

Spec §16/02 §4:

| Workload | p50 | p99 |
|---|---|---|
| Single shard, K=10 | 10 ms | 25 ms |
| Single shard, K=100 | 15 ms | 35 ms |
| Single shard, K=1000 | 30 ms | 80 ms |
| Cross-shard (2–3 shards), K=10 | 15 ms | 30–50 ms |

Throughput: ~5–20 K RECALLs / sec / shard.

## Substrate vs knowledge

| Mode | Behaviour |
|---|---|
| No schema declared | Pure HNSW search; response carries `contributing_retrievers = []` and `fused_score = 0.0`. |
| Schema declared | Routed through the hybrid query engine (semantic + lexical + graph retrievers, RRF-fused). Same response shape; `contributing_retrievers` is populated. Default top-level retrievers can be tuned via filter hints. |

## See also

- [`encode.md`](encode.md) — the write side.
- [`../schema-dsl/`](../schema-dsl/) — how to enable the hybrid path.
- [`../../architecture/11-hybrid-retrieval-rrf.md`](../../architecture/11-hybrid-retrieval-rrf.md) — RRF fusion mechanics.

**Spec:** §09/03.
