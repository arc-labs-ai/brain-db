# 00.02 Document Map

For each detail spec, this file gives a one-paragraph summary, the spec's key dependencies, and what depends on it. This is the "where is X documented?" lookup table.

---

## 01. System Architecture

**Summary.** The conceptual whole of Brain. Defines the five cognitive primitives (`ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`), the seven architectural layers (Connection → Embedding → Planner → Execution → Storage → Background → Sharding), the hardware envelope, capacity targets, and the explicit list of non-goals. Reads as the "introduction to Brain" for any audience; required reading before any other spec.

**Depends on.** Nothing.

**Depended on by.** Everything.

**Where to find specific topics:**

- The five primitives → [`03_primitives.md`](../01_system_architecture/03_primitives.md)
- The seven layers → [`04_layers.md`](../01_system_architecture/04_layers.md)
- Capacity targets → [`06_targets.md`](../01_system_architecture/06_targets.md)
- Comparison with vector DBs, graph DBs, frameworks → [`08_comparison.md`](../01_system_architecture/08_comparison.md)

---

## 02. Data Model

**Summary.** The entities Brain stores and the relationships between them. Defines `Memory` (vector + metadata + edges), `Context` (logical scope), `Edge` (typed link between memories). Specifies identifier formats: `MemoryId` (slot + version), `AgentId` (UUIDv7), `ContextId` (agent-scoped), `RequestId` (UUIDv7 for idempotency), `ShardId` (UUIDv7 for storage). Defines the salience model, the eight edge types, the three memory kinds, and the lifecycle states (Active → Tombstoned → Reclaimed).

**Depends on.** [01. System Architecture](../01_system_architecture/).

**Depended on by.** Every other spec.

---

## 03. Wire Protocol

**Summary.** The binary protocol between clients and Brain. Defines the 32-byte fixed frame header (magic + version + opcode + flags + crc + stream_id + payload_len + payload_crc), the rkyv-encoded structured payloads, the bytemuck-cast raw vector bytes, the 14 client opcodes, the 14 server opcodes, the connection handshake (`HELLO` → `WELCOME` → `AUTH` → `AUTH_OK`), the streaming model, and the error code space.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/).

**Depended on by.** [04. Embedding Layer](../04_embedding_layer/) (request shapes), [09. Cognitive Operations](../09_cognitive_operations/) (operation semantics on the wire), [13. SDK Design](../13_sdk_design/) (clients consume this).

---

## 04. Embedding Layer

**Summary.** Layer L2 of the architecture. Documents the chosen model (`bge-small-en-v1.5`), the alternatives considered, the tokenization (BERT WordPiece, max 512 tokens), the inference path (candle-based), the LRU caching keyed on text hash, the optional GPU batching path, and the model migration procedure (when an embedding model is upgraded, all stored vectors must be re-embedded).

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/).

**Depended on by.** [05. Storage](../05_storage_arena_wal/) (vector layout), [09. Cognitive Operations](../09_cognitive_operations/) (which embeds cues).

---

## 05. Storage: Arena & WAL

