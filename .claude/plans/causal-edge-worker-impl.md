# CausalEdgeWorker — implement `Caused` auto-inference

## Context

The scope plan (`temporal-causal-auto-edges.md`) flagged two paths for
inferring `Caused` edges: extractor-driven (from causal statements
the knowledge layer already produces) and LLM-judge (heavier,
deferred). This plan implements **the extractor-driven v1**.

Why this matters: in agent traces, "the outage was caused by the
deploy" is exactly the kind of structure the user wants Brain to
infer automatically. The extractor pipeline already produces typed
statements like `Statement { subject: Outage, predicate: caused_by,
object: Deploy, evidence: [m_outage, m_deploy] }`. What's missing is
the projection: walk those statements, find the memories on each
side, and write a memory→memory `Caused` edge.

This worker runs **only when the knowledge layer is active**. On
substrate-only deployments the worker either isn't spawned or runs
empty (extractor pipeline produces no statements).

## What "good enough for v1" means

- A `Statement` whose predicate is in the **causal whitelist**
  (default: `caused_by`, `triggered`, `led_to`, `resulted_in`,
  `because_of`) and whose confidence ≥ threshold (default 0.6)
  produces one `Caused` edge from the **cause-side memory** to the
  **effect-side memory**.
- Asymmetric edges (causality has direction).
- Edge weight = statement confidence.
- `EdgeAdded(AUTO_DERIVED, kind=Caused)` event fires post-commit so
  the change feed surfaces it.
- Multiple causal statements covering the same memory pair are
  idempotent: the disambiguator + upsert semantics in `edge::link`
  let later writes refresh the weight without duplicating.

## Files to touch

| File | Purpose |
|---|---|
| `crates/brain-workers/src/workers/causal_edge.rs` | New worker module. Subscribes to (or polls) `StatementCreated`-shaped events. |
| `crates/brain-workers/src/lib.rs` | `pub use` the new worker + knobs + metrics. |
| `crates/brain-ops/src/context.rs` | New `write_causal_edges(pairs: &[(MemoryId, MemoryId, f32)]) -> Result<usize, _>`. Same shape as `write_auto_edges` / `write_temporal_edges`; writes `EdgeKind::Caused`; publishes `EdgeAdded(AUTO_DERIVED)`. |
| `crates/brain-ops/src/ops/writer/mod.rs` | New `CausalEdgeEnqueue` channel + `set_causal_edge_sender()`. Enqueue tuple = `(statement_id, predicate_id, confidence, evidence_entries)`. |
| `crates/brain-ops/src/ops/knowledge/statement_create.rs` (or the statement-write path) | After WAL fsync + redb commit, enqueue if predicate is in the whitelist. Enqueue is non-blocking — statement commit never depends on the worker. |
| `crates/brain-server/src/config/mod.rs` | New `[workers.causal_edge]` section. |
| `crates/brain-server/src/shard/mod.rs` | Spawn the worker per shard when `enabled=true`. Wire metrics. |
| `config/dev.toml` + `config/docker.toml` | Default values incl. predicate whitelist. |

## Critical APIs to reuse

- **Statement evidence walk**: `EvidenceRef::Inline(Box<SmallVec<EvidenceEntry; 8>>)` and `EvidenceRef::Overflow(EvidenceOverflowId)`. For the inline case, the entries are in the statement metadata directly. For the overflow case, call `brain_metadata::statement_ops::evidence_overflow_load(rtxn, id)`.
- **Statement lookup**: `brain_metadata::statement_ops::statement_get(rtxn, id)` returns the full `Statement`. Use this to fetch the cause-side and effect-side statements when chasing the causal graph.
- **Predicate lookup**: `brain_metadata::predicate_ops::predicate_lookup_by_qname(rtxn, namespace, name)` resolves the whitelist (`"brain"` + `"caused_by"`, etc.) at worker startup. Cache the resolved `PredicateId` set in `AtomicU32`s on the worker handle so the cycle doesn't re-resolve per statement.
- **Edge write helper**: Mirror `write_auto_edges` from `crates/brain-ops/src/context.rs` (modified in O1 to publish events).

