# DR-02: Reading traces, metrics, and logs

**When to use:** any time you need to orient inside the
observability stack — at the start of a P1, mid-incident
when a runbook says "check the dashboard," or while
doing routine post-incident review.

This document is the field guide for navigating Brain's
telemetry. The substrate emits three kinds of signal
(metrics, logs, traces); each is useful for a different
class of question.

---

## The three signals

| Signal | What it answers | Latency | Cost to query |
|---|---|---|---|
| **Metrics** | "What's the rate / percentile / count of X?" | seconds | cheap |
| **Logs** | "What did the substrate say about this event?" | seconds | medium |
| **Traces** | "Where in the request did time go?" | minutes | expensive |

The rough mental model: metrics tell you *what* is wrong;
logs tell you *what was said* when it went wrong; traces
tell you *where* it was slow.

Start with metrics. Drill into logs for context. Open
traces when you need to know where the time went on a
specific request.

---

## Metrics — Prometheus and Grafana

Brain exposes Prometheus-format metrics at
`metrics_addr` (default `127.0.0.1:9091/metrics`). Scrape
into Prometheus; visualise in Grafana. The reference
catalog is at
[`../reference/metrics.md`](../reference/metrics.md).

### The starter dashboards

Three dashboards exist by default:

- **`Brain — Overview`** — global health: latency
  percentiles per op, request rate, error rate,
  shard count, snapshot age.
- **`Brain — Per-shard`** — drilldown by `shard_id`:
  per-shard latency, queue depth, worker status,
  HNSW health.
- **`Brain — Workers`** — per-worker timing,
  last-run timestamps, error rates.

When an alert fires, the Overview is the first
dashboard to look at. The alert's annotation usually
points you to a specific panel.

### Useful metric families

Listed by the question they answer most often.

**"Is the substrate healthy at all?"**

```promql
brain_uptime_seconds                              # process uptime
brain_shards_total                                # configured shards
brain_shards_active                               # shards that spawned successfully
up{job="brain"}                                   # scrape success
```

**"How busy is it?"**

```promql
rate(brain_request_duration_ms_count[1m])         # request rate by op
rate(brain_encode_total[5m])                      # encode rate
rate(brain_recall_total[5m])                      # recall rate
```

**"How slow is it?"**

```promql
histogram_quantile(0.50,
  sum by (op, le) (rate(brain_request_duration_ms_bucket[5m])))
histogram_quantile(0.99,
  sum by (op, le) (rate(brain_request_duration_ms_bucket[5m])))
histogram_quantile(0.999,
  sum by (op, le) (rate(brain_request_duration_ms_bucket[5m])))
```

The three queries above (p50, p99, p999) are the
single most useful trio. A latency incident almost
always starts here.

**"Are errors happening?"**

```promql
rate(brain_request_errors_total[5m])              # overall error rate
sum by (error_code) (rate(brain_request_errors_total[5m]))
```

**"How's the index?"**

```promql
brain_hnsw_node_count                             # nodes in index
brain_hnsw_tombstone_ratio                        # forgotten / total
brain_hnsw_last_rebuild_unixtime                  # last rebuild
```

**"How's the WAL?"**

```promql
brain_wal_fsync_duration_ms                       # write latency
brain_wal_segment_count                           # retained segments
brain_wal_bytes_pending                           # buffered, not yet fsynced
```

**"How's memory / disk?"**

```promql
process_resident_memory_bytes{job="brain"}        # RSS
brain_arena_bytes_used                            # arena disk usage
brain_arena_bytes_capacity                        # arena allocated
brain_metadata_db_bytes                           # redb size
node_filesystem_avail_bytes{mountpoint="..."}     # underlying disk
```

**"How are the workers?"**

```promql
brain_worker_last_run_unixtime                    # per worker
brain_worker_cycle_duration_ms                    # cycle time
rate(brain_worker_errors_total[5m])               # errors per worker
```

### Pattern matching: which shard, which op?

Most Brain metrics carry `shard_id` and `op` labels.
For an incident on one shard, scope queries with the
label:

```promql
histogram_quantile(0.99,
  sum by (le) (
    rate(brain_request_duration_ms_bucket{shard_id="3"}[5m])))
```

Drop the `{shard_id="3"}` to see the global picture
again.

### Knowing what's normal

Before you can spot an anomaly, you need a sense of
baseline. Two patterns:

1. **Look at the last 24 hours** for the same metric.
   If you're at 200ms p99 and yesterday was 200ms p99,
   that's not an incident — it's reality. (May still
   be a problem worth fixing, but not a *new* one.)
