# `brain` shell — guided tour

A 20-minute walk through every major surface of the `brain`
interactive shell. By the end you'll have:

- Encoded memories under a named agent
- Recalled them and chained the result LSN into a subscribe
- Watched live events stream in real time
- Run a TXN batch atomically
- Cleaned up

This is a **tutorial** — narrative, sequential. For the
look-it-up reference, see
[`../reference/shell/`](../reference/shell/). For specific
workflow recipes, see [`../guides/shell/`](../guides/shell/).

Prereqs: a running `brain-server` (default `127.0.0.1:9090`). The
[Docker quickstart](01-quickstart-docker.md) gets you one in five
minutes if you don't have one.

---

## 1. First contact

Drop into the REPL with no arguments:

```bash
$ brain
brain shell — connected to 127.0.0.1:9090 as 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e (ephemeral).
Type `help` for commands, `quit` to exit.
brain>
```

The connection banner tells you:

- The server address
- The agent id you're bound to
- The **source** of that binding — `(ephemeral)` means the shell
  minted a fresh UUID because you didn't specify one

Confirm:

```
brain> \agent
agent_id = 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e
source   = ephemeral (no --agent / BRAIN_AGENT set)
```

Ephemeral agents are perfect for poking; their data evaporates
when you reconnect. Quit and let's bind to something stable.

```
brain> quit
```

---

## 2. Create a named agent

Named agents persist across sessions:

```bash
$ brain agent create demo --note "tutorial agent"
created agent 'demo' (019e3d1f-bd66-7890-a4bc-947ab6ca9c3e)

$ brain agent list
NAME      ID                                       CREATED              NOTE
demo      019e3d1f-bd66-7890-a4bc-947ab6ca9c3e     2026-05-19T20:00:00Z tutorial agent
```

Bind to it:

```bash
$ brain --agent demo
brain shell — connected as 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e (via --agent demo).
brain>
```

Now every encode is attributed to `demo`. Survives across REPL
sessions.

For sticky cross-session use, drop into your shell rc:

```bash
export BRAIN_AGENT=demo
```

For the deep dive on named agents, see
[`../guides/shell/named-agents.md`](../guides/shell/named-agents.md).

---

## 3. Encode + read the response

```
brain> encode "Alice merged the auth-rewrite branch" --context 7 --salience 0.7
ok  s2/m1/v1  lsn=1
    agent=019e… · ctx=7 · episodic · sal=0.700 · fp=00000000…
```

The two-line output is intentional:

- **Line 1** — the bits you'll chain off: the **short memory id**
  (`s2/m1/v1` = shard 2 / slot 1 / version 1) and the **WAL LSN**.
- **Line 2** — provenance: which agent, context, kind, salience,
  edges_out (only shown when >0), and the embedding-model
  fingerprint.

Why the LSN matters: `brain subscribe --start-lsn 2` would
replay every event from after this encode. We'll do that in §6.

Encode two more for variety:

```
brain> encode "auth tokens now use BLAKE3 instead of SHA-1" --context 7 --kind semantic --salience 0.9
ok  s2/m2/v1  lsn=2
    agent=019e… · ctx=7 · semantic · sal=0.900 · fp=00000000…
brain> encode "the auth-rewrite was triggered by a security audit in Q3" --context 7 --salience 0.6
ok  s2/m3/v1  lsn=3
    agent=019e… · ctx=7 · episodic · sal=0.600 · fp=00000000…
```

Three memories, three LSNs. Total state: 3 rows in `MEMORIES_TABLE`,
3 records in the WAL, 3 vectors in HNSW.

---

## 4. Recall

```
brain> recall "what happened with the auth rewrite" --top-k 5 --include-text
#1  s2/m2/v1  semantic  ctx=7  sal=0.900  score=0.0164
    auth tokens now use BLAKE3 instead of SHA-1

#2  s2/m3/v1  episodic  ctx=7  sal=0.600  score=0.0161
    the auth-rewrite was triggered by a security audit in Q3

#3  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0159
    Alice merged the auth-rewrite branch

3 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

Three things to notice:

1. **Two-line per result.** Metadata on line 1; text on line 2 so
   it can breathe. The single most-asked-for column gets the
   whole line.
2. **Short memory ids** (`s2/m1/v1`) — paste them right back into
   `forget`, `link`, etc.
3. **The cluster warning footer.** "Δ<0.001" means every score is
   within a thousandth — your embedder isn't actually ranking
   these. In production this signals either:
   - The embedder isn't loaded (test mode)
   - All results are near-duplicates of the query
   When you see it, **don't trust the order.**

For the field-level reference, see
[`../reference/shell/output-formats.md#recall`](../reference/shell/output-formats.md#recall).

---

## 5. JSON output

The same recall, but JSON-shaped:

```
brain> \set output json
brain> recall "what happened with the auth rewrite" --top-k 1 --include-text
{ "op": "recall", "result": [ { "memory_id": "0x00020000000000020000000100000000", "similarity_score": 0.0164, "salience": 0.9, "salience_initial": 0.9, "access_count": 0, "lsn": 2, "flags": 1, "kind": "semantic", "context_id": 7, "consolidated_at_unix_nanos": null, "edges_out_count": 0, "edges_in_count": 0, "text": "auth tokens now use BLAKE3 instead of SHA-1" } ] }
```

JSON mode is line-delimited — pipe to `jq`:

```bash
$ brain --agent demo --output json recall "auth" --top-k 5 --include-text \
    | jq -r '.result[] | "lsn=\(.lsn) score=\(.similarity_score) text=\(.text)"'
lsn=2 score=0.0164 text=auth tokens now use BLAKE3 instead of SHA-1
lsn=3 score=0.0161 text=the auth-rewrite was triggered by a security audit in Q3
lsn=1 score=0.0159 text=Alice merged the auth-rewrite branch
```