## The mapping algorithm — what edges to write

Given a new statement `S = { id: SID, subject: ES, predicate: PID, object: EO, confidence: c, evidence: ES_mems }`:

1. If `PID ∉ causal_whitelist` → skip. No edge.
2. If `c < min_confidence` → skip. (Worker default 0.6.)
3. If `ES_mems` is empty → skip (nothing on the effect side to anchor the edge).
4. The **effect-side** memories are `S.evidence` directly (these are the memories that *describe the effect*).
5. The **cause-side** memories require a graph hop. The cause entity is `EO` (statement object). Query the **statement-by-subject** index for statements whose subject is `EO`:
   - `brain_metadata::tables::knowledge::statement::STATEMENTS_BY_SUBJECT_TABLE` lookup at `EO`.
   - For each such statement `S'`, if it's still `is_current` and not tombstoned, collect its evidence memories — these are the cause-side memories.
6. **Write edges**: for each `(cause_mem, effect_mem)` pair, write `Caused` edge with weight `c × c'` (statement-pair confidence product), capped at 1.0.

**Caps to keep edge-count bounded:**
- Top 3 effect-side memories per statement (highest per-entry `confidence_milli`).
- Top 3 cause-side memories per related statement (same).
- Top 5 related statements per cause entity (highest `confidence`, current only).
- Net per causal statement: up to 3 × 3 × 5 = 45 edges. Tunable via config; default tighter.

**Edge case: object is not an entity.** `StatementObject::Entity(id)` is the only variant that produces a `Caused` edge in v1. `Value(_)`, `Memory(_)`, `Statement(_)` variants either don't make sense for causal chaining (Value) or require different traversal (Memory → just write the edge directly to that memory; Statement → reify, defer to v2).

For `StatementObject::Memory(mid)`: write `Caused` edge from `mid` → effect_mem directly. No graph hop needed.

## Implementation order

| # | Commit | LoC | Touches |
|---|---|---|---|
| **C1** | Predicate-whitelist resolver: on worker spawn, look up each qname in `causal_whitelist` config, cache the `PredicateId` set. Skip qnames the deployment hasn't declared (substrate-only deployments end up with an empty set → worker effectively no-ops). | ~80 | brain-workers/src/workers/causal_edge.rs |
| **C2** | Enqueue channel + writer integration. Statement-create path checks `whitelist.contains(predicate_id)` post-commit and pushes the tuple. Non-blocking. Metric for drops. | ~80 | brain-ops writer + statement_create.rs |
| **C3** | Worker drain loop. Read each enqueued tuple, look up the statement, walk the cause/effect mappings, build edge pairs, call `ctx.ops.write_causal_edges`. | ~250 | brain-workers + brain-ops/context.rs |
| **C4** | `write_causal_edges`: same shape as `write_auto_edges` (modified in O1); writes `EdgeKind::Caused`; asymmetric (no mirror); publishes `EdgeAdded(AUTO_DERIVED)`. | ~80 | brain-ops/context.rs |
| **C5** | Config + spawn. `[workers.causal_edge]` with `enabled`, `interval_ms`, `batch_size`, `min_confidence`, `causal_whitelist`, top-K caps, `channel_capacity`. | ~80 | brain-server config, dev.toml, docker.toml |
| **C6** | Tests: (a) statement with non-causal predicate → no edge. (b) causal statement with matching evidence on both sides → edge with right weight + direction. (c) confidence below threshold → no edge. (d) deployment without the predicate declared → no edge, no error. (e) subscribe sees `EdgeAdded(AUTO_DERIVED, kind=Caused)`. | ~300 | brain-workers tests |
| **C7** | Help + reference docs + drift tests. The shell didn't gain a flag, but the recall `--include-edges` and `--include-graph` cards now surface `Caused` edges; docs + the auto-edge validation script append a phase. | ~30 | docs + repl/help.rs + drift tests |
| **C8** | Verify gate: fmt, clippy -D warnings, test sweep, end-to-end against the dev container with a declared schema. | — | — |

