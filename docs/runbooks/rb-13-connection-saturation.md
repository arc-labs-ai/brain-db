# RB-13: Connection saturation

**Severity:** **P2**.
**Alert:** `BrainConnectionsExhausted` (TCP accept
queue overflowing, or per-IP connection limits being
hit, or open-FD count near the limit).
**SLO impact:** new clients can't connect. Existing
connections continue, but new ones get
connection-refused or timeout.
**Estimated duration:** 30 minutes to 2 hours.
**Skill level:** comfortable with TCP-level
diagnostics (`ss`, `netstat`).

The substrate's network layer is rejecting new
connections — accept queue full, per-IP limits hit,
or file descriptors exhausted.

---

## Am I in the right runbook?

You should see:

- New clients reporting "connection refused" or
  "connection timeout."
- `brain_connections_active` near its configured
  ceiling.
- `brain_connections_rejected_total` rate elevated.
- `ss -ln 'sport = :9090'` shows a large `Recv-Q`
  (un-accepted connections backing up).
- Or: open file descriptors near `ulimit -n`.

If the substrate is **down or unresponsive**, that's
[RB-01](rb-01-substrate-down.md) or
[RB-08](rb-08-unresponsive.md). Connection saturation
specifically means the listener is alive but
rejecting.

---

## Stop the bleeding

Connection saturation can cascade — clients retry,
clients open more connections, more rejections,
worse load. Defensive action:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle ([DR-01](dr-01-diagnostic-bundle.md)).
3. **Check the source.** Is one IP / one client
   dominating, or is it spread across many sources?

   ```bash
   ss -tn 'sport = :9090' | awk '/ESTAB/ {print $5}' \
       | awk -F: '{print $1}' | sort | uniq -c | sort -rn | head
   ```

   Output looks like:

   ```
       420 10.0.42.17        ← likely runaway client
        50 10.0.42.18
        45 10.0.42.19
        ...
   ```

   - **One IP dominant (> 50% of connections):**
     likely a single misbehaving client. **Drop their
     connections at the LB or firewall** to relieve
     pressure immediately.
   - **Spread across many:** the substrate is just
     popular today. Continue to capacity diagnosis.

---

## Diagnose

### 1. How many connections?

```bash
# Total connections to brain-server.
ss -tn 'sport = :9090' | wc -l

# Compare to configured limit.
brain-cli admin config | grep -A2 '\[server\]' | grep max_connections
```

If you're at the limit, the substrate is correctly
refusing — the question is whether the limit is
right or whether the actual workload is wrong.

### 2. File-descriptor headroom

Each connection consumes an FD. So do open files,
sockets to other services, etc.

```bash
ulimit -n
ls /proc/$(pgrep brain-server)/fd | wc -l
```

If FDs in use are close to `ulimit -n`, raise the
limit. The substrate's systemd unit (or Docker /
k8s spec) should set `LimitNOFILE=65536` or higher.

### 3. Accept-queue depth

```bash
# Recv-Q is the kernel's accept queue for the listening socket.
ss -ln 'sport = :9090'
```

Output:

```
State    Recv-Q  Send-Q  Local Address:Port
LISTEN   42      128     127.0.0.1:9090
```

`Recv-Q` is the queue depth (pending connections
not yet accepted by the substrate). `Send-Q` is the
configured backlog limit.

If `Recv-Q` is at or near `Send-Q`, the accept loop
isn't keeping up. This is a deeper substrate issue —
either the accept loop is slow, or it's intentional
backpressure.

### 4. Are connections being held open too long?

```bash
ss -tn 'sport = :9090' -o | head -20
```

Each row has a `timer:(keepalive,...)` field. Long-
lived connections (hours) are normal for streaming
subscribes. Many short-lived connections (seconds)
are normal for one-shot requests.

What's *not* normal:

- Many connections in `CLOSE_WAIT`: the substrate
  isn't closing connections the client has closed.
  Substrate bug or FD leak.
- Many connections in `ESTABLISHED` with high
  inactivity: clients holding connections without
  using them. Could be misconfigured connection
  pools.

### 5. Rate of new connections

```promql
rate(brain_connections_accepted_total[1m])
rate(brain_connections_rejected_total[1m])
```

If accept rate is high (>100/sec) and rejected rate
is also elevated, you have a connection storm. Most
likely: a client (or many) opening connections in a
tight loop instead of reusing them.

### 6. Per-IP limits

