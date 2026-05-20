# `brain subscribe`

Tail the change feed. Two modes: a **streaming** mode that prints
events as they arrive until Ctrl-C, and a **batch** mode
(`--collect N`) that waits for exactly N events and exits.
`--start-lsn` replays history before joining the live tail; without
it, you see only events that arrive after subscribe lands.

```
brain subscribe
        [--context <N>]...
        [--kind episodic|semantic|consolidated]...
        [--start-lsn <N>]
        [--collect <N>]
```

---

## Modes

### Streaming (default)

Real-time tail. Events render one at a time, each followed by a
`stdout.flush()` so `brain subscribe | jq …` shows events as they
arrive instead of buffering until EOF.

SIGINT and SIGTERM handlers are installed **once** at start (not
per-iteration). On signal the loop prints `closing stream…` to
stderr, sends `UNSUBSCRIBE` to the server (capped at 2s — a hung
server can't pin the shell), then prints a footer summarising the
session.

A second Ctrl-C during the unsubscribe wait short-circuits the
2-second wait and exits immediately.

**Streaming auto-selects ndjson** for any structured output. If you
pass `-o json`, `-o yaml`, or `-o jsonpath=…` while streaming, the
renderer downgrades to ndjson — pretty JSON and YAML buffer poorly
across event boundaries. Table output stays as table.

### Batch (`--collect N`)

Waits for exactly N events, returns the collected list as a single
rendered value, exits. Useful for tests and short-lived scripts.

No signal handlers are installed in this mode — Ctrl-C in batch mode
returns control via the standard exit path.

---

## Flags

### `--context <N>` (repeatable)

Subscribe only to events from these context ids. Repeat once
per id; up to 16. No filter → events from every context the
agent can see.

```bash
brain subscribe --context 4 --context 7
```

The wire's `context_id` is the substrate's coarse partition;
filtering at subscribe time keeps cross-context event noise
out of your stream. For when to scope subscriptions vs not,
see [`docs/concepts/26-contexts.md`](../../../concepts/26-contexts.md).

### `--kind <KIND>` (repeatable)

Subscribe only to events on memories of these kinds. Repeat once
per kind:

```bash
brain subscribe --kind episodic --kind semantic
```

### `--start-lsn <N>`

Replay history before joining the live tail.

| Value | Behaviour |
|---|---|
| omitted | Live tail only — first event you see is the first event published after subscribe lands. |
| `0` | Sugar for "from the oldest available LSN". Replays the entire retained WAL window. |
| `N` where `N ≤ current tail` | Server replays from `N` from the WAL, then transparently cuts over to the live tail. No gap, no dupes. |
| `N` where `N > current tail` | Treated as a future LSN — the server immediately joins the live tail. You'll see events as soon as their LSN reaches N. |
| `N` where `N < oldest available LSN` | Returns `SubscriptionLsnTooOld`. The error message includes the actual oldest LSN so you can resume from a valid point. |

```bash
# Tail from a specific encode going forward
LSN=$(brain encode "kickoff" --context 4 -o json | jq -r '.result.lsn')
brain subscribe --start-lsn $((LSN+1)) --context 4
```

### `--collect <N>`

Batch mode. Wait for exactly N events, then exit with the list.
Mutually exclusive with the streaming path's signal handling — no
Ctrl-C cleanup is installed.

```bash
brain subscribe --start-lsn 0 --collect 50 -o json | jq '.result | length'
```

---

## Output

### Streaming table (one line per event)

```
     1  Encoded     0x00020000000000010000000100000000  ctx=7    Episodic     Alice merged the auth-rewrite branch
     2  Forgotten   0x00020000000000010000000100000000  ctx=7    Episodic
```

| Column | Meaning |
|---|---|
| LSN (right-aligned) | WAL log sequence number of the event. |
| Event type | `Encoded` / `Forgotten` / `Linked` / `Unlinked` / `ExtractionCompleted` / etc. |
| Memory id | Long-form hex id (line-stable across event types). |
| `ctx=N` | Origin context id. |
| Kind | Memory kind, when the event carries one. |
| Text | Body, when the event carries one (Encoded events do; others may not). |

**Footer (stderr, after stream end).** One of:

| Footer | When |
|---|---|
| `(unsubscribed; N events)` | Client closed the stream cleanly (Ctrl-C / SIGTERM / `--collect` satisfied). |
| `(stream closed by server; N events)` | Server sent EOS without a client signal. |
| `(stream error; N events delivered)` | Stream errored mid-flight (e.g. `Overloaded`). The error itself goes to stderr above the footer; the footer reports how many events landed before the error. |

The banner `subscribed — Ctrl-C to stop` prints to stderr at start
for human-output modes. Structured output (ndjson / json / yaml /
jsonpath) suppresses both banner and footer so the stream stays
machine-parseable.

### Streaming JSON (ndjson — one event per line)

```json
{ "op": "subscribe_event",
  "result": {
    "lsn": 1,
    "event_type": "Encoded",
    "memory_id": "0x00020000000000010000000100000000",
    "context_id": 7,
    "kind": "Episodic",
    "salience": 0.7,
    "timestamp_unix_nanos": 1779153941479431250,
    "text": "Alice merged the auth-rewrite branch"
  } }
```

Each event = one line; per-event `stdout.flush()`. `jq -c` and `awk`
work as expected.

### Batch (`--collect N`)

Single envelope wrapping the collected list:

```json
{ "op": "subscribe",
  "result": [ { "...event 1..." }, { "...event 2..." } ] }
```

See [`../output-formats.md`](../output-formats.md) for the full
event-field reference.

---

## Examples

```bash
# Tail the live feed for one context
brain subscribe --context 4

# Collect a fixed number of recent events as JSON
brain subscribe --start-lsn 0 --collect 100 -o json \
  | jq '.result | map(select(.event_type == "Encoded")) | length'

# Encode and follow downstream events in one pipeline
LSN=$(brain encode "kickoff" --context 4 -o json | jq -r '.result.lsn')
brain subscribe --start-lsn $((LSN+1)) --context 4

# ndjson stream for a long-running watcher
brain subscribe --context 4 -o ndjson \
  | while read -r line; do
      echo "$line" | jq -r '"[\(.result.lsn)] \(.result.event_type) \(.result.memory_id)"'
    done

# Watch only forget events across two contexts
brain subscribe --context 4 --context 7 \
  | grep -E 'Forgotten|HardForgotten'

# Catch up after downtime (with graceful handling of pruned LSNs)
LAST=$(cat last_seen_lsn.txt)
brain subscribe --start-lsn "$LAST" --context 4 -o ndjson \
  > events.ndjson 2> sub.log \
  || grep -q SubscriptionLsnTooOld sub.log \
  && OLDEST=$(grep -oE 'oldest_available_lsn: [0-9]+' sub.log | awk '{print $2}') \
  && brain subscribe --start-lsn "$OLDEST" --context 4 -o ndjson > events.ndjson
```

---

## Errors

| Code | Trigger | Fix |
|---|---|---|
| `InvalidArgument` | Bad `--kind` value. `--collect 0`. | Re-issue with valid input. |
| `SubscriptionLsnTooOld` | `--start-lsn N` where N is below the oldest available LSN. Error message includes the actual oldest. | Re-subscribe with the oldest reported LSN, or bump server-side `wal_retention.minimum_age_seconds`. |
| `Overloaded` | Receiver lagged > `subscription_broadcast_capacity` (default 1024). Server drops the subscription. | Reconnect with a fresh `--start-lsn` (the footer reports `final_lsn`); raise server-side `subscription_broadcast_capacity` if chronic. |
| `StreamIdInUse` | Internal — two subscribes raced on the same stream id. Shouldn't fire in normal use. | File a bug. |
| `ShardUnavailable` | Coordinator shard down. | Wait + retry. |

Full catalogue: [`../errors.md`](../errors.md).

---

## See also

- [`encode.md`](encode.md) — produces `lsn` to chain into `--start-lsn`
- [`recall.md`](recall.md) — every hit carries `lsn` for the same chaining trick
- [`forget.md`](forget.md) — emits `Forgotten` / `HardForgotten` events
- [`link.md`](link.md) / [`unlink.md`](unlink.md) — emit `Linked` / `Unlinked` events
- [`../output-formats.md`](../output-formats.md) — full event-field reference
- [`../errors.md`](../errors.md) — error codes
- Spec: [`spec/09_cognitive_operations/09_subscribe.md`](../../../../spec/09_cognitive_operations/09_subscribe.md)