Total ~900 LoC.

## Config shape

```toml
[workers.causal_edge]
# Master switch.
enabled = true
# Scheduler tick.
interval_ms = 200
# Max statements drained per cycle.
batch_size = 64
# Minimum statement confidence. Below this, no edge — causal inference
# at low confidence produces more noise than signal.
min_confidence = 0.6
# Predicate qnames whose presence triggers causal-edge derivation.
# Substrate-only deployments leave this empty by default; declaring a
# schema with these predicates activates the worker.
causal_whitelist = [
    "brain:caused_by",
    "brain:triggered",
    "brain:led_to",
    "brain:resulted_in",
    "brain:because_of",
]
# Per-statement caps on edge fan-out.
max_effect_memories_per_statement = 3
max_cause_memories_per_statement = 3
max_related_statements_per_entity = 5
# Writer→worker queue depth.
channel_capacity = 1024
```

## Metrics

| Name | Type | Meaning |
|---|---|---|
| `brain_causal_edge_edges_written_total{shard}` | counter | logical Caused edges written |
| `brain_causal_edge_drops_total{shard}` | counter | enqueue dropped (channel full) |
| `brain_causal_edge_skipped_total{shard, reason}` | counter | `reason="non_causal_predicate"`, `"low_confidence"`, `"no_evidence"`, `"object_not_entity"`, `"no_related_statement"` |
| `brain_causal_edge_cycle_duration_seconds{shard}` | histogram | |
| `brain_causal_edge_predicate_whitelist_resolved_total{shard}` | gauge | how many causal predicates the worker actually resolved against the schema (0 = inactive on this deployment) |

## Subscribe surface

Edges fire `EdgeAdded` events with:
- `event_type: EventType::EdgeAdded`
- `edge_payload.edge_kind_tag: 0` (Builtin)
- `edge_payload.edge_kind_byte: 0` (`Caused` per `EdgeKind` enum)
- `edge_payload.origin: 1` (AUTO_DERIVED)
- `edge_payload.weight: c × c'`

The shell's existing `edge_kind_label(0, 0)` returns `"Caused"`; the
`--wait-auto-edges-ms` delta line will render these alongside
`SimilarTo` and `FollowedBy` once T (temporal) and C (causal) workers
both ship.

## Edge cases

- **Statement re-extraction**. An extractor re-running on the same
  memory may produce the same causal statement with updated
  confidence. The edge writer's upsert semantics handle this — the
  existing edge gets its weight refreshed.
- **Statement supersession**. When a causal statement is superseded
  (a fresh extraction overrode it), the old statement is no longer
  `is_current`. The worker only emits edges from current statements;
  a supersession event doesn't retract the old edge. **This is a
  known v1 limitation** — superseded causal edges linger.
  Mitigation: edge_scrub worker can be extended (v2) to remove
  edges whose source statement is no longer current.
- **Statement tombstoned**. Same handling — skip in the cause-side
  walk. The forward-side edge from a tombstoned statement's evidence
  is also not written.
- **Symmetric causal statement**. Some predicates (e.g. "correlated_with")
  could go either way. The whitelist is asymmetric-only by design
  (`caused_by` means object→subject); symmetric predicates aren't on
  the list. Declare new asymmetric predicates explicitly.
- **Cross-context causality**. Unlike `FollowedBy`, `Caused` is
  allowed to cross context boundaries — causal claims often span
  domains (e.g., "the deploy caused the outage" where the deploy is
  in `engineering` context and the outage is in `incidents`). The
  worker does NOT filter by context.

## Done when

- A schema declaring `brain:caused_by` is uploaded.
- Encoding "we deployed v1.7.3 to production" (mem A) then "the
  payment service went down at 14:02" (mem B), with the extractor
  producing `Statement(subject: outage, predicate: caused_by,
  object: deploy, evidence: [B], confidence: 0.85)` and a separate
  statement for the deploy (subject: deploy, predicate: was_performed,
  evidence: [A]), results in a `Caused` edge A → B with weight ≈ 0.85.
