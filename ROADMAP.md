# Roadmap

Work that isn't done yet, kept flat on purpose — no milestone tags, no version
gates, no dates. Items are promoted out of this file as they land. **Nothing
here is a commitment.**

For what Brain does today, see [`README.md`](README.md). For scope boundaries
that are deliberate rather than pending, see
[Known limitations](README.md#known-limitations).

---

## Performance verification

**Reference-hardware performance verification is optional and off the critical
path for v1.0** (owner decision, 2026-09-07).

v1.0 locks on functional and operational correctness — hardware-independent,
runnable in CI — not on measured latency, throughput or storage numbers. The
targets in [`spec/19_benchmarks/02_performance_targets.md`](spec/19_benchmarks/02_performance_targets.md)
stay as engineering targets to capture *after* the lock. A miss is a tuning
follow-up, never a v1.0 blocker. See
[`spec/19_benchmarks/06_complete_acceptance.md`](spec/19_benchmarks/06_complete_acceptance.md).

- **Acceptance suite on reference hardware.** The end-to-end harness lives in
  the [`brain-eval`](https://github.com/arc-labs-ai/brain-eval) rig
  (`brain-eval acceptance --scale 1m`, `soak`) and covers latency, throughput,
  recall@K, system scenarios and restart-recovery. It runs today; what is
  deferred is a quiet run on reference hardware (16-core x86_64, 64 GiB RAM,
  NVMe SSD) with the wall-time numbers captured.
- **Classifier inference latency on reference hardware.** The GLiNER forward
  pass (DeBERTa-v3 backbone → projection → label MLP → BiLSTM → markerV0 span
  head → einsum scoring → sigmoid decode) is implemented, validated against
  real weights and dispatched live. On the dev box (aarch64, `opt-level=2`) it
  runs ~60–80 ms per short memory against a §11/01 p99 budget of 15 ms. Because
  classification is enqueued off the `ENCODE` hot path this is not a blocker;
  the reference-hardware number (x86_64, `opt-level=3` + LTO, optionally the
  `mkl` candle feature) is deferred.
- **Production-scale benches.** In-crate criterion benches run at 10K-corpus
  scale in CI; the 1M-per-shard mixed-workload run is driven by `brain-eval` on
  reference hardware.

---

## Outstanding work

- **Live LLM provider validation.** The Anthropic and OpenAI clients are
  exercised through a mock-client integration suite. A pass against real
  providers — through the broker, with real cost accounting — is still needed.
  Independent of reference hardware: it runs on any box with a working broker.
- **Tutorial polish.** The end-to-end tutorial (blank deployment → working
  query) needs one "follow it on a fresh laptop" pass.

---

## Planned improvements

- **Streaming retrieval results.** A `limit > 100` would stream across multiple
  `QueryResponse` frames; today's response is single-frame.
- **Richer transactional lensing.** `RECALL` inside a transaction already
  overlays pending writes on committed data; deeper lens layering across
  statements and relations is planned.
- **Schema migration plan computation.** The `keep` / `re-extract` /
  `tombstone` action vocabulary is specified in
  [`spec/03_schema/05_versioning.md`](spec/03_schema/05_versioning.md). The
  worker sweeps and flags schema-version-behind rows today; computing and
  executing the full per-row action plan is planned.
- **Partial WAL replay on tantivy recovery.** A `NeedsRebuild` today rebuilds
  from scratch; partial replay via indexer cursors is planned.
- **Slot free-list on the live write path.** The `SlotAllocator` (free list +
  version bump on realloc) is implemented and exercised by recovery, but the
  writer still mints slots from a `next_slot` atomic, so physical slot reuse is
  deferred. Slot *versioning* is enforced regardless, via the `MemoryId`
  encoding.
- **Entity hard reclamation.** `EntityGc` tombstones orphaned entities with an
  audit row; physical reclamation of those rows rides the existing
  tombstone-grace flow.
- **Initial snapshot for similarity subscriptions.** A `SUBSCRIBE` with a
  `similar_to` filter gates live events on cosine similarity to a reference
  memory; `include_history` currently applies the other filters only.

---

## Larger architectural changes

Capability changes that would touch the wire protocol, the on-disk formats or
the cluster architecture — each a multi-month design in its own right, and each
gated on a demonstrated operator need the current single-node architecture
cannot satisfy.

- **Multi-node clustering.** Distributed coordination, range-based sharding,
  cross-node query fan-out.
- **Replication.** Today, node loss means restore-from-snapshot. Synchronous
  WAL streaming and asynchronous follower replication are both candidates, with
  different trade-offs.
- **Tenant offloading / lazy loading.** Cold tenants serialized to object
  storage, lazy-loaded on first query.
- **Storage–compute separation with a freshness layer.** Blob storage as the
  source of truth plus an in-memory freshness layer for recent writes — a
  Pinecone-serverless-style architecture.
- **IVF + PQ on top of HNSW.** For billion-vector scale; today's HNSW is
  RAM-heavy past ~10⁷ vectors per shard.
- **Range-based sharding with Raft replication.** CockroachDB-style auto-split
  / auto-merge ranges. Today's `BLAKE3(space_id) % shard_count` caps a single
  space's throughput at one shard.
- **Decoupled roles.** FoundationDB-style coordinator / proxy / log / resolver
  / storage as separate processes.
- **Multi-region active-active.** Cross-region writes with replication.
- **Federated knowledge graphs.** Cross-node entity and statement queries — a
  different system from Brain's local-first design.
- **Embedding-model migration.** An offline re-embed path behind the reserved
  `ADMIN_MIGRATE_EMBEDDINGS` opcode, plus a register-model /
  retire-fingerprint admin surface. Today a model change means a fresh
  deployment and a re-ingest.
- **A query language.** The wire protocol is typed RPC; a SQL-like text
  language would be a separate, much larger effort.

Several of these (offloading, the storage–compute split, IVF+PQ, range
sharding, decoupled roles) are explored in more depth as future-direction
candidates in
[`spec/01_architecture/07_wedges_and_roadmap.md`](spec/01_architecture/07_wedges_and_roadmap.md)
§Roadmap.
