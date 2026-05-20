# TemporalEdgeWorker — implement `FollowedBy` auto-inference

## Context

Agents stream observations in order. The next memory almost always
follows the previous one in a narrative sense, but today the agent
must hand-attach `--edge followed_by:<prev>` to get a `FollowedBy`
edge — exactly the manual escape hatch the auto-edge work is killing.
The AutoEdgeWorker (`SimilarTo`) is the only worker today; this plan
adds the second worker.

The scope file `temporal-causal-auto-edges.md` set the high-level
shape; this plan turns it into a sequenced implementation.

The trigger is the same enqueue path the AutoEdgeWorker uses
(post-WAL, post-commit) so the two workers are independent and either
can be disabled without affecting the other.

## What "good enough" means

- A new memory M encoded by agent A within `window_seconds` of A's
  previous memory P produces exactly one `FollowedBy` edge P → M.
- Weight decays linearly with the gap so a 0-second gap gets ~1.0 and
  a gap == window gets 0.
- No edges cross agents. No edges cross contexts (the previous memory
  must share context with the new one — narrative threads are per-
  context).
- `EdgeAdded(AUTO_DERIVED)` event fires post-commit so subscribers see
  it on the change feed.
- `enabled=false` removes the worker cleanly; metrics and channel go
  away with it.

## Files to touch

| File | Purpose |
|---|---|
| `crates/brain-metadata/src/tables/memory.rs` | New secondary index `MEMORIES_BY_AGENT_TIMELINE: (agent_bytes, created_at_be_bytes) → memory_id` — the temporal worker needs to find "most-recent memory for agent A" cheaply. Today `MEMORIES_TABLE` is keyed by id only; a full scan is unacceptable on the worker hot path. |
| `crates/brain-metadata/src/memory_ops.rs` (or wherever the writer commits memory metadata) | On encode commit, also write the timeline-index row. On forget/tombstone, cascade-delete the row. |
| `crates/brain-ops/src/ops/writer/mod.rs` | New `TemporalEdgeEnqueue` channel + `set_temporal_edge_sender()` parallel to `set_auto_edge_sender`. Enqueue tuple = `(memory_id, agent_id, context_id, created_at_unix_nanos)`. |
| `crates/brain-ops/src/ops/writer/encode.rs` | After `try_enqueue_auto_edge`, call `try_enqueue_temporal_edge`. Non-blocking; same drop-with-metric behaviour. |
| `crates/brain-workers/src/workers/temporal_edge.rs` | New module. `TemporalEdgeWorker` with the same lifecycle as `AutoEdgeWorker`. |
| `crates/brain-workers/src/lib.rs` | `pub use` the new worker + its `Knobs` / `Metrics`. |
| `crates/brain-ops/src/context.rs` | New `write_temporal_edges(pairs: &[(MemoryId, MemoryId, f32)]) -> Result<usize, _>` method. Same shape as `write_auto_edges` but writes `EdgeKind::FollowedBy` and publishes `EdgeAdded(AUTO_DERIVED)` events through `self.events`. |
| `crates/brain-server/src/config/mod.rs` | New `[workers.temporal_edge]` config section parallel to `[workers.auto_edge]`. |
| `config/dev.toml` + `config/docker.toml` | Default values. |
| `crates/brain-server/src/shard/mod.rs` | Spawn the worker per shard when `enabled=true`. Wire metrics into the handle bag the `/metrics` endpoint reads. |

## Critical reuse opportunities

- **Enqueue channel pattern**: `crates/brain-ops/src/ops/writer/mod.rs:1198–1229` (auto-edge enqueue) is the template. Copy the shape — bounded `flume`, `try_send`, drop-with-metric on full, debug-log on disconnect.
- **Worker scheduler integration**: `crates/brain-workers/src/workers/auto_edge.rs` shows how to register a worker on the per-shard scheduler with `interval_ms` cycle + drain loop.
- **Edge write helper**: `crates/brain-ops/src/context.rs::write_auto_edges` (modified in O1 to publish events) is the template for `write_temporal_edges`. The new method differs in one place: pass `EdgeKind::FollowedBy` instead of `EdgeKind::SimilarTo`, no symmetric mirror (temporal edges are directional).
- **Subscribe-event payload**: O1's `edge_payload_to_event(..., origin::AUTO_DERIVED)` works as-is.

## Implementation order

