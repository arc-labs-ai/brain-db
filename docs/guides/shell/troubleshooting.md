# Troubleshooting the `brain` shell

When something doesn't behave the way you expect, work down this
list. Most issues come from misconfiguration or shell↔server
version skew; deep issues are runbook territory
([`../../runbooks/README.md`](../../runbooks/README.md)).

For the wire-error → exit-code mapping, see
[`../../reference/shell/errors.md`](../../reference/shell/errors.md).

---

## "It hangs on `brain` start"

### Server not running

```
$ brain encode "x"
error: failed to connect to 127.0.0.1:9090: Connection refused
```

Check that `brain-server` is up:

```bash
pgrep -af brain-server
```

Start it if needed:

```bash
./target/release/brain-server --config config/dev.toml &
```

### Wrong host:port

```
$ brain --server 127.0.0.1:7878 encode "x"
error: handshake timed out
```

Compare `--server` against `[server] listen_addr` in your TOML.
Override per-invocation with `--server` or `BRAIN_SERVER`.

### TLS misconfigured

If the server is TLS-enabled but the shell isn't:

```
error: invalid framing on first read (likely TLS expected)
```

Pass `--token` (when v2 auth lands) or run with `--insecure`
against a non-TLS dev server.

---

## "Encodes succeed but RECALL returns nothing"

### You're encoding under a different agent than you're recalling

```bash
$ BRAIN_AGENT=demo brain encode "hello"
$ brain recall "hello"   # ← ephemeral session, different agent
# → 0 results
```

Fix: bind the same agent on both sides.

```bash
$ BRAIN_AGENT=demo brain recall "hello"
# → matches
```

`\agent` in the REPL prints the current binding so you can verify.

### Wrong context filter

```bash
$ brain --agent demo encode "hello" --context 7
$ brain --agent demo recall "hello" --filter-context 8
# → 0 results
```

Drop the filter or align it.

### Embedder not loaded

```bash
$ brain --agent demo recall "anything" --top-k 5
# → 3 results, ALL score=0.0164 (tightly clustered)
# 3 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

The cluster-warning footer signals the embedder isn't
discriminating — typically because the server is running on the
`NopDispatcher` (test mode) or BGE failed to load.

Check server logs:

```bash
grep -i 'embedder\|bge\|dispatcher' /tmp/brain-server.log
```

Look for "loaded BGE model" or similar; the absence of it means
the embedder is the noop fallback.

---

## "I see other agents' events in my subscribe stream"

You forgot the `--agents` filter. On a shared shard, the default
subscribe sees every agent's events.

```bash
$ brain --agent work subscribe   # sees EVERYTHING on the shard