**Summary.** The vector arena (mmap'd flat file, 1600-byte slots, MAP_SHARED), the per-shard write-ahead log (O_DIRECT append-only, 256 MiB segments, group commit via io_uring pwritev2 with RWF_DSYNC), the durability barrier sequence (allocate → WAL → fsync → arena → metadata → publish), the recovery procedure (replay WAL forward from the last checkpoint), and the WAL retention policy.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/).

**Depended on by.** [06. ANN Index](../06_ann_index/) (slot allocation interaction), [10. Concurrency + Epoch Model](../10_concurrency_epochs/) (writer ordering), [15. Failure Modes + Recovery](../15_failure_recovery/).

---

## 06. ANN Index (HNSW)

**Summary.** The HNSW graph: layer count, M (max edges per node), ef_construction (build search width), ef_search (query search width). The insertion algorithm, the search algorithm, the deletion strategy (tombstones + periodic rebuild), the parameter-tuning methodology, and the maintenance worker that rebuilds parts of the index when topology degrades.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [05. Storage](../05_storage_arena_wal/).

**Depended on by.** [08. Query Planner](../08_query_planner/), [11. Background Workers](../11_background_workers/) (maintenance).

---

## 07. Metadata + Graph Store

**Summary.** The redb-backed metadata layer. Documents the table schemas (`memories`, `edges`, `contexts`, `idempotency`, `subscriptions`, `model_fingerprints`), the secondary indexes (by salience, by context, by timestamp), the edge representation, the idempotency table's TTL behavior, and how transactions interact with WAL durability.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [05. Storage](../05_storage_arena_wal/).

**Depended on by.** [08. Query Planner](../08_query_planner/), [09. Cognitive Operations](../09_cognitive_operations/), [11. Background Workers](../11_background_workers/).

---

## 08. Query Planner + Execution Engine

**Summary.** The planner (pure function from query+stats to plan) and the four executors (ANN, attractor, graph, VSA algebra). Documents strategy selection rules, plan caching, the execution-engine's no-allocation discipline, the single-writer-per-shard write path, and the lock-free read path.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [06. ANN Index](../06_ann_index/), [07. Metadata + Graph Store](../07_metadata_graph/), [10. Concurrency + Epoch Model](../10_concurrency_epochs/).

**Depended on by.** [09. Cognitive Operations](../09_cognitive_operations/) (the operations dispatch through the planner).

---

## 09. Cognitive Operations

**Summary.** The full semantics of `ENCODE`, `RECALL`, `PLAN`, `REASON`, `FORGET`. Each operation is documented with: parameters, return values, side effects, idempotency behavior, latency targets, error conditions, and edge cases. Also covers the supporting operations: `SUBSCRIBE`, `TXN_*`, `ADMIN_*`.

**Depends on.** [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/), [03. Wire Protocol](../03_wire_protocol/), [04. Embedding Layer](../04_embedding_layer/), [08. Query Planner](../08_query_planner/).

**Depended on by.** [13. SDK Design](../13_sdk_design/) (clients call these), [16. Benchmarks](../16_benchmarks_acceptance/) (tested against these).

---

## 10. Concurrency + Epoch Model

**Summary.** The lock-free read path: how readers traverse shared data structures (HNSW graph, salience tables) without locks, using crossbeam-epoch-based reclamation. The single-writer-per-shard discipline that funnels all mutations through one task per shard. The use of arc-swap for atomic config swaps. The channels and queues used for inter-task communication. The cooperative scheduling within a Glommio executor.

**Depends on.** [01. System Architecture](../01_system_architecture/).

**Depended on by.** [05. Storage](../05_storage_arena_wal/), [06. ANN Index](../06_ann_index/), [08. Query Planner](../08_query_planner/), [11. Background Workers](../11_background_workers/).

---

## 11. Background Workers

**Summary.** The four background-worker classes: decay (lowers salience over time), consolidation (clusters similar episodic memories into semantic summaries), HNSW maintenance (rebuilds index sections when topology degrades), snapshot (creates point-in-time backups). Documents scheduling, resource budgeting, isolation from request-serving cores, and the snapshot-based read pattern that lets background workers see consistent state without blocking the writer.

**Depends on.** [01. System Architecture](../01_system_architecture/), [05. Storage](../05_storage_arena_wal/), [06. ANN Index](../06_ann_index/), [07. Metadata + Graph Store](../07_metadata_graph/), [10. Concurrency + Epoch Model](../10_concurrency_epochs/).

**Depended on by.** [12. Sharding + Clustering](../12_sharding_clustering/) (rebalancing uses snapshot infrastructure).

---

## 12. Sharding + Clustering

**Summary.** The cluster topology: nodes, the stateless router, shard-to-node mapping. The routing algorithm (`hash(agent_id) % shard_count`). The rebalancing procedure (snapshot source → stream to destination → catch up via WAL → switch routing). The cluster control plane (where shard-to-node mappings live). The cutover protocol that ensures no double-writes during rebalance.

**Depends on.** [01. System Architecture](../01_system_architecture/), [05. Storage](../05_storage_arena_wal/), [11. Background Workers](../11_background_workers/).

**Depended on by.** [13. SDK Design](../13_sdk_design/) (SDKs talk to the router), [14. Observability + Operations](../14_observability_ops/) (cluster operations).

---

## 13. SDK Design

**Summary.** Language-level client interfaces. Defines the Rust SDK as the canonical implementation. Specifies bindings for Python (PyO3), TypeScript (NAPI-RS or wasm), and Go (cgo or pure Go). Documents the connection-pool design, the streaming-iterator pattern for `RECALL`/`PLAN`/`REASON`/`SUBSCRIBE`, the error mapping from wire codes to language-native errors, and the idempotency helpers.

**Depends on.** [01. System Architecture](../01_system_architecture/), [03. Wire Protocol](../03_wire_protocol/), [09. Cognitive Operations](../09_cognitive_operations/).

**Depended on by.** Application developers (not other specs).

---

## 14. Observability + Operations

**Summary.** The metrics surface (Prometheus-style histograms, gauges, counters), the tracing model (OpenTelemetry-compatible spans), the structured logging (JSON to stdout), the configuration system (figment-based, environment + file + flags), the deployment patterns (single-node, multi-node), and the security model (TLS, authentication, agent-scoped authorization).

**Depends on.** [01. System Architecture](../01_system_architecture/), [12. Sharding + Clustering](../12_sharding_clustering/).

**Depended on by.** [15. Failure Modes + Recovery](../15_failure_recovery/) (operational procedures).

---

## 15. Failure Modes + Recovery

**Summary.** What can go wrong and what to do about it: process crash (recover from WAL), host crash (start fresh from local disk), disk corruption (detect via checksums, restore from snapshot), network partition (per-shard isolation limits blast radius), embedding-model corruption (detected by fingerprint), partial WAL writes (detected and refused at recovery), bad data from clients (validated and rejected). Defines the recovery procedures for each.

**Depends on.** [01. System Architecture](../01_system_architecture/), [05. Storage](../05_storage_arena_wal/), [14. Observability + Operations](../14_observability_ops/).

**Depended on by.** [16. Benchmarks + Acceptance Criteria](../16_benchmarks_acceptance/) (chaos tests verify failure modes).

---

## 16. Benchmarks + Acceptance Criteria

**Summary.** The complete test suite that validates Brain's claims. Latency benchmarks (single-shard p50/p99/p99.9 for each operation), throughput benchmarks (sustained QPS at saturation), correctness benchmarks (recall quality, calibration error), durability tests (crash-and-recover), and chaos tests (random kills, disk pressure, network jitter). Each test is mapped to a specific target from [01. System Architecture §06_targets.md](../01_system_architecture/06_targets.md).

**Depends on.** Everything else.

**Depended on by.** Nothing (this is the validation, not a dependency).

---

## Cross-cutting topics

Some topics aren't spec-shaped and are scattered across multiple specs.

- **Authentication and authorization** — primarily in [14. Observability + Operations](../14_observability_ops/), referenced by [03. Wire Protocol](../03_wire_protocol/).
- **Configuration** — primarily in [14. Observability + Operations](../14_observability_ops/), referenced by every spec that has tunable parameters.
- **The model fingerprint** — defined in [04. Embedding Layer](../04_embedding_layer/), used in [02. Data Model](../02_data_model/) and [07. Metadata + Graph Store](../07_metadata_graph/), referenced from [15. Failure Modes + Recovery](../15_failure_recovery/) for cross-model error detection.
- **The CRC32C checksum scheme** — defined in [03. Wire Protocol](../03_wire_protocol/) for frames, [05. Storage](../05_storage_arena_wal/) for WAL records.

---

*Continue to [`03_versioning.md`](03_versioning.md) for how specs evolve over time.*
