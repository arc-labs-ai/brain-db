# REASON

Find supporting and contradicting evidence for a query. Brain
finds a base set via RECALL, then traverses `SUPPORTS` and
`CONTRADICTS` edges.

**Opcode:** `ReasonReq = 0x0023` / `ReasonResp = 0x00A3` (streaming).
**Spec:** §09/05. **Source:** `crates/brain-ops/src/ops/reason.rs`.

## Request fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `query_text` | `String` | yes | Claim or question. Embedded; not parsed as logic. |
| `agent_id` | `AgentId` | yes | Scopes to the agent's memories. |
| `max_supporting` | `u32` | no | Default 5, max 50. |
| `max_contradicting` | `u32` | no | Default 5, max 50. |
| `include_text` | `bool` | no | Default `true` (REASON is meant to be interpretable). |
| `confidence_min` | `f32` | no | Filter evidence below this. |

## Response fields

| Field | Type | Notes |
|---|---|---|
| `supporting` | `Vec<EvidenceItem>` | Sorted by individual confidence, descending. |
| `contradicting` | `Vec<EvidenceItem>` | Same. |
| `confidence` | `f32` | Aggregate, in `[-1.0, +1.0]`. See below. |
| `base_memories` | `Vec<MemoryId>` | Initial anchors found via RECALL. |

### `EvidenceItem`

| Field | Type | Notes |
|---|---|---|
| `memory_id` | `MemoryId` | |
| `text` | `Option<String>` | Present when `include_text` was true. |
| `score` | `f32` | Per-item confidence. |
| `edge_path` | `Vec<EdgeKind>` | How this evidence connects back to a base memory. Empty for direct-similarity matches. |
| `distance` | `usize` | Graph hops from the nearest base memory. |

### Supporting vs contradicting

- **Supporting** = directly similar to the query (high cosine) **OR** reached from a base via `SUPPORTS` / `DERIVED_FROM` edges. Edge-traversed evidence is treated as stronger than pure-similarity evidence.
- **Contradicting** = reached from a base via **explicit `CONTRADICTS` edges**. v1 does **not** infer contradiction from vector geometry (similar-topic-but-opposite is unreliable). For LLM-based contradiction detection see spec §09/05 §7 (deferred).

### Aggregate confidence

```
confidence = (support_strength − contradict_strength)
           / (support_strength + contradict_strength)
```

Range:
- `+1.0` → all supporting, no contradicting.
- `0.0` → balanced.
- `-1.0` → all contradicting.

Treat this as a heuristic, not as truth. The number is most useful
for ranking *between* REASON calls, not as a hard threshold.

## Side effects

None. REASON is read-only.

## Errors

| Code | When |
|---|---|
| `EmbeddingError` | Embedder failed on the query. |
| `IndexError` | Base-set RECALL failed. |
| `MetadataError` | Edge traversal failed. |

Empty `supporting` and `contradicting` is not an error.

## Idempotency

N/A — read-only.

## Performance target

Spec §16/02 §6:

| Workload | p50 | p99 |
|---|---|---|
| Typical query, ~5 supporting + ~5 contradicting | 30 ms | 70 ms |

Faster than PLAN — narrower edge kinds, shallower traversal.

## Substrate vs knowledge

REASON works on the substrate edge graph. The knowledge layer is
not consulted. The same opcode works in both modes.

## See also

- [`plan.md`](plan.md) — same machinery, different question shape.
- [`../../architecture/05-redb-metadata.md`](../../architecture/05-redb-metadata.md) — edge storage.

**Spec:** §09/05.
