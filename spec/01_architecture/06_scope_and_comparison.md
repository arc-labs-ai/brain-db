# 01.06 Scope and Comparison

> **TL;DR.** What Brain explicitly does NOT do (the non-goals list, so scope creep is visible) and how Brain sits among adjacent systems (SQL, vector DBs, graph DBs, agent-memory frameworks).

## Non-Goals

A specification's non-goals are at least as important as its goals. The following are **explicitly out of scope** for Brain. If any of these become required later, they will be added by amendment with appropriate spec revisions, not snuck in.

This section is here so that scope creep is visible. When someone proposes "could Brain also do X?", checking this list answers most of the conversations.


## 1. General-purpose vector database

Brain stores vectors as part of cognitive memories. It does not provide a general-purpose API for storing and searching arbitrary vectors.

If your need is "I have 100 million pre-computed embeddings of documents and I want to search them by similarity with metadata filters", use [Qdrant](https://github.com/qdrant/qdrant), [Milvus](https://github.com/milvus-io/milvus), [Weaviate](https://github.com/weaviate/weaviate), [Chroma](https://github.com/chroma-core/chroma), or [LanceDB](https://github.com/lancedb/lancedb). They are excellent at this and Brain doesn't try to compete.

The architectural difference: vector databases treat the collection-of-vectors as the primary abstraction; Brain treats the agent's memory as primary. The implications cascade through the entire stack.

## 2. Multi-modal storage in v1

The first version handles English text only. Image, audio, and video memory are deferred to a future version.

The architecture is open to multi-modality:

- The embedding layer is replaceable; a multi-modal embedding model could plug in.
- The storage layer doesn't care about modality — vectors are vectors.
- The operations would need rework to handle modality-specific filtering.

But making it actually work — handling modality-specific salience, modality-aware filters, multi-modal queries ("find images similar to this text"), efficient encoding of large blobs — is enough additional design that we treat it as a v2 milestone, not a v1 feature.

## 3. Multi-language understanding

`bge-small-en-v1.5` is English-only. Multi-language deployments use a different embedding model (e.g., `bge-m3` for multilingual support) and accept different storage characteristics (different vector dimensionality, larger model, slower inference).

Brain's architecture supports swapping the embedding model — the embedding layer is well-defined — but a deployment is configured for a single model at a time. Mixed-language deployments need either:

- One Brain cluster per language family.
- A single multilingual model accepted as the configured choice (with the latency and storage costs).

Brain does not try to be all things to all languages in v1.

## 4. A query language

The first version exposes a typed RPC API, not a SQL-like text language. The wire protocol carries structured opcodes with CBOR-encoded parameters.

A query language might be added later if it earns its complexity:

- It could enable richer filter expressions in `RECALL`.
- It could expose `PLAN` and `REASON` configuration in a more flexible way.
- It would help users write queries by hand for debugging.

But a proper query language has costs: parser, optimizer, error reporting, dialect drift. v1 doesn't take on that cost. The typed RPC interface is more discoverable from a typed client and adequate for operations.

## 5. Built-in LLM inference

Brain does not run language models. The agent (the LLM-driven application) calls Brain; Brain doesn't call the LLM.

This is a clean separation: Brain owns memory, the agent owns reasoning. Coupling them would force every Brain deployment to also run an LLM, which is a much heavier resource commitment with completely different operational characteristics.

For deployments that want LLM inference colocated with Brain, run them as separate processes on the same node or use a sidecar pattern. Brain is Brain; the LLM is the user.

## 6. Generic graph database

Brain has a memory-edge graph for cognitive purposes (causality, derivation, similarity-derived edges, etc.). It is not a [Cypher](https://neo4j.com/developer/cypher/)-compatible property graph database.

Brain's graph supports:

- A fixed set of typed edges (8 types in v1; see [02. Data Model](../02_data_model/00_purpose.md) §7).
- Traversal during operations.
- Auto-generated edges from similarity.

It does not support:

- Arbitrary user-defined edge schemas with rich properties.
- Cypher / Gremlin / GQL query languages.
- Pattern matching on subgraphs.

If your application needs a real property graph, use [Neo4j](https://neo4j.com/) or [Memgraph](https://memgraph.com/). If your application needs a graph as cognitive scaffolding for an agent, Brain's typed edges are sufficient.

## 7. Time-series database

Memories are timestamped, but Brain is not optimized for time-series aggregation queries — "average salience over the last 24 hours grouped by context" or similar analytical operations.

The metadata store has time-bounded queries (range scans by timestamp), but they're for operations like "memories from the last day", not for analytical reporting.

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

Brain doesn't aim to be a cross-shard transaction system. The agent model — one agent per shard — makes cross-shard transactions rare enough that Brain does not justify the implementation cost.

If you find yourself wanting cross-shard transactions, you're probably modeling something as a single-system that should be modeled as multiple agents communicating. Brain's primitive isn't suited to "one logical entity spread across shards."

## 11. Browser/client-side embedded use

Brain is a server. There is no embedded mode that runs in a browser, a mobile process, or a serverless function.

The architecture (mmap'd files, glommio, io_uring, persistent connections) is fundamentally server-side. Embedded vector libraries exist ([usearch](https://github.com/unum-cloud/usearch), [annoy](https://github.com/spotify/annoy)) and serve a different need.

For client applications that want local memory, a thin client plus a Brain server is the recommended pattern.

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

## 14. Cross-version compatibility

Brain ships monolithic: each release pairs one server build with one wire version and one set of on-disk format versions. A client at any other wire version is rejected at handshake; an on-disk file at any other format version refuses to load. There is no compatibility window across versions, no version negotiation, and no upgrade-in-place across major releases.

Upgrades use `brainctl migrate` to bring on-disk files to the current format ([`../00_overview/03_versioning.md`](../00_overview/03_versioning.md)).

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

## 17. Patterns Brain explicitly refuses

The following are concrete anti-patterns Brain refuses, with the rationale for each. These are not deferred — they're decisions, recorded here so the trade-off is visible.

| Pattern | Refused because |
|---|---|
| **Save-everything via LLM** (Mem0's default ingest path) | Blows ENCODE p99 latency budgets. Junk rate is the problem this approach has, not the solution. Brain's three-tier extractor pipeline addresses it at write time. |
| **Agent owns the layout** (Letta-style hierarchical memory blocks declared by the agent) | Breaks single-writer-per-shard. Brain controls layout; clients see typed records, not heap regions. |
| **Concatenate prior context as recall output** (ChatGPT-style "stuff the window") | Brain does not own inference. Concatenation is the agent's responsibility, not the database's. |
| **Schema-aware only** (require a schema declaration to use the system) | The typed graph is the differentiator, but schemaless mode is the on-ramp. Forcing schema up-front pushes small agents toward simpler stores. |
| **Write-back deferred WAL** (acknowledge before fsync, defer the durability barrier) | Violates WAL-before-ack. Non-negotiable; see [`07_wedges_and_roadmap.md`](07_wedges_and_roadmap.md) Wedge 3. |
| **Multi-region active-active writes in v1** | Out of scope. Deferred to a later release if a customer needs it. |
| **Open-source plugin marketplace** (runtime-loadable extractors / retrievers) | Out of scope. Plugins are compile-time registered; the trust boundary stays at the binary. |
| **Self-rebalancing across heterogeneous hardware** | Out of scope. Today's deployment assumes hash-shard with equal nodes; heterogeneous rebalancing is a major release away if it lands at all. |

---

## Comparison with Adjacent Systems

A reader new to this space deserves to know what's already out there and how Brain differs. The remainder of this section is informational, not normative — it shouldn't change how you implement Brain, but it should change how you talk about Brain to others.

## 1. vs. SQL databases (PostgreSQL, MySQL, etc.)

SQL databases store structured rows in tables with secondary indexes. Brain stores cognitive memories with embedded vectors, salience scores, and typed edges.

### When to use SQL

- The data model is naturally tabular: orders, users, accounts, line items.
- Queries are SELECT/JOIN/GROUP BY.
- Constraints and referential integrity matter.
- The application has a transactional model (multi-row updates that must succeed or fail together).

### When to use Brain

- The data is a stream of agent observations.
- Queries are similarity-based, planning-based, or reasoning-based.
- "What's most relevant?" is a frequent question.
- Memory should accumulate, decay, and consolidate over time.

### How they coexist

A real agent application typically uses both:

- **SQL** for transactional structured data: user accounts, billing, order history, configuration.
- **Brain** for the agent's working knowledge: observations, derived insights, plans, conversation history.

The two communicate via the application layer. Brain doesn't need to know about SQL; SQL doesn't need to know about Brain. The agent code stitches them together.

A common pattern: store user-facing structured records in SQL, encode the agent's private observations and reasoning into Brain. When the agent needs to act on structured data (place an order, update a setting), it consults Brain for context and executes the action against SQL.

## 2. vs. vector databases (Qdrant, Milvus, Weaviate, Chroma, LanceDB)

The full list with one-line summaries is in [`02_background.md`](02_background.md) §4. Here we focus on the comparison.

### What vector databases do well

Vector databases provide a `search(vector, k, filter) → top_k_with_metadata` API over a collection of vectors with attached metadata. They are excellent at this. They're optimized for:

- Bulk ingest of many vectors from an offline pipeline.
- Filtered ANN search with rich metadata predicates.
- Replication, sharding, and cluster management of large collections.
- Multi-tenancy at the infrastructure level.

### When to use a vector database

- Your application's primary need is "search over a corpus of pre-computed embeddings."
- Examples: RAG over documentation, image search, recommendation candidate generation, semantic deduplication.
- The corpus is largely static or grows in batches; queries dominate writes.
- You handle embedding outside the store (your own pipeline, your own model).

### When to use Brain

- Your application needs operations beyond vector search.
- Examples: agent memory with decay, planning over remembered structure, causal reasoning, salience-aware ranking, agent-scoped isolation.
- Writes are continuous, single-item, latency-sensitive (an agent encoding observations as it processes turns).
- You want Brain to own embedding so deduplication, model migration, and caching are first-class.

### The clearest test

If you can describe what you want as "top-k similar vectors with this filter", use a vector database. If you find yourself building scaffolding for working memory, episode boundaries, salience updates, plan trees, or causal traces, you've reinvented part of Brain.

### How they relate

Vector databases can absolutely be a **component** underneath a memory database. We've considered (and currently rejected) the design where Brain delegates ANN search to an embedded vector database. The reason: Brain prefers one fewer process boundary on the hot path and we want the index intimately coupled with our metadata. But the design space is open; a future Brain version could reasonably swap in a vector-database engine for the ANN layer if the trade-offs change.

## 3. vs. agent memory frameworks (Letta, Mem0, LangChain memory, LlamaIndex)

These are application-level frameworks, not infrastructure. They run in the agent's process and compose memory operations on top of pluggable backends. The full list is in [`02_background.md`](02_background.md) §5.

### What they do well

- Provide opinionated memory APIs in Python.
- Integrate with many embedding providers, vector stores, and LLM APIs.
- Lower the entry barrier for building agent applications.
- Encode best practices for memory (hierarchical memory, memory CRUD, etc.).

### When to use a framework

- You're building an agent application in Python.
- You want an opinionated runtime that handles agent state, tool use, and memory.
- You're prototyping or in early production where moving fast matters more than infrastructure independence.

### When to use Brain

- Memory is a separable concern that should outlive any single agent process.
- Multiple application processes / multiple language runtimes / multiple agents need to share infrastructure.
- You want a wire-protocol contract that survives framework version churn.
- You operate the system at scale and need the operational characteristics of a database (replication, snapshot, observability).

### The architectural relationship

Brain and Letta are not in direct competition; they sit at different architectural levels. Letta is to Brain roughly as SQLAlchemy is to PostgreSQL — the framework is useful at the application layer; Brain is what frameworks would talk to if a substrate existed at this level.

A future version of Letta or Mem0 could plausibly use Brain as its storage and recall backend instead of (or in addition to) Postgres + a vector store. We hope this happens — it's the right architectural arrangement.

## 4. vs. graph databases (Neo4j, Memgraph, ArangoDB, NebulaGraph)

Graph databases provide rich path queries (Cypher), pattern matching, and traversals over property graphs.

### What graph databases do well

- Express complex graph queries in a high-level language.
- Optimize traversals with cost-based query planning.
- Support arbitrary node and edge property schemas.
- Handle deeply-nested relationships efficiently.

### When to use a graph database

- Your data is naturally a property graph: social networks, knowledge graphs, dependency graphs.
- Your queries are pattern matches: "find users who follow users who follow X".
- Schema flexibility (different nodes have different properties) is important.
- You're building tooling on top of the graph (visualizations, exploration UIs).

### When to use Brain

- Graph structure is *one input* to operations rather than the primary access pattern.
- The graph has a fixed semantic vocabulary (causality, derivation, similarity) — not arbitrary user-defined types.
- You need vector similarity *and* graph traversal in the same operation, with Brain handling the join.

### How they coexist

A complex application might use both: Neo4j for an explicit knowledge graph that domain experts curate, Brain for the agent's experiential memory. Cross-references between them flow through application code.

Brain does not try to be Neo4j. The graph in Brain is intentionally limited — it serves cognition, not arbitrary graph queries.

## 5. vs. caches and KV stores (Redis, Memcached, RocksDB)

Caches store opaque values keyed by strings, with a TTL. KV stores like RocksDB add ordered keys, transactions, and persistence.

### When to use a cache

- Ephemeral key-based lookup: session data, computed results, rate-limit counters.
- Pure-memory or on-disk-with-eviction storage of bounded data.
- Speed matters more than richness of query.

### When to use Brain

- Persistent, similarity-queryable, agent-scoped state.
- Memory that should accumulate, decay, and consolidate rather than expire on TTL.
- Queries beyond key lookup: "what's similar to this?"

### How they relate

Redis is to Brain roughly as a hash map is to a database. Both have legitimate roles; they don't substitute for each other. An agent application typically uses both:

- **Redis** for the agent's session-scoped working memory: current tool-use state, token usage tracking, ephemeral caches.
- **Brain** for the agent's persistent cognitive memory: observations, derived knowledge, plans.

## 6. vs. document stores (MongoDB, Elasticsearch)

Document stores hold semi-structured documents (typically JSON) and offer full-text and field-level queries.

### When to use a document store

- The data is naturally document-shaped: articles, products, log records.
- Queries mix exact-match (field values) and full-text search.
- Schema is flexible per document.

### When to use Brain

- The "document" is an agent observation that benefits from being embedded into vector space.
- Queries are similarity-based, not full-text or field-match.
- The data should participate in operations like planning and reasoning.

### Elasticsearch specifically

Elasticsearch is increasingly adding vector capabilities. It's becoming a vector database with full-text search bolted on. For agent memory, the same comparison as §2 applies: ES does great vector search but doesn't speak cognition.

## 7. vs. RAG systems (LangChain RAG, LlamaIndex, custom RAG pipelines)

Retrieval-Augmented Generation (RAG) systems combine a vector store with an LLM to answer questions over a corpus.

### What RAG does well

- Take a question, retrieve relevant context from a corpus, prompt the LLM with context + question, return the answer.
- Scale to large corpuses by indexing and chunking.
- Stay current via re-indexing.

### When to use RAG

- Your need is "answer questions over this fixed corpus of documents".
- The corpus is well-bounded (a documentation set, a knowledge base, a product catalog).
- Each query is independent; no continuity between queries needed.

### When to use Brain

- Memory is dynamic, continuous, and agent-specific.
- Across queries, the agent accumulates state.
- Brain decides what's salient based on patterns of access, not just one-off retrieval.

### The combination

An agent might use both: RAG over the company's documentation (a static corpus), and Brain for the agent's own working memory (dynamic, agent-specific). Both are queried during a conversation; the agent's prompt gets context from both.

## 8. vs. SQLite (the embedded option)

SQLite is the canonical embedded database. ACID, SQL queries, runs in-process.

### When to use SQLite

- Single-machine application that needs structured data.
- No need to operate a database service.
- Simple deployment story (a file).

### When to use Brain

- The memory database needs to outlive any single client process.
- Multiple processes / languages need to access the same memory.
- The workload requires the latency optimizations of a server (mmap arena, persistent connections, thread-per-core).

### The mismatch

SQLite is brilliant for embedded structured data. It doesn't try to be a vector store, an ANN engine, or a memory database. The architectural shape is different — Brain is a server because cognition belongs out-of-process; SQLite is a library because structured data often belongs in-process.

## 9. vs. proprietary AI memory products

Several closed-source products offer "memory for AI" as a hosted service: OpenAI's persistent memory in ChatGPT, Anthropic's project knowledge, custom enterprise solutions.

### What they do well

- Trivial integration (use their API, they handle everything).
- Operated by the LLM provider, so no infrastructure to manage.
- Quality of memory is tied to the LLM provider's quality.

### When to use them

- You're using a single LLM provider and want managed memory.
- You don't operate infrastructure.
- You accept vendor lock-in.

### When to use Brain

- You operate infrastructure and want control over your data.
- You use multiple LLM providers, or change providers.
- You need operations not exposed by the LLM provider's memory API.
- You want the memory to be a portable, queryable, exportable artifact.

### The trade-offs

Brain is more work to operate than a hosted memory service. In exchange, it's an open substrate you can audit, tune, and extend. Different deployments will make different choices; both are legitimate.

## 10. The comparison summary

| System type | Primary abstraction | Best for | Use with Brain? |
|---|---|---|---|
| SQL database | Tabular data | Transactional structured data | Yes, for structured data |
| Vector database | Vector + metadata | Pre-computed embedding search | Possibly, if Brain delegates ANN |
| Graph database | Property graph | Rich graph queries | Yes, for explicit knowledge graphs |
| Agent memory framework | Memory operations in-process | Quick agent prototyping | Brain is a backend they could use |
| Cache / KV | Opaque value by key | Ephemeral state | Yes, for session-scoped state |
| Document store | Semi-structured documents | Document corpora | Possibly, depends on access patterns |
| RAG system | Retrieval + LLM | Q&A over fixed corpus | Yes, for static corpus alongside Brain |
| Embedded DB (SQLite) | In-process structured data | Single-machine apps | Different scope; both can coexist |
| Hosted AI memory | Managed memory API | No-ops integration | Trade-off: ops vs control |

---

*Continue to [`07_wedges_and_roadmap.md`](07_wedges_and_roadmap.md) for the five competitive differentiators.*
