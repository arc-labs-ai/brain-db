# 01.07 Non-Goals

A specification's non-goals are at least as important as its goals. The following are **explicitly out of scope** for Brain. If any of these become required later, they will be added by amendment with appropriate spec revisions, not snuck in.

This file is here so that scope creep is visible. When someone proposes "could Brain also do X?", checking this list answers most of the conversations.

## 1. General-purpose vector database

Brain stores vectors as part of cognitive memories. It does not provide a general-purpose API for storing and searching arbitrary vectors.

If your need is "I have 100 million pre-computed embeddings of documents and I want to search them by similarity with metadata filters", use [Qdrant](https://github.com/qdrant/qdrant), [Milvus](https://github.com/milvus-io/milvus), [Weaviate](https://github.com/weaviate/weaviate), [Chroma](https://github.com/chroma-core/chroma), or [LanceDB](https://github.com/lancedb/lancedb). They are excellent at this and Brain doesn't try to compete.

The architectural difference: vector databases treat the collection-of-vectors as the primary abstraction; Brain treats the agent's memory as primary. The implications cascade through the entire stack.

## 2. Multi-modal storage in v1

The first version handles English text only. Image, audio, and video memory are deferred to a future version.

The architecture is open to multi-modality:

- The embedding layer is replaceable; a multi-modal embedding model could plug in.
- The storage layer doesn't care about modality — vectors are vectors.
- The cognitive operations would need rework to handle modality-specific filtering.

But making it actually work — handling modality-specific salience, modality-aware filters, multi-modal queries ("find images similar to this text"), efficient encoding of large blobs — is enough additional design that we treat it as a v2 milestone, not a v1 feature.

## 3. Multi-language understanding

`bge-small-en-v1.5` is English-only. Multi-language deployments use a different embedding model (e.g., `bge-m3` for multilingual support) and accept different storage characteristics (different vector dimensionality, larger model, slower inference).

Brain's architecture supports swapping the embedding model — the embedding layer is well-defined — but a deployment is configured for a single model at a time. Mixed-language deployments need either:

- One Brain cluster per language family.
- A single multilingual model accepted as the configured choice (with the latency and storage costs).

We don't try to be all things to all languages in v1.

## 4. A query language

The first version exposes a typed RPC API, not a SQL-like text language. The wire protocol carries structured opcodes with rkyv-encoded parameters.

A query language might be added later if it earns its complexity:

- It could enable richer filter expressions in `RECALL`.
- It could expose `PLAN` and `REASON` configuration in a more flexible way.
- It would help users write queries by hand for debugging.

But a proper query language has costs: parser, optimizer, error reporting, dialect drift. v1 doesn't take on that cost. The typed RPC interface is more discoverable from a typed SDK and adequate for cognitive operations.

## 5. Built-in LLM inference

Brain does not run language models. The agent (the LLM-driven application) calls Brain; Brain doesn't call the LLM.

This is a clean separation: Brain owns memory, the agent owns reasoning. Coupling them would force every Brain deployment to also run an LLM, which is a much heavier resource commitment with completely different operational characteristics.

For deployments that want LLM inference colocated with Brain, run them as separate processes on the same node or use a sidecar pattern. Brain is the substrate; the LLM is the user.

## 6. Generic graph database

Brain has a memory-edge graph for cognitive purposes (causality, derivation, similarity-derived edges, etc.). It is not a [Cypher](https://neo4j.com/developer/cypher/)-compatible property graph database.

Brain's graph supports:

- A fixed set of typed edges (8 types in v1; see [02. Data Model](../02_data_model/) §7).
- Traversal during cognitive operations.
- Auto-generated edges from similarity.

It does not support:

- Arbitrary user-defined edge schemas with rich properties.
- Cypher / Gremlin / GQL query languages.
- Pattern matching on subgraphs.

If your application needs a real property graph, use [Neo4j](https://neo4j.com/) or [Memgraph](https://memgraph.com/). If your application needs a graph as cognitive scaffolding for an agent, Brain's typed edges are sufficient.

## 7. Time-series database

Memories are timestamped, but Brain is not optimized for time-series aggregation queries — "average salience over the last 24 hours grouped by context" or similar analytical operations.

The metadata store has time-bounded queries (range scans by timestamp), but they're for cognitive operations like "memories from the last day", not for analytical reporting.

For time-series analytics over Brain's data, the recommended pattern is to export memory metadata to a dedicated time-series database ([InfluxDB](https://www.influxdata.com/), [TimescaleDB](https://www.timescale.com/)) via a streaming export job.

## 8. Real-time analytics

Aggregations across all memories are not a hot-path operation. Brain doesn't expose `SELECT COUNT(*) FROM memories WHERE ...` as a query.

Background workers compute aggregates for internal use (decay sweep statistics, consolidation candidate identification, etc.). These are visible via `ADMIN_STATS` opcodes but at a coarse granularity, not as ad-hoc analytical queries.

For analytical queries, export to a real analytical store. This separation is similar to the operational/analytical split everywhere in data engineering.

## 9. Multi-tenancy in the strong sense

Each agent is isolated by shard. Within a shard, the agent's data is fully separated from other agents'. Across shards, agents share infrastructure.

Brain does **not** enforce:

- CPU quotas across agents on shared cores.
- Memory quotas across agents on shared physical RAM.
- I/O quotas across agents on shared NVMe.
- Network quotas across agents on shared NIC.

This is not multi-tenancy in the cloud-provider sense. For hard isolation between mutually-distrusting tenants, run separate Brain clusters.

For cooperative multi-tenancy (multiple agents from the same organization, all trusted), Brain's per-agent shard isolation is sufficient.

## 10. Strong cross-shard consistency

Each shard is internally linearizable. Operations across shards (the rare admin operations like agent migration) are eventually consistent.

Brain doesn't aim to be a cross-shard transaction system. The agent model — one agent per shard — makes cross-shard transactions rare enough that we don't justify the implementation cost.

If you find yourself wanting cross-shard transactions, you're probably modeling something as a single-system that should be modeled as multiple agents communicating. Brain's primitive isn't suited to "one logical entity spread across shards."

## 11. Browser/client-side embedded use

Brain is a server. There is no embedded mode that runs in a browser, a mobile process, or a serverless function.

The architecture (mmap'd files, glommio, io_uring, persistent connections) is fundamentally server-side. Embedded vector libraries exist ([usearch](https://github.com/unum-cloud/usearch), [annoy](https://github.com/spotify/annoy)) and serve a different need.

For client applications that want local memory, a thin client SDK plus a Brain server is the recommended pattern.

## 12. SQL replacement

Brain is **not** a replacement for a SQL database. If your application's structured data is naturally tabular — orders, users, accounts, products — store it in SQL. Brain is for the agent's working knowledge, which has a different shape.

Real agent applications use both. SQL holds the structured operational data; Brain holds the agent's cognitive state.

## 13. Replication in v1

Replication is *intentionally deferred* from v1. Single-replica per shard means loss of a node's storage means loss of its agents until restored from snapshot.

This is not a permanent design choice — it's a v1 simplification. Replication options range from synchronous WAL streaming (best durability, latency cost) to asynchronous follower replication (best performance, eventual durability). Each has design choices that warrant their own spec.

For v1, the operational story for durability is:

- WAL provides crash safety on a single node.
- Snapshots provide point-in-time backup.
- Restore-from-snapshot recovers from node loss.

This is acceptable for many use cases (research, internal tools, medium-criticality deployments). It is not acceptable for high-availability production. Replication is the v2 work.

## 14. Forward compatibility past one version

The wire protocol and storage formats commit to backward compatibility one version (N supports N-1 readers/writers). They do **not** commit to forward compatibility (N readers handling N+1 writers). Newer formats refuse to load on older servers with a clean error.

A theoretical "everyone runs the same version forever" deployment doesn't need to think about this. Real rolling upgrades only need one version of compatibility, which we provide.

Longer compatibility windows (N supporting N-3) are out of scope; they multiply test matrix and maintenance cost without proportional benefit.

## 15. Fine-grained access control

Brain has authentication (who is connecting?) and shard-level authorization (does this connection's agent_id own this shard?). It does **not** have:

- Per-memory access control lists.
- Field-level security (e.g., "this agent can read text but not vectors").
- Time-bounded permissions.
- Auditable per-operation authorization decisions beyond connection-level.

If your application needs these, layer them in front of Brain (an access-control proxy) or use Brain only for memories the application has already determined the user is allowed to access.

## 16. Anti-features

Three features are not just out of scope — they're things we've decided we won't add, ever:

- **Eval()-style query injection.** No query language operator that takes runtime-provided code.
- **Untrusted embedding models.** The embedded model is determined at deployment time by the operator, not by the agent's request.
- **Implicit cross-region writes.** A write goes to one region; cross-region propagation is explicit.

These three together prevent a class of supply-chain and cross-tenant attacks that have plagued other database systems.

---

*Continue to [`08_comparison.md`](08_comparison.md) for comparison with adjacent systems.*
