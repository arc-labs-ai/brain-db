# `brain` shell — errors + exit codes

The shell turns three classes of failure into output:

1. **Local errors** (config file unreadable, bad CLI usage, network down) → stderr + non-zero exit.
2. **Server-returned `ERROR` frames** → printed as a one-line diagnostic; non-zero exit in one-shot, prompt stays in REPL.
3. **Stream-level errors** (subscribe lag, broken pipe) → footer line + non-zero exit (one-shot) or graceful stream close (REPL).

This page maps each to exit codes, error codes, and the rendered
message. Wire-level error codes are documented in
[`../wire-protocol/`](../wire-protocol/).

---

## Exit codes (one-shot mode)

| Code | When |
|---|---|
| `0` | Op succeeded. |
| `2` | Server-returned error frame. |
| `3` | Local error — config file unreadable / bad usage / connection refused. |
| `130` | Interrupted by Ctrl-C (SIGINT). |
| `143` | Killed by SIGTERM. |

In REPL mode the exit code is whatever the **last command** would
have produced; `quit` always exits `0`.

---

## Wire error codes you'll actually see

Full catalog: [`../wire-protocol/error-codes.md`](../wire-protocol/error-codes.md).
The ones the shell surfaces day-to-day:

| Code | Class | Typical trigger |
|---|---|---|
| `InvalidArgument` | Protocol | Empty `encode` text; salience out of `[0,1]`; bad `--filter-kind` value. |
| `NotFound` | App | `forget` / `link` / `unlink` referencing a memory id that doesn't exist. |
| `Conflict` | App | Same `request_id` retried with **different** params (idempotency hash mismatch). |
| `Overloaded` | Runtime | WAL group commit queue full, or subscribe receiver lagged > `subscription_broadcast_capacity`. |
| `SubscriptionLsnTooOld` | App | `subscribe --start-lsn N` where N < oldest available LSN (WAL retention GC'd the range). Error message includes the actual oldest LSN. |
| `StreamIdInUse` | Protocol | Trying to subscribe twice on the same `stream_id`. (Internal — shouldn't fire in normal use.) |
| `ShardUnavailable` | Runtime | Target shard is down or being restarted. |
| `BadFrame` | Protocol | Internal frame issue. If you see this from the shell, file a bug. |
| `Internal` | Server | Catch-all. Server log has details. |

---

## Common cases + remedies

### `Conflict: encode request_id=… hash mismatch`

You retried an encode with the same `request_id` but different
text / context / kind / etc. The idempotency cache caught the
divergence (spec §07/06 §5).

**Fix:** generate a fresh `request_id` for genuinely new content,
or send the **exact** same params if you meant a retry. The shell
mints fresh ULIDs by default, so this typically only fires from
custom scripts that set `request_id` manually.

---

### `SubscriptionLsnTooOld: from_lsn N below oldest available LSN M`

You asked for replay from an LSN that the WAL retention worker
has already pruned. The error message includes the actual
`oldest_available_lsn`.

**Fix:**

- For "follow this memory" use cases: re-run `recall` to get a
  fresh `lsn`, then `subscribe --start-lsn <that lsn> + 1`.
- For "catch up after downtime" use cases: bump
  `wal_retention.minimum_age_seconds` in the server config so the
  WAL keeps more history.

---

### `Overloaded: subscription lagged; reconnect with a fresh from_lsn`

Your subscriber couldn't drain the broadcast buffer fast enough
(default `1024` events). The server dropped the subscription.

**Fix:**

- For interactive use: just reconnect. The footer (or JSON output)
  tells you the `final_lsn` so you can resume.
- For server-side tuning: raise `subscription_broadcast_capacity`
  in the server config. See
  [`../../guides/shell/troubleshooting.md`](../../guides/shell/troubleshooting.md).

---

### `error: unknown agent 'wokr'. Try \`brain agent list\` to see known agents`

You passed `--agent NAME` for a name not in your config file. The
shell suggests the nearest match via Levenshtein.

**Fix:** `brain agent list` to see what's there; `brain agent
create wokr` if you wanted a fresh one.

---

### `error: BRAIN_AGENT and BRAIN_AGENT_ID are both set; unset one`

Both env vars set. The shell refuses to guess which wins.

**Fix:** `unset BRAIN_AGENT_ID` (or whichever you didn't mean).

---

### `error: invalid value 'yaml' for 'output' (allowed: "table", "json")`

You set an out-of-schema value via `brain config set` or the env.

**Fix:** use one of the listed values. The schema is closed —
unknown keys + invalid values both reject up front.

---

### `error: internal: subscribe: WAL open: <io error>`

Server-side WAL replay failed to open the segments. Almost always
file-permission or disk issue.

**Fix:** check `data/<shard>/wal/` permissions; check `df -h` on
the data partition.

---

### `error: server error (80): memory not found: LINK source memory … not found`

`link`/`unlink` referencing a non-existent memory. The id is in
the message.

**Fix:** double-check the id (paste-error?); the memory may have
been hard-forgotten and reclaimed.

---

## Connection-time failures

| Rendered line | Meaning |
|---|---|
| `error: failed to connect to 127.0.0.1:9090: Connection refused` | Server not running, or wrong port. |
| `error: handshake timed out` | TLS misconfiguration, or wire-protocol version mismatch. |
| `error: AUTH rejected: server policy forbids unauthenticated connections` | Server enforces auth and you didn't provide credentials. (v2.) |

For the broader connection-troubleshooting flow, see
[`../../guides/shell/troubleshooting.md`](../../guides/shell/troubleshooting.md).

---

## In the REPL, errors don't kill the session

```
brain> forget 0xDEADBEEF
error: NotFound: memory 0x00000000000000000000000000deadbeef not found
brain> _
```

The prompt stays. The local `\unset txn` / `\agent use …` /
`\connect …` meta-commands work even after an error.

In **one-shot** mode the same error exits 2.

---

## See also

- [`../wire-protocol/error-codes.md`](../wire-protocol/error-codes.md) — full code catalog
- [`commands.md`](commands.md) — per-verb flags + validation rules
- [`../../guides/shell/troubleshooting.md`](../../guides/shell/troubleshooting.md) — diagnostic flowcharts
