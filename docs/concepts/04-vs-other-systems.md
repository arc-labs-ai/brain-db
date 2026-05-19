# 04 — Brain vs other systems

"How is this different from X?" is the question that comes up
most. This chapter answers it for the obvious comparisons:
Postgres, vector databases, graph databases, RAG frameworks,
and the "I'll just build it myself" option.

The honest version: Brain solves a problem that *combines*
pieces of all of those. None of them is wrong; they just each
solve a different slice.

---

## The dimensions that matter

Before the comparisons, here's how I'll grade each system.
A "cognitive memory store" for an AI agent needs:

| Dimension | What it means |
|---|---|
| **Vector search** | Can it find content semantically similar to a query? |
| **Lexical search** | Can it find exact terms / phrases (BM25)? |
| **Typed knowledge** | Can it store typed entities + statements + relations with provenance? |
| **Memory verbs** | Does it speak the agent's vocabulary (encode/recall/forget) or its own? |
| **Owns embedding** | Does the store generate embeddings itself, or does the caller? |
| **Forgetting** | Does it have a real "forget with grace period" story, or only `DELETE`? |
| **Background cognition** | Does it consolidate, decay, re-derive? Or just sit there? |
| **Crash durability** | Hard guarantees about what survives a crash? |
| **Operational shape** | Single binary or a stack? |

A row of mostly ✅ means the system was designed for cognitive
agent memory. A row mostly ❌ means it can be *adapted* to the
role but you'll be writing glue.

---

## Brain vs Postgres