Switch back:

```
brain> \set output table
```

For scripting recipes, see
[`../guides/shell/scripting-with-json.md`](../guides/shell/scripting-with-json.md).

---

## 6. The killer feature: `recall → subscribe` chain

The `lsn` returned by both `encode` and `recall` is what makes
"follow this memory's downstream events" possible. Try it:

```bash
# Terminal 1 — get the target memory's LSN.
$ brain --agent demo --output json recall "audit" --top-k 1 | jq '.result[0].lsn'
3

# Terminal 2 — subscribe from LSN 4 onwards (everything after the
# target memory was written).
$ brain --agent demo subscribe --start-lsn 4
subscribed — Ctrl-C to stop
```

The subscribe stream sits there waiting. Now back in terminal 1:

```bash
$ brain --agent demo encode "follow-up event"
ok  s2/m4/v1  lsn=4
    agent=019e… · ctx=0 · episodic · sal=0.500 · fp=00000000…
```

Terminal 2 wakes up:

```
     4  Encoded     0x00020000000000040000000100000000  ctx=0    Episodic     follow-up event
```

You can `Ctrl-C` terminal 2:

```
^C
closing stream…
(unsubscribed; 1 events)
```

The `closing stream…` message confirms the signal landed. The
shell sends UNSUBSCRIBE to the server (capped at 2s) and exits
cleanly.

For the workflow guide, see
[`../guides/shell/subscribe-and-replay.md`](../guides/shell/subscribe-and-replay.md).

---

## 7. Transactions

Atomic multi-op batches. The REPL has sticky-txn semantics — once
you `txn begin`, subsequent ops auto-attach `--txn`:

```
brain> txn begin
ok  txn_id=0x019e3ac4b1c67f73b96be363b5d48d50

brain*> encode "first batched memory" --context 8
ok  (buffered)

brain*> encode "second batched memory" --context 8
ok  (buffered)

brain*> link s2/m5/v1 followed-by s2/m6/v1
ok  (buffered)

brain*> txn commit
ok  committed=true  ops=3
```

Notice the prompt: `brain*> ` (the `*` shows active txn) until
commit. Three ops, one redb write transaction, one WAL bracket
(`TxnBegin` → 3 ops → `TxnCommit`) — all-or-nothing.

To bail without committing:

```
brain> txn begin
brain*> encode "this won't survive"
brain*> txn abort
ok  aborted=true
```

For the full reference, see
[`../reference/shell/commands.md#txn`](../reference/shell/commands.md#txn).

---

## 8. Forget

Soft-tombstone a memory:

```
brain> forget s2/m1/v1
ok  s2/m1/v1  outcome=Tombstoned  edges_removed=0
```

It vanishes from RECALL:

```
brain> recall "Alice" --top-k 5 --include-text
(no Alice memory anymore — was s2/m1/v1)
```

The slot is reclaimed by a background worker after the tombstone
grace period (default 7 days). For immediate vector zeroing, use
`--mode hard`:

```
brain> forget s2/m2/v1 --mode hard
ok  s2/m2/v1  outcome=Tombstoned  edges_removed=0
```

A `Forgotten` event publishes to subscribe; you could watch that
flow with the chain pattern from §6.

---

## 9. Multi-agent isolation (optional, requires 2 terminals)

If you want to see the `agents` filter in action:

```bash
# Terminal 1 — subscribe as `demo` with the agents filter.
$ DEMO_ID=$(brain --agent demo --output json agent show demo | jq -r .id)
$ brain --agent demo subscribe --agents "$DEMO_ID"

# Terminal 2 — encode from a DIFFERENT agent.
$ brain agent create other
$ brain --agent other encode "noise from another agent"

# Terminal 1 — receives NOTHING. (Without the filter, it would
# have seen the `other` event because both agents land on the
# same shard.)

# Terminal 2 — now encode from `demo`.
$ brain --agent demo encode "real signal"

# Terminal 1 — sees the `demo` event.
```

This is the multi-tenant isolation pattern — see
[`../guides/shell/named-agents.md`](../guides/shell/named-agents.md).

---

## 10. Cleanup

```
brain> quit
```

Optional, to wipe everything you just created:

```bash
brain agent delete demo
brain agent delete other   # if you ran §9
```

If you also want to nuke the server-side data:

```bash
pkill brain-server
rm -rf data/*
```

---

## What's next

You've used: `encode`, `recall`, `subscribe`, `txn`, `forget`,
`link`, named agents, JSON output, `\agent`, `\set`. That's most
of the surface.

For deeper paths:

| Topic | Where |
|---|---|
| Every flag of every verb | [reference/shell/commands.md](../reference/shell/commands.md) |
| REPL meta-commands (\agent, \config, …) | [reference/shell/repl-meta.md](../reference/shell/repl-meta.md) |
| JSON schemas per verb | [reference/shell/output-formats.md](../reference/shell/output-formats.md) |
| Bulk encoding from files | [guides/shell/bulk-encode.md](../guides/shell/bulk-encode.md) |
| Subscribe + replay deep dive | [guides/shell/subscribe-and-replay.md](../guides/shell/subscribe-and-replay.md) |
| Scripting with `jq` | [guides/shell/scripting-with-json.md](../guides/shell/scripting-with-json.md) |
| Troubleshooting | [guides/shell/troubleshooting.md](../guides/shell/troubleshooting.md) |

To use Brain from code instead of the shell, see
[reference/sdk-rust.md](../reference/sdk-rust.md) — same wire
protocol, programmatic surface.
