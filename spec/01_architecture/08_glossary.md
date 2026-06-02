# 01.08 Glossary

Terms defined here are used consistently across the entire specification series. If a term is used in a sense different from this glossary, the using doc is wrong, not this one. The glossary is normative.

The same glossary is hosted in [`../00_overview/01_glossary.md`](../00_overview/01_glossary.md) for cross-spec reference; the two files MUST stay in sync.

---

**Active.** A memory's lifecycle state when it is queryable and not tombstoned. The default state for an encoded memory until `FORGET` is invoked or background eviction occurs.

**Agent.** An LLM-driven application that connects to Brain. From Brain's perspective, an agent is identified by an `agent_id` and owns a memory namespace.

**Agent ID (`agent_id`).** A 128-bit UUIDv7 that identifies an agent. Used as the routing key for sharding: `shard_id = hash(agent_id) % shard_count`. The value zero is reserved.

**Arena.** The memory-mapped flat file holding the vectors for a shard. One slot per memory; fixed-size slots; mmap'd with `MAP_SHARED`. See [08. Storage: Arena & WAL](../08_storage/00_purpose.md).

**Background worker.** A process running on dedicated cores (not the request-serving cores) that performs maintenance work: decay, consolidation, index maintenance, snapshots. See [15. Background Workers](../15_background_workers/00_purpose.md).

**bge-small-en-v1.5.** The embedding model Brain ships with. From BAAI's [FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding) project. 384-dim `f32` vectors, 512-token context, English-only, MIT-licensed.

