# Subscribe and replay

`brain subscribe` is a tail-the-event-stream verb. It does two
things in one connection:

1. **Replay** historical events from a WAL `--start-lsn`.
2. **Stream** live events as they arrive.

The cutover between the two is invisible — no gap, no dupes.

This guide covers the workflows the verb unlocks. For the
reference, see
[`../../reference/shell/commands.md#subscribe`](../../reference/shell/commands.md#subscribe).

---

## The mental model

```
   WAL on disk           live in-memory bus
   ─────────────         ─────────────────
   LSN 1  ENCODE  ─┐
   LSN 2  ENCODE  ─┤
   LSN 3  FORGET  ─┤ ──┐
   LSN 4  ENCODE  ─┘   │
                       │   replayed by subscribe
                       │   --start-lsn 1
                       ▼
                  ┌─────────────────────┐
                  │ subscribe pipe      │
                  │  (one frame per     │
                  │   event, in order)  │
                  └─────────────────────┘
                       ▲
   LSN 5  ENCODE  ─────┘  live: arrives after the cutover
   LSN 6  ENCODE  ─────┘
```

The server takes a snapshot of `current_tail_lsn` *before* it
starts reading the WAL, so any event with `lsn >= cutover` is
deduplicated from the replay (it'll arrive on the live channel
instead). You see every event between `--start-lsn` and "now,"
then the stream continues live.

---

## 1. Subscribe live (no replay)

The simplest form: see new events from the moment you connect.

```bash
brain --agent demo subscribe
# subscribed — Ctrl-C to stop
#
# (waits for events to arrive)
```

Encode from another terminal:

```bash
brain --agent demo encode "live event"
```

Back at the subscribe terminal:

```
     N  Encoded     0x00020000000000010000000100000000  ctx=0    Episodic     live event
```

Ctrl-C cleans up:

```
^C
closing stream…
(unsubscribed; 1 events)
```

---

## 2. Replay everything still in the WAL

`--start-lsn 0` is sugar for "everything still in the WAL." Useful
when you reconnect after downtime and don't know your last LSN:

```bash
brain --agent demo subscribe --start-lsn 0 --collect 10
```

`--collect 10` runs in batch mode: blocks until 10 events arrive
(or EOS), then prints them as a table and exits. For interactive
streaming, omit `--collect`.

---

## 3. Resume from a known LSN

Persist the LSN you've consumed up to (per-client bookkeeping),
then resume on reconnect:

```bash
# Last time, you remembered the highest LSN you saw was 42.
brain --agent demo subscribe --start-lsn 43
# Replays nothing new (you're already caught up), then streams
# live from LSN 43 onwards.
```

If your saved LSN is **beyond** the current tail (e.g. the server
restarted but you stored a stale value), the subscription is
accepted but yields zero replay events and transitions straight
to the live tail — no `LsnTooOld` error.

---

## 4. Chain `recall → subscribe` (follow a specific memory)

The killer feature: every `recall` result carries the WAL `lsn`
the memory was written at. You can recall a memory, then subscribe
from `lsn+1` to follow every subsequent event:

```bash
# Find the memory.
LSN=$(brain --agent demo --output json recall "target memory" --top-k 1 \
  | jq '.result[0].lsn')

echo "Memory was written at LSN $LSN"
# → Memory was written at LSN 42

# Follow everything that happened after it.
brain --agent demo subscribe --start-lsn $((LSN+1))
```

Use cases:

- **"Why did this memory get tombstoned?"** — recall it, then
  subscribe from its LSN to see the chain of edges / forgets that
  followed.
- **"Audit a single agent's activity since a known checkpoint"** —
  pin a memory at the checkpoint, then subscribe from its LSN+1
  with `--agents <that agent>`.

---

## 5. Chain `encode → subscribe` (follow your own writes)

`encode` returns its WAL `lsn` too. The pattern:

```bash
# Encode and capture the LSN.
LSN=$(brain --agent demo --output json encode "anchor" \
  | jq '.result.lsn')

# In another terminal: subscribe from anchor+1 to see everything
# that happens after.
brain --agent demo subscribe --start-lsn $((LSN+1))
```

This is the "anchor point" pattern — establish a known LSN, then
follow forward.

---

## 6. Filter the stream

Three filter axes, all set-membership and combinable:

```bash
brain --agent demo subscribe \
    --context 7                   # only ctx=7 events
    --context 8                   # OR ctx=8
    --kind episodic               # only Episodic
    --agents 01HMK_AGENT_A        # only this agent
```

All three filters intersect (logical AND between axes; OR within
each axis). The most useful in production is `--agents` on a
shared shard — see
[`named-agents.md`](named-agents.md#4-subscribing-only-to-your-agents-events-on-a-shared-shard).

---

## 7. Cross-restart durability

The WAL survives server restarts. So:

```bash
# Encode some memories.
brain --agent demo encode "before-restart"

# Restart the server.
pkill brain-server; sleep 2
./target/release/brain-server --config config/dev.toml &
sleep 2

# Encode more.
brain --agent demo encode "after-restart"

# Subscribe from the start — sees BOTH (the pre-restart one
# replays from the WAL that survived).
brain --agent demo subscribe --start-lsn 1 --collect 2
```

The pre-restart events come back because the WAL is the durable
log — server restart loses in-memory subscribers, but the WAL is
intact.

---

## Edge cases

### `--start-lsn N` where N is below WAL retention

```
error: SubscriptionLsnTooOld: from_lsn 1 is below the oldest
available LSN (10000); WAL retention has GC'd that range
```

The error includes the actual `oldest_available_lsn` — you can
choose to:

- Reconnect with `--start-lsn 0` (everything still in WAL).
- Reconnect with `--start-lsn 10000` and accept a gap.
- Adjust server config (`wal_retention.minimum_age_seconds`) to
  keep more history.

### Subscriber lag → `Overloaded`

If your subscriber can't drain events fast enough (default
`subscription_broadcast_capacity = 1024`), the server drops the
subscription with `Overloaded`:

```
error: Overloaded: subscription lagged; reconnect with a fresh from_lsn
```

The footer prints `(stream error; N events delivered)` so you
know where to resume from. Reconnect with `--start-lsn N+1`.

### Ctrl-C latency

The stream prints `closing stream…` to stderr the moment Ctrl-C
arrives, so you know the signal landed even if the server-side
UNSUBSCRIBE RPC takes a moment. Capped at 2 seconds — a second
Ctrl-C bails immediately.

---

## Performance characteristics

- **Replay** runs on a server-side thread pool (default cap 64
  concurrent across all subscribers). A reconnect storm of 10K
  clients queues at this limit instead of saturating the pool.
- **Live stream** is a tokio broadcast channel; each subscriber
  gets a `recv` independently — slow ones don't block fast ones.
- **Filter evaluation** is server-side and cheap (HashSet lookups).
  Move filters there instead of doing them in your client to save
  bandwidth.

---

## See also

- [`../../reference/shell/commands.md#subscribe`](../../reference/shell/commands.md#subscribe) — flag reference
- [`../../reference/shell/output-formats.md#subscribe`](../../reference/shell/output-formats.md#subscribe) — event JSON shape
- [`named-agents.md`](named-agents.md) — `--agents` filter context
- [`scripting-with-json.md`](scripting-with-json.md) — jq pipelines for the event stream
