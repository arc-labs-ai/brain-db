# 01.04 The Architectural Layers

Brain is a single Rust binary that exposes a binary protocol over TCP. Internally, it is organized into seven layers. Each layer has a defined interface to the layers above and below; cross-layer access is explicitly forbidden.

This file presents the layers in summary, with cross-references to the detail specs that fully document each one. Each section here should be enough to understand the layer's role, responsibilities, and key dependencies — but not its byte-level details.

## The full diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│                            CLIENTS                                   │
│   Client (any language; speaks the wire protocol directly)          │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
                           Custom binary protocol over TCP
                           CBOR (structured), little-endian f32 (vectors)
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L1 │ CONNECTION LAYER  (glommio, thread-per-core, io_uring)         │
│     │   • One runtime per CPU core, no work-stealing                  │
│     │   • Each core owns its connections + its slice of agent state   │
│     │   • Auth, session establishment, frame parsing                  │
│     │   • Backpressure and flow control                               │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L2 │ EMBEDDING LAYER  (candle + bge-small-en-v1.5)                  │
│     │   • Tokenization, batching, inference                           │
│     │   • LRU cache keyed on text hash                                │
│     │   • Optional GPU batching path                                  │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L3 │ QUERY PLANNER                                                   │
│     │   • Pure function: (op, query, stats) → execution plan          │
│     │   • Strategy selection: schemaless | hybrid (default)        │
│     │   • Plan caching for repeated query shapes                      │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L4 │ EXECUTION ENGINE                                                │
│     │   ┌─────────┐ ┌─────────┐ ┌────────┐ ┌──────────────┐          │
│     │   │ ANN     │ │ Lexical │ │ Graph  │ │ VSA algebra  │          │
│     │   │ (HNSW)  │ │(tantivy)│ │ walk   │ │ (bind/bundle)│          │
│     │   └─────────┘ └─────────┘ └────────┘ └──────────────┘          │
│     │   Lock-free read path. Single-writer-per-shard discipline.     │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L5 │ STORAGE LAYER                                                   │
│     │   ┌──────────────┐  ┌────────────────┐  ┌─────────────────┐   │
│     │   │ Vector Arena │  │ Episodic WAL   │  │ Metadata + Graph│   │
│     │   │ mmap'd f32   │  │ append-only    │  │ redb (B-tree)   │   │
│     │   └──────────────┘  └────────────────┘  └─────────────────┘   │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L6 │ BACKGROUND WORKERS                                              │
│     │   • Decay, consolidation, index maintenance, snapshot           │
│     │   • On dedicated cores, never on request-serving cores         │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │
┌──────────────────────────────────▼──────────────────────────────────┐
│  L7 │ SHARDING + CLUSTERING                                           │
│     │   • Hash(agent_id) → shard. Per-shard isolation.               │
│     │   • Stateless router for cross-node queries.                   │
│     │   • Horizontal scaling = add nodes, rebalance shards.          │
└─────────────────────────────────────────────────────────────────────┘
```

---

## L1: Connection Layer

**Implemented on:** [Glommio](https://github.com/DataDog/glommio).

**Detail spec:** This document, [`02_background.md`](02_background.md) §6 covers the runtime choice. The connection layer's specifics are in [04. Wire Protocol](../04_wire_protocol/00_purpose.md).

### Responsibilities

- TCP accept loop on a configurable port (default `7474`).
- Optional TLS termination via [rustls](https://github.com/rustls/rustls).
- Session establishment: protocol version check, authentication, namespace selection.
- Frame parsing (binary-protocol; 32-byte header, CBOR-decoded bodies).
- Dispatch into the embedding layer or the planner depending on opcode.
- Backpressure: when downstream queues fill, the read loop awaits, the TCP window narrows, and the client experiences flow control end-to-end with no application-level signaling.

### Key design choice: thread-per-core, no work-stealing

Each CPU core runs an independent Glommio executor. There is no shared scheduling state and no work-stealing. A new TCP connection is bound to the core that accepted it and stays there for its lifetime, unless explicitly migrated.

This is the most consequential architectural decision for tail latency. It means:

- No cross-core synchronization on the request path.
- State that's owned by core 5 stays in core 5's L1/L2 cache.
- Adding cores scales linearly until you hit memory bandwidth (typically ~64 cores per node before NUMA pain).
- Scheduling is predictable: a connection's work happens on its assigned core, never elsewhere.

### Latency contribution

Approximately 1–2 µs per request on the warm path (frame parse + dispatch + frame build). This is below the noise floor of every other layer.

### Per-shard affinity

Within the connection layer, when a frame arrives, it is routed to the core that owns the target shard. This may not be the core that received the connection; if it isn't, the frame is forwarded via a per-core SPSC queue. Brain accepts this one cross-core hop because:

- It's the only cross-core operation on the request path.
- It happens at the front of the request, before any expensive work.
- It's a single atomic-write + wakeup, no shared data structures.

The alternative — routing connections to shard-owning cores at accept time — requires the client to know the shard layout, which complicates the protocol.

---

## L2: Embedding Layer

**Implemented on:** [candle](https://github.com/huggingface/candle), HuggingFace's "minimalist ML framework for Rust with a focus on performance (including GPU support)".

**Model:** [`bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5) from [FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding).

