# 17.03 Alerts

The alert rules Brain ships with — what conditions trigger alerts, and what severity.

## 1. The alerting model

Brain ships Prometheus alert rules. Operators consume them via Alertmanager (or equivalent). Each rule has:

- A name.
- A condition (PromQL).
- A duration (how long the condition must hold before firing).
- A severity.
- Labels and annotations (for routing and context).

## 2. Severity levels

- **Critical (P1)**: page someone immediately. Brain down, data loss imminent.
- **High (P2)**: notify within hours. Persistent errors, degraded performance.
- **Medium (P3)**: review within day. Trends worth attention.
- **Low (P4)**: informational. Enabled for review but not paging.

Brain's defaults map issues to severity. Operators tune for their environment.

> **Series not yet emitted.** Four families these rules reference are declared
> as deferred in `crates/brain-server/src/metrics/mod.rs:31-40` and are absent
> from the `/metrics` body: `brain_embedder_*`, `brain_executor_*`,
> `brain_hnsw_recall_estimate` / `brain_hnsw_search_visits` /
> `brain_hnsw_rebuild_*`, and `brain_metadata_last_checkpoint_unixtime`. Each
> waits on instrumentation inside the component it measures — embedder hooks,
> Glommio reactor counters, HNSW search sampling. A rule written against one of
> them never fires, which is worse than no rule, so each is marked below with
> the emitted series to alert on meanwhile.


## 3. Critical alerts

### substrate_down

```yaml
- alert: BrainSubstrateDown
  expr: up{job="brain-server"} == 0
  for: 1m
  severity: critical
  summary: "Brain server is unreachable"
  description: "{{ $labels.instance }} has not been scraped successfully for 1 minute"
```

The server is unreachable. Page immediately. Brain halts the process on a
fail-stop invariant violation (CRC mismatch, WAL fsync failure), so a hard halt
surfaces here as a scrape failure rather than as a Brain-emitted series.

`up` is Prometheus's own scrape-result series, so it carries `job` and
`instance` and **never** a `shard` label — a description templating
`{{ $labels.shard }}` renders empty. The `job` value must match the scrape-job
name the rules ship with (`config/monitoring/alerts/brain-rules.yml:17`).

### high_error_rate

```yaml
- alert: BrainHighErrorRate
  expr: |
    sum(rate(brain_request_total{status="error"}[5m]))
      / clamp_min(sum(rate(brain_request_total[5m])), 1) > 0.05
  for: 5m
  severity: critical
  summary: "Brain error-response rate above 5%"
```

More than 5% of dispatched requests terminating in an ERROR status for 5
minutes.

`brain_request_total` carries exactly two labels, `op` and `status`
(`crates/brain-server/src/metrics/format.rs:1602-1612`) — there is no `shard`
label on the request family, so `by (shard)` yields one series with an empty
shard and a summary that templates to nothing. `status` takes exactly
`success`, `error` or `timeout` (`crates/brain-server/src/metrics/request.rs:56`);
a `status=~"error.*"` regex buys nothing over `status="error"`. The `clamp_min`
keeps the ratio defined when the denominator rate is zero.

### data_loss_potential

```yaml
- alert: BrainCheckpointStalled
  expr: |
    time() - max by (shard) (
      brain_worker_last_run_unixtime{worker="snapshot"}
    ) > 10800
  for: 15m
  severity: critical
  summary: "Checkpointing has stalled on shard {{ $labels.shard }}"
```

A checkpoint bounds how much WAL the next restart has to replay. When
checkpointing stops, recovery time grows without bound and WAL retention has
nothing to retire against.

`brain_metadata_last_checkpoint_unixtime` is not emitted, so the alert has to
watch the worker that writes CHECKPOINT records — the **snapshot** worker, not
the WAL-retention worker; retention deletes segments a checkpoint has already
made redundant, so it is downstream of the problem, not the problem. Pair the
staleness check with `brain_wal_segments`, which is what actually grows when
checkpointing stalls, and which has the advantage of being a direct measure
rather than a proxy:

```yaml
- alert: BrainWalSegmentsGrowing
  expr: delta(brain_wal_segments[6h]) > 0 and brain_wal_segments > 64
  for: 1h
  severity: high
  summary: "WAL segment count is growing on shard {{ $labels.shard }}"
```