**BLAKE3.** A fast cryptographic hash function used for content fingerprinting (text deduplication) and model fingerprinting. See [BLAKE3 specification](https://github.com/BLAKE3-team/BLAKE3).

**Brain.** This system. The memory database.

**bytemuck.** A Rust crate for safe bit-cast operations between types of the same memory layout. Used for zero-copy conversion between `&[u8]` and `&[f32]` when reading vectors.

**candle.** HuggingFace's Rust ML framework. Brain uses candle for embedding-model inference. See [candle](https://github.com/huggingface/candle).

**Checkpoint.** A point in time at which the in-memory state has been fully persisted to disk (arena msync'd, metadata flushed) and the `last_checkpoint_lsn` advances. Lets the WAL retention worker delete sealed segments below that LSN.

**Cluster.** A set of Brain nodes serving a single deployment, plus a stateless router that dispatches client connections to shard owners.

**Cognitive operation.** One of the five primitives (`ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`) plus the supporting operations. Brain's external interface.

**Confidence.** A normalized score in [0, 1] returned with `RECALL` results, derived from raw similarity score and metadata. A confidence of 0.8 means "80% chance this is the correct/relevant memory" given the calibration on the benchmark dataset.

**Consolidated memory.** A memory of `kind = Consolidated`, produced by the background consolidation worker by clustering and summarizing related episodic memories.

**Consolidation.** The background process that compresses similar episodic memories into semantic summaries. The "sleep" analogue from cognitive science. Detailed in [15. Background Workers](../15_background_workers/00_purpose.md).

**Context.** A logical scope for memories within an agent (e.g., "work", "personal"). Used as a coarse filter on `RECALL`. Each memory belongs to exactly one context. The default context has `context_id = 0`.

**Context ID (`context_id`).** A 64-bit integer, agent-scoped, identifying a context. Allocated by the server when a context name is first referenced.

**crossbeam-epoch.** A Rust crate providing epoch-based memory reclamation for lock-free data structures. Brain uses crossbeam-epoch on the read path of the HNSW index and other shared structures. See [crossbeam-epoch](https://github.com/crossbeam-rs/crossbeam/tree/master/crossbeam-epoch).

**Cue.** The input to `RECALL`, `PLAN`, or `REASON`. Typically text that Brain embeds into a query vector.

**Decay.** The background process that exponentially lowers salience over time, modeling the Ebbinghaus forgetting curve. Memories below an eviction threshold become eligible for consolidation or eviction.

**Edge.** A directed, typed link between two memories. Eight edge types in v1: `CAUSED`, `FOLLOWED_BY`, `DERIVED_FROM`, `SIMILAR_TO`, `CONTRADICTS`, `SUPPORTS`, `REFERENCES`, `PART_OF`.

**Embedding.** The act of converting text to a vector. The result is a 384-dim `f32` vector (for `bge-small-en-v1.5`). Brain owns embedding; clients send text, vectors are internal.

**Embedding layer.** Layer L2 of the architecture. Owns the embedding model, tokenizer, inference, batching, and caching. Detailed in [07. Embedding Layer](../07_embedding/00_purpose.md).

**Epoch.** A logical unit of time in the lock-free reclamation system. Readers acquire an epoch handle while traversing shared structures; writers wait for old epochs to drain before reclaiming memory. See [14. Concurrency](../14_concurrency/00_purpose.md).

**Episodic memory.** A memory of `kind = Episodic`. A specific event the agent observed, tied to time and place. Default for all `ENCODE` operations.

**Eviction.** The removal of a memory from Brain via background work (not via explicit `FORGET`). Eligibility is determined by salience and capacity pressure.

**Execution engine.** Layer L4 of the architecture. The implementation of execution strategies (ANN, attractor, graph, VSA algebra) selected by the planner.

**Forgetting curve.** The exponential decay of memory retention over time, named for [Hermann Ebbinghaus's 1885 work](https://en.wikipedia.org/wiki/Forgetting_curve). The functional form Brain uses for salience decay.

**Glommio.** The Rust thread-per-core async runtime Brain uses. From DataDog. Built on `io_uring`. Linux-only. See [Glommio](https://github.com/DataDog/glommio).

**HNSW (Hierarchical Navigable Small World).** The approximate-nearest-neighbor algorithm Brain uses for vector search. Multi-layer graph; greedy descent for queries. Introduced in [Malkov & Yashunin's 2016/2018 paper](https://arxiv.org/abs/1603.09320).

**Hot path.** The code path executed for an in-flight client request. Optimized for latency and zero allocations. Distinguished from background work, which is allowed to be slower.

**`io_uring`.** The modern Linux async I/O interface, kernel 5.1+. Brain uses io_uring for both wire-protocol I/O and storage I/O. See [liburing](https://github.com/axboe/liburing).

**Idempotency horizon.** The time window during which a `request_id` is retained for deduplication. Default 5 minutes for substrate operations (`ENCODE`, `FORGET`); 24 hours for typed-graph operations (entity / statement / relation creates and queries). Retries within the horizon are deduplicated; retries beyond it may be treated as new operations.

**LSN (Log Sequence Number).** A per-shard monotonic identifier for WAL records. Used for replay ordering, idempotency, subscription resumption, and checkpoint advancement.

**Memory.** A single stored unit: vector + metadata + edges. The unit of storage in Brain. Identified by `MemoryId`.

**Memory ID (`memory_id`).** The public identifier of a memory. 128 bits encoding shard_id (16 bits), slot_id (48 bits), version (32 bits), reserved (32 bits). Opaque to clients.

**Memory kind.** One of `Episodic`, `Semantic`, or `Consolidated`. Determines salience decay rate and cognitive-operation weighting.

**Metadata store.** The redb-backed B-tree holding non-vector memory data: salience, context, timestamps, edge lists, contexts. Per-shard. See [10. Metadata + Graph Store](../10_metadata/00_purpose.md).

**Model fingerprint.** A 16-byte BLAKE3-derived identifier for an embedding model. Tagged on every memory; used to detect cross-model query attempts and to guide model migration.

**Page cache.** The Linux kernel's filesystem-backed cache. Brain's arena lives in the page cache via mmap; the OS handles working-set vs cold-set transitions automatically.

**Plan (verb / noun).** Verb: the cognitive primitive of searching for a path from start to goal. Noun: the result of `PLAN`, a sequence of memory references.

**Planner.** Layer L3 of the architecture. The pure function from `(operation, query, stats)` to `execution plan`. Selects strategies; doesn't execute.

**redb.** The Rust embedded ACID key-value store Brain uses for metadata. Copy-on-write B-trees; pure Rust. See [redb](https://github.com/cberner/redb).

**Reclaimed.** A slot's lifecycle state after its memory has been forgotten and the slot has been reused. The `version` field has incremented; old `MemoryId`s that referenced the previous occupant are now stale.

**Recall (verb / noun).** Verb: the cognitive primitive of retrieving memories similar to a cue. Noun: the result, a list of memories with scores.

**Reflink.** Block-level copy-on-write between files. Supported on btrfs and xfs (with `mkfs.xfs -m reflink=1`). Used for instant snapshots.

**Request ID (`request_id`).** A 16-byte client-supplied identifier for idempotency. Required on `ENCODE` and `FORGET`. UUIDv7 recommended.

**Reservation.** A pre-allocated stack/scratch buffer used on the hot path to avoid runtime allocation. Brain reserves these per core at startup.

**rkyv.** A zero-copy deserialization framework for Rust. Brain uses rkyv for internal on-disk storage (redb values, WAL records, arena slots). Wire payloads use CBOR (see [04](../04_wire_protocol/00_purpose.md)). See [rkyv](https://github.com/rkyv/rkyv).

**Salience.** A numeric score in [0, 1] representing how important a memory is. Drives decay and consolidation; influences ranking in `RECALL`. Updated on access; decayed by background workers.

**Semantic memory.** A memory of `kind = Semantic`. Stable knowledge derived from many observations; promoted from episodic by an agent assertion or by consolidation.

**Session.** The duration of a single client connection's authenticated state. Identified by `session_id` from the WELCOME frame. Bound to one `agent_id` for its lifetime.

**Shard.** A unit of horizontal scaling. One agent's data lives in exactly one shard. Identified by 16-bit runtime `shard_id` (in MemoryIds) and a stable UUIDv7 `shard_uuid` (in storage). See [16. Sharding & Clustering](../16_sharding/00_purpose.md).

**Shard ID (`shard_id`).** Two senses depending on context:
- **Wire/MemoryId sense:** 16-bit runtime identifier mapping to a shard.
- **Storage sense:** UUIDv7 identifying a shard persistently across cluster reorganizations.

**Slot.** A fixed-size cell in the arena holding one vector and its flags. Identified by `slot_id`.

**Slot ID (`slot_id`).** 48-bit internal handle for a slot. Always paired with `version` when used to identify a memory externally.

**Snapshot.** A point-in-time copy of an arena, WAL, and metadata, suitable for backup or replication. Created via reflink (where supported) or full file copy.

**SSE 4.2 / NEON.** SIMD instruction sets Brain uses for accelerated vector dot products. SSE 4.2 on x86_64; NEON on ARM64. CRC32 acceleration is also provided by SSE 4.2 and the ARMv8 CRC32 extension.

**Stream ID (`stream_id`).** 32-bit identifier for a logical request/response stream within a connection. Client-allocated. Odd values for client-initiated streams; even reserved for server push (not used in v1).

**Tokenizer.** The component that converts text to a sequence of token IDs for the embedding model. Brain uses HuggingFace [`tokenizers`](https://github.com/huggingface/tokenizers) with the BERT WordPiece vocab.

**Tombstone.** A marker indicating a slot has been forgotten but not yet reclaimed. Hidden from queries; recoverable until reclamation.

**Transaction (`txn`).** A grouping of multiple operations that commit atomically. Identified by `txn_id`. Supports rollback before commit. See [05. Operations](../05_operations/00_purpose.md) §Transactions.

**Transparent Huge Pages (THP).** Linux kernel feature for using 2 MiB pages instead of 4 KiB pages, reducing TLB pressure. **Does NOT apply to Brain's arena** because THP doesn't work on regular file-backed mmaps (only anonymous and tmpfs/shmem).

**UUID v7.** A time-ordered UUID format from [RFC 9562](https://datatracker.ietf.org/doc/rfc9562/). Brain uses UUIDv7 for `agent_id`, `request_id`, and shard storage UUIDs.

**Vector.** A 384-dim array of `f32` produced by the embedding model. L2-normalized. Stored in the arena.

**Vector arena.** See *Arena*.

**Version (slot version).** A 32-bit monotonic counter on a slot. Increments on each reuse after `FORGET`. Lets external `MemoryId` references detect that the slot's content has changed.

**VSA (Vector Symbolic Architecture).** A family of techniques for representing structured information in high-dimensional vectors, using algebraic operations like bind, bundle, and unbind. Used during `REASON` to manipulate compositional representations.

**WAL (Write-Ahead Log).** Per-shard append-only log of state mutations. The durability barrier for `ENCODE` and `FORGET`. Source of truth for crash recovery. See [08. Storage: Arena & WAL](../08_storage/00_purpose.md).

**Working set.** The set of memories currently resident in the page cache. Determined by access patterns. Brain's mmap-based design lets the OS manage the working set automatically.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved architectural questions.*
