# 01.08 Comparison with Adjacent Systems

A reader new to this space deserves to know what's already out there and how Brain differs. This file is informational, not normative — it shouldn't change how you implement Brain, but it should change how you talk about Brain to others.

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

- Your application needs cognitive operations beyond vector search.
- Examples: agent memory with decay, planning over remembered structure, causal reasoning, salience-aware ranking, agent-scoped isolation.
- Writes are continuous, single-item, latency-sensitive (an agent encoding observations as it processes turns).
- You want the substrate to own embedding so deduplication, model migration, and caching are first-class.

### The clearest test

If you can describe what you want as "top-k similar vectors with this filter", use a vector database. If you find yourself building scaffolding for working memory, episode boundaries, salience updates, plan trees, or causal traces, you've reinvented part of Brain.

### How they relate

Vector databases can absolutely be a **component** underneath a cognitive substrate. We've considered (and currently rejected) the design where Brain delegates ANN search to an embedded vector database. The reason: we want one fewer process boundary on the hot path and we want the index intimately coupled with our metadata. But the design space is open; a future Brain version could reasonably swap in a vector-database engine for the ANN layer if the trade-offs change.

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

- Graph structure is *one input* to cognitive operations rather than the primary access pattern.
- The graph has a fixed semantic vocabulary (causality, derivation, similarity) — not arbitrary user-defined types.
- You need vector similarity *and* graph traversal in the same operation, with the substrate handling the join.

### How they coexist

A complex application might use both: Neo4j for an explicit knowledge graph that domain experts curate, Brain for the agent's experiential memory. Cross-references between them flow through application code.

We don't try to be Neo4j. The graph in Brain is intentionally limited — it serves cognition, not arbitrary graph queries.

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
- The data should participate in cognitive operations like planning and reasoning.

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
- The substrate decides what's salient based on patterns of access, not just one-off retrieval.

### The combination

An agent might use both: RAG over the company's documentation (a static corpus), and Brain for the agent's own working memory (dynamic, agent-specific). Both are queried during a conversation; the agent's prompt gets context from both.

## 8. vs. SQLite (the embedded option)

SQLite is the canonical embedded database. ACID, SQL queries, runs in-process.

### When to use SQLite

- Single-machine application that needs structured data.
- No need to operate a database service.
- Simple deployment story (a file).

### When to use Brain

- The cognitive substrate needs to outlive any single client process.
- Multiple processes / languages need to access the same memory.
- The workload requires the latency optimizations of a server (mmap arena, persistent connections, thread-per-core).

### The mismatch

SQLite is brilliant for embedded structured data. It doesn't try to be a vector store, an ANN engine, or a cognitive substrate. The architectural shape is different — Brain is a server because cognition belongs out-of-process; SQLite is a library because structured data often belongs in-process.

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
- You need cognitive operations not exposed by the LLM provider's memory API.
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

*Continue to [`09_glossary.md`](09_glossary.md) for vocabulary.*
