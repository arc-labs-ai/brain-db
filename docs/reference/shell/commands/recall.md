# `brain recall`

Vector-similarity search. Embeds the cue text, performs a top-K HNSW
search in the active context's index, returns ranked
`MemoryResult`s. Inherits the session's active txn unless
`--txn` overrides.

```
brain recall <QUERY>
        [--top-k <N>]
        [--confidence <FLOAT>]
        [--filter-context <N>]...
        [--filter-kind episodic|semantic|consolidated]...
        [--include-text]
        [--include-graph]
        [--txn <HEX>]
```

---

## Flags

### `--top-k <N>`

Result cap. Default `10`. The server enforces an upper bound (per spec
§09/03); requests above the cap are clamped, not rejected.

### `--confidence <FLOAT>`

Similarity threshold in `[0.0, 1.0]`. Default `0.0` (no filter).

The substrate path compares the threshold to `similarity_score`. The
hybrid (knowledge-layer) path compares it to the RRF-fused score. See
[`../output-formats.md#recall`](../output-formats.md#recall) for the
field semantics.

### `--filter-context <N>` (repeatable)

Keep only results from these context ids. Repeat the flag once
per id; up to 16 in a single query. No filter → search the
agent's union of contexts (rarely what you want — see below).

```bash
brain recall "build status" --filter-context 4 --filter-context 7
```

Contexts are the substrate's coarse partition. Most recalls
want exactly one context filter so high-salience memories from
unrelated projects don't crowd out the relevant ones. For the
full story (why contexts exist, how they get created, when to
use the default vs explicit), see
[`docs/concepts/26-contexts.md`](../../../concepts/26-contexts.md).

### `--filter-kind <KIND>` (repeatable)

Keep only memories of these kinds. Repeat once per kind:

```bash
brain recall "auth" --filter-kind episodic --filter-kind semantic
```

### `--include-text`

Populate the `text` column. Off by default — RECALL returns ids and
scores only, which avoids a batched read against the metadata `texts`
table. Pass when you want to read the memory bodies inline; omit when
chaining into another `recall` or `link` call.

### `--include-graph`

**Knowledge-layer only.** Request per-hit enrichment: linked entities,
top statements, summarised relations. Currently scaffolded only — the
wire RecallResp doesn't yet carry these fields, so the renderer prints
empty enrichment sections. See [Gated features](#gated-features).

### `--txn <HEX>`

Run inside an open transaction. Reads see the txn's pending writes, so
you can encode + recall as one atomic unit. In the REPL, an active
`txn begin` auto-attaches.

---

## Output

### Table — two-line per result + footer

```
#1  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0164
    Alice merged the auth-rewrite branch

#2  s2/m2/v1  semantic  ctx=7  sal=0.900  score=0.0161
    auth tokens now use BLAKE3 instead of SHA-1

2 results
```

| Field | Meaning |
|---|---|
| `#N` | Rank (1-indexed). |
| `s2/m1/v1` | Short `MemoryId`. |
| `episodic†` | Kind. `†` suffix = consolidated row (summary produced by the consolidation worker). |
| `ctx=…` | Origin context id. |
| `sal=0.700` | Current salience. |
| `sal=0.500↓0.700` | Decayed since write — current ↓ initial. `↑` for boost. |
| `score=…` | Similarity score (substrate) or fused RRF score (hybrid). |
| `acc=N` | Only shown when N>0 — RECALL hit count for this row. |
| `edges=Nin/Nout` | Only shown when either side is >0 — denormalised connectivity. |

**Cluster warning.** If every top-K score is within `Δ<0.001` of the
highest, the footer reads
`N results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful`.

This signals one of:

- The embedder isn't loaded (test mode / `NopDispatcher`).
- The query genuinely doesn't discriminate among the results.
- All results are near-duplicates of the query.

**Don't trust the order** when you see this — treat results as
equal-scored.

**Text suppression.** Without `--include-text`, the body line reads
`(text not fetched — re-run with --include-text)`.

### JSON

```json
{ "op": "recall",
  "result": [
    {
      "memory_id": "0x00020000000000010000000100000000",
      "similarity_score": 0.0164,
      "confidence": 0.0164,
      "salience": 0.7,
      "salience_initial": 0.7,
      "access_count": 0,
      "lsn": 1,
      "flags": 1,
      "kind": "episodic",
      "context_id": 7,
      "created_at_unix_nanos": 1779153941479431250,
      "last_accessed_at_unix_nanos": 1779153941479431250,
      "consolidated_at_unix_nanos": null,
      "edges_out_count": 0,
      "edges_in_count": 0,
      "fused_score": 0.0,
      "text": "Alice merged the auth-rewrite branch"
    }
  ] }
```

| Field | Notes |
|---|---|
| `similarity_score` | Per-retriever score; what the table shows. |
| `confidence` | Equals `similarity_score` on substrate; equals `fused_score` on hybrid. |
| `salience` vs `salience_initial` | Decay signal. Equal at write; diverges over time. |
| `access_count` | Hotness; bumped every time RECALL surfaces this row. |
| `lsn` | The WAL LSN this row was encoded at — chain with `subscribe --start-lsn lsn+1`. |
| `flags` | Bit-OR of `ACTIVE=0x1` / `HARD_FORGOTTEN=0x2` / `CONSOLIDATED=0x4` / `DEDUP_BACKREF=0x8`. |
| `text` | Empty unless `--include-text` was passed. |
| `contributing_retrievers` / `fused_score` | Hybrid only. |

---

## Examples

```bash
# Default: top-10, no text, no filters
brain recall "auth rewrite"

# Trim noise: highest 3 with text bodies
brain recall "auth rewrite" --top-k 3 --include-text

# Restrict to a specific context + kind
brain recall "outage" --filter-context 12 --filter-kind episodic

# JSON for jq
brain recall "auth" --top-k 5 -o json \
  | jq '.result[] | {id: .memory_id, score: .similarity_score}'

# JSONPath to pluck just the top id
brain recall "auth" --top-k 1 -o "jsonpath={.result[0].memory_id}"

# Follow downstream events on the best match
ID=$(brain recall "deploy" --top-k 1 -o json | jq -r '.result[0].memory_id')
LSN=$(brain recall "deploy" --top-k 1 -o json | jq -r '.result[0].lsn')
brain subscribe --start-lsn $((LSN+1))
```

### Inside a transaction

```bash
brain> txn begin
brain*> encode "the deploy fixed the bug" --context 4
brain*> recall "deploy bug fix" --top-k 5 --include-text
# RECALL sees the just-encoded memory in the txn's read view
brain*> txn commit <id>
```

---

## Errors

| Code | Trigger | Fix |
|---|---|---|
| `InvalidArgument` | Bad `--filter-kind` value. Threshold out of `[0,1]`. | Re-issue with valid input. |
| `ShardUnavailable` | Target shard down. | Wait + retry. |
| `Overloaded` | Server shedding load. | Back off. |

Full catalogue: [`../errors.md`](../errors.md).

---

## Gated features

| Flag | Blocked on |
|---|---|
| `--include-graph` | Wire `RecallResp` growing `entities` / `statements` / `relations` side-channel fields. Renderer surface already in place; lights up automatically when the wire field arrives. |

---

## See also

- [`encode.md`](encode.md) — the write side
- [`subscribe.md`](subscribe.md) — `lsn` chaining
- [`entity.md`](entity.md) / [`statement.md`](statement.md) — drill into the knowledge layer when a hit is enriched
- [`../output-formats.md`](../output-formats.md) — field reference
- [`../../../guides/shell/scripting-with-json.md`](../../../guides/shell/scripting-with-json.md) — jq pipelines
- Spec: [`spec/09_cognitive_operations/03_recall.md`](../../../../spec/09_cognitive_operations/03_recall.md)