| # | Commit | LoC | Touches |
|---|---|---|---|
| **T1** | New `MEMORIES_BY_AGENT_TIMELINE` redb table + index-write on encode-commit + index-delete on forget. Migration: backfill on first start (one-time full scan if the table doesn't exist yet — running deployments rebuild lazily). Property test: 1000 encodes by various agents → for each agent, "most recent within window" matches a naive scan. | ~180 | brain-metadata |
| **T2** | `TemporalEdgeEnqueue` channel + writer wiring. Mirror auto-edge shape; non-blocking; metric counter `brain_temporal_edge_drops_total`. | ~80 | brain-ops writer, server shard spawn |
| **T3** | `TemporalEdgeWorker` module: drain channel, look up prev memory via the timeline index, compute decay weight, build `(prev, current, weight)` pairs, call `ctx.ops.write_temporal_edges`. | ~150 | brain-workers |
| **T4** | `write_temporal_edges` — same shape as `write_auto_edges` minus the symmetric mirror; uses `EdgeKind::FollowedBy`; publishes `EdgeAdded(AUTO_DERIVED)` via O1's path. | ~80 | brain-ops/context.rs |
| **T5** | Config + spawn wiring. `[workers.temporal_edge]` with `enabled`, `interval_ms`, `batch_size`, `window_seconds` (default 300), `weight_min` (default 0.1, hard floor below which we don't emit even if computed), `channel_capacity`. | ~80 | brain-server config, dev.toml, docker.toml |
| **T6** | Tests: (a) two consecutive encodes by same agent within window → 1 FollowedBy edge with weight ≈ 1.0, asymmetric. (b) Same agent across context boundary → no edge. (c) Different agents → no edge. (d) Window exceeded → no edge. (e) Subscribe sees `EdgeAdded(AUTO_DERIVED, kind=FollowedBy)`. | ~250 | brain-workers tests |
| **T7** | Help card + reference docs + drift tests. The shell didn't have a flag to surface, but the validation script gets a new phase. | ~30 | docs, repl/help.rs (optional row), drift tests |
| **T8** | Verify gate: fmt, clippy -D warnings, full test sweep, end-to-end validation against the dev container. | — | — |

Total ~850 LoC. T1 is the largest piece because of the migration story.

## Weight function

```rust
// gap_seconds = (current.created_at - prev.created_at) / 1e9
// linear decay: 1.0 at gap=0, weight_min at gap=window, 0 beyond.
let normalized = 1.0 - (gap_seconds / window_seconds).clamp(0.0, 1.0);
let weight = (normalized * (1.0 - weight_min) + weight_min).max(0.0);
```

This is intentionally NOT `exp(-gap/window)` (the scope file's first
guess) — exponential decay buries too many edges below the
visualization threshold at modest gaps. Linear keeps the signal
flatter for the agent-narrative case.

## Edge cases

- **Backfilled / out-of-order encodes**. If a memory arrives with a
  past `created_at` (e.g., import path, replay), it shouldn't create
  a `FollowedBy` from a future memory. Worker only writes the edge
  when the new memory's `created_at` is **strictly greater than** the
  candidate prev memory's `created_at`. Otherwise skip.
- **Self-reference**. The new memory's id MUST NOT equal the candidate
  prev. Index lookup excludes self by construction (the new row
  hasn't been committed yet when the worker drains; even if it had,
  the worker compares ids).
- **First memory ever for an agent**. Index lookup returns `None`;
  worker emits zero edges. Silent success.
- **Cross-context boundary**. Worker filters by `context_id` on the
  index lookup. Optional knob `cross_context: bool` in config
  (default `false`) — flip to `true` for deployments where contexts
  are session-thin and temporal threads cross them.
- **Tombstoned candidate prev**. If the most recent memory for the
  agent was just FORGOTTEN, we shouldn't link from a tombstone. The
  worker reads `MemoryMetadata.flags`; if `HARD_FORGOTTEN` is set
  (bit 2), skip. Soft-tombstoned (bit 1 — recoverable) is also
  skipped to avoid edges that become stale on reclamation.
- **Race with subsequent encode**. Worker may drain a queue with two
  entries from the same agent within one cycle. Process them in
  enqueue order so each one sees the previous one's commit.

## Config shape

```toml
[workers.temporal_edge]
# Master switch; false → no worker, no channel, no overhead.
enabled = true
# Scheduler tick. Smaller = faster encode → edge visibility.
interval_ms = 100
# Max memories drained per cycle.
batch_size = 256
# Temporal window. Memories older than this are not candidates.
window_seconds = 300
# Hard floor on the decay-weight curve. Edges below this aren't
# written; keeps the table from filling with near-zero weight rows.
weight_min = 0.1
# Writer→worker queue depth.
channel_capacity = 1024
# Allow FollowedBy across context boundaries.
cross_context = false
```

## Metrics

| Name | Type | Meaning |
|---|---|---|
| `brain_temporal_edge_edges_written_total{shard}` | counter | logical FollowedBy edges written |
| `brain_temporal_edge_drops_total{shard}` | counter | enqueue dropped (channel full) |
| `brain_temporal_edge_skipped_total{shard, reason}` | counter | skipped (`reason="no_prev"`, `"out_of_order"`, `"tombstoned"`, `"cross_context"`) — helps diagnose "why no edges?" |
| `brain_temporal_edge_cycle_duration_seconds{shard}` | histogram | per-cycle wall time |
| `brain_temporal_edge_gap_seconds{shard}` | histogram | observed gap distribution; sanity check that the window is well-tuned |

## Subscribe surface

Each edge written publishes one `EdgeAdded` event via
`self.events.publish(env)` with:
- `event_type: EventType::EdgeAdded`
- `edge_payload.edge_kind_tag: 0` (Builtin)
- `edge_payload.edge_kind_byte: 1` (FollowedBy per `EdgeKind` enum)
- `edge_payload.origin: 1` (AUTO_DERIVED)
- `edge_payload.weight: <decay value>`

The shell's `--wait-auto-edges-ms` watcher already filters on
`origin == AUTO_DERIVED` — it surfaces FollowedBy edges automatically
with the kind label "FollowedBy" (the renderer's existing
`edge_kind_label(0, 1)` returns `"FollowedBy"`).

## Done when

- Two consecutive encodes by the same agent in the dev container,
  300 ms apart, produce one `FollowedBy` edge with weight ≈ 0.998.
- The edge surfaces in `recall --include-edges` AND in
  `subscribe -o ndjson | jq '.edge_payload'` AND in
  `encode --wait-auto-edges-ms 500`'s delta line.
- Cross-agent encodes produce no edge.
- Cross-context encodes produce no edge (with default config).
- `BRAIN__WORKERS__TEMPORAL_EDGE__ENABLED=false` removes worker
  registration + metrics.
- `cargo fmt && cargo clippy -D warnings && cargo test` green across
  brain-metadata, brain-ops, brain-workers, brain-server.
- New validation phase appended to the auto-edge validation script.

## Risks

- **Index migration on existing deployments**. Adding
  `MEMORIES_BY_AGENT_TIMELINE` requires a backfill pass. If the
  deployment has 10M+ memories the backfill could take minutes.
  Mitigation: backfill on first start, gate the worker on the
  backfill's completion (existing `MetadataDb` migration framework
  handles this).
- **Edge-table growth**. Adding ~1 FollowedBy per encode roughly
  doubles edge-table size. The edge_scrub worker handles cleanup;
  but if the deployment runs without scrub enabled, the table grows
  unboundedly. Surface in the config defaults.
- **Subscriber surprise**. Existing subscribers that grep for
  `EdgeAdded` events will start seeing `FollowedBy` mixed in with
  `LINK` and `SimilarTo`. Document in spec §02/06 + the help card.

## Verification recipe

```bash
# After implementation, against the dev container:

brain encode "first observation" --wait-auto-edges-ms 500 -o table
brain encode "second observation 100ms later" --wait-auto-edges-ms 500 -o table
# Second card's delta line shows FollowedBy from first.

# Subscribe stream:
brain subscribe --collect 4 -o ndjson > /tmp/events.ndjson &
brain encode "a"; sleep 0.2; brain encode "b"
sleep 1; jq -c 'select(.event_type == "EdgeAdded") | .edge_payload | {kind: (.edge_kind_byte), origin}' /tmp/events.ndjson
# Expect: {"kind": 1, "origin": 1} for the FollowedBy auto-edge.

# Recall view:
brain recall "first observation" --top-k 1 --include-edges
# Expect: FollowedBy outgoing edge to the second memory, weight ≈ 1.0.

# Window check:
brain encode "x"; sleep 305; brain encode "y"
# After the worker drains: no FollowedBy edge between them
# (gap > window_seconds).

# Cross-agent check (requires two agents):
brain --agent alice encode "alice 1"
brain --agent bob encode "bob 1"
sleep 1
brain recall "alice 1" --filter-context 0 --include-edges
# Expect: no outgoing FollowedBy to bob's memory.
```

## What this plan does NOT cover

- **`brain edge list <memory_id>`** as a first-class shell command —
  if we keep growing edge kinds we'll want it. Filed as its own
  plan separately.
- **Temporal-window auto-tuning**. Deployments with rapid-fire agent
  loops will want a smaller window than the 5-minute default.
  Operator-tunable today; auto-tuning is a v2 idea.
- **Multi-strand temporal graphs**. If an agent's stream branches
  (e.g., a search hits multiple parallel observations), this worker
  produces a single linear chain. Multi-strand is deferred.
