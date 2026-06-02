# 01.07 Design Wedges and Roadmap

> **TL;DR.** The five competitive differentiators every Brain decision is checked against (the wedges) and the five future-looking patterns Brain may borrow from established databases (the roadmap). Wedges are why pick Brain over alternatives; roadmap items are candidates for future adoption that must not weaken a wedge.

## Design Wedges

The five competitive differentiators that distinguish Brain from existing agent-memory systems. Higher-level than the seven engineering invariants (see CLAUDE.md §5): the invariants are **how Brain stays correct**; the wedges are **why pick Brain over alternatives**.

Every architectural decision is checked against these five. A change that weakens a wedge needs explicit justification, not silent drift.


## Wedge 1 — Typed graph + bi-temporal

Brain stores four record types — Memory, Entity, Statement, Relation — with explicit types, explicit provenance, and four timestamps per Statement (`valid_from`, `valid_to`, `extracted_at`, `record_invalidated_at`). Not flat vectors.

**What it costs.** Schema authoring overhead. Storage overhead vs. pure-vector stores (~2× metadata for typed records).

**What it buys.** Time-travel queries (`what did I believe about Alice on date X?`), structured filters (`WHERE kind = 'preference' AND confidence ≥ 0.7`), entity-anchored retrieval (`tell me everything about Alice`), supersession that's distinguishable from contradiction, and provenance back to source memories on every claim.

**Competitive context.** Pure vector stores (Pinecone, Weaviate, Chroma) can't express any of this — they treat "Alice prefers dark mode" as a string and rely on similarity search. Mem0 stores text but no typed graph. Zep has a graph layer but needs Neo4j as a dependency. Brain stores the typed graph natively.

---

## Wedge 2 — Owned embedding model

Brain runs the embedding model itself (BGE-small via candle, 384-dim). Clients send text. There is no external API call on the write path.

**What it costs.** Operator must distribute model weights (handled by the bootstrap script: `$XDG_DATA_HOME/brain/models/bge-small-en-v1.5/`). ~30 MB on disk.

**What it buys.** Latency under operator control — no network hop to OpenAI/Anthropic embedding APIs. Determinism — the same text always produces the same vector (modulo dtype). No per-call cost. Privacy — text never leaves the deployment for embedding purposes. No vendor lock — switching the embedding model is a config change + re-embed pass, not a contract negotiation.

**Competitive context.** Most cloud vector DBs offload embedding to OpenAI by default. Mem0 calls OpenAI per-write. Brain owns the embedding step end-to-end.

---

## Wedge 3 — WAL-first durability

Every write goes to the per-shard write-ahead log with `pwritev2(RWF_DSYNC)` group commit before the wire ack returns. Single-writer-per-shard discipline eliminates write-side locking. CRC32C on every WAL record and arena slot — reads verify, mismatches halt.

**What it costs.** No multi-master writes. Brain doesn't support multi-region active-active in v1 (deferred to a future major version if needed). Writes are bounded by fsync latency on the per-shard volume.

**What it buys.** No "eventually consistent" cliff edges. A successful ack means the write is durable. Recovery is replay from the WAL — deterministic, observable, fast (gigabytes per minute). No silent corruption: every CRC failure halts and alerts; the system never returns wrong data.

**Competitive context.** Most modern vector DBs are eventually-consistent under the hood (segment files commit at intervals; queries between commits miss recent writes). Brain treats durability as a first-class contract.

---

## Wedge 4 — One codebase across deployment modes

Brain ships the same Rust crates whether the deployment is embedded (in-process, single shard, no network layer), self-hosted (single-binary server on operator hardware), or cluster (multi-node with the connection-layer router). Same wire format. Same schema model. Same crates.

**What it costs.** No "cloud edition" with extra features. No per-mode optimizations that fork the codebase.

**What it buys.** Operators can prototype embedded, deploy self-hosted, and grow to cluster without rewriting application code. Bug fixes propagate to all three. Tests against embedded validate self-hosted. No "the cloud product has graph features the open-source product doesn't" trap — every feature lives in the open Apache 2.0 crates.

**Competitive context.** Mem0 gates its graph features behind a $249/mo cloud tier. Pinecone serverless is cloud-only. Zep's enterprise tier has features the open-source server doesn't. Brain refuses this split.

---

## Wedge 5 — Apache 2.0, no SaaS lock-in

Brain — the typed graph, the extractor pipeline, the reranker, the schema DSL — is Apache 2.0. Operators can self-host any feature documented in the spec.

**What it costs.** No premium commercial features to up-sell. Revenue model relies on hosting + support, not feature gating.

**What it buys.** Adoption velocity — agent developers can prototype in `cargo run` without provisioning anything. Long-term trust — no rug pulls, no "open core" license switch. Source available for audit at every layer.