**Detail spec:** [07. Embedding Layer](../07_embedding/00_purpose.md).

### Responsibilities

- Tokenization of input text (BERT WordPiece, max 512 tokens) via [HuggingFace tokenizers](https://github.com/huggingface/tokenizers).
- Inference to produce a 384-dimensional `f32` vector.
- L2 normalization of the output (so cosine similarity becomes dot product).
- LRU caching keyed on a hash of the input text — repeated cues skip inference entirely.
- Optional GPU batching: when CUDA is available, requests within a small time window are batched onto the GPU.

### Why this layer exists

Brain owns embedding. We argued in [`01_problem.md`](01_problem.md) §4.2 that Brain should take text, not pre-computed vectors, because embedding ownership is what enables:

- **Deduplication** by semantic content (two memories with same embedding are recognized as duplicates).
- **Automatic re-embedding** when the model is upgraded.
- **Caching** keyed on text rather than on caller-computed hashes.
- **Per-deployment model lock-in** — operators choose the model, agents don't.

The cost is making Brain ML-aware. Brain accepts this; it's the layer where this awareness belongs.

### Latency contribution

This layer dominates request latency in CPU-only deployments:

- Cache hit: <0.001 ms (just a hashmap lookup).
- Cache miss, CPU inference: 5–10 ms.
- Cache miss, GPU batched: <1 ms amortized.

In a typical workload with high cue diversity (most cues are distinct), expect ~10 ms per encode/recall on CPU.

---

## L3: Query Planner

**Detail spec:** [12. Query Optimizer](../12_query_optimizer/00_purpose.md).

### Responsibilities

The query planner is a **pure function** from `(operation, query parameters, current shard statistics) → execution plan`. The plan is a typed Rust enum, not a string; there is no SQL-like text parsing.

For `RECALL`, the planner chooses among:

- **Hybrid** (default) — runs ANN, lexical (tantivy), and memory-edge graph retrievers in parallel and fuses ranks via RRF.
- **Schemaless** — pure ANN over the memory HNSW, no fusion. Selected automatically when the request carries a `txn_id`: read-your-writes requires the per-txn buffer overlay, which the lexical and graph retrievers (working off committed indexes) cannot see. Not selectable from the wire — RECALL is one verb. A shard with an unwired retriever refuses to spawn, so the planner never picks substrate as a degraded fallback at query time.

For `PLAN`, the planner chooses among A* and MCTS. For `REASON`, the planner constructs an inference DAG.

### Strategy selection inputs

The planner sees:

- The operation type (`RECALL`, `PLAN`, etc.).
- The query parameters (cue, filters, top_k, budget, hints).
- Current shard statistics (memory count, salience distribution, recent load).
- Cached plans for similar query shapes.

It does not see the actual data; the plan is structural, not value-dependent.

### Plan caching

Plans are cached for repeated query shapes (identical structure modulo cue text). The planner adds <1 µs to the request latency for cached plans, ~10 µs for fresh plans.

### What the planner is not

The planner does not execute the plan. It produces the plan and hands it off to the execution engine. Keeping these separated lets us evolve them independently — the planner gets smarter without changing the executor's interface.

---

## L4: Execution Engine

**Detail spec:** [12. Query Optimizer](../12_query_optimizer/00_purpose.md) §3 onward.

### Responsibilities

The execution engine consists of four parallel implementations, one per execution strategy:

#### vector executor

Wraps the HNSW index. The hot loop is SIMD-accelerated dot products against candidate vectors. Reads are lock-free under [crossbeam-epoch](https://github.com/crossbeam-rs/crossbeam/tree/master/crossbeam-epoch) reclamation; writes are funneled through a single writer per shard.

The vector executor is Brain's vector arm of every `RECALL` — it always contributes to the hybrid fusion, and is the sole contributor for the transactional substrate path (the only case where the lexical and graph retrievers are skipped). See [09. Indexing](../09_indexing/00_purpose.md).

#### Lexical executor

Wraps the tantivy text index. Returns BM25-ranked memory ids for the cue's tokenized form. One of the three retrievers fused by the hybrid path.

#### Graph executor

Traverses the typed-edge graph in `redb`-backed metadata storage. Used during `PLAN`, `REASON`, and as the third hybrid-RECALL retriever (memory-edge graph walks; entity-anchored graph traversal when a schema is declared).

#### VSA algebra

Implements bind, bundle, and unbind operations from Vector Symbolic Architectures. Used during `REASON` to manipulate compositional representations.

### The shared invariant: no allocations on the hot path

The engine maintains preallocated scratch buffers per core; no allocations occur on the hot path after startup. This is critical for predictable p99 latency — allocation jitter is the most common cause of tail-latency spikes in async services.

The implementation discipline:

- Per-core arenas for transient state.
- Pre-warmed scratch buffers sized for typical operations.
- `SmallVec` and similar stack-allocating structures for short collections.
- Profiling rules out new allocations in any merged code on hot paths.

### The single-writer-per-shard discipline

Within a shard, all writes (ENCODE, FORGET, salience updates, edge additions) funnel through a single writer task on the shard's owning core. This eliminates write-write contention and removes the need for locks on most data structures — the writer is the only mutator.

Multiple readers run concurrently on the same core (via cooperative async multitasking) and may run concurrently with the writer. Reader-writer coordination uses epoch-based reclamation rather than locks. See [14. Concurrency](../14_concurrency/00_purpose.md).

---

## L5: Storage Layer

**Detail specs:** [08. Storage: Arena & WAL](../08_storage/00_purpose.md) and [10. Metadata + Graph Store](../10_metadata/00_purpose.md).

### Three coordinated stores per shard

#### Vector arena

A memory-mapped flat file (`mmap` with `MAP_SHARED`), one slot per memory at fixed alignment. Slot reads are zero-copy `&[f32]` views — the bytes from disk are directly readable by the SIMD code without any conversion.

Slot size for our 384-dim `f32` vectors is 1600 bytes (1536 vector + 64 metadata/padding, aligned for AVX-512).

#### Episodic WAL

An append-only log of every state-mutating operation. Written via `O_DIRECT` with `RWF_DSYNC` for durability, using `io_uring` for batched submission. Per-shard WAL means group commits are localized; no cross-shard fsync coupling.

The WAL is the source of truth for crash recovery. Every other store (arena, metadata) can be reconstructed from the WAL.

#### Metadata + graph

[redb](https://github.com/cberner/redb), a "simple, portable, high-performance, ACID, embedded key-value store" with copy-on-write B-trees. Holds salience, context, timestamps, edge lists.

redb gives us:

- ACID transactions for the metadata side of operations.
- Copy-on-write B-trees, so readers see consistent snapshots without locking.
- Pure Rust, no C dependency.

### Coordination between stores

The arena and metadata stores are coordinated via two-phase logging in the WAL. The sequence on `ENCODE`:

1. Allocate a slot in the arena.
2. Write a WAL record describing the encode (slot_id, vector, metadata).
3. fsync the WAL via group commit.
4. Memcpy the vector to the arena slot.
5. Update metadata in redb.
6. Publish the new memory_id (epoch advance).

A reader that sees the new memory_id sees all of: the vector in the arena, the metadata in redb, the edges in the graph. The two-phase structure ensures consistency under crash: if the WAL record is durable, recovery can reconstruct everything else; if it isn't durable, the operation is treated as never happened.

### Storage-layer latency

The storage layer is fast on the warm path:

- WAL group commit: ~50–500 µs (dominated by NVMe fsync latency).
- Arena slot read: ~50 ns (page cache hit) to ~10–100 µs (page cache miss, NVMe read).
- Arena slot write: <1 µs (memcpy into mmap'd region).
- Metadata read: <10 µs (redb point lookup, copy-on-write B-tree).
- Metadata write: 5–20 µs (redb transaction).

For a typical `RECALL`, the storage layer contributes ~5–50 µs total — well below the 5–10 ms embedding cost.

---

## L6: Background Workers

**Detail spec:** [15. Background Workers](../15_background_workers/00_purpose.md).

### Four classes of background work

All running on cores reserved away from the request-serving pool.

#### Decay worker

Periodically scans memories and lowers their salience according to the [Ebbinghaus forgetting curve](https://en.wikipedia.org/wiki/Forgetting_curve). High-salience memories decay slowly; low-salience memories decay quickly. Memories below a threshold become eligible for eviction.

#### Consolidation worker

Periodically compresses similar episodic memories into semantic summaries — the "sleep" analogue from cognitive science. Unlike decay, consolidation creates new memories. The agent observes its memory becoming more abstract over time as repeated experiences are summarized into general patterns.

#### Index maintenance worker

Rebuilds parts of the HNSW index when the topology degrades. ANN indexes get worse over time as nodes are added and (especially) removed; periodic maintenance keeps recall quality high. Heuristics for when to rebuild are in [09. Indexing](../09_indexing/00_purpose.md).

#### Snapshot worker

Takes consistent backups via reflink-based file copies (where available) and metadata snapshots. Snapshots are the basis for cold backup, replication, and disaster recovery.

### Why these are background

These operations are intentionally low-priority. They can be paused entirely under load. A Brain server under heavy request load may defer its decay sweep for hours; this is acceptable because decay is a slow process anyway.

The discipline: foreground (request-serving) cores never call into background workers, and background workers only access shard state via well-defined snapshot reads (no live writes that could race with the foreground writer).

---

## L7: Sharding and Clustering

**Detail spec:** [16. Sharding & Clustering](../16_sharding/00_purpose.md).

### Shard model

Horizontal scaling is shard-based. Each agent's memory is owned by exactly one shard, identified by `hash(agent_id) % shard_count`. Within a shard, all the agent's data — vectors, metadata, edges, WAL — is colocated.

Shards are the unit of:

- Storage placement (one shard's arena and WAL live together on one node).
- Concurrency isolation (a shard's writer task is separate from every other shard).
- Load balancing (when shards are rebalanced across nodes, all of their state moves together).

### Cluster topology

A cluster is a set of nodes plus a stateless router. The router accepts client connections and forwards each to the node hosting the target shard. The router holds no state beyond the current shard-to-node mapping (which is gossiped or pulled from a consensus store).

Nodes do not communicate with each other on the request path. Each node serves its assigned shards independently. This is a deliberate constraint: it keeps the per-request critical path short and avoids cross-node coordination overhead.

### Rebalancing

Moving a shard from one node to another (during scale-out, scale-in, or load balancing) is a background operation. It involves:

1. Snapshotting the source shard.
2. Streaming the snapshot to the destination.
3. Catching up via WAL replay.
4. Switching the routing table.
5. Acknowledging the cutover.

Specified in [16. Sharding & Clustering](../16_sharding/00_purpose.md).

### What's deferred

**Replication** is intentionally deferred from v1. The first version assumes a single replica per shard; loss of a node means loss of its agents until restored from snapshot. This is documented as a known limitation, with replication slated for a future revision (see [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) OQ-2).

**Cross-region active-active** is out of scope. The architecture supports cross-region disaster recovery via snapshot replication, but not active-active across regions.

---

## Inter-layer communication

A few rules govern how layers interact:

1. **Strictly downward calls.** Layer L_n calls only into L_(n+1) and below. L_4 doesn't call back up into L_3.
2. **No skipping.** L_2 doesn't call directly into L_5; it goes through the planner.
3. **Async boundaries.** Each layer's interface is async. Layers below may yield (await) to the runtime; the layer above is unaware of the suspension.
4. **No shared mutable state across layers.** Mutable state (for example, the salience update queue) is owned by exactly one layer; other layers communicate via channels or read-only references.
5. **Errors propagate up.** A failure in L_5 (storage error) becomes an error result in L_4, propagated through L_3 and L_2 unchanged, surfaced to the client by L_1.

These rules are enforced socially (code review) rather than by the compiler. The compiler enforces module boundaries; the rules add semantic discipline on top.

---

## What's not a layer

Three things conspicuously absent from the layer diagram:

**Authentication and authorization.** These cut across layers — auth is established at L_1, but every layer respects the authenticated `agent_id`. They're documented in [17. Observability](../17_observability/00_purpose.md).

**Observability (metrics, tracing, logs).** Every layer emits its own observability data; there's no central observability layer. Conventions are in [17. Observability](../17_observability/00_purpose.md).

**Configuration.** Every layer reads from the same configuration system (`figment`-based) at startup. Configuration changes typically require a restart; live reconfiguration is out of scope for v1.

---

*Continue to [`05_hardware_and_targets.md`](05_hardware_and_targets.md) for the hardware envelope.*