[Postgres](https://www.postgresql.org/) is the default
production database. Battle-tested, ACID, mature tooling,
extensions for almost everything (including
[pgvector](https://github.com/pgvector/pgvector) for vector
search).

| Dimension | Postgres + pgvector | Brain |
|---|---|---|
| Vector search | ✅ via pgvector | ✅ |
| Lexical search | ✅ via FTS extensions | ✅ via tantivy (knowledge layer) |
| Typed knowledge | ⚠️ you model it yourself in SQL | ✅ as a first-class concept |
| Memory verbs | ❌ you write the queries | ✅ |
| Owns embedding | ❌ caller computes | ✅ |
| Forgetting | ❌ just `DELETE` | ✅ tombstone + grace + cascade |
| Background cognition | ❌ you write workers | ✅ twelve built-in |
| Crash durability | ✅ | ✅ |
| Operational shape | one binary | one binary |

**Use Postgres when:** you have tabular data that benefits
from SQL — joins, aggregations, transactions across many
unrelated tables. Most apps need this layer regardless. Brain
isn't a replacement for it.

**Use Brain when:** the agent's memory is the thing you're
modelling. You don't really want to write SQL for it; you
want the verbs that match the agent's mental model.

**Use both:** common pattern. Postgres for your app's
business data (users, orders, line items), Brain for the
agent's memory of conversations and the typed knowledge it
extracts.

> **What's pgvector?**
>
> An extension for Postgres that adds a `vector` data type and
> approximate-nearest-neighbour indexes (IVF, HNSW). Makes
> Postgres into "Postgres + a vector column" — same DBMS, new
> column type.
>
> See [pgvector on GitHub](https://github.com/pgvector/pgvector).

---

## Brain vs vector databases

[Pinecone](https://www.pinecone.io/),
[Qdrant](https://qdrant.tech/),
[Milvus](https://milvus.io/),
[Weaviate](https://weaviate.io/),
[Chroma](https://www.trychroma.com/) — these are the
purpose-built vector stores. They give you `upsert(id, vector,
metadata)` and `query(vector, top_k, filter)` over a managed
or self-hosted index.

| Dimension | Vector DB | Brain |
|---|---|---|
| Vector search | ✅ (the whole product) | ✅ |
| Lexical search | ⚠️ recent additions in some | ✅ tantivy |
| Typed knowledge | ❌ | ✅ |
| Memory verbs | ❌ | ✅ |
| Owns embedding | ❌ (caller computes) | ✅ |
| Forgetting | ⚠️ `delete` only | ✅ tombstone + grace + cascade |
| Background cognition | ❌ | ✅ |
| Crash durability | varies | ✅ WAL + fsync |
| Operational shape | one binary (or managed) | one binary |

The hardest comparison, because vector DBs *are* the
substrate of Brain — they solve the "find similar vectors"
problem really well. Brain's substrate-only mode is in the
same ballpark.

**Where vector DBs win:** if all you need is fast vector
search at scale (millions to billions of vectors), and your
caller is happy generating embeddings themselves, the
specialised vector DB is a smaller, simpler, more mature
piece of software. Pinecone in particular has years of
operational polish.

**Where Brain wins:** the moment you want anything past "find
similar vectors":

- The substrate owns the embedder. Your agent sends text, not
  vectors. Dedup by semantic content is automatic. A model
  upgrade re-embeds everything; you don't.
- Forgetting is a first-class verb with a grace period and
  cascading effects, not a delete-and-forget.
- Salience decays in the background; consolidation runs
  periodically; the index gets rebuilt to drop tombstones.
  You don't write workers.
- The knowledge layer (entities/statements/relations) sits in
  the same database, with provenance back to the source
  memories. A vector DB plus a separate graph DB plus an
  extractor pipeline isn't equivalent — the layers wouldn't
  share transactions or trust.

**Use a vector DB when:** all you need is vector search.
You'll glue everything else together yourself, but you want
the vector-search part to be excellent.

**Use Brain when:** you want the whole substrate, plus
optionally the knowledge layer, plus the cognitive verbs, in
one binary.

---

## Brain vs graph databases

[Neo4j](https://neo4j.com/),
[JanusGraph](https://janusgraph.org/),
[Dgraph](https://dgraph.io/),
[TigerGraph](https://www.tigergraph.com/) — these are the
property-graph databases. They model nodes + edges with
properties on both, queried with Cypher or Gremlin or
SPARQL.

| Dimension | Graph DB | Brain |
|---|---|---|
| Vector search | ⚠️ recent integrations (Neo4j) | ✅ |
| Lexical search | ⚠️ via plugins | ✅ |
| Typed knowledge | ✅ (their whole product) | ✅ (the knowledge layer) |
| Memory verbs | ❌ | ✅ |
| Owns embedding | ❌ | ✅ |
| Forgetting | ⚠️ delete only | ✅ |
| Background cognition | ❌ | ✅ |
| Crash durability | ✅ | ✅ |
| Operational shape | one binary (or cluster) | one binary |

A property graph is exactly the shape of Brain's knowledge
layer — entities are nodes, relations are edges, statements
are typed "this entity says this about that entity."

**Where graph DBs win:** arbitrary graph algorithms. Brain's
graph traversal is a focused depth-limited walk anchored on
an entity, with relation-type filters. It's not Cypher. If
you need shortest paths, PageRank, betweenness centrality on
arbitrary subgraphs, a graph DB is the right tool.

**Where Brain wins:** for the cognitive-memory use case, you
don't usually want arbitrary graph queries — you want "what
does the agent know about X." Brain's typed retrieval
(chapter 17) is shaped for exactly that, and it's coupled
with vector search in the same query in a way bolt-on graph
DBs aren't.

Also: graph DBs assume *you* maintain the graph. Brain's
extractors maintain it for you from text.

**Use a graph DB when:** you have a real graph workload —
social network, supply chain, fraud detection — and need
graph-algorithmic queries.

**Use Brain when:** the graph is *derived* from text and the
queries are agent-shaped ("what does X believe").

---

## Brain vs RAG frameworks

[LangChain](https://www.langchain.com/),
[LlamaIndex](https://www.llamaindex.ai/),
[Haystack](https://haystack.deepset.ai/),
[Semantic Kernel](https://github.com/microsoft/semantic-kernel) —
these are the orchestration frameworks. They wire together a
vector DB, an LLM, a prompt template, and some glue code into
a "chain" that does retrieval-augmented generation.

| Dimension | RAG framework | Brain |
|---|---|---|
| Vector search | via your chosen vector DB | ✅ built-in |
| Lexical search | via your chosen text index | ✅ built-in |
| Typed knowledge | ⚠️ via knowledge-graph integrations | ✅ |
| Memory verbs | ⚠️ a "memory module" that wraps storage | ✅ first-class |
| Owns embedding | usually no | ✅ |
| Forgetting | ❌ depends on backend | ✅ |
| Background cognition | ❌ | ✅ |
| Crash durability | depends on backend | ✅ |
| Operational shape | a library + N backends | one binary |

This is the most miscategorised comparison. RAG frameworks
and Brain aren't competitors; they're at different layers.

A RAG framework is an *orchestrator* — its job is to take a
user query, plan retrieval, call your LLM, post-process the
response. It does *not* itself store anything; it points at a
vector DB and/or a database and runs the pipeline.

Brain is a *substrate*. Its job is to be the storage layer
the orchestrator points at — but with cognitive verbs and a
knowledge layer instead of `upsert/query`.

**Use a RAG framework with Brain.** This is the natural fit.
Replace the framework's vector-DB backend with Brain (it
fits anywhere a vector store does) and you get the cognitive
verbs + knowledge layer underneath. Your prompt-engineering
and chain-orchestration layer stays where it was.

**Don't use a RAG framework alone for memory.** The default
"memory module" in most RAG frameworks is a thin shim over a
vector store — you'll outgrow it the moment you need real
forgetting or typed knowledge.

---

## Brain vs building it yourself

The "I'll just glue these together" approach. Pick a vector
DB, pick a graph DB or model entities in Postgres, pick an
embedding model, write extractor code, write the workers
(decay, consolidation, garbage collection), write the
forgetting story, write the audit log, write recovery.

This is genuinely a reasonable choice for some teams. It's
also the way many of the systems Brain is compared to
started.

**Cost-of-ownership for the DIY version:**

- 2–5 services to operate (vector DB, graph DB, queue/job
  runner, embedder service, audit DB).
- Each one has its own deployment, monitoring, backup,
  upgrade story.
- You write the extractor pipeline, the worker scheduler,
  the cascading-forget logic, the schema migration story,
  the idempotency table, the consolidation algorithm.
- The "WAL across all of these so a crash is recoverable"
  problem is yours.

**When DIY wins:** you have a team that wants to own the
substrate, your scale is unusual (millions of agents,
billions of memories), or your domain pushes against Brain's
specific design choices (e.g., you need not-Linux, or you
need GPU embedding from day one, or you need
cross-region active-active).

**When Brain wins:** you don't want to spend three engineers
for two years building this. You want the substrate to be
one binary that works on a Linux box and ships with the
cognitive verbs already baked in.

---

## Where Brain doesn't fit

Stating non-fits directly because they save everyone time:

- **Not Linux.** Brain depends on `io_uring`, a Linux-specific
  kernel API. macOS and Windows are dev-only.
- **Need GPU embedding for throughput?** v1 is CPU-only; GPU
  is a future iteration. If you're at the scale where CPU
  embedding is the bottleneck, evaluate carefully.
- **Cross-region active-active?** Not in v1. You can run
  Brain in each region with separate data; you can't run an
  active-active cluster across them.
- **Multi-tenant strict isolation (compliance)?** Per-agent
  isolation works for soft isolation. If you need
  hardware-isolated tenants for compliance reasons, run a
  Brain instance per tenant.
- **Sub-millisecond p99 read latency?** Brain's substrate-only
  recall hits this for small shards, but the embedding step
  (5–10 ms on CPU) is the floor. Cache hits get there;
  cache misses don't.

---

## Summary

| Use case | Best fit |
|---|---|
| Tabular business data, SQL joins | Postgres |
| Pure vector search, at scale, you generate embeddings | Vector DB |
| Arbitrary graph algorithms on a real graph | Graph DB |
| Orchestrating an LLM pipeline | RAG framework (LangChain et al.) — *over* Brain |
| Cognitive memory for an AI agent: substrate + knowledge layer, one binary | **Brain** |

The most common production shape is "Postgres for app data +
Brain for agent memory + LangChain/LlamaIndex on top." Each
piece does what it's good at.

---

## Where to go next

- **Get hands-on:** [chapter 03](03-guided-tour.md) — what
  the Brain experience actually looks like.
- **Dig into the substrate side:**
  [chapter 05](05-memories.md) onward.
- **Dig into the knowledge side:**
  [chapter 10](10-entities.md) onward.
- **Operating Brain in production:**
  [`../guides/deployment/`](../guides/deployment/).
- **Reading the source:**
  [`../architecture/01-system-architecture.md`](../architecture/01-system-architecture.md).
