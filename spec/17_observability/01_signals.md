# 17.01 Signals (Metrics, Logs, Traces)

> **TL;DR.** Brain emits three observability signals: Prometheus / OpenMetrics-format metrics (counters, gauges, histograms — per-shard and per-operation, never per-tenant), structured logs in a compact or JSON format with consistent fields, and OpenTelemetry traces that nest the shard's spans under the connection layer's request span. Traces are server-side only: the wire carries no inbound trace context. All three are continuously emitted; operators consume them via Prometheus / Loki / Tempo or equivalents.

## Metrics

The metrics Brain exposes — what they mean, how to read them, and why they exist.

## 1. The format

Brain emits metrics in OpenMetrics format (Prometheus-compatible) on the `/metrics` HTTP endpoint:

```
# HELP brain_request_total Total requests.
# TYPE brain_request_total counter
brain_request_total{op="encode",status="success"} 12345
brain_request_total{op="recall",status="success"} 67890

# HELP brain_request_duration_ms Request duration histogram.
# TYPE brain_request_duration_ms histogram
brain_request_duration_ms_bucket{op="encode",le="0.005"} 100
brain_request_duration_ms_bucket{op="encode",le="0.010"} 9000
brain_request_duration_ms_bucket{op="encode",le="0.025"} 12000
brain_request_duration_ms_bucket{op="encode",le="+Inf"} 12345
```

The endpoint is on a separate port from the data plane (default 9091).

## 2. The metric naming

Brain follows Prometheus conventions:

- All metrics start with `brain_`.
- `_total` suffix for counters.
- `_seconds` or `_ms` suffix for durations.
- `_bytes` suffix for sizes.
- Label keys are lowercase snake_case.