- The edge surfaces in `recall --include-edges`, in
  `subscribe -o ndjson | jq '.edge_payload'`, and in
  `encode --wait-auto-edges-ms 500`'s delta line (kind=Caused).
- Substrate-only deployment → worker runs but produces zero edges
  (whitelist resolves to empty set).
- Setting `BRAIN__WORKERS__CAUSAL_EDGE__ENABLED=false` removes
  registration + metrics.
- `cargo fmt && cargo clippy -D warnings && cargo test` green across
  brain-ops, brain-workers, brain-server.

## Risks

- **False positives in causal inference**. The whole reason the LLM-
  judge path was deferred is that causal inference is hard to do
  well. The extractor-driven approach only fires when the *extractor*
  asserts causality — so the false-positive rate is bounded by the
  extractor's predicate-assignment accuracy. If the deployment has a
  bad extractor that liberally produces `caused_by` statements, the
  edge table will fill with noise.
  - Mitigation: `min_confidence = 0.6` default is conservative.
    Operators can raise it.
- **Reverse-direction bug**. `Statement("Outage caused_by Deploy")`
  is the most natural English phrasing, BUT it means **Deploy caused
  Outage** (causality is reversed in the predicate name).
  Implementation MUST honor this — edge direction = object → subject.
  Pin in the test suite.
- **Schema migration surprise**. A deployment that adds a new
  `caused_by` predicate after running for months will see a sudden
  spike in causal-edge writes as the worker backfills (if we choose
  to backfill — current plan does NOT; only new statements trigger
  the worker). Document this.

## Verification recipe

```bash
# Pre-req: schema declared with brain:caused_by predicate.

# A — encode the cause memory
A=$(brain encode "we deployed v1.7.3 to production" --wait-for-extraction \
    -o jsonpath='{.memory_id}')

# B — encode the effect memory
B=$(brain encode "the payment service went down at 14:02" \
    --wait-for-extraction --wait-auto-edges-ms 1000 -o table)
# Expected delta on B's card:
#   → 1 auto-edge landed in ~Nms
#       Caused s0/m<A>/v1  weight=0.85

# Subscribe view
brain subscribe --collect 6 -o ndjson > /tmp/events.ndjson &
brain encode "fresh cause" --wait-for-extraction
brain encode "fresh effect" --wait-for-extraction
sleep 2; jq -c 'select(.event_type == "EdgeAdded" and .edge_payload.edge_kind_byte == 0) | .edge_payload | {origin, weight}' /tmp/events.ndjson
# Expect: {"origin": 1, "weight": ~0.85} for the Caused edge.

# Substrate-only deployment (no schema)
# Worker runs but writes zero edges. Confirm:
curl -s localhost:9091/metrics | grep brain_causal_edge_predicate_whitelist_resolved_total
# Expect: 0 — no causal predicates resolved.
```

## What this plan does NOT cover

- **LLM-judge causal inference**. Deferred to v2. Adds an LLM call per
  new memory to ask "does this describe something caused by recent
  events?" Heavy; depends on extractor budget and cache hit rate.
- **Causal-chain inference**. If A caused B and B caused C, this
  worker doesn't infer A→C. Multi-hop causal closure is a planning-
  time operation (REASON verb), not a write-time materialization.
- **Counterfactual edges**. "X would have caused Y if Z" is out of
  scope. Real causal reasoning is hard; v1 deliberately stops at the
  extractor's evidence.

## Dependencies

- **`auto-edge-subscribe-visibility.md` (O1)** — shipped. The new
  worker reuses the EdgeAdded broadcast path.
- **`temporal-edge-worker-impl.md`** — independent; can ship in
  either order. Both add `EdgeKind` variants to the AUTO_DERIVED
  surface.
- **Schema with at least one causal predicate declared** — without
  this the worker is a no-op. The system schema (Phase 20.7
  bootstrap) doesn't currently declare causal predicates; deployments
  must add them via `SCHEMA_UPLOAD` for the worker to do anything.
  Note in the docs that a starter schema with the recommended
  whitelist is worth shipping in `docs/concepts/causal-edges.md` as
  a separate doc artifact.