`10800` is three hours: three times the cadence, which is **hourly and not
configurable**. `WorkerKind::Snapshot` carries a 3600 s interval
(`crates/brain-workers/src/config.rs:168`) and nothing overrides it.
`workers.snapshot_interval_sec` exists in the config schema and is validated
non-zero, but it is **read by no worker**: registration goes through
`register_phase8_workers`, whose signature takes no config argument
(`crates/brain-server/src/shard/mod.rs:2681-2689`), and which constructs
`SnapshotWorker::new(snapshot_source)` bare (`:2710`); the worker then takes
`WorkerConfig::defaults_for(WorkerKind::Snapshot)`
(`crates/brain-workers/src/workers/snapshot.rs:178`).

This is a known and deliberately guarded property, not an accident. A test
enumerates **eleven** `[workers] *_interval_sec` keys that reach no scheduler
and asserts the size of that list, so that "a cadence that reaches no scheduler
has to be a deliberate choice rather than an oversight that survives a release"
(`crates/brain-server/src/config/mod.rs:2078-2145`). Setting any of the eleven
changes nothing about when a worker runs.

Nor can the interval move at runtime: the scheduler reads it once into `cfg`
before entering the loop (`crates/brain-workers/src/scheduler.rs:377`) and
sleeps on `cfg.interval` every iteration (`:462`). The worker-control admin
routes offer `stop`, `start` and `run-now` — no cadence change.

**Two reasons this alert must not fire on a healthy shard**, both of which an
age-based rule gets wrong by default:

- A shard whose index is empty **never checkpoints at all**. Both the worker
  and the shutdown path skip it, so that a shard which never received an encode
  does not write a redundant `CHECKPOINT_BEGIN`/`END` pair and throw off
  consumers that reason about exact LSN positions
  (`crates/brain-workers/src/workers/snapshot.rs:228-239`,
  `crates/brain-server/src/shard/mod.rs:5160-5175`). Gate the rule on the shard
  holding data — `and on(shard) brain_hnsw_node_count > 0`.
- The worker skips its first tick, so the earliest checkpoint on a fresh
  process is at T+1h. Give the rule a startup grace longer than that, or it
  pages on every deploy.

There is also **no checkpoint worker** to look for: `WorkerKind` has no
`Checkpoint` variant, and the shared `write_checkpoint` helper has no
non-test caller — every production CHECKPOINT pair comes from a copy inlined in
`take_snapshot`, so the arena borrow never spans an `await`
(`crates/brain-server/src/shard/adapters.rs:540-560`). Three things produce one:
the hourly snapshot worker, graceful shutdown, and
`POST /v1/snapshots?shard=N`. Watching the `snapshot` worker therefore covers
the only automatic producer.

Do not confuse this cadence with WAL retention's, which is a different worker
doing a different job — retention deletes segments a checkpoint has already
made redundant. Its live cadence is **60 s**
(`crates/brain-workers/src/config.rs:160`), and
`workers.wal_retention_interval_sec` is inert by the same mechanism as the
snapshot key (`crates/brain-workers/src/workers/wal_retention.rs:163`,
registered at `crates/brain-server/src/shard/mod.rs:2706`). Retention cadence
says nothing about when a checkpoint last completed.

## 4. High alerts

### high_latency

```yaml
- alert: BrainHighLatency
  expr: histogram_quantile(0.99, sum by (le) (rate(brain_request_duration_ms_bucket[5m]))) > 50
  for: 10m
  severity: high
  summary: "Brain p99 request latency above 50ms"
```

p99 latency above target for a sustained period. Add `op` to the `by` clause to
page per operation instead of in aggregate — `op` is a real label on this
family, one of ten dispatched operations
(`crates/brain-server/src/metrics/request.rs:38-49`). `shard` is not.

### worker_stuck

```yaml
- alert: BrainWorkerStuck
  expr: sum(rate(brain_worker_cycles_total[15m])) == 0
  for: 15m
  severity: high
  summary: "Brain background workers have stalled"
```

No worker cycle completed anywhere — decay, consolidation, extraction and
statement-embed may all be wedged behind one scheduler.

The aggregate form is deliberate: `brain_worker_last_run_unixtime` is emitted
per `(shard, worker)` across the full worker set
(`crates/brain-workers/src/config.rs:96-135`), so a per-series staleness rule
pages once per worker per shard the first time any infrequent worker's interval
exceeds the window. Narrow to a named worker when one matters on its own:

```yaml
  expr: time() - brain_worker_last_run_unixtime{worker="statement_embed"} > 7200
```

### high_memory_pressure

```yaml
- alert: BrainMemoryPressure
  expr: |
    process_memory_resident_bytes
      / on(instance) group_left() node_memory_MemTotal_bytes > 0.85
  for: 10m
  severity: high
  summary: "Brain resident memory above 85% of machine RAM"
```

Brain is using > 85% of available RAM.

The emitted name is `process_memory_resident_bytes` — resource-first, then
quality (`crates/brain-server/src/metrics/format.rs:399-407`). `MemTotal` comes
from node_exporter, a different scrape job with a different label set, so the
bare division matches no series and silently returns empty; the vector match
has to be pinned with `on(instance) group_left()`. These are whole-process
figures: Brain is one process with per-core shards, so there is no per-shard
RSS to template into the summary.

### disk_filling

```yaml
- alert: BrainDiskFilling
  expr: |
    predict_linear(
      node_filesystem_free_bytes{fstype!~"tmpfs|overlay"}[1h], 24*3600
    ) < 0
  for: 30m
  severity: high
  summary: "Disk projected to fill in 24h on {{ $labels.mountpoint }}"
```

Linear projection: disk fills within 24 hours.

Also a node_exporter series, one per filesystem — the labels are `device`,
`mountpoint` and `fstype`, never `shard`. Without a filter the rule fires on
every pseudo-filesystem on the host; scope it to the mount holding the data
directory.

## 5. Medium alerts

### tombstone_high

```yaml
- alert: BrainHighTombstoneRatio
  expr: brain_hnsw_tombstone_ratio > 0.30
  for: 1h
  severity: medium
  summary: "Tombstone ratio > 30% on {{ $labels.shard }}"
```

HNSW degraded. Maintenance worker should rebuild; check why it isn't.

### recall_quality_degraded

```yaml
- alert: BrainRecallQualityDegraded
  expr: brain_hnsw_recall_estimate < 0.85
  for: 30m
  severity: medium
  summary: "Estimated recall < 85% on {{ $labels.shard }}"
```

HNSW recall has degraded. Rebuild needed.

`brain_hnsw_recall_estimate` is not emitted, and the figure behind it is not
merely missing — it is a **constant**. The maintenance worker stamps
`recall_estimate = 1.0` because query-sample logging is not wired
(`crates/brain-workers/src/workers/hnsw_maint.rs:14-15`, `:211`). So the rule
above cannot fire, and neither can either recall branch of the worker's own
rebuild decision (`:92`, `:95`): a real estimate needs a sampled exact-search
comparison that does not exist yet.

The emitted proxy is
`brain_hnsw_tombstone_ratio`, which is the mechanism behind most recall decay —
a graph thick with tombstones is one whose neighbour lists point at deleted
nodes.

That proxy is the same series the tombstone alert above already watches, so a
recall-quality rule is a second, tighter threshold on it rather than a second
metric. The threshold to pick is **0.15 to 0.20, not 0.30**, and the reason is
specific: the worker rebuilds by itself above 0.30, but between its
`tombstone_schedule` of 0.15 and that 0.30 it emits a log line and returns
without queueing anything (`crates/brain-workers/src/workers/hnsw_maint.rs:68-74`,
`:220-225`). That band is the gap an alert has to cover, because nothing in the
system reclaims it — a shard parked at 0.2 degrades until an operator rebuilds
it by hand (RB-6).

`brain_hnsw_tombstone_ratio` carries `shard`
(`crates/brain-server/src/metrics/format.rs:574-578`), so both thresholds
template `{{ $labels.shard }}` correctly.

### embedder_slow

```yaml
- alert: BrainEmbedderSlow
  expr: histogram_quantile(0.99, sum(rate(brain_embedder_duration_ms_bucket[5m])) by (le)) > 50
  for: 10m
  severity: medium
  summary: "Embedder p99 > 50ms"
```

Embedder is slower than expected.

The `brain_embedder_*` family is not emitted. Until it is, embed cost shows up
inside `brain_request_duration_ms{op="encode"}`, which is emitted and which an
embedder stall dominates.