# Fix:
$ MY_ID=$(brain --agent work --output json agent show work | jq -r .id)
$ brain --agent work subscribe --agents "$MY_ID"   # only 'work'
```

See [`named-agents.md`](named-agents.md#4-subscribing-only-to-your-agents-events-on-a-shared-shard).

---

## "Subscribe replay errors with `LsnTooOld`"

```
error: SubscriptionLsnTooOld: from_lsn 1 is below the oldest
available LSN (10000); WAL retention has GC'd that range
```

The WAL retention worker has pruned segments below your requested
LSN. The error includes the actual `oldest_available_lsn` —
options:

- Reconnect with `--start-lsn 0` (everything still in WAL).
- Reconnect with `--start-lsn 10000` and accept the gap.
- Bump `wal_retention.minimum_age_seconds` server-side to keep
  more history.

For the recovery flow, see
[`subscribe-and-replay.md`](subscribe-and-replay.md).

---

## "Subscribe dies with `Overloaded`"

```
error: Overloaded: subscription lagged; reconnect with a fresh from_lsn
```

Your subscriber couldn't drain the broadcast buffer fast enough.
Default capacity is 1024 events; on a 10K-events/sec shard,
that's ~100ms of grace before the server drops you.

Reconnect with `--start-lsn N+1` where N is the last LSN you saw
(printed in the footer of the failed stream).

For server-side tuning:

- Raise `subscription_broadcast_capacity` in the server config.
- Or speed up your consumer — JSON output + `jq` adds latency;
  raw `cat` is faster.

---

## "REPL eats my Ctrl-C"

The new streaming subscribe handler installs a persistent
SignalKind receiver, so Ctrl-C should be picked up instantly. If
it isn't:

- You may be on the old binary (pre-fix). `brain --version`
  to confirm.
- The unsubscribe RPC may be hung — the second Ctrl-C bails
  immediately without waiting.

If neither helps, kill the shell from another terminal:

```bash
pkill -INT brain   # graceful (sends SIGINT)
pkill brain        # less graceful (SIGTERM, ~143 exit)
```

---

## "REPL completion is stale / wrong"

The REPL's tab completion is built into `brain` itself. For
shell-level completion (bash/zsh/fish), regenerate:

```bash
brain generate-completion bash > /etc/bash_completion.d/brain
brain generate-completion zsh  > "${fpath[1]}/_brain"
brain generate-completion fish > ~/.config/fish/completions/brain.fish
```

…then restart your shell.

---

## "I forgot the active txn_id"

It's in the prompt (`brain*> ` means active txn) but the id isn't
shown. To recover:

```
brain*> \unset txn
```

This drops the **local** sticky binding. The server-side txn ages
out on its own (default 60s). To force-abort it:

```
brain> txn abort <ID>
```

(You need the id — pull from your shell history or just wait for
the timeout.)

---

## "`brain config set` rejected my key"

```
$ brain config set out json
error: unknown key 'out' (did you mean 'output'?)
```

The settings schema is closed by design — unknown keys reject up
front. The hint is Levenshtein-nearest; type the suggested name
or `brain config list` to see all known keys.

---

## "I keep getting fresh ephemeral agents"

Your invocations aren't selecting a named agent. The shell mints
a fresh ULID per session when nothing is specified.

```bash
$ brain
brain shell — connected as 019e3d1f-… (ephemeral).
```

Fix one of:

- `--agent <name>` per invocation
- `export BRAIN_AGENT=<name>` in your rc file
- `\agent use <name>` inside the REPL (live-session only)

---

## "I migrated the legacy config and now my data is invisible"

The migration renames the old singleton `agent_id` to
`[agents.default]` but does **not** auto-bind — every invocation
goes ephemeral. To reach yesterday's memories:

```bash
brain --agent default recall "..."
# or persistent:
export BRAIN_AGENT=default
```

The first run after migration prints:

```
note: migrated legacy config.toml to named-agent schema; backup at ~/.config/brain/config.toml.bak-20260519T200000Z
```

The backup is full; recover by `cp` if needed.

---

## "My encode landed on a different shard than I expected"

Shard routing is `BLAKE3(agent_id) % shard_count`. Different
agents land on different shards. If you have a 4-shard cluster
and run with 1000 different agents, encodes spread evenly.

Inspect via `brain --output json encode … | jq '.result.memory_id'`
— the high 16 bits of the MemoryId are the shard prefix.

---

## "Output is JSON when I wanted table (or vice versa)"

Defaults auto-pick based on TTY:

| stdout | Default |
|---|---|
| Terminal | `table` |
| Pipe / redirect | `json` |

Override:

```bash
brain --output table … | cat        # force table when piped
brain --output json …               # force JSON in TTY
```

Persist across sessions:

```bash
brain config set output json
# or
brain config set output table
```

---

## When to escalate

- **Server errors that look like data corruption**
  (`CrcMismatch`, `WalBroken`, `BadFrame` repeatedly):
  → [`../../runbooks/README.md`](../../runbooks/README.md).
- **Subscribe lag at scale (10K agents):**
  → [`../tuning/`](../tuning/) for `subscription_broadcast_capacity`.
- **WAL replay seems wrong / events missing:**
  → [`../../architecture/`](../../architecture/) for the durability
    model.

---

## See also

- [`../../reference/shell/errors.md`](../../reference/shell/errors.md) — error → exit code mapping
- [`../../reference/shell/configuration.md`](../../reference/shell/configuration.md) — config + agent resolution
- [`named-agents.md`](named-agents.md) — multi-agent workflows
- [`subscribe-and-replay.md`](subscribe-and-replay.md) — subscribe details