**Competitive context.** Several established memory systems (Mem0, Zep) operate the source on cloud-only tiers + restricted self-host. Brain's stance: the data layer should be open, period.

---

## What's NOT a wedge

For clarity — these are features Brain has but doesn't differentiate on:

- **HNSW for vector indexing** — table stakes. Pinecone, Weaviate, Qdrant all use HNSW (or close kin). Brain's `M=16, ef_construction=200, ef_search=64` defaults are standard.
- **Rank fusion** — RRF with `k=60` is standard for hybrid retrieval. Brain's weighted variant + adaptive top-K is incremental, not category-defining.
- **Per-shard tantivy for lexical** — common pattern. Brain's choice but not novel.
- **Glommio for async I/O** — implementation detail. Could be Tokio + io_uring + careful structuring; same result.

---

## Already implemented: bucketed shard storage

Within a shard, Brain stores each concern in its own bucket on disk: memory HNSW, entity HNSW, statement HNSW, memory tantivy, statement tantivy, redb metadata, and the LLM extractor cache. Each bucket has its own tuning surface — different compaction schedules, different page sizes, different LSM levels — and can be operated, sized, and migrated independently. Weaviate documents this pattern explicitly for multi-tenancy isolation.

This is not a wedge — operators do not pick Brain over alternatives because of it, and several other systems (Weaviate, Pinecone) ship comparable layouts. It's a structural property worth surfacing because it constrains other architectural decisions: the offload, replication, and storage-compute-split patterns in the [Roadmap](#roadmap) below all assume per-bucket independence as a starting point.

## Drift watch

A wedge weakens when:

- **Wedge 1 weakens** if Brain ships a schemaless-only mode where the typed graph is opt-in afterthought.
- **Wedge 2 weakens** if Brain adds a "BYO embedding" path that becomes the recommended default.
- **Wedge 3 weakens** if write-path durability requirements relax for performance ("eventually consistent mode" flag).
- **Wedge 4 weakens** if the cluster build gains features the embedded build doesn't have.
- **Wedge 5 weakens** if a feature lands behind a commercial license, or if the Apache 2.0 commitment quietly softens to AGPL or BSL.

Flag any change that weakens a wedge for explicit review.

### Resolved — mode bifurcation (substrate vs hybrid)

**Status:** Resolved in phase 26 (`f2d2f61` / `778fe54` / `2bc5181`).

Historically Brain branched between a "substrate" path (no schema declared → memory-only ANN search) and a "hybrid" path (schema declared → typed-graph fan-out). The branch lived in the read pipeline (`substrate_recall` vs `hybrid_recall`), the planner inputs (`has_active_schema`, `has_llm_extractor`), and the shard wiring (`Option<Arc<dyn LexicalRetriever>>`). The bifurcation violated Wedge 4 (one codebase, one shape) and made client behavior depend on what the deployment happened to have uploaded.

Phase 26 collapsed it: every shard wires the three retrievers and the tantivy indexes at spawn, `SCHEMA_UPLOAD` became associative-merge against the seeded `brain:` namespace, and extractors run on every ENCODE with per-entity persistence gating. A new `GET_CAPABILITIES` opcode (`0x0032` / `0x00B2`) lets clients introspect the shard's enabled tiers. The "schemaless mode" framing is retired throughout the spec.

Kept in the wedges log for the historical record; the bifurcation no longer exists in code or spec.

---

## Roadmap {#roadmap}

Future-looking architectural patterns that Brain may borrow from established databases. None are commitments. Each entry frames a capability gap, the pattern that closes it, and a path forward. The roadmap is read-only — items move into the spec proper only when they're scheduled and scoped.

The patterns below are the ones with the clearest fit. The design wedges above constrain which patterns Brain can adopt: anything that weakens a wedge needs explicit justification, not silent drift.


## Tenant offloading and lazy loading

**Pattern.** Tenants live in three lifecycle states — ACTIVE, INACTIVE, OFFLOADED. Cold tenants are serialized to object storage, freeing local memory. The first query on an offloaded tenant lazy-loads it back. Shards and segment files load on demand. Weaviate uses this to fit millions of tenants on shared infrastructure.

**What it enables.** Memory-layer deployments hosting many cold agents without paying full per-shard memory cost. Today Brain holds every shard's HNSW, tantivy indexes, and redb in RAM for the lifetime of the process — a deployment with 10K cold agents pays full memory cost for all of them.

**Deferred because.** Touches every storage handle, the shard spawn path, and the connection-layer router. A new offload protocol must serialize HNSW + tantivy segments + redb to object storage, restore them on cold-start, and enforce an eviction policy (LRU by `last_used_at`) — substantial scope.

**Path.** A new `brain-coldstore` crate behind a feature flag, three-state lifecycle in the shard control plane, route-and-warm logic in the connection layer. Major version event; gated by demonstrated demand from operators hitting the per-shard memory ceiling.

---

## Storage-compute separation with a freshness layer