Per the [Prometheus naming guide](https://prometheus.io/docs/practices/naming/).

## 3. Request metrics

Per-operation counters and histograms:

```
brain_request_total{op=, status=}
brain_request_duration_ms{op=}           # histogram
brain_request_active{op=}                # currently in-flight
```

The request family is **process-wide, not per-shard**. It is recorded at the
connection layer before the request reaches a shard, so there is no `shard`
label on any of the three
(`crates/brain-server/src/metrics/format.rs:1595-1635`). Per-shard read and
write volume comes from `brain_query_*` and `brain_writer_*`, which are
recorded inside the shard and do carry one.

`op` takes exactly ten values — the operations that travel the dispatch path
(`crates/brain-server/src/metrics/request.rs:38-49`):

```
encode, recall, plan, reason, forget, link, unlink,
txn_begin, txn_commit, txn_abort
```

The subscribe family, `ping`, `hello`, `auth` and `bye` are **not** on that
path and are not counted here; they are visible through the connection and
frame counters instead (`crates/brain-server/src/metrics/request.rs:122-125`).
There is no `admin_*` op: admin traffic arrives on its own HTTP listener.

`status` takes exactly three values — `success`, `error`, `timeout`
(`crates/brain-server/src/metrics/request.rs:51-56`). `error` is the catch-all
for every wire-level error response; there is no `error_<code>` form, so a
matcher like `status=~"error.*"` buys nothing over `status="error"` and a
per-code breakdown has to come from the logs.

## 4. Server and shard metrics

```
brain_up                                      # 1 if accepting requests
brain_build_info{version=, git_commit=}
brain_config_info{shard_count=, arena_capacity_bytes=, hnsw_m=, embedder_model=}
brain_shards_total
```

`brain_up` is liveness only. `brain_build_info` and `brain_config_info` are
info-style gauges whose value is always `1`; the labels carry the payload, which
is what lets an operator join "this behaviour" to "this build and this config"
in a query.

## 5. Storage metrics

```
brain_arena_used_bytes{shard=}        # bytes backing allocated slots (occupied + tombstoned)
brain_arena_capacity_bytes{shard=}
brain_arena_slots_used{shard=}        # allocated (occupied + tombstoned)
brain_arena_slots_free{shard=}        # reclaimed, on the free list
brain_wal_size_bytes{shard=}
brain_wal_segments{shard=}
brain_metadata_size_bytes{shard=}
```

Note that `used` and `slots_used` both **include tombstoned** slots: they
measure what is allocated, not what is live. The difference between them and
the live memory count is the reclamation backlog, which is the thing worth
alerting on.

## 5a. Write-path metrics

The write path publishes its own family, which is where a slow write is
diagnosed — `brain_request_duration_ms{op="encode"}` says a write is slow, these
say which part of it is:

```
brain_writer_stage_duration_ms{shard=,stage=}      # histogram, per write stage
brain_writer_submit_total{shard=,phase=,outcome=}
brain_writer_submit_duration_seconds{shard=,phase=} # histogram
brain_writer_idempotency_total{shard=,outcome=}    # hit | miss | conflict
brain_writer_wal_skip_total{shard=,phase=}
brain_writer_apply_errors_total{shard=,phase=,error=}
```

`stage` takes exactly five values
(`crates/brain-ops/src/metrics/write_stage.rs:111-117`):

| `stage` | What it times |
|---|---|
| `wal_append` | durable WAL append through the shard's sink, including the group-commit wait |
| `redb_apply` | every phase applied into the open write transaction, plus the idempotency stamp — all the redb work *before* the commit |
| `redb_commit` | `WriteTransaction::commit` on the shard's metadata DB |
| `hnsw_insert` | vector insert into the shard's HNSW graph |
| `tantivy_index` | hand-off of the memory text to the lexical indexer queue (the queue hand-off, not the indexing) |

`redb_apply` and `redb_commit` are deliberately separate. Apply is how much
work the write asks of redb; commit is what durability costs. One combined
`redb` timer cannot tell a wide multi-phase write from a slow disk, and those
have different remedies.

Two consequences for reading these:

- `wal_append` includes the group-commit window, so under light load it sits
  near the 100 µs window rather than near the device's latency. A rise under
  light load is the device; a fall under heavy load is batching working.
- `tantivy_index` is a hand-off, so it is small and flat. Lexical-index lag
  shows up in the indexer's own worker metrics, not here.

Mind the two label vocabularies: `stage` on the stage histogram takes the five
values above, while `phase` on the submit and skip counters names the write
phase (`upsert_memory`, `link`, …). They are different axes — a stage is a step
every write goes through, a phase is a kind of work a particular write asks
for — and a query that mixes them up returns nothing rather than erroring
(`crates/brain-server/src/metrics/format.rs:1175-1292`).

## 6. HNSW metrics

```
brain_hnsw_node_count{shard=}                 # active nodes
brain_hnsw_tombstone_count{shard=}            # tombstoned nodes
brain_hnsw_tombstone_ratio{shard=}            # tombstoned / (active + tombstoned), 0..1
```

The ratio is exposed as its own gauge rather than left to PromQL because it is
the alert threshold operators actually use, and computing it from two gauges
sampled at different instants produces spurious spikes.

## 7. Query and retrieval metrics

```
brain_query_total{shard=}                             # recalls served
brain_query_latency_ms{shard=}                        # end-to-end recall latency (histogram)
brain_query_outcome_total{shard=, outcome=}           # single | many | none
brain_query_rerank_invoked_total{shard=}              # recalls the rerank stage reordered
brain_query_fusion_k{shard=}                          # effective adaptive fusion-k, per served recall (histogram)

brain_retriever_invocations_total{shard=, retriever=} # semantic | lexical | graph
brain_retriever_candidates_total{shard=, retriever=}
brain_retriever_latency_ms{shard=, retriever=}        # per-lane wall-clock (histogram)
```

`brain_query_outcome_total` is the one to watch for retrieval quality: a rising
`none` share is the signal that reads are degrading, and it is visible without
any per-query inspection. `rerank_invoked_total` stays at zero on a default
deployment, since rerank ships disabled.


## 7a. Aggregate metrics

```
brain_agg_counters_ready{shard=}                      # 1 = counters complete; 0 = online build after upgrade
brain_agg_counter_build_rows_total{shard=}            # memories counted by that build
brain_agg_counter_verify_runs_total{shard=}           # full verify passes ([aggregates] verify_interval_secs)
brain_agg_counter_drifted_buckets_total{shard=}       # buckets found wrong and repaired — alert if > 0
brain_memory_aggregate_counters_total{shard=}         # MEMORY_AGGREGATE answered from live counters
brain_memory_aggregate_snapshot_total{shard=}         # ... answered from the analytics snapshot
brain_memory_aggregate_errors_total{shard=}
brain_analytics_exports_total{shard=}                 # [analytics] only
brain_analytics_export_failures_total{shard=}
brain_analytics_last_export_seconds{shard=}
brain_analytics_snapshot_rows{shard=}
brain_analytics_snapshot_age_seconds{shard=}          # staleness of snapshot answers
```

`brain_agg_counter_drifted_buckets_total` should stay 0: every write to the
memories table updates the counters in the same transaction, so a non-zero value
means some write path bypassed them (the verify pass has already repaired the
numbers, but the bypass is a bug). `brain_analytics_snapshot_age_seconds`
should stay below `[analytics] export_interval_secs` plus one export's duration.
## 8. Extractor and derivation metrics

```
brain_extractor_drops_total{shard=}                      # encode-side enqueues dropped (channel full)
brain_extractor_cycle_duration_seconds{shard=}           # histogram
brain_extractor_tier_runs_total{shard=, tier=, status=} # per-tier outcome per memory
brain_extractor_items_written_total{shard=, item_kind=}  # typed-graph rows persisted, by item kind
brain_extractor_schema_filtered_total{shard=, predicate=} # dropped: predicate/relation_type not in an active schema
brain_extractor_apply_dropped_total{shard=, reason=}     # real items the apply pass could not persist
brain_extractor_predicate_consolidated_total{shard=}     # open-vocab surface forms folded onto a near neighbour
brain_extractor_resolver_outcome_total{shard=, tier=}    # which resolver tier satisfied each mention
brain_extractor_llm_micro_usd_spent_total{shard=}        # LLM-tier spend, in 1e-6 USD
```

The three label vocabularies on this family are fixed sets
(`crates/brain-ops/src/metrics/histograms.rs:37-50`):

| Label | Family | Values |
|---|---|---|
| `tier` | `tier_runs_total` | `pattern`, `classifier`, `llm` |
| `status` | `tier_runs_total` | `ran`, `skipped`, `failed` |
| `item_kind` | `items_written_total` | `entity`, `statement`, `relation`, `mention`, `hype` |
| `tier` | `resolver_outcome_total` | `exact`, `alias`, `fuzzy`, `embedding`, `disambiguated`, `create` |

Note that `tier` means two different things across the two families it appears
on: extraction tiers on `tier_runs_total`, resolution strategies on
`resolver_outcome_total`. The second is not a subset of the first.

`schema_filtered_total` is the one family here whose label value is **not** a
fixed set: it keys on `predicate`, the qname that was rejected, so its
cardinality follows the open predicate vocabulary a deployment actually
produces (`crates/brain-server/src/metrics/format.rs:1391-1403`). Watch it
with that in mind.

Two of these are easy to confuse and are deliberately separate:
`schema_filtered_total` counts candidates dropped **by policy** (their type is
not declared), which is normal and expected; `apply_dropped_total` counts real
items the apply pass **could not persist**, which is not. A nonzero rate on the
second deserves investigation.

`llm_micro_usd_spent_total` is the cost surface. It is a counter in dollar-micro
units, so a spend rate is `rate(...)/1e6` dollars per second.

```
brain_auto_edge_edges_written_total{shard=}              # logical SimilarTo edges persisted
brain_auto_edge_neighbours_found_per_cycle{shard=}       # above-threshold neighbours per cycle (histogram)
brain_auto_edge_cycle_duration_seconds{shard=}
brain_auto_edge_drops_total{shard=}

brain_temporal_edge_edges_written_total{shard=}          # logical FollowedBy edges persisted
brain_temporal_edge_gap_seconds{shard=}                  # predecessor→memory gap distribution (histogram)
brain_temporal_edge_skipped_total{shard=, reason=}       # enqueues that produced no edge
brain_temporal_edge_cycle_duration_seconds{shard=}
brain_temporal_edge_drops_total{shard=}

brain_causal_edge_edges_written_total{shard=}            # logical Caused edges persisted
brain_causal_edge_skipped_total{shard=, reason=}
brain_causal_edge_predicate_whitelist_resolved{shard=}   # causal predicates resolved against the active schema
brain_causal_edge_cycle_duration_seconds{shard=}
brain_causal_edge_drops_total{shard=}

brain_statement_embed_cycles_total{shard=}
brain_statement_embed_rows_embedded_total{shard=}        # statements inserted into the statement HNSW
brain_statement_embed_rows_skipped_total{shard=}         # dropped without embedding (tombstoned, superseded)
brain_statement_embed_errors_total{shard=}               # embedder batch failures
brain_statement_embed_batch_duration_seconds{shard=}
```

Each derivation worker exposes the same four shapes — written, skipped, cycle
duration, and **drops**. `drops_total` is the one that matters structurally: it
counts enqueues lost because the worker's channel was full, which means the
write succeeded and its derivation silently did not happen. It should be zero.

`*_skipped_total` is the benign counterpart — an enqueue that legitimately
produced no edge — and carries a `reason` label so the two never have to be told
apart by guesswork.

## 8a. Derivation backlog metrics

The durable extraction queue — where an acknowledged write waits to become
statements, entities and relations:

```
brain_extraction_queue_depth{shard=}                # memories queued, awaiting the extractor
brain_extraction_queue_oldest_age_seconds{shard=}   # how long the oldest has waited
```

This is a separate family from `brain_extractor_*` for a structural reason:
those are worker counters, which vanish when the worker is off, while the queue
is a table on disk that keeps growing exactly then
(`crates/brain-server/src/metrics/format.rs:681-731`). A derivation outage is
invisible on the worker counters and obvious here.

Depth is the leading indicator, because writes arrive far faster than
derivation drains them. Age is the one that says how far a workspace has
already drifted from its own data: a memory that has waited an hour is a
question the system would now answer wrongly.

Both are read from shard metadata on each scrape, and a shard whose read
failed emits **no rows rather than zeros** — a fabricated `0` is a silent
all-clear, an absent series is a legible gap. That is why the queue has an
alert on *missing* series as well as on the values.

## 8b. Derivation admission metrics

What provider calls and prepared context are holding right now, and the budget
that bounds them (`crates/brain-server/src/metrics/format.rs:163-284`):

```
brain_llm_calls_in_flight                           # provider calls outstanding
brain_llm_bytes_in_flight                           # request payload bytes outstanding
brain_llm_peak_calls_in_flight                      # high-water mark since start
brain_llm_peak_bytes_in_flight                      # high-water mark since start

brain_derivation_prep_holds_in_flight               # prepared-context reservations
brain_derivation_prep_bytes_in_flight               # neighbour texts, candidate embeddings
brain_derivation_prep_peak_bytes_in_flight          # high-water mark since start

brain_derivation_bytes_in_flight                    # payload + prep, held together
brain_derivation_admission_budget_bytes             # what derivation may hold at once
brain_derivation_admission_available_bytes          # still admittable right now
```

Every one is a process-global gauge with **no `shard` label**. Shards are
share-nothing for data, but they face one provider over one connection pool, so
this pressure belongs to the process rather than to any shard.

`brain_derivation_bytes_in_flight` is the figure the budget actually bounds.
Payload and prep are metered apart because the remedies differ — payload
pressure is relieved by narrowing LLM fan-out, prep pressure by narrowing the
batch or the neighbour fan-out that feeds it — but they are admitted against
one budget, so neither pool alone tells an operator how close derivation is to
parking.

`available_bytes` sustained at zero means derivation is waiting on memory, not
on the provider. That is the distinction this family exists to draw: a backlog
with available budget is a slow provider, a backlog with no available budget is
a sizing problem.

The peak gauges are the sizing input. The budget is a byte bound, and the
footprint of one call — a long text with a large response schema is not the
same load as a one-liner at the same call count — is what the peaks measure.

## 9. Worker metrics

Every worker, whatever it does, exposes the same set:

```
brain_worker_cycles_total{shard=, worker=}
brain_worker_processed_total{shard=, worker=}
brain_worker_errors_total{shard=, worker=}
brain_worker_panics_total{shard=, worker=}      # a subset of errors_total
brain_worker_cycle_duration_ms{shard=, worker=} # most recent successful cycle
brain_worker_last_run_unixtime{shard=, worker=} # last attempted cycle: success, error, or caught panic
brain_worker_pending_work{shard=, worker=}      # last-observed estimate of outstanding work
```

The `worker` label is the worker's registry name, a fixed snake_case set
declared alongside the worker kinds (`crates/brain-workers/src/config.rs:96-135`)
— `access_boost`, `consolidation`, `hnsw_maintenance`,
`idempotency_cleanup`, `slot_reclamation`, `wal_retention`, `edge_scrub`,
`counter_reconcile`, `statistics`, `embedder_cache_evict`, `snapshot`,
`backfill`, `forget_cascade`, `schema_migration`, `supersession_sweeper`,
`statement_reclaim`, `predicate_gc`, `audit_log_sweeper`, `llm_cache_sweeper`,
`stale_extraction_detector`, `entity_gc`, `auto_edge`, `extractor`,
`temporal_edge`, `causal_edge`, `statement_embed`, `confidence_sweep`,
`ambiguity_resolver`. The name is the full word: `hnsw_maintenance`, not an
abbreviation of it.

A worker contributes rows only on shards where it is registered, so a selector
naming a worker that is disabled in a deployment returns nothing rather than
zero.

`brain_worker_panics_total` is broken out of `errors_total` rather than folded
into it because the two call for different responses: an error is a cycle that
failed and will retry, a panic is a bug. **Nonzero is worth alerting on.**

`last_run_unixtime` advances on a *failed* cycle too. A worker that is erroring
every cycle still looks alive on this gauge, which is intentional — the gauge
answers "is the scheduler running it", and `errors_total` answers "is it
working". A stalled worker is the one where this timestamp stops moving.

## 9a. Connection and frame metrics

```
brain_connections_active                        # currently in flight
brain_connections_total                         # lifetime accepts
brain_connections_closed_total{reason=}
brain_connections_rejected_total                # shed at accept by the admission gate (global or per-IP cap)
brain_frame_recv_total
brain_frame_send_total
brain_frame_size_bytes{direction=}              # recv | send (histogram)
```

`connections_rejected_total` counts connections refused *before* they became
connections, which is why it is separate from `closed_total` — a shed connection
never opened and would otherwise be invisible. It is a single labelless series:
the gate sheds at accept time, before the protocol has said anything that could
classify the reason (`crates/brain-server/src/metrics/format.rs:311-321`).

`closed_total` does carry `reason`, over five values — `bye`,
`protocol_error`, `timeout`, `eof`, `fatal`
(`crates/brain-server/src/network/connection.rs:270-271`). `bye` is an orderly
client goodbye; the other four are not.

Every family in this section is process-wide. None of them carries a `shard`
label: connections terminate at the connection layer, which fronts all shards.

## 9b. Tracing metrics

```
brain_tracing_spans_dropped_total               # export buffer was full
brain_tracing_export_errors_total               # OTLP export failed or timed out
```

The observability pipeline reports its own failures. A dropped span is a hole in
the trace that nothing else would reveal.

## 10. Resource metrics

Standard Linux process metrics, read from `/proc/self`:

```
process_cpu_seconds_total          # from /proc/self/stat
process_memory_resident_bytes      # VmRSS
process_memory_virtual_bytes       # VmSize
process_open_fds                   # entries in /proc/self/fd
process_uptime_seconds             # since admin-server start
process_start_time_seconds         # unix timestamp of process start
```

Mind the word order on the two memory gauges: resource first, then quality —
`process_memory_resident_bytes`, not `process_resident_memory_bytes`
(`crates/brain-server/src/metrics/format.rs:399-416`). The transposed spelling
is the one most dashboards reach for by reflex, and it returns an empty series.

A `/proc` read that fails emits **no** line rather than a zero, so these can be
legitimately absent on a scrape without anything being wrong with the process.

These are read on scrape rather than tracked continuously, which is why they are
standard-named and unprefixed: they describe the process, not Brain.

## 11. The "rate" derivations

Most metrics are counters. Rates (req/s, errors/s) are derived in PromQL:

```
rate(brain_request_total{op="encode",status="success"}[5m])
sum(rate(brain_request_total[5m])) by (op)
```

The dashboards (next file) show these.

## 12. Histogram buckets

Default histogram bucket boundaries, in milliseconds:

```
1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, +Inf
```

Thirteen bounds plus the `+Inf` overflow bucket, so fourteen counts per
histogram (`crates/brain-server/src/metrics/histogram.rs:31-34`). The bounds are
**fixed in the binary**, not configurable: histogram bounds that differ between
deployments cannot be aggregated across them, and the whole point of exposing a
histogram rather than a quantile is that it aggregates.

These are the default bounds, which the request and query latency families use.
A family whose range is known to be narrower sets its own — the write-stage
histogram gives each stage its own bounds, because a WAL append and a Tantivy
hand-off do not share a useful scale. Read the `le` values on the series rather
than assuming the defaults.

A millisecond histogram accumulates its `_sum` scaled by 1000 and divides at
exposition, which is how the rendered `_sum` carries a decimal place without an
atomic float; byte and item histograms use a scale of 1 and their `_sum` is the
exact integer total (`crates/brain-server/src/metrics/histogram.rs:8-19`).
Bucketing is unaffected — only the `_sum` is scaled — so `histogram_quantile`
needs no adjustment.

## 13. Cardinality

Labels with high cardinality (per-space, per-session) create explosion. Brain avoids them in metrics:

- ✓ Shard label: low cardinality, one series per configured shard.
- ✓ Operation label: low cardinality, exactly ten values.
- ✓ Retriever / tier / worker / reason labels: low cardinality, fixed sets.
- ✗ `space_id`: high cardinality (up to 10⁶ per namespace); **never** a metric label.
- ✗ `session_id`: unbounded, client-chosen; never a metric label.
- ✗ `namespace`: bounded but tenant-identifying; not a metric label.

Per-space observability uses logs and the audit trail, not metrics. This is the
one rule in this file that is not a preference: a `space_id` label on a counter
multiplies every series by the tenant count, and on a shard serving a million
spaces that is a metrics-system outage, not a slow dashboard.

## 14. Sampling

Sampling is the design for two HNSW signals — search visits on every Nth
search, a recall-quality estimate on an hourly exact-search comparison — and
the sampling infrastructure those need does not exist yet, so neither series is
emitted (`crates/brain-server/src/metrics/mod.rs:31-40`).

Everything in the families above is every-event, which is affordable because
all of them are fixed-cardinality counters, gauges or fixed-bucket histograms
read on scrape. Volume is bounded by the label rules in the previous section
rather than by sampling.

The emitted stand-in for index quality is `brain_hnsw_tombstone_ratio`: a graph
thick with tombstones is one whose neighbour lists point at deleted nodes,
which is the mechanism behind most recall decay.

## 15. Labels: shard ID

The `shard` label is the shard's **index**, a small unsigned integer counting
from zero (`crates/brain-core/src/ids.rs:27`). It is not a UUID:

```
brain_arena_slots_used{shard="0"} 1234
brain_arena_slots_used{shard="1"} 987
```

Cardinality is therefore the configured shard count, which `brain_shards_total`
reports and `brain_config_info{shard_count=}` carries as a label — so a query
can check that every shard is reporting without knowing the deployment's size
in advance.

For dashboards, operators may add a friendly name via metric relabeling:

```
brain_arena_slots_used{shard="0", shard_name="prod-shard-0"} 1234
```

## 16. The `up` metric

`up` is Prometheus's own scrape-result series, not one Brain emits. It carries
the scrape job's labels — `job` and `instance` — and never a `shard` label:

```
up{job="brain-server", instance="10.0.0.4:9091"} 1
```

`up=0` means the server did not answer the scrape. Alerting on `up == 0`
catches outages, and because Brain halts the process on a fail-stop invariant
violation, a hard halt surfaces here rather than in any Brain-emitted series.

Brain also emits its own `brain_up`, always `1` while the `/metrics` handler is
answering (`crates/brain-server/src/metrics/format.rs:143-151`). It is a
liveness marker for "this body came from a live server", useful as the `and
on()` guard in a rule about a *missing* series — it cannot itself report `0`,
because a server that cannot answer cannot emit it.

## 17. The `_info` metrics

Static information, value always `1`, payload in the labels
(`crates/brain-server/src/metrics/format.rs:113-141`):

```
brain_build_info{version="1.0.0", git_commit="<sha>"} 1
brain_config_info{shard_count="16", arena_capacity_bytes="1073741824",
                  hnsw_m="16", embedder_model="bge-small-en-v1.5"} 1
```

Both are process-wide: `brain_config_info` describes the loaded config, so it
carries `shard_count` as a label rather than being emitted per shard. Label
names are snake_case throughout — `git_commit`, `hnsw_m` — and every label
value is a config knob rather than client input, which is what keeps the
cardinality bounded.

Because the value never moves, these families must be queried on their
**labels**. A change retires one label combination and starts another; it does
not change a value, so `changes(brain_config_info[1h])` is always zero. Count
distinct label sets over the window instead.

## 18. Histogram vs summary

Brain uses histograms (server-side aggregation friendly), not summaries (client-side quantiles).

Histograms work better with multi-replica deployments. Summaries don't aggregate across instances.

## 19. The metrics endpoint security

`/metrics` is unauthenticated — typical for Prometheus scraping, and the
endpoint carries no tenant-identifying labels by construction, so a scrape
leaks operational shape rather than customer data.

There is no auth knob on it. Isolation is by listener: the metrics endpoint
binds `[server] metrics_addr`, separate from both the data plane
(`listen_addr`) and the admin plane (`admin_addr`), which has its own mandatory
bearer token (`crates/brain-server/src/config/mod.rs:221-228`). Bind the
metrics listener to an interface the scraper can reach and nothing else can,
and firewall the port.

## 20. The metric catalog

This file is the catalog. The authority behind it is the exposition itself:
every series and every label value on it is written by one function, which
walks the server state and renders the whole `/metrics` body in a stable order
(`crates/brain-server/src/metrics/format.rs:53-81`). Reading that function is
how a name gets settled when a dashboard disagrees with a document.

The shipped alert rules are checked against it in CI: the suite parses the
emitted names out of the exposition and rejects any rule referencing a series
the server does not emit, so an alert cannot silently point at a metric that
does not exist. The dashboard JSON is checked for shape and for the `brain_*` /
`process_*` prefix taxonomy, which catches a mistyped family but **not** a
plausible-looking name that was never emitted — dashboard panels need the names
confirmed against the exposition by hand.

---

## Logs

How Brain emits structured logs.

## 21. The log format

Two formatters, selected by `[monitoring.logging] format`
(`crates/brain-server/src/bootstrap/logging.rs:23-28`):

- `compact` — one line per event, `<ts> <LEVEL> <target>: <message>`. The dev
  default; readable in a terminal.
- `json` — newline-delimited JSON. What production sets; ingestible by Loki,
  Elastic or Splunk.

Under `json`:

```json
{
  "ts": "2026-05-07T12:00:00.123Z",
  "level": "info",
  "logger": "brain.executor",
  "shard": 3,
  "operation": "encode",
  "space_id": "agent-001",
  "request_id": "...",
  "duration_ms": 8,
  "msg": "encode completed"
}
```

One JSON object per line. Readable with `jq`, ingestible by Loki / Elastic / Splunk / etc.

## 22. The log levels

- **TRACE**: per-frame protocol details. Used during deep debugging.
- **DEBUG**: per-request details. Used during development.
- **INFO**: normal lifecycle events (startup, shutdown, worker cycles).
- **WARN**: unusual conditions (retries, slow queries, capacity warnings).
- **ERROR**: errors that need attention (failed operations, etc.).

Default level: INFO. Production deployments may use WARN to reduce volume.

There is exactly one level knob with one override: `[monitoring.logging] level`
is the source of truth and `BRAIN_LOG` overrides it at runtime. `RUST_LOG` is
deliberately **not** consulted (`crates/brain-server/src/bootstrap/logging.rs:30-38`)
— a second environment variable that silently wins over the configured level is
the kind of surprise that costs an hour mid-incident.

## 23. The destination

Logs go to stdout. Brain writes no log files and does not rotate anything —
redirection and retention are the deployment's job, which is what every
container runtime already does:

- A file, by shell redirection.
- A log aggregator, via stdout capture in containers.
- syslog, via the runtime's log driver.

Config (`crates/brain-server/src/config/mod.rs:1344-1360`):

```toml
[monitoring.logging]
level = "info"               # trace | debug | info | warn | error
output = "stdout"
format = "compact"           # or "json"
```

The config section is `[monitoring.logging]`, grouped with
`[monitoring.tracing]`, and it rejects unknown fields — a file naming a knob
that does not exist fails to load rather than silently ignoring it.

## 24. The fields

Common fields in all entries:

- `ts`: ISO 8601 timestamp.
- `level`: severity.
- `logger`: which subsystem (brain.executor, brain.worker.decay, etc.).
- `msg`: human-readable message.

Operation-specific fields:

- `operation`: encode, recall, etc.
- `shard`: shard index, an integer counting from zero.
- `space_id`: agent (when applicable).
- `request_id`: request UUID.
- `duration_ms`: latency.

Error-specific fields:

- `error_code`: stable error identifier.
- `error_message`: human-readable.
- `stack`: Rust backtrace (DEBUG/TRACE only).

## 25. The "logger" hierarchy

The logger is a dotted path:

- `brain` — top level.
- `brain.executor` — request handlers.
- `brain.worker.<name>` — workers.
- `brain.storage.arena` — arena layer.
- `brain.storage.wal` — WAL layer.
- `brain.hnsw` — index.
- `brain.embedder` — embedding service.
- `brain.network` — connection layer.

Operators can filter by logger to focus:

```
| jq 'select(.logger | startswith("brain.worker"))'
```

## 26. The per-level guidance

What logs at each level:

```
TRACE:
  - Per-frame send/receive.
  - Per-iteration of a search.

DEBUG:
  - Per-request begin/end.
  - Per-cycle of a worker.
  - Per-checkpoint.

INFO:
  - Brain startup / shutdown.
  - Each worker's hourly summary.
  - Each major event (rebuild started, snapshot created).

WARN:
  - Retries.
  - Slow operations (> p99 expectation).
  - Approaching capacity.
  - Recovered from transient error.

ERROR:
  - Failed operations after retries.
  - Storage errors.
  - Crashes.
```

## 27. The "no PII" rule

Logs should not contain user data by default:

- ✗ Memory text.
- ✗ Cue text.
- ✓ Memory IDs (opaque).
- ✓ Counts and durations.

For debugging that requires user data, use TRACE level (which can be enabled selectively, with auth).

## 28. The audit log

Separate from the log stream, Brain keeps an audit trail in shard metadata —
not a file — and serves it on the admin plane at `GET /v1/audit` and
`GET /v1/audit/export`.

What it records is **extraction and entity resolution**: which extractor ran
against which memory, at which extractor and schema version, with what outcome,
cost and outputs (`crates/brain-metadata/src/tables/audit.rs:60-96`):

```jsonc
{
  "extractor_id": 3,
  "extractor_version": 2,
  "schema_version": 7,
  "started_at_unix_nanos": "...",
  "completed_at_unix_nanos": "...",
  "status": "success",          // or failure / skipped_filter /
                                // skipped_duplicate / skipped_disabled
  "status_reason": "",          // free-form; empty on success
  "outputs": [ /* entity / statement / relation ids */ ],
  "cost_micro_usd": 1420,       // 0 for the pattern and classifier tiers
  "input_hash": "blake3:...",   // BLAKE3 of the memory text
  "namespace": "acme"           // the tenant this extraction is billed to
}
```

Entries are append-only and time-ordered by a UUIDv7 key, and indexed three
ways — by memory, by extractor, by time — so an auditor can ask "what was
derived from this memory" as cheaply as "what happened in this window".

Two properties are worth stating plainly, because the trail is easy to
over-read:

- `input_hash` is there to **detect an edited memory**, which is what the
  idempotency probe uses it for. It is not a chain: no entry references its
  predecessor's hash, so the trail is append-only by construction and not
  cryptographically verifiable. Tamper evidence comes from exporting to
  append-only external storage.
- `namespace` is the one place a tenant id is durably recorded against cost.
  It matches the key the LLM spend ledger uses, so a namespace's model spend is
  this field summed with that ledger — the two must name the tenant
  identically. An unattributed row counts toward no namespace rather than
  toward a guessed one.

This is also the surface that answers the per-tenant questions metrics
deliberately cannot.

## 29. Audit log retention

Audit entries live in shard metadata, so retention is bounded by a worker
rather than by a log-rotation policy. The `audit_log_sweeper` worker runs daily
and hard-deletes rows past its retention window — **90 days by default**
(`crates/brain-workers/src/workers/audit_log_sweeper.rs:22`,
`crates/brain-workers/src/config.rs:183`). It sweeps the extraction audit with
all three of its indexes, and the entity-resolution audit. There is no
`[audit]` config section; the sweeper is configured like any other worker.

Merge and unmerge audit rows are the exception: they live on a separate table
and are **kept forever**, untouched by the sweeper. Identity decisions stay
answerable indefinitely because a merge is not recoverable from anything else
in the store.

For retention longer than the sweeper's window, export: `GET /v1/audit/export`
hands the trail out for external storage, which is also where regulatory
retention (1–7 years) and tamper evidence belong, since neither is something a
metadata table should be asked to guarantee.

## 30. Log sampling

For very high-volume operations, Brain may sample:

- 100% of errors.
- 10% of warnings.
- 1% of info.

Default is no sampling (full fidelity). Sampling is opt-in for cost-conscious deployments.

## 31. Per-request logging

Each request, by default, emits one INFO log at completion:

```json
{"ts":"...","level":"info","operation":"encode","shard":3,"space_id":"...","request_id":"...","duration_ms":8,"status":"success","msg":"encode completed"}
```

For DEBUG, additional logs at start, mid-points, end.

## 32. Log-rate adaptive

Under load, log volume can become a problem. Brain has:

- Rate-limited error logging (don't log "same error" thousands of times).
- Backpressure on the log pipeline (drop with warning if backed up).

These prevent observability from becoming a performance issue.

## 33. The "message" field guidance

Messages should be:

- Short (< 80 chars typical).
- Action-focused ("encode completed", "cache miss", "rebuild started").
- Avoid raw IDs/values in the message — those are in dedicated fields.

Bad:
```
"msg": "Encoded memory abc-123 in session xyz at 12:34:56 with salience 0.8"
```

Good:
```
"msg": "encode completed",
"memory_id": "abc-123",
"session_id": "xyz",
"salience": 0.8
```

## 34. The "trace ID" propagation

If the client provides a trace ID (per OpenTelemetry):

```
"trace_id": "abc...",
"span_id": "def..."
```

These propagate through to Brain's logs. Operators can join logs and traces by trace ID.

## 35. The "structured exception" handling

Errors include structured fields:

```json
{
  "ts": "...",
  "level": "error",
  "msg": "encode failed",
  "operation": "encode",
  "space_id": "...",
  "error": {
    "code": "LlmCredentialMissing",
    "message": "namespace has no registered LLM credential",
    "details": {
      "namespace": "acme"
    }
  }
}
```

Code, message, and details are separate fields. Operators can alert on specific codes.

The example uses a code that actually reaches clients: a namespace with no
registered provider key has its writes refused terminally, naming the namespace
(`crates/brain-ops/src/handlers/encode.rs:113`,
`crates/brain-ops/src/error.rs:35`).

**Do not alert on `QuotaExceeded` or `RateLimited`.** Brain enforces no
per-space quota and no rate limit: `OpError::QuotaExceeded` is constructed only
in a test (`crates/brain-ops/src/lib.rs:113`), so neither code is ever emitted
and an alert on either is dead. The limits that do refuse a write are physical
or per-field — arena capacity, disk, and the byte caps in
`crates/brain-ops/src/handlers/limits.rs`.

## 36. The "logger configuration" surface

Per-subsystem levels are set through the level string itself, which takes the
standard `EnvFilter` directive syntax rather than a separate config table:

```toml
[monitoring.logging]
level = "info,brain_server::network=debug,brain_index=warn"
```

The same string is what `BRAIN_LOG` carries, so a focused-debugging session
needs no config edit and no restart loop through a TOML file. Directives are
matched against Rust module paths, which is what `tracing` filters on.

## 37. The log retention default

Brain has no retention policy, because it owns no log files — it writes to
stdout and the surrounding system decides what to keep. Retention lives in the
container runtime's log driver, in the aggregator (Loki, Elastic), or in
whatever rotates the file the output is redirected into.

## 38. The "graceful logging shutdown"

On shutdown, Brain flushes pending log buffers:

```
1. Stop accepting new operations.
2. Existing ones complete and emit logs.
3. Logger flushes to disk / network.
4. Process exits.
```

This avoids losing logs at shutdown.

---

## Tracing

How Brain integrates with distributed tracing systems.

## 39. The tracing standard

Brain uses [OpenTelemetry](https://opentelemetry.io/) — the industry standard. Traces are exported in OTLP format to any compliant backend (Jaeger, Tempo, Honeycomb, Datadog, etc.).

## 40. The span hierarchy

A typical request produces a hierarchy:

```
[client.request] (span, opened at the connection layer)
  └── [brain.encode] (span, on the shard)
        ├── [brain.wal.append] (span)
        ├── [brain.metadata.write] (span)
        └── [brain.hnsw.insert] (span)
```

`client.request` is the trace root, opened in the connection layer; the shard
re-enters a clone of it so the handler span nests under it across the
Tokio→Glommio hop (`crates/brain-server/src/bootstrap/tracing.rs:17-32`). The
storage spans are opened inside the writer's submit path
(`crates/brain-ops/src/writer/submit.rs:316-368`).

Each span has:

- A name.
- Start / end timestamps.
- Status (success / error).
- Optional attributes (key-value pairs).
- A parent span ID (for the tree structure).

## 41. The instrumented operations

Brain creates spans for:

- Each request.
- Major phases (planning, execution).
- Storage operations (arena, WAL, metadata).
- HNSW operations.
- Embedder calls.
- Cross-shard fan-outs.
- Background worker cycles.

This gives operators a complete picture of where time is spent.

## 42. Sampling

Tracing every request is expensive. Brain samples:

```toml
[monitoring.tracing]
sampler = "ratio"
sample_ratio = 0.01         # 1% of requests
```

The sampler takes one of four names
(`crates/brain-server/src/config/mod.rs:1370-1376`):

- `always_off`: 0%. The default, and it builds no pipeline at all.
- `always_on`: 100%. For debugging.
- `ratio`: the fraction in `sample_ratio`, clamped to `[0.0, 1.0]`. Read only
  when the sampler is `ratio`.
- `parent_based`: defer to the incoming decision.

## 43. The "head sampling" vs "tail sampling"

Brain implements head-based sampling: the decision is made at request start.

For tail-based sampling (decide based on latency or errors after the fact), use a tracing collector that supports it (Tempo, Honeycomb).

## 44. The export

Traces are exported over **OTLP/HTTP** to a configured collector
(`crates/brain-server/src/config/mod.rs:1362-1386`):

```toml
[monitoring.tracing]
enabled = true
endpoint = "http://otel-collector:4318/v1/traces"
service_name = "brain-server"
```

The transport is HTTP, not gRPC, so the collector's port is the OTLP/HTTP one —
4318 by OTel convention, and that path is the default when the endpoint is left
unset (`crates/brain-server/src/bootstrap/tracing.rs:46`). Batching is the
OpenTelemetry SDK's own and is not exposed as config. `service_name` becomes
the OTel `service.name` resource attribute on every span.

`enabled` is the master switch and defaults to false, so a deployment that
configures nothing exports nothing. With `enabled = true` and an empty
endpoint, tracing degrades to a stdout exporter rather than failing.

The collector handles forwarding to the backend (Jaeger, Tempo, etc.).

## 45. Span attributes

The `client.request` root records
(`crates/brain-server/src/network/dispatch.rs:873-885`):

- `brain.operation`: the opcode.
- `brain.space_id`: the **effective** identity — the target under delegation.
- `brain.shard`: the target shard's index.
- `brain.stream_id`: which multiplexed stream carried the request.
- `trace_id` / `span_id`: recorded once the ids are known.

Under delegation it additionally records `brain.act_as`,
`brain.principal_space_id`, `brain.principal_key_hash` and
`brain.effective_namespace`. The acting party is never erased from the trail:
the span names both who asked and on whose behalf.

Duration and status are the span's own — a span carries its start and end, so
re-recording the elapsed time as an attribute would be a second, disagreeable
copy of it.

## 46. The propagation

Brain emits **server-side spans only**. The wire protocol carries no
`traceparent` field, so a client-supplied trace context is not consumed and
`client.request` is a trace root rather than a child of the caller's span
(`crates/brain-server/src/bootstrap/tracing.rs:10-15`).

The consequence for an operator is concrete: a client trace and a Brain trace
are two separate traces, joined by timestamp and by `brain.space_id`, not by a
shared trace id. Carrying the standard W3C `traceparent` on the wire is what
would make them one tree.

## 47. The cross-shard propagation (future)

In a clustered deployment, a cross-node call propagates trace context:

```
Node A's span (parent)
  Cross-node call carries trace context
    Node B's span (child, on a different node)
```

The trace shows the call across nodes — useful for diagnosing latency in distributed setups.

## 48. The sampling propagation

The `parent_based` sampler defers to an incoming decision, which is the right
behaviour once the wire carries one. Until it does, there is no parent context
to read, so `parent_based` falls through to its own root decision and the
client's choice has no effect on what Brain samples. Set the sampler to match
the client's rate rather than expecting it to be inherited.

## 49. Span events

Within a span, point-in-time events:

```rust
span.add_event("hnsw.search.start", attrs);
span.add_event("hnsw.search.end", attrs);
```

Events are like logs but tied to a span. Used for fine-grained timing within a span.

## 50. The performance overhead

Tracing has overhead:

- Span creation: ~1 µs.
- Attribute setting: ~100 ns per attribute.
- Export: batched, async, ~1% CPU at modest sample rates.

For 1% sampling: < 0.1% overhead. For 100%: 1-2% overhead.

## 51. The "trace not exported" path

If the export pipeline is down or backed up:

- Spans queue in the OpenTelemetry SDK's batch processor.
- When that queue is full, a finished span is discarded and counted.
- Operations continue normally. Export failure never fails a request.

Both counters below are fed from OpenTelemetry's process-global error handler,
which classifies by error variant: a failed or timed-out export attempt is an
export error, the batch processor's "buffer full" send failure is a dropped
span (`crates/brain-server/src/metrics/otel.rs:1-18`).

```
brain_tracing_spans_dropped_total
brain_tracing_export_errors_total
```

## 52. The "high-cardinality" warning

Some attributes would have very high cardinality — memory ids, request ids —
and Brain does not record them on spans. The root span's attributes are bounded
by the identity and routing of the request: operation, space, shard, stream.

This matters because the cardinality rule that governs metric labels does not
transfer cleanly to traces: a tracing backend indexes attributes, so an
unbounded attribute inflates the index rather than multiplying series. A
one-off investigation that needs a memory id reads it from the logs, where the
id is already a field, and joins to the trace by trace id.

## 53. The async runtime tracing

Span context is thread-local, and so is `tracing`'s current-span stack.
Glommio's per-core executors keep their own thread-locals, so context does not
follow the Tokio→Glommio hop on its own: the connection layer carries the
`tracing::Span` handle itself across the shard channel and the shard re-enters
it, which is what keeps the request and its storage spans in one tree
(`crates/brain-server/src/bootstrap/tracing.rs:17-32`).

Span creation and close are thread-agnostic. Only the batched OTLP export needs
the Tokio runtime, and that runs on its own task, so emitting spans from a
Glommio thread is sound.

## 54. The "trace ID in logs"

Logs include the trace ID:

```json
{"ts":"...","level":"info","trace_id":"abc...","span_id":"def..."}
```

Operators can pivot from a trace to logs (in Tempo / Loki / etc.) by trace ID.
The root span records `trace_id` and `span_id` as fields once they are known,
which is what puts them on the log line.

## 55. The "no trace" fallback

If tracing isn't configured:

- Brain runs normally.
- No spans are created or exported.
- Performance is unchanged.

Tracing is opt-in. Brain doesn't require a tracing backend.

## 56. The "useful traces" examples

Examples of what traces help diagnose:

- "Why is this RECALL slow?" → trace shows time in embedder vs HNSW vs metadata fetch.
- "Where's the bottleneck?" → trace shows the sequential dependencies.
- "Did the client retry?" → trace shows multiple spans on the same request.

Traces complement metrics (which show aggregates) by showing individual requests.

---

*Continue to [`02_dashboards.md`](02_dashboards.md) for dashboards.*