### connections_growing

```yaml
- alert: BrainConnectionsChurning
  expr: rate(brain_connections_closed_total[5m]) > 20
  for: 10m
  severity: medium
  summary: "Connections are churning"
```

Clients are opening and closing connections rapidly. Check client pooling and
the admission limits. Both `brain_connections_total` (lifetime accepts) and
`brain_connections_closed_total` are emitted process-wide; only the closed
counter carries a label, `reason`, over five values — `bye`,
`protocol_error`, `timeout`, `eof`, `fatal`
(`crates/brain-server/src/network/connection.rs:270-271`). Breaking the rate
down `by (reason)` separates orderly client turnover from protocol faults.

## 5a. Derivation-pipeline alerts

A write is acknowledged before its derivation runs, so the queue between the two
is where a healthy-looking server silently stops learning. These watch it.

```yaml
- alert: BrainExtractionQueueUnobserved
  expr: |
    (absent(brain_extraction_queue_depth) and on() (max(brain_up) == 1))
      or (count by (instance) (brain_extraction_queue_depth) < scalar(max(brain_shards_total)))
  for: 10m
  severity: critical
  summary: "A shard is not reporting its derivation backlog"

- alert: BrainDerivationQueueStale
  expr: max by (instance) (brain_extraction_queue_oldest_age_seconds) > 3600
  for: 30m
  severity: high
  summary: "Oldest queued memory has waited over an hour for derivation"

- alert: BrainExtractorApplyDropping
  expr: increase(brain_extractor_apply_dropped_total[15m]) > 0
  for: 5m
  severity: high
  summary: "Extracted typed-graph items are failing to persist"

- alert: BrainDerivationAdmissionStarved
  expr: brain_derivation_admission_available_bytes == 0
  for: 15m
  severity: medium
  summary: "Derivation admission budget has been fully held for 15m"
```

The first is the unusual one and the reason the exposition emits no `0`
placeholder for a shard whose metadata read failed: a fabricated zero is a
silent all-clear, while a missing series is a legible gap
(`crates/brain-server/src/metrics/format.rs:681-731`). `absent()` catches the
family vanishing wholesale; the series count against `brain_shards_total`
catches one shard going quiet while the rest report.

`brain_extractor_apply_dropped_total` counts real extracted items the apply
pass could not persist — any nonzero value is signal loss, which is why it
pages rather than tickets. Its benign counterpart is
`brain_extractor_schema_filtered_total`, candidates dropped because their type
is not declared; do not alert on that one.

## 6. Low alerts

### config_change

```yaml
- alert: BrainConfigChanged
  expr: |
    count(
      count by (shard_count, arena_capacity_bytes, hnsw_m, embedder_model)
        (brain_config_info)
    ) > 1
  severity: low
```

Configuration changed in the last hour. Informational.

`brain_config_info` is an info-style gauge whose value is always `1`; the
payload rides in the labels (`crates/brain-server/src/metrics/format.rs:126-141`).
`changes()` inspects the *value*, which never moves, so a `changes()` rule on
this family cannot fire. A config change instead retires one label combination
and starts another, which is what counting distinct label sets detects. The
same holds for `brain_build_info` and a deployment-tracking rule built on it.

### worker_errors_warning

```yaml
- alert: BrainWorkerErrorWarning
  expr: rate(brain_worker_errors_total[1h]) > 0.01
  for: 1h
  severity: low
```

Workers had errors (low rate). Worth review. `brain_worker_errors_total`
carries `shard` and `worker`, so the firing series names both. A panicked cycle
is counted here too and separately in `brain_worker_panics_total` — a panic is a
bug rather than a retryable failure, so it warrants its own rule at a higher
severity.

### trace_export_failing

```yaml
- alert: BrainTraceExportFailing
  expr: increase(brain_tracing_export_errors_total[15m]) > 0
  for: 5m
  severity: low
```

The OTLP exporter reported failed or timed-out attempts — traces are not
reaching the collector. Its companion,
`brain_tracing_spans_dropped_total`, counts spans discarded because the batch
buffer was full, which is back-pressure rather than a broken pipe. Both are
process-global and labelless
(`crates/brain-server/src/metrics/format.rs:86-111`).

### extractor_backpressure