2. **Look at week-over-week.** Some workloads cycle
   weekly. Tuesday at 14:00 has very different load
   than Sunday at 04:00.

Grafana's compare-to-previous-period overlay is
useful for this. If your dashboard doesn't have it
turned on, add it for `Brain — Overview`.

### When the metric you want doesn't exist

A few possibilities:

- **Recently added; cardinality limit.** Some metrics
  ship in newer versions of Brain only.
- **Wrong scrape config.** Check that Prometheus is
  actually scraping `metrics_addr`. The `up` metric
  for the Brain job tells you.
- **Filtered out at collection.** Some Prometheus
  setups drop high-cardinality metrics; check the
  scrape config.

If you genuinely need a metric that doesn't exist,
file a ticket. The reference doc is the place to
propose additions.

---

## Logs

Brain logs to stdout/stderr in **structured JSON** by
default (configurable). Each line:

```json
{
  "ts": "2024-09-12T14:31:22.123Z",
  "level": "INFO",
  "target": "brain_server::shard",
  "msg": "shard spawned",
  "shard_id": 3,
  "duration_ms": 142
}
```

Important fields:

- `ts` — UTC timestamp.
- `level` — `TRACE | DEBUG | INFO | WARN | ERROR`.
  Production usually runs at `INFO`; raise to `DEBUG`
  with care.
- `target` — the Rust module that emitted the line.
  Tells you which subsystem (shard / wal / hnsw /
  etc).
- `msg` — a short human-readable summary.
- Custom fields — vary per log line. Common ones
  include `shard_id`, `memory_id`, `request_id`,
  `duration_ms`, `error`.

### Where logs go

- **systemd**: `journalctl -u brain-server`.
- **Docker**: `docker logs brain-server`.
- **Kubernetes**: `kubectl logs -l app=brain-server`.
- **File**: wherever your supervisor writes them
  (often `/var/log/brain/`).
- **Centralised**: if you're shipping to a log
  aggregator (Loki, Elasticsearch, Datadog), use that
  UI.

### Useful queries

If you have a log aggregator like Loki:

```logql
# Recent errors
{job="brain"} |= "level" |= "ERROR" | json | line_format "{{.ts}} {{.msg}} {{.error}}"

# Activity on a specific shard
{job="brain"} | json | shard_id = "3"

# Anything mentioning a specific MemoryId
{job="brain"} |~ "mem_018f2b1e"
```

Without a log aggregator, fall back to grep:

```bash
# Errors in the last hour
journalctl -u brain-server --since "1 hour ago" -p err

# Lines mentioning shard 3
journalctl -u brain-server --since "1 hour ago" | grep '"shard_id":3'

# Lines about WAL fsync
journalctl -u brain-server --since "1 hour ago" \
    | grep '"target":"brain_storage::wal"'
```

### Log levels worth knowing

- `ERROR` — something the substrate considers wrong.
  Always investigate.
- `WARN` — degraded but not necessarily an incident.
  Often the early warning of an upcoming incident.
- `INFO` — significant events: shard spawn, snapshot
  completion, worker cycle finished. Useful for
  reconstruction.
- `DEBUG`/`TRACE` — verbose internal events. Off in
  production; turn on briefly with caution.

A pattern of repeated WARNs that you've been ignoring
becomes the cause of the next P1. Skim WARN-level
periodically during routine review.

### Raising log level temporarily

If a runbook says "raise log level for the embedder,"
the substrate supports per-target log levels via the
`RUST_LOG` env var (Tokio-style). Restart with:

```
RUST_LOG=info,brain_embed=debug brain-server --config /etc/brain/config.toml
```

…or change the corresponding TOML field if your build
exposes it without restart. Roll back as soon as
you've collected what you needed; DEBUG/TRACE
generate large log volumes.

---

## Traces — OpenTelemetry

Tracing is the most powerful but most expensive signal.
It shows the lifecycle of a single request across the
substrate's components. Tokio → flume → Glommio →
embedder → arena → WAL → HNSW → response, each as a
nested span with timing.

Brain emits OpenTelemetry spans when configured to do
so. The destination depends on your observability
stack: Jaeger, Tempo, Honeycomb, Datadog APM.

### When traces help most

- **"This one request was slow."** Trace shows where
  the time went.
- **"Some recalls are slow."** Filter traces by op +
  long duration, look for common patterns.
- **"I think the WAL is the bottleneck."** Trace's
  WAL span tells you definitively.

### When traces don't help

- **"Throughput is low."** Traces tell you about
  individual requests, not aggregate throughput.
  Metrics are better.
