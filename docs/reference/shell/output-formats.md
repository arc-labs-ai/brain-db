# `brain` shell — output formats

The `brain` binary emits one of two formats per invocation:

- **`table`** — human-readable; defaults when stdout is a TTY.
- **`json`** — one line per command; defaults when stdout is piped.

Override with `--output <FORMAT>` (one-shot) or `\set output <FORMAT>`
(REPL session) or `brain config set output <FORMAT>` (persistent).

This page documents the **exact shape** of each. For per-verb
flag references, see [`commands.md`](commands.md).

---

## JSON envelope

Every JSON line wraps the verb's response body in a stable envelope:

```json
{ "op": "<verb>", "result": <body> }
```

The envelope is line-delimited (one `\n` between lines). Trivially
`jq`-pipeable:

```bash
brain --agent demo --output json recall "auth" --top-k 5 \
  | jq '.result[] | {id: .memory_id, score: .similarity_score}'
```

For full scripting recipes see [`../../guides/shell/scripting-with-json.md`](../../guides/shell/scripting-with-json.md).

---

## Memory ids

Two representations:

| Form | Where | Example |
|---|---|---|
| **Short** | Table output | `s2/m1/v1` (shard / slot / version) |
| **Long** | JSON output + CLI args | `0x00020000000000010000000100000000` |

The short form is purely a display optimisation; the long hex is
the canonical id you pass back to `forget`, `link`, etc. **Both
forms are accepted by every command that takes a `<MemoryId>`** —
including pasting from a recall table directly.

---

## `encode`

### Table

```
ok  s2/m1/v1  lsn=1
    agent=00000000… · ctx=7 · episodic · sal=0.700 · fp=00000000…
```

Fields are surfaced on demand:

- `dedup=…` appears only when `--deduplicate` was passed (`hit` or `miss`).
- `edges_out=N` appears only when N>0.

### JSON

```json
{ "op": "encode",
  "result": {
    "memory_id": "0x00020000000000010000000100000000",
    "lsn": 1,
    "dedup": "off",
    "was_deduplicated": false,
    "salience": 0.7,
    "auto_edges_added": 0,
    "agent_id": "0x000000000000000000000000000000aa",
    "context_id": 7,
    "kind": "Episodic",
    "created_at_unix_nanos": 1779153941479431250,
    "edges_out_count": 0,
    "embedding_model_fp": "0xabcdef000000000000000000000000ab"
  } }
```

| Field | Notes |
|---|---|
| `memory_id` | Canonical 32-hex form. |
| `lsn` | WAL position. Use for `subscribe --start-lsn lsn+1`. |
| `dedup` | `"off"` / `"miss"` / `"hit"` — friendly form. |
| `was_deduplicated` | Raw boolean for scripts that already parse it. |
| `salience` | Final salience (may differ from `--salience` hint). |
| `agent_id` | Authenticated agent for the request. |
| `embedding_model_fp` | Model fingerprint stamped on the row — used by the migration worker. |

---

## `recall`

### Table — two-line per result

```
#1  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0164
    Alice merged the auth-rewrite branch

#2  s2/m2/v1  semantic  ctx=7  sal=0.900  score=0.0161
    auth tokens now use BLAKE3 instead of SHA-1

2 results
```

The footer changes when scores cluster:

