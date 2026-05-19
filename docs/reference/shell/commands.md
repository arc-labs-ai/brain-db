# `brain` shell — per-verb command reference

Authoritative reference for every subcommand of the `brain` binary.
Companion to the [overview](../brain-shell.md). For *task-oriented*
how-tos, see [`../../guides/shell/`](../../guides/shell/).

Every verb works in both one-shot and REPL mode unless noted.
Outputs shown are `--output table`; JSON shapes are in
[`output-formats.md`](output-formats.md).

---

## `encode`

Write a memory. Returns the persistent `memory_id` plus the WAL
position (`lsn`) you can chain into `subscribe --start-lsn lsn+1`.

```
brain encode <TEXT>
        [--context N]                            # context id (default 0)
        [--kind episodic|semantic|consolidated]
        [--salience 0.0..1.0]
        [--deduplicate]                          # skip if fingerprint matches
        [--txn HEX]                              # bind to active transaction
```

**Output (table):**

```
ok  s2/m1/v1  lsn=1
    agent=00000000… · ctx=7 · episodic · sal=0.700 · fp=00000000… · edges_out=0
```

| Field | Meaning |
|---|---|
| `s2/m1/v1` | Short MemoryId — shard / slot / version. Full hex available in JSON. |
| `lsn=1` | WAL position. Use `subscribe --start-lsn 2` to follow events from after this encode. |
| `agent=…` | Authenticated agent (first 4 hex chars). |
| `ctx=…` | Context the row was filed under. |
| `episodic` | Memory kind. |
| `sal=…` | Final salience (may differ from `--salience` hint if backend adjusted). |
| `fp=…` | Embedding-model fingerprint (first 4 hex chars). |
| `edges_out=N` | Only shown when N>0 — outgoing edges that actually landed. |
| `dedup=hit\|miss` | Only shown when `--deduplicate` was passed. `off` is suppressed. |

### Dedup states

| `dedup=` | When |
|---|---|
| (not shown) | `--deduplicate` not passed → fresh slot, no fingerprint check. |
| `miss` | `--deduplicate` passed; no existing memory matched; fresh slot allocated AND fingerprint recorded. |
| `hit` | `--deduplicate` passed; existing memory matched; returned that id, no new slot. |