```yaml
- alert: BrainExtractorBackpressure
  expr: increase(brain_extractor_drops_total[1h]) > 0
  severity: low
```

The extractor channel was full and enqueues were dropped: the write succeeded
and its derivation did not happen. Low severity only because the durable queue
usually catches the memory on a later pass; sustained firing alongside
`BrainDerivationQueueStale` is the pairing that means work is being lost.

## 7. The "alert grouping"

Alertmanager groups related alerts:

```yaml
group_by: [alertname, severity]
group_wait: 30s
group_interval: 5m
```

Group on labels the alerts actually carry. Only the rules whose expression
preserves `shard` — the HNSW, arena, worker and writer families — emit one, so
`group_by: [shard, ...]` collapses everything else into a single
empty-shard group.

A burst of alerts (e.g., a shard going down triggers many) becomes a single notification with all alerts.

## 8. The "alert routing"

Different severities go to different destinations:

```yaml
routes:
  - match:
      severity: critical
    receiver: pagerduty
  - match:
      severity: high
    receiver: slack-oncall
  - match:
      severity: medium
    receiver: slack-team
  - match:
      severity: low
    receiver: email
```

Configurable per-deployment.

## 9. The "silencing"

For maintenance windows, operators silence alerts:

```
amtool silence add alertname=BrainSubstrateDown instance=<host:port> -d 2h
```

Silences alerts during planned work.

## 10. The "auto-resolution"

Most alerts resolve when the condition clears:

- Latency drops back → alert resolves.
- Worker runs → alert resolves.
- Connection restored → alert resolves.

Critical alerts may stay "fired" longer for ack tracking.

## 11. The thresholds

Default thresholds as shipped:

- Latency p99 > 50 ms = high.
- Error rate > 5% = critical.
- Tombstone ratio > 30% = medium.
- Arena above 90% of capacity = medium.
- Derivation backlog oldest-age > 1 h = high.

Operators tune to match their SLOs:

```yaml
# Production SLO: p99 < 50 ms.
- alert: ProdLatencyHigh
  expr: histogram_quantile(0.99, ...) > 50
  ...
```

## 12. The "noise"

False positives are a real cost. Brain's defaults err toward fewer alerts:

- Sustained-condition requirements (`for: 5m`).
- Reasonable thresholds.
- Reasonable severities.

Operators monitor "alert noise" and adjust:

```
ALERTS{alertstate="firing", severity=}
```

These are Alertmanager's own series, not Brain's: Brain exports no metric about
its own alerts, and nothing named `brain_alerts_*` exists. Alert noise is
measured where the alerts are evaluated.

## 13. The runbook links

Each alert has a runbook URL:

```yaml
- alert: BrainSubstrateDown
  annotations:
    runbook: "RB-1"
```

When paged, the responder gets a pointer into the runbooks (next file), keyed
by the runbook id.

## 14. The "test alerts"

In staging:

- Synthetic alerts to verify routing works.
- "Test fire" via Alertmanager API.

This ensures the alert pipeline is healthy.

## 15. The alert-rule deployment

The rules live at `config/monitoring/alerts/brain-rules.yml` and are loaded
from file by Prometheus. CI gates them: the suite asserts the file's shape, the
required-alert set, the severity set, **and** that every metric an expression
references is one the server actually emits — the check parses the emitted
names out of `crates/brain-server/src/metrics/format.rs`, so a rule cannot
reference a series that does not exist. `promtool check rules` is the
authoritative PromQL check where the Prometheus toolchain is available.

## 16. The "alert fatigue"

Common signs:

- Same alert fires repeatedly.
- Alerts ignored.
- Many simultaneous alerts confuse responders.

Mitigations:

- Tighten thresholds.
- Group related alerts.
- Suppress predictable alerts (e.g., during maintenance).
- Review and prune low-value alerts.

Brain's defaults are reasonable starting points; operators iterate.

## 17. The SLO-based alerting (advanced)

Alternative to threshold alerts: SLO-based alerts.

```
Error budget: 99% of requests should succeed (1% error budget).
If 25% of monthly budget is consumed in 1 hour, alert.
```

This is more nuanced than threshold alerts. Brain doesn't ship SLO alerts by default (too deployment-specific) but the metrics support implementing them.

---

*Continue to [`04_admin_ops.md`](04_admin_ops.md) for admin operations.*
