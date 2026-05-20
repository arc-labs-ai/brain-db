# `brain encode`

Write a memory: text → embedding → arena slot → WAL → durable. Returns
the persistent `memory_id` plus the WAL position (`lsn`) so you can
chain `subscribe --start-lsn $((lsn+1))` to follow downstream events.

```
brain encode [TEXT]
        [--context <N>]
        [--kind episodic|semantic|consolidated]
        [--salience <FLOAT>]
        [--allow-duplicate]
        [--edge <KIND>:<TARGET_ID>]...
        [--request-id <UUID>]
        [--from-file <PATH>]
        [--from-stdin]
        [--vector <CSV>]
        [--wait-for-extraction]
        [--txn <HEX>]
```

Inherits the session's **sticky context** (`\set context N`) and
**active txn** (`txn begin`) unless the corresponding flag overrides.

---

## Sources for the memory text

Exactly one source must be supplied — `clap` enforces mutual
exclusivity at parse time:

| Source | When to use |
|---|---|
| Positional `TEXT` | Inline string. The default. |
| `--from-file <path>` | Read text from a file. `.jsonl` files are planned to batch one encode per line inside an auto-opened txn (not yet wired — see [Gated features](#gated-features)). |
| `--from-stdin` | Shorthand for `--from-file -`. Useful in pipelines (`cat note.txt \| brain encode --from-stdin`). |
| `--vector <CSV>` | Skip the embedder, supply the float vector directly. Comma-separated floats; length must equal the deployment's embedding dim. **Not yet wired** — see [Gated features](#gated-features). |

```bash
# inline
brain encode "Alice merged the auth-rewrite branch"

# from a file
brain encode --from-file ./design-note.txt --context 4

# from stdin
git log -1 --format=%B | brain encode --from-stdin --context 4 --kind semantic

# explicit vector (gated)
brain encode --vector "0.01,-0.04,...,0.07"
```

---

## Flags

### `--context <N>`

Context id (`u64`). Defaults to `0` (the **default context** —
always present, can't be deleted), or to the session's sticky
context set via `\set context N` if you're in the REPL.

Contexts are the substrate's coarse partition of an agent's
memories. Same agent, different `--context` values → separate
memory pools that don't mix on recall, don't dedup across, and
don't share salience ranking. The wire field is a `u64`
allocated lazily on first reference — pass any positive
integer and the substrate creates the context if it doesn't
exist yet. No "create context first" step needed.

```bash
# Bare — lands in context 0 (default).
brain encode "casual note"

# Explicit context. Created lazily.
brain encode "atlas project standup" --context 7

# In the REPL with a sticky context, the flag is the override.
brain> \set context 7
brain[ctx=7]> encode "..."             # lands in 7
brain[ctx=7]> encode "..." --context 9 # lands in 9 (override)
```

**Same text, different contexts → different memories.** Dedup is
keyed on `(agent_id, context_id, content_hash)`, so a memory
encoded into context 7 is a different memory from the same text
in context 12, and the substrate won't merge them:

```bash
brain encode "the build broke" --context 7    # m1
brain encode "the build broke" --context 12   # m2 (different memory)
brain encode "the build broke" --context 7    # dedup hit → returns m1
```

**For the full story** — when to use `0` vs explicit contexts,
the lazy-creation model, anti-patterns ("contexts as tags",
"contexts as access control"), best practices, and how contexts
interact with edges, salience, and the knowledge layer — see
[`docs/concepts/26-contexts.md`](../../../concepts/26-contexts.md).

### `--kind episodic|semantic|consolidated`

Memory kind. Defaults to `episodic` server-side.

| Kind | Meaning |
|---|---|
| `episodic` | A specific event ("Alice merged at 14:02"). |
| `semantic` | A general fact ("auth tokens use BLAKE3"). |
| `consolidated` | Produced by the consolidation worker. **Not normally chosen by clients** — pass at your own risk. |

### `--salience <FLOAT>`

Hint in `[0.0, 1.0]`. Default `0.5`. The server may adjust it (e.g.
when the consolidation worker decides this memory is a hot consolidation
target). The salience surfaced in the response is the final value, not
the hint.

### `--allow-duplicate`

Deduplication is **on by default**. Encoding the same text twice in the
same `(agent_id, context_id)` returns the existing memory rather than
allocating a new slot.

Pass `--allow-duplicate` for episodic memory where the same content
really is a second distinct event:

```bash
brain encode "the build broke" --context 12         # Monday
brain encode "the build broke" --context 12         # Tuesday — dedup hit; same id
brain encode "the build broke" --context 12 --allow-duplicate  # NEW memory
```

The response surfaces this in `dedup=`:

| `dedup=` | Meaning |
|---|---|
| `off` | `--allow-duplicate` was passed; no fingerprint check ran. |
| `miss`| Dedup was on; no existing memory matched; fresh slot allocated. |
| `hit` | Dedup was on; existing memory matched; returned its id. |

**Dedup scope.** Per `(shard, agent_id, context_id)`. The same text
encoded by a different agent or under a different `--context` is always
a miss. Tombstoned memories don't count — FORGET evicts the fingerprint
in the same write transaction as the tombstone.

### `--edge <KIND>:<TARGET_ID>` (repeatable)

Attach an outgoing edge at create time, so you don't need a follow-up
`link` call. The edge weight is fixed at `1.0`; vary weights via
`brain link` after the fact.

```bash
brain encode "auth rewrite passed CI" \
  --edge derived_from:s2/m1/v1 \
  --edge references:s2/m17/v1
```

Accepted kinds (with hyphen/underscore variants):

`similar_to`, `derived_from`, `references`, `co_occurs` (mapped to
`similar_to`), `caused`, `followed_by`, `contradicts`, `supports`,
`part_of`.

Target ids accept any of the three [`MemoryId` input forms](../output-formats.md#memory-ids):
short (`s2/m17/v1`), long hex (`0x…`), decimal `u128`.

### `--request-id <UUID>`

Explicit idempotency key. Omitted → the SDK mints a fresh UUIDv7.

Passing the **same** `request_id` on a retry short-circuits to the
cached response (24-hour TTL per spec §07/06). Passing the **same**
request_id with **different** params returns `Conflict` — the
idempotency cache caught the divergence.

```bash
# Retry-safe encode loop
RID=$(uuidgen)
until brain encode "snapshot $(date -u +%FT%T)" --request-id "$RID"; do
  sleep 1
done
```

### `--wait-for-extraction`

After the encode returns, open a subscribe stream at `lsn+1` and block
until the knowledge-layer extractor emits `ExtractionCompleted` (or
`ExtractionFailed`) for this memory. Honours the global `--timeout`;
a 60-second hard cap prevents the shell from pinning on a server that
never publishes the event.

Only useful when the knowledge layer is active (schema declared). On a
substrate-only deployment this flag will time out silently.

```bash
brain encode "Alice mentions Bob and Carol" --wait-for-extraction
# Returns only after entities Alice/Bob/Carol are persisted in the KG.
```

### `--txn <HEX>`

Attach to a server-side transaction. 32-hex-char id (`0x` prefix
optional). In the REPL, an active `txn begin` auto-attaches without
needing this flag.

---

## Output

### Table — card-framed

Default-mode encode renders as a card framed by `─` rules: a status
heading (badge on the left, routing cluster on the right), a content
echo, a `type / salience / context` row, then a `next` footer hint
pointing at the LSN to subscribe from.

```
──────────────────────────────────────────────────────────────────────
  ✓ ENCODED                              LSN 1 · s2/m1/v1 · 12s ago
  content     "Alice merged the auth-rewrite branch"
  type        episodic      salience  0.70      context  7

→ next       subscribe --start-lsn 2   to watch for extraction
──────────────────────────────────────────────────────────────────────
```

On a dedup hit the badge flips to `⟳ DEDUP HIT`, the routing cluster
reads `matched · s2/m1/v1`, the type row becomes a `match` row
(`same content in context 7`), and the footer becomes a muted
`× no fresh write — nothing to do` line.

Wide mode (`-o wide`) adds an `agent / embedder / edges / created /
dedup` block between the type row and the footer:

```
  agent       019e3d1f-bd66-7890-a4bc-947ab6ca9c3e
  embedder    fp ab10 8e23 · 7c4d 0019 · …
  edges       2 auto · 1 explicit · 3 total
  created     1779153941479431250  [2026-05-20T10:05:41Z, 12s ago]
  dedup       ✗ no — fresh write
```

The `agent` row carries the **caller's** UUID (the auth-time identity),
not the writer's per-shard default — wide mode shows the real id, not
`default`. The `default` placeholder only renders against the nil
UUID, which is the unauthenticated / test path.

### JSON

```json
{ "op": "encode",
  "result": {
    "memory_id": "0x00020000000000010000000100000000",
    "lsn": 1,
    "dedup": "miss",
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

See [`../output-formats.md`](../output-formats.md) for `wide` / `yaml` /
`jsonpath=` variants.

---

## Examples

```bash
# Minimal one-shot
brain encode "hello world"

# Full envelope into a known context with explicit salience and kind
brain encode "auth tokens now use BLAKE3 instead of SHA-1" \
  --context 7 --kind semantic --salience 0.9

# Same text twice, distinct episodes (dedup off)
for d in 2026-05-15 2026-05-16 2026-05-17; do
  brain encode "daily standup happened" --context 9 --allow-duplicate
done

# Idempotent CI step
RID=$(uuidgen)
brain encode "deploy-$BUILD_ID succeeded" --context 4 --request-id "$RID"

# Encode + follow downstream events in one pipeline
LSN=$(brain encode "kickoff" --context 4 -o json | jq -r '.result.lsn')
brain subscribe --start-lsn $((LSN+1)) --context 4 --collect 10
```

---

## Errors

| Code | Trigger | Fix |
|---|---|---|
| `InvalidArgument` | Empty text. Salience out of `[0,1]`. Bad `--kind`. | Re-issue with valid input. |
| `Conflict` | `--request-id` reused with different params. | Generate a fresh `request_id`, or retry with the exact original params. |
| `Overloaded` | WAL group-commit queue full. | Back off; the server is shedding load. |
| `ShardUnavailable` | Target shard down. | Wait + retry; check `brain info`. |

Full catalogue: [`../errors.md`](../errors.md).

---

## Gated features

The shell parses these flags today, but the server-side support is
not wired. Each emits a `tracing` warning and either returns a stub
error or panics (`todo!()`); none corrupt state.

| Flag | Blocked on |
|---|---|
| `--vector <CSV>` | `ENCODE_VECTOR_DIRECT` wire op + SDK builder. |
| `--from-file *.jsonl` | Multi-encode batch helper in the SDK that opens a TXN, sends N encodes, commits. |

Track resolution in the `crates/brain-shell` and `crates/brain-sdk-rust`
issue queues.

---

## See also

- [`recall.md`](recall.md) — the read side
- [`forget.md`](forget.md) — the inverse
- [`link.md`](link.md) — add edges after the fact
- [`subscribe.md`](subscribe.md) — `--start-lsn` chaining
- [`txn.md`](txn.md) — multi-op atomicity
- [`../output-formats.md`](../output-formats.md) — table + JSON + ndjson + yaml + jsonpath
- [`../errors.md`](../errors.md) — error codes
- Spec: [`spec/09_cognitive_operations/02_encode.md`](../../../../spec/09_cognitive_operations/02_encode.md)