**Pattern.** Blob storage is the source of truth. An Index Builder writes immutable segments to object storage. A Freshness Layer keeps a compact in-memory index over recent unindexed writes for sub-second read-after-write. Query Executors load segments on demand and cache locally. Pinecone serverless ships this design.

**What it enables.** Pay-per-access economics: hot agents stay in cache; cold agents cost nothing beyond object-storage residency. Cleaner read-after-write semantics than WAL replay — the freshness layer makes recent writes visible without rebuild.

**Deferred because.** Brain's current write path goes WAL → HNSW directly, in-process. Splitting the index into a "live" tier (recent writes, RAM-resident, compact) and a "main" tier (object storage, paged in on demand) requires a new compactor worker, a new query path that consults both tiers and dedupes by `(id, version)`, and a redesign of how the metadata store survives a process restart without the WAL holding everything.

**Path.** New executor mode in indexing, scheduled compaction worker, segment-aware query planner. Major version event; the cloud-deployment story for Brain at multi-tenant scale.

---

## IVF + Product Quantization on top of HNSW

**Status:** Partial — **HNSW + PQ in active development for v1.x** per [`spec/09_indexing/07_hnsw_pq.md`](../09_indexing/07_hnsw_pq.md) (phase 25). Pure IVF (no graph) remains deferred.

**Pattern.** Pure HNSW is RAM-heavy at billion-vector scale because the index references every full-precision vector. IVF (inverted-file index) partitions the vector space; PQ (product quantization) compresses each vector to ~8-16 bytes. Memory drops roughly 10× with modest recall loss. Pinecone uses this hybrid at scale.

**What it enables.** Brain at billion-vector scale without provisioning 1.5 TB of vector RAM. Today's HNSW loads every 384-d vector at full precision (~1.5 KB per vector); at 1M memories that's 1.5 GB, at 1B it's 1.5 TB.

**Resolved-in-part because.** HNSW+PQ (graph payload compressed; arena still full-precision for re-rank) is the lower-risk increment — keeps the §09 search interface unchanged and the existing two-tier `MainEpoch` model intact. The §19 acceptance suite re-runs under a PQ profile to gate the recall trade-off.

**Still deferred.** Pure IVF (no HNSW graph) would replace traversal with coarse-cell scan + `nprobe`. The architectural change is larger; HNSW+PQ resolves the immediate memory pressure at Brain's target scale and below.

---

## Auto-split, auto-merge ranges with Raft replication

**Pattern.** Data is partitioned into ranges. Ranges auto-split above a size threshold and auto-merge below. Each range is replicated 3× via Raft. A cluster control plane rebalances on node add/remove. CockroachDB ships this design.

**What it enables.** Big tenants scale horizontally instead of being capped at single-shard throughput. Replication closes the data-loss window when a node fails (today: single-replica per shard, restore from snapshot).

**Deferred because.** Brain shards by fixed `hash(agent_id) mod N`. A tenant accumulating 100× more memories than peers stays on one shard. Switching to range-based sharding requires a range descriptor table, per-range Raft groups, a rebalancer, and a cluster control plane — all out of scope for v1.

**Path.** Range-based sharding behind range descriptors, Raft per range, a rebalancer that responds to size and load metrics. Major release; gated by a customer hitting the single-shard ceiling, not by an internal target.

---

## Decoupled roles (separate processes per concern)

**Pattern.** Coordinators, Proxies, Transaction Logs, Resolvers, and Storage Servers each run as separate processes with distinct roles. Independent scaling, rolling upgrades, fault isolation. FoundationDB demonstrates this at scale.

**What it enables.** Operators scale each concern independently — more planner workers for read-heavy deployments, more extractor workers for ingest-heavy ones. Fault domains tighten: a runaway query in the planner doesn't cripple ingest.

**Deferred because.** Brain has one role (the shard) plus the connection layer. Splitting wire-dispatch, query planning, and storage into separate processes adds substantial operational complexity (internal RPC, process supervision, multi-process deployment manifests) without a clear customer-visible benefit at current scale.

**Path.** Process boundary between connection layer and shards, internal RPC over an existing transport, promotion of extractor workers from tasks to separate processes. Major release; requires explicit operational justification before pursuing.

---

## Sources

- Weaviate multi-tenancy architecture — <https://weaviate.io/blog/weaviate-multi-tenancy-architecture-explained>
- Pinecone serverless architecture — <https://www.pinecone.io/blog/serverless-architecture/>
- Pinecone IVF + PQ explainer — <https://www.pinecone.io/learn/vector-database/>
- CockroachDB design.md — <https://github.com/cockroachdb/cockroach/blob/master/docs/design.md>
- FoundationDB architecture — <https://apple.github.io/foundationdb/architecture.html>

---

*Continue to [`08_glossary.md`](08_glossary.md) for the shared glossary.*