**Why dedup is opt-in.** The same text legitimately becomes
different memories in different episodic contexts ("the build
broke" said on Monday vs Friday). Dedup is off by default so the
substrate never silently merges them.

**Dedup scope.** Per `(shard, agent_id, context_id)`. Same text
under a different agent or `--context` is a miss. Tombstoned
memories don't count — FORGET evicts the fingerprint in the same
write transaction as the tombstone.

---

## `recall`

Vector-similarity search. Returns ranked `MemoryResult`s.

```
brain recall <QUERY>
        [--top-k N]                              # default 10
        [--confidence FLOAT]                     # similarity threshold
        [--include-text]                         # default omits text bodies
        [--filter-context N]...                  # repeatable
        [--filter-kind K]...                     # repeatable
```

**Output (table) — two-line per result + footer:**

```
#1  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0164
    Alice merged the auth-rewrite branch

#2  s2/m2/v1  semantic  ctx=7  sal=0.900  score=0.0161
    auth tokens now use BLAKE3 instead of SHA-1

2 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

| Field | Meaning |
|---|---|
| `#N` | Rank (1-indexed). |
| `s2/m1/v1` | Short MemoryId. |
| `episodic†` | Kind. `†` marker = consolidated row (summary produced by background worker). |
| `ctx=…` | Context filter / origin. |
| `sal=0.700` | Current salience. |
| `sal=0.500↓0.700` | Decayed since write (current ↓ initial). `↑` for boost. |
| `score=…` | Similarity score. |
| `acc=N` | Only shown when N>0 — RECALL hit count. |
| `edges=Nin/Nout` | Only shown when either side is >0 — denormalised connectivity. |

**Cluster warning:** if every top-K result is within `Δ<0.001` of
the highest score, the footer reads
`scores tightly clustered (Δ<0.001) — ranking may not be meaningful`.
This signals one of:

- The embedder isn't loaded (test mode / NopDispatcher).
- The query genuinely doesn't discriminate among the results.
- All results are near-duplicates of the query.

When you see it, **don't trust the order** — treat all results as
equal-scored.

**Text not shown.** Without `--include-text`, the text line reads
`(text not fetched — re-run with --include-text)`. Defaults to
omission to keep RECALL cheap.

---

## `plan`

Stepwise causal/temporal path from one state to another.

```
brain plan <FROM> <TO>
        [--max-steps N]
        [--max-wall-time-ms N]
```

Returns `Vec<PlanStep>` plus a `plan_status` footer:

- `GoalReached` — the goal was reached.
- `BudgetExhausted` — hit `--max-steps` or `--max-wall-time-ms`.
- `NoPathFound` — search terminated without a path.
- `Timeout` — server-side timeout.

When status ≠ `GoalReached`, the table footer surfaces it
explicitly so you don't misread a partial result as a complete one.

---

## `reason`

Inference chain from observed evidence.

```
brain reason <OBSERVATION>
        [--depth N]
        [--confidence FLOAT]
        [--max-inferences N]
```

Returns `Vec<InferenceStep>` plus a `reason_status` footer
(`Complete` / `BudgetExhausted` / `DepthLimitReached` / `Cancelled`).

---

## `forget`

Tombstone a memory.

```
brain forget <ID>
        [--mode soft|hard]                       # default soft
```

`<ID>` accepts either:

- Short form: `s2/m1/v1` (shard/slot/version)
- Long form: `0x` + 32 hex chars
- Decimal `u128`

**Soft forget** marks the memory `HARD_FORGOTTEN`, evicts the
FINGERPRINTS entry (if dedup-on), and emits a `Forgotten`
subscribe event. The slot is reclaimed by the background worker
after the tombstone grace period (default 7 days).

**Hard forget** additionally zeroes the vector in the arena
immediately.

---

## `link`

Add a typed edge between two memories.

```
brain link <SRC> <KIND> <TGT> [--weight 0.95] [--txn HEX]
```

`<KIND>` is one of: `derived-from`, `followed-by`, `caused-by`,
`causes`, `contradicts`, `supports`, `analogous`, `same-as`,
`mentions`, `summarises`, `references`, `part-of`.

`already_existed=true` in the response means the edge was already
there (the weight was overwritten); `false` means it was inserted.

---

## `unlink`

Remove an edge. Non-existent edge → `removed=false`, not an error
(idempotent).

```
brain unlink <SRC> <KIND> <TGT> [--txn HEX]
```

---

## `txn`

Multi-op atomic batch. Three subcommands.

```
brain txn begin                                  # returns a new txn_id (hex)
brain txn commit <ID>
brain txn abort  <ID>
```

**REPL behaviour:** inside the REPL, `txn begin` makes the txn
**sticky** for the session — subsequent `encode`/`forget`/`link`/
`unlink` calls auto-attach `--txn` unless overridden. The prompt
switches to `brain*> ` while a txn is active. `txn commit`/`txn
abort` clears it; `\unset txn` clears it locally without issuing
an op.

**One-shot mode:** no session state — `--txn` must always be
explicit.

**TXN_COMMIT semantics:** the writer applies the buffered ops in
one redb write transaction (all-or-nothing) and one WAL bracket
(`TxnBegin` → ops → `TxnCommit`). A crash between op buffering
and commit drops the whole batch.

---

## `subscribe`

Live + replay event stream.

```
brain subscribe [--context N]... [--kind K]... [--agents ID]...
                [--start-lsn N]
                [--collect N]
```

### Modes

| Mode | When | Behaviour |
|---|---|---|
| Streaming (default) | No `--collect` | Renders events as they arrive; Ctrl-C cleans up via UNSUBSCRIBE RPC. |
| Batch | `--collect N` | Blocks until N events arrive, then prints them as a table and exits. Useful in tests/scripts. |

### `--start-lsn` (subscribe + replay)

`--start-lsn N` makes the server replay any historical events with
`lsn >= N` from the WAL before joining the live tail. The cutover
is invisible to the client (no gap, no dupes).

| `--start-lsn` value | Behaviour |
|---|---|
| (omitted) | Live tail only — no historical replay. |
| `0` | "Everything still in the WAL" — sugar for the oldest available. |
| `N > current_tail` | No replay, transitions straight to live. |
| `N < oldest_available_lsn` | Server errors with `SubscriptionLsnTooOld`; the message includes the actual oldest LSN. |

Pair with `recall.lsn` to follow a specific memory's downstream
events — see [`../../guides/shell/subscribe-and-replay.md`](../../guides/shell/subscribe-and-replay.md).

### Filters

- `--context N` — repeatable; intersect with the event's `context_id`.
- `--kind K` — repeatable; intersect with the event's `kind`.
- `--agents UUID` — repeatable; only events from these agents. **The single most useful filter on a shared shard** — without it you see every other agent's events too. See [`../../guides/shell/named-agents.md`](../../guides/shell/named-agents.md).

### Streaming output

One line per event, flushed:

```
     1  Encoded     0x00020000000000010000000100000000  ctx=7    Episodic     Alice merged the auth-rewrite branch
     2  Encoded     0x00020000000000020000000100000000  ctx=7    Semantic     auth tokens now use BLAKE3 instead of SHA-1
```

Ctrl-C prints `closing stream…` to stderr (so you know the signal
landed), then cleans up via UNSUBSCRIBE with a 2-second cap. A
second Ctrl-C bails immediately.

### Footer (streaming)

| Footer | Meaning |
|---|---|
| `(unsubscribed; N events)` | Clean Ctrl-C exit. |
| `(stream closed by server; N events)` | Server-side close (EOS frame). |
| `(stream error; N events delivered)` | Stream errored mid-flight. |

---

## `agent`

Named-agent CRUD. See [`configuration.md`](configuration.md) for
the persistent file shape; this section is the command list.

```
brain agent list
brain agent show [<NAME>]
brain agent create <NAME> [--note <TEXT>]
brain agent rename <OLD> <NEW>
brain agent delete <NAME>
brain agent import <NAME> <ULID>
brain agent use <NAME>                           # REPL only
```

`brain agent list` marks the currently-bound agent for this
invocation with `*` (or `<ephemeral>` if no name was selected).

---

## `config`

Persistent shell settings. See [`configuration.md`](configuration.md)
for the file schema.

```
brain config list                                # effective merged settings
brain config get <KEY>
brain config set <KEY> <VALUE>
brain config path                                # print config path
brain config edit                                # $EDITOR → $VISUAL → vi
```

Known settings keys: `output`, `timing`, `sticky_context`, `server`.
Unknown keys are rejected with a "did you mean…" hint.

`\config set X Y` inside the REPL **also** mutates the live
session, not just the file (mongosh-style).

---

## `shell`

Explicit REPL entry. Equivalent to running `brain` with no
subcommand. Useful for clarity in shell scripts that conditionally
drop into the REPL.

---

## `generate-completion`

```
brain generate-completion bash > /etc/bash_completion.d/brain
brain generate-completion zsh  > "${fpath[1]}/_brain"
brain generate-completion fish > ~/.config/fish/completions/brain.fish
brain generate-completion powershell > brain.ps1
```

Emits a `clap_complete`-generated completion script.

---

## See also

- [`../brain-shell.md`](../brain-shell.md) — overview
- [`repl-meta.md`](repl-meta.md) — `\agent`, `\config`, `\set`, …
- [`output-formats.md`](output-formats.md) — table + JSON schemas
- [`configuration.md`](configuration.md) — config file + agents
- [`errors.md`](errors.md) — error codes + exit codes