```
2 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

Optional inline indicators:

| Indicator | Meaning |
|---|---|
| `episodic†` (`†` suffix) | Consolidated row — produced by background consolidation worker, not direct ENCODE. |
| `sal=0.500↓0.700` | Decayed since write — current ↓ initial. |
| `sal=0.900↑0.700` | Boosted since write (rare). |
| `acc=N` (shown only N>0) | RECALL hit count for this row. |
| `edges=Nin/Nout` (shown only N>0) | Connectivity. |

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
| `confidence` | Equals `similarity_score` on the substrate path; equals `fused_score` on hybrid. |
| `salience` vs `salience_initial` | Decay signal. Equal at write time; diverges as the decay worker runs. |
| `access_count` | Hotness; bumped every time RECALL surfaces this row. |
| `lsn` | The WAL LSN this memory was encoded at — chain with `subscribe --start-lsn lsn+1`. |
| `flags` | Bit-OR of: `ACTIVE=0x1`, `HARD_FORGOTTEN=0x2`, `CONSOLIDATED=0x4`, `DEDUP_BACKREF=0x8`. |
| `consolidated_at_unix_nanos` | `Some(t)` for consolidation-worker rows. |
| `contributing_retrievers` | (Hybrid only) list of retriever names that surfaced this row. |
| `fused_score` | (Hybrid only) post-RRF rank score. |
| `text` | Empty unless `--include-text` was passed. |

---

## `forget`

### Table

```
ok  s2/m1/v1  outcome=Tombstoned  edges_removed=0
```

| `outcome=` | Meaning |
|---|---|
| `Tombstoned` | Memory was Active; tombstoned. |
| `AlreadyTombstoned` | Idempotent no-op. |
| `MemoryNotFound` | No such memory; treated as success per spec §08/06 §10. |

### JSON

```json
{ "op": "forget",
  "result": {
    "memory_id": "0x00020000000000010000000100000000",
    "outcome": "Tombstoned",
    "was_already_forgotten": false,
    "edges_removed": 0
  } }
```

---

## `link` / `unlink`

### Table

```
ok  s2/m1/v1 --[Caused]--> s2/m2/v1  weight=0.7000  already_existed=false
```

### JSON

```json
{ "op": "link",
  "result": {
    "source": "0x...",
    "target": "0x...",
    "kind": "caused",
    "weight": 0.7,
    "created_at_unix_nanos": 1779153941479431250,
    "already_existed": false
  } }
```

---

## `subscribe`

### Streaming table (one line per event)

```
     1  Encoded     0x00020000000000010000000100000000  ctx=7    Episodic     Alice merged the auth-rewrite branch
     2  Forgotten   0x00020000000000010000000100000000  ctx=7    Episodic
```

After Ctrl-C / EOS, a footer line goes to stderr:

```
(unsubscribed; 2 events)
```

### Streaming JSON

```json
{ "op": "subscribe_event",
  "result": {
    "lsn": 1,
    "event_type": "Encoded",
    "memory_id": "0x...",
    "context_id": 7,
    "kind": "Episodic",
    "salience": 0.7,
    "timestamp_unix_nanos": 1779153941479431250,
    "text": "Alice merged the auth-rewrite branch"
  } }
```

Each event = one line; per-event `stdout.flush()` so `brain
subscribe | jq …` shows events as they arrive.

### Batch (`--collect N`)

```json
{ "op": "subscribe",
  "result": [ { …event 1… }, { …event 2… }, … ] }
```

---

## `plan` / `reason`

### Table

```
#1  s2/m1/v1  Causal       conf=0.8  est_to_goal=0.42
    Alice merged the auth-rewrite branch

#2  s2/m2/v1  Similarity   conf=0.6  est_to_goal=0.15
    auth tokens now use BLAKE3 instead of SHA-1

3 steps  ·  status=GoalReached
```

`status=` footer surfaces non-`GoalReached` outcomes loudly so you
don't confuse partial results for complete ones.

### JSON

Standard `Vec<PlanStep>` / `Vec<InferenceStep>` shapes — see
[`../cognitive-operations/`](../cognitive-operations/) for the
wire structs.

---

## `txn`

| `txn begin` | `txn commit` | `txn abort` |
|---|---|---|
| `ok  txn_id=0x019e3ac4b1c67f73…` | `ok  committed=true  ops=5` | `ok  aborted=true` |

JSON wraps in the standard envelope.

---

## See also

- [`commands.md`](commands.md) — per-verb flags
- [`repl-meta.md`](repl-meta.md) — `\agent`, `\config`, `\set`
- [`../../guides/shell/scripting-with-json.md`](../../guides/shell/scripting-with-json.md) — jq pipelines
- [`../wire-protocol/`](../wire-protocol/) — frame format