If the substrate has per-IP connection limits set
(`server.max_connections_per_ip`):

```bash
brain-cli admin connections | jq '.by_ip' | head -20
```

Top-N IPs by connection count. If one IP is at the
per-IP limit, that's by design — but the client may
be confused (they think they're not connecting
because of a bug, when actually they're at the cap).

### 7. Memory per connection

Each connection costs ~32-256 KB of memory
(buffers, state). At 5000 connections, that's
~1 GB.

```bash
# Approx memory used by network buffers.
ss -mn 'sport = :9090' | grep -E 'r[bm]:' | head
```

If you're near host RAM, raising the connection
limit may not be safe. Consider rejecting more
aggressively or scaling out.

---

## Remediate

### Block the misbehaving client

If one source is dominant:

```bash
# Quick block via iptables.
sudo iptables -I INPUT -s 10.0.42.17 -p tcp --dport 9090 -j DROP

# Or via the LB if you have one.
```

This relieves pressure immediately. Investigate the
client afterwards; iptables block is temporary.

### Raise connection limits

If the workload legitimately needs more concurrent
connections:

```toml
[server]
max_connections = 10000       # was 5000
max_connections_per_ip = 500  # was 100 (if configured)
```

Roll out ([OP-08](op-08-config-change-rollout.md)).

Also raise file-descriptor limit if needed:

```ini
# systemd unit:
[Service]
LimitNOFILE=65536
```

```yaml
# Docker:
ulimits:
  nofile:
    soft: 65536
    hard: 65536
```

After raising, verify with `ulimit -n` inside the
process namespace.

### Investigate FD leak

If `CLOSE_WAIT` count is high and growing:

The substrate isn't closing connections that the
client has closed. This is a bug.

```bash
ss -tn 'sport = :9090' | grep CLOSE_WAIT | wc -l
```

If the number is growing over time during a normal
workload, escalate. Workaround: restart the
substrate to clean up (lose connection state but
restore FD count).

### Encourage connection reuse

If clients are opening many short-lived
connections instead of reusing:

- **Documentation reminder** to client teams: the
  wire protocol supports many requests over one
  connection. There's no reason to disconnect
  between requests.
- **HTTP-style "keep-alive."** Brain's wire protocol
  is request-response over a long-lived TCP stream;
  it's not HTTP, and the "open-close per request"
  pattern is anti-idiomatic.

This is a longer-term educational fix, not an
incident response.

### Scale out (more shards on more hosts)

If load is genuinely high and connection limits are
reasonable, the substrate just needs more capacity.
Brain is single-host in v1; scaling means more
shards on a bigger host, or fronting with an LB
that routes by agent ID to multiple instances.

Out of scope for this runbook; refer to the
deployment guide.

---

## Verify

```bash
# Connection count back to manageable.
ss -tn 'sport = :9090' | wc -l

# Rejection rate normal.
```

```promql
rate(brain_connections_rejected_total[1m])  # → 0
```

Smoke test from a fresh client:

```bash
brain-cli encode "post-saturation smoke $(date +%s)"
```

The `BrainConnectionsExhausted` alert clears once
active connections are below threshold.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Symptom: connections exhausted at HH:MM; mostly from IP 10.0.42.17.
Root cause: <e.g., client connection-pool bug opening new TCP per request>.
Remediation: <e.g., blocked offending IP, raised connection limit
from 5K to 10K>.
User impact: new connections rejected for ~Xm; existing connections unaffected.
Follow-up: TICKET-NNNN (work with client team on connection pooling).
Postmortem: <yes/no>.
```

Postmortem rule for RB-13:

- **Usually yes** for first occurrence.
- **Skip** for recurring known-client issues (just
  ticket the action).
- **Yes** if the cause was a substrate FD leak (bug).

---

## Prevention

- **Set per-IP connection limits.** Even a generous
  cap (500/IP) prevents one runaway client from
  taking down everyone else.
- **Monitor `CLOSE_WAIT` count** as a leading
  indicator of FD leaks. A growing trend is the
  warning before exhaustion.
- **Set `LimitNOFILE` aggressively** in systemd
  units. The cost of headroom is zero; the cost of
  running out is an incident.
- **Educate client teams** on connection reuse. The
  wire protocol is stream-shaped, not request-per-
  connection.
- **Alert on connection-accept rate** above a
  threshold. A storm is detectable before it
  saturates.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [RB-08 — Substrate becoming unresponsive](rb-08-unresponsive.md)
- [DR-02 — Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