- **"Errors are happening."** The trace of a failed
  request shows where it failed, but logs show *what*
  the error was.
- **"The substrate won't start."** No requests, no
  traces.

### Reading a trace

A typical recall trace:

```
recall (45 ms)
├─ frame_decode (0.1 ms)
├─ dispatch (0.2 ms)
├─ shard_call (44 ms)                ← time is in here
│   ├─ embedder (35 ms)              ← embedder dominates
│   │   ├─ cache_lookup (0.01 ms)
│   │   ├─ tokenize (0.1 ms)
│   │   ├─ forward (34 ms)            ← the model itself
│   │   └─ normalize (0.1 ms)
│   ├─ hnsw_search (5 ms)
│   ├─ metadata_fetch (3 ms)
│   └─ response_assemble (0.5 ms)
└─ frame_encode (0.5 ms)
```

The pattern: most time is in *one* span. Find that
span; that's where the bottleneck is. For the
example above, the bottleneck is `embedder.forward`
(the model inference itself). The runbook for "high
latency on encode/recall" knows what to do with that.

### Sampling

Traces are expensive. Most deployments sample (e.g.
1 in 100 requests gets a full trace). When you're
trying to find a slow trace and can't, the sampling
may have missed it.

Options during an incident:

1. **Bump sampling to 100 %** temporarily via config.
   Run a few minutes; revert.
2. **Force-trace specific request IDs.** If your
   client SDK supports it, set the trace header on
   suspect requests so they're always sampled.

Refer to your APM vendor's docs for the specifics.

---

## When the dashboards are down

The most uncomfortable case: an alert fires and you
can't view metrics because Grafana / Prometheus is
also broken.

Two fallbacks:

### Direct Prometheus endpoint

If Prometheus is up but Grafana is down, query
Prometheus directly:

```bash
curl -G 'http://prometheus:9090/api/v1/query' \
  --data-urlencode 'query=brain_hnsw_tombstone_ratio'
```

Returns JSON. Unfun to read but functional.

### Direct Brain endpoint

Brain's `/metrics` endpoint serves the raw Prometheus
text format:

```bash
curl http://brain-server:9091/metrics
```

This bypasses Prometheus entirely. You get a snapshot
of the *current* metrics (no history). Useful for
spot-checking "is this metric there at all?" and "what
is its value right now?"

### Logs as last resort

If even `/metrics` is unreachable, logs are the only
remaining signal. The substrate logs key events
(shard spawn, snapshot finish, worker cycles, errors)
to stdout. Even without aggregation, `journalctl` or
`docker logs` will tell you the substrate is at least
running.

---

## A typical diagnostic flow

How an experienced operator typically navigates the
three signals:

```
1. Alert fires. Open the linked runbook.
2. Open Grafana → Brain — Overview.
3. Identify the abnormal metric panel.
4. Drill into the per-shard or per-op dashboard.
5. Look for correlations (workers? disk? embedder?).
6. If symptoms point to a specific code path:
   a. Open the logs for the relevant subsystem.
   b. Filter to that target or shard.
   c. Look for recent ERRORs or unusual WARNs.
7. If logs don't explain it:
   a. Open traces, filter by op + duration.
   b. Look at the slowest few; find the dominant span.
   c. That's your bottleneck.
8. Match what you found to the runbook's Diagnose
   branches; proceed to Remediate.
```

The hierarchy is metrics → logs → traces, in order of
cheapness. Start cheap; go expensive only when you need
to.

---

## Anti-patterns

### Reading metrics without a hypothesis

Staring at Grafana hoping the anomaly will jump out at
you. Sometimes it does; often it doesn't and you
waste 20 minutes.

Better: form a hypothesis first ("I think the WAL is
slow"). Pick the metric that would confirm or deny
that hypothesis. Look at *that* metric.

### Drowning in DEBUG logs

Cranking log level to TRACE because "I want to see
everything" produces an unreadable stream. Pick a
specific target (e.g., `brain_embed=debug`) before
raising any log level.

### Treating traces as logs

Traces are for "where did this single request spend
its time?" Not for "what happened during this
30-minute window?" The trace UI is overwhelmed by
volume at that scale; use metrics + logs instead.

### Ignoring the labels

A metric without context is just a number. Always
note: which shard? which op? which time window? Two
queries that *look* equivalent may differ in the
label filters.

---

## Related runbooks

- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)
- [Reference: metrics catalog](../reference/metrics.md)
- [Architecture: observability](../guides/observability.md)

---

## Last validated

*Update on first use.*
