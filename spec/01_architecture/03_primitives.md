# 01.03 The Five Cognitive Primitives

Every interaction between an agent and Brain is one of five **cognitive primitives**, plus a small set of supporting operations. These map roughly onto operations a brain performs, though we make no biological claim — the names are evocative shorthand for well-defined operations.

The primitives are introduced briefly here, with their conceptual shape and basic latency expectations. Their exact semantics, parameters, and edge cases are specified in [05. Operations](../05_operations/00_purpose.md).

The five primitives:

1. [`ENCODE`](#1-encode) — store a memory.
2. [`RECALL`](#2-recall) — retrieve memories similar to a cue.
3. [`PLAN`](#3-plan) — search for a path through memory from start to goal.
4. [`REASON`](#4-reason) — explain an observation in terms of stored memories.
5. [`FORGET`](#5-forget) — remove a memory.

The supporting operations:

6. [Connection management](#6-supporting-operations-connection) — `HELLO`, `WELCOME`, `AUTH`, `PING`, `BYE`.
7. [Subscription](#7-subscribe) — push notifications for memory events.
8. [Transactional grouping](#8-transactional-grouping) — `TXN_BEGIN`, `TXN_COMMIT`, `TXN_ABORT`.
9. [Admin operations](#9-admin-operations) — snapshot, restore, integrity check, stats.

---

## 1. ENCODE

**Form:** `ENCODE(text, context, salience_hint?, request_id) → memory_id`

The agent commits a piece of content to memory.

### 1.1 What Brain does

1. Tokenizes the text and runs the embedding model to produce a 384-dim `f32` vector.
2. L2-normalizes the vector (so cosine similarity is the same as dot product).
3. Allocates a slot in the vector arena.
4. Writes a WAL record (the durability barrier).
5. Inserts the vector into the HNSW index.
6. Stores metadata (context, timestamp, initial salience, edges).
7. Returns the `memory_id`.

### 1.2 Idempotency

The `request_id` is a client-supplied 16-byte identifier (UUIDv7 recommended) used for idempotency. Retrying an `ENCODE` with the same `request_id` is a no-op: the server returns the existing `memory_id` without re-encoding.

The idempotency horizon is configurable (default: 5 minutes). Retries delayed beyond the horizon may be treated as new operations. See [08. Storage: Arena & WAL](../08_storage/00_purpose.md) §4.4.2 for the deduplication mechanism.

### 1.3 Salience hint

The optional `salience_hint` lets the agent signal importance. It's a number in [-1, +1]; positive values raise initial salience, negative values lower it. The actual initial salience is computed from a combination of the hint, the embedding's surprise (distance from the centroid of recent memories), and configured constants. See [02. Data Model](../02_data_model/00_purpose.md) §6 for the formula.

### 1.4 Latency

`ENCODE` is the only primitive that mutates persistent state on the hot path. Latency budget:

- Embedding inference: 5–10 ms (CPU), <1 ms (GPU, batched).
- WAL append (group-committed): 0.05–0.5 ms.
- Arena slot write: <0.001 ms.
- HNSW insert: 0.1–1 ms (scales with `log(N)`).
- Metadata write: 0.005–0.02 ms.

Total p50: ~12 ms (CPU), ~3 ms (GPU). p99: ~25 ms (CPU), ~8 ms (GPU). Embedding dominates; everything else is in the noise.

### 1.5 Position in the architecture

`ENCODE` is the **only** primitive that takes the write path through the storage layer. All other primitives are reads (or, in the case of `RECALL`, read-mostly: salience updates are an asynchronous side effect that doesn't block the response).

This is intentional: by concentrating mutations into one well-defined primitive, the read path can be lock-free and Brain can scale reads independently from writes.

---

## 2. RECALL

**Form:** `RECALL(cue_text, context?, top_k, confidence?, age_bound?, filters?) → [(memory_id, content, score, metadata)]`

The agent asks for memories relevant to a cue.

### 2.1 What Brain does

1. Embeds the cue text.
2. Asks the planner which strategy to use:
   - **Hybrid** (default) — ANN + lexical + memory-edge graph fused via RRF.
   - **Schemaless** — fast vector-only similarity.
3. Executes the strategy.
4. Filters by context, age, kind, and other criteria.
5. Computes confidence scores.
6. Returns the top-k results, ranked.
7. Asynchronously updates salience for accessed memories (fire-and-forget).

### 2.2 Strategy selection

The planner's strategy choice is invisible to the client. The agent says "recall things similar to this cue"; Brain decides which path to run. This is the SQL-like abstraction: declarative query, the planner picks the algorithm.

For RECALL specifically there are exactly two paths and one selection rule: a request carrying a `txn_id` runs Brain path so read-your-writes works against the per-txn buffer overlay; every other request runs the hybrid path (semantic + lexical + memory-edge graph, fused via RRF). The selection is server-side; no wire field controls it.

### 2.3 Confidence calibration

Raw similarity scores from HNSW are uncalibrated — a 0.85 cosine similarity could be "highly confident" or "barely above noise" depending on the score distribution for this query. Brain calibrates these into a `confidence` value in [0, 1] using the local neighborhood's score distribution.

The `min_confidence` parameter filters results below the threshold. Default: 0.0 (no filter); typical value for "definitely relevant": 0.7.

### 2.4 Streaming

`RECALL` is streamed: results arrive one frame at a time, sharing a stream_id, with the last frame marked end-of-stream. This lets clients start processing top results before the full top-k is computed.

For small `top_k` (≤ 10), Brain may emit all results in a single frame. Clients present both cases with the same async-iterator interface.

### 2.5 Latency

Latency budget (CPU):

- Embedding the cue: 5–10 ms (cache hit: <0.001 ms).
- Strategy selection: <0.001 ms.
- HNSW search: 0.1–1 ms.
- Filtering and ranking: 0.005–0.05 ms.
- Response framing: 0.005–0.02 ms.

Total p50: ~8 ms (CPU). p99: ~20 ms. Embedding remains the dominant cost; cue caches help significantly for repeated queries.

### 2.6 Position in the architecture

`RECALL` is the most frequent operation. It must be cheap, predictable, and lock-free on the read path. No allocations after warmup; no waiting for writes. The architecture's read-path optimizations exist primarily for `RECALL`.

---

## 3. PLAN

**Form:** `PLAN(start_state, goal_state, budget?) → stream of plan steps`

The agent asks Brain to construct a path from a start state to a goal, using its memory as the world model.

### 3.1 What Brain does

1. Resolves start and goal states (which may be referenced by memory_id or by text — text gets embedded).
2. Asks the planner which search algorithm to use:
   - **A*** — heuristic search with admissible heuristic from vector distance.
   - **MCTS** — Monte Carlo Tree Search for stochastic domains.
3. Runs the search with budget enforcement.
4. Streams plan steps as they're discovered.
5. Emits a terminal frame when goal is reached, budget is exhausted, or no path is found.

### 3.2 Streaming behavior

Intermediate plan steps stream back to the client as they're discovered. The client may cut off the search by sending a cancel frame. Long plans that don't fit in a single response don't block the agent's progress — it can start executing the first steps while later steps are still being computed.

### 3.3 Budget enforcement

The optional `budget` parameter constrains:

- `max_steps` — maximum plan length.
- `max_wall_time_ms` — wall-clock limit on search.
- `max_branches_explored` — search-space size limit.

When the budget is exhausted, `PLAN` returns whatever partial plan has been found, with a status indicating budget exhaustion rather than goal reached.

### 3.4 Latency

`PLAN` is heavier than `RECALL`. Latency is workload-dependent:

- Simple plans (few steps, dense memory graph): 10–50 ms.
- Complex plans (long horizon, sparse graph): 100 ms – seconds.
- Pathological cases (no path exists, must exhaust budget): up to budget limit.

The streaming nature reduces perceived latency: the agent sees the first step quickly even when the full plan takes a second to compute.

### 3.5 Position in the architecture

`PLAN` exercises the graph store, the vector index (for heuristic estimates), and the attractor dynamics. It is the most complex primitive in terms of execution variety, and the planner's strategy choice is most consequential here.

---

## 4. REASON

**Form:** `REASON(observation, depth?) → stream of inference steps`

The agent presents an observation and asks Brain why — what stored memories causally explain the observation, what predictions Brain makes, what evidence supports each conclusion.

### 4.1 What Brain does

Operationally, `REASON` is graph traversal over causal/temporal edges combined with vector-space algebra. The execution:

1. Embeds the observation.
2. Finds memories closely related to the observation (similarity).
3. Walks `CAUSED` and `SUPPORTS` edges backward to find candidate explanations.
4. Walks `CONTRADICTS` edges to find rebuttals.
5. Scores candidate explanations by combination of similarity and graph evidence.
6. Streams inference steps as they're constructed.

Each inference step includes a claim, supporting memories, contradicting memories, and a confidence score.

### 4.2 The maturity caveat

`REASON` is the **least mature** of the five primitives. The first version supports limited inference patterns:

- Causal explanation ("why did X happen?").
- Evidence accumulation ("what supports / contradicts this claim?").
- Analogical inference (limited; via vector algebra on bound concepts).

Future iterations expand the operator set. Brain's architecture supports arbitrary inference DAGs; v1 limits the patterns to what we can confidently calibrate.

### 4.3 Latency

Comparable to `PLAN`: workload-dependent, typically 50 ms – seconds. Streamed for the same reasons as `PLAN`.

### 4.4 Position in the architecture

`REASON` is where Vector Symbolic Architectures (VSA) algebra earns its keep. Bind, bundle, and unbind operations let Brain manipulate compositional representations — "memory A combined with role B" — without losing structure. See [05. Operations](../05_operations/00_purpose.md) §REASON for the algebra.

---

## 5. FORGET

**Form:** `FORGET(memory_id, request_id, mode) → ack`

The agent explicitly removes a memory.

### 5.1 Modes

The `mode` parameter selects between two semantics:

**Soft forget** — the slot is tombstoned: hidden from queries, eligible for reuse but recoverable until consolidation. The slot's `version` increments when reused, so old `memory_id` references won't accidentally point to the new occupant. Soft forget is the default.

**Hard forget** — the slot is overwritten with zeros, removed from the index, and the original content is unrecoverable. Used for compliance scenarios (GDPR right-to-erasure, removal of secrets accidentally encoded). Hard forget is destructive and cannot be undone.

### 5.2 What Brain does

For both modes:

1. Validates that the `memory_id` exists and is owned by the requesting agent.
2. Writes a `FORGET` record to the WAL.
3. Removes the memory from the HNSW index.
4. Marks (soft) or clears (hard) the slot in the arena.
5. Updates metadata in redb.
6. Acknowledges to the client.

For hard forget, additional zeroing passes are performed on the arena slot to ensure the data is truly gone (not just removed from the active index).

### 5.3 Idempotency

Like `ENCODE`, `FORGET` carries a `request_id` for idempotent retry. Retrying `FORGET` for an already-forgotten memory returns success with `was_already_forgotten = true`.

### 5.4 Edges and forgetting

When a memory is forgotten, its outgoing edges are gone (they're owned by the source memory). Incoming edges (from other memories pointing to this one) are eagerly invalidated when the slot is reclaimed; until reclamation, edges to a tombstoned memory are filtered during traversal.

### 5.5 Decay vs FORGET

`FORGET` is explicit, agent-initiated removal. There is also a *passive* form of forgetting — **decay** — handled by background workers without agent intervention. Decay lowers salience over time according to the Ebbinghaus forgetting curve; memories below a threshold are eligible for eviction during consolidation.

Decay is not a cognitive primitive — the agent doesn't trigger it — but it is part of Brain's behavior. Documented in [15. Background Workers](../15_background_workers/00_purpose.md).

### 5.6 Latency

`FORGET` is fast: ~1–5 ms. It's a write, but a small one — no embedding to compute, just a WAL record and metadata update.

---

## 6. Supporting Operations: Connection

These operations manage the client-server connection. They're not cognitive primitives, but the protocol can't function without them.

- **`HELLO`** (client → server) — first frame after TCP and TLS. Declares client identity, protocol version range, requested features.
- **`WELCOME`** (server → client) — confirms the session, returns server identity, negotiated protocol version, session_id.
- **`AUTH`** (client → server) — authentication credentials (token, mTLS, etc.).
- **`AUTH_OK`** (server → client) — confirms auth, binds the session to an `agent_id`.
- **`PING` / `PONG`** (bidirectional) — liveness check, RTT measurement, keepalive against load balancers.
- **`BYE`** (bidirectional) — graceful close.

The complete handshake is specified in [04. Wire Protocol](../04_wire_protocol/00_purpose.md) §10.

---

## 7. SUBSCRIBE

**Form:** `SUBSCRIBE(filter, include_history?, from_lsn?) → stream of events`

The agent registers interest in events matching a filter; Brain pushes notifications when matching memories are encoded or modified.

### 7.1 Use cases

- **Reactive agents** — wake up when relevant new information arrives.
- **Cross-session continuity** — when one session of an agent encodes new memories, another session is notified.
- **Audit / observability** — external systems can tail the memory stream for compliance or analysis.

### 7.2 Filters

A subscription's filter can specify:

- Contexts to include.
- Memory kinds (Episodic / Semantic / Consolidated).
- Similarity threshold to a reference memory.

Multiple subscriptions per connection are allowed; each has its own stream_id.

### 7.3 Resumption

The optional `from_lsn` parameter resumes a subscription from a specific LSN (Log Sequence Number). A client whose connection drops can reconnect with the LSN of the last event it received and continue from there, as long as the WAL hasn't been retention-truncated past that point. See [08. Storage: Arena & WAL](../08_storage/00_purpose.md) §4.2.9.

---

## 8. Transactional Grouping

The agent groups multiple operations into a single atomic unit.

- **`TXN_BEGIN`** (client → server) — opens a transaction with a client-supplied `txn_id`.
- **`TXN_COMMIT`** (client → server) — commits all operations in the transaction.
- **`TXN_ABORT`** (client → server) — rolls back all operations in the transaction.

### 8.1 Semantics

Operations within a transaction carry the `txn_id`. Brain buffers them; on `TXN_COMMIT`, all operations are applied atomically (all succeed or all fail). On `TXN_ABORT` or connection drop before commit, none of the operations take effect.

### 8.2 Use cases

- **Episode learning** — an agent processes a multi-turn interaction and learns several things from it. If the episode is later determined to be invalid (the user said "ignore that"), the entire batch can be rolled back.
- **Atomic relationship establishment** — encoding two memories and the edges between them, where partial application would be wrong.
- **Bulk import** — initial ingest of an agent's known facts.

### 8.3 Limits

- Maximum operations per transaction: 1000 (configurable).
- Maximum transaction wall time: 60 seconds (configurable).
- A transaction that exceeds either limit is auto-aborted.

Transactions are local to a single shard. Cross-shard transactions are not supported in v1.

---

## 9. Admin Operations

A small set of operations for administrative use, typically invoked via a CLI tool rather than from the agent's hot path.

- **`ADMIN_SNAPSHOT`** — take a consistent backup of one or more shards.
- **`ADMIN_RESTORE`** — restore a shard from a snapshot.
- **`ADMIN_STATS`** — current statistics: memory counts, salience distribution, throughput, etc.
- **`ADMIN_INTEGRITY_CHECK`** — full or partial integrity verification.
- **`ADMIN_MIGRATE_EMBEDDINGS`** — re-embed all memories with a new model.

These require elevated permissions (see [17. Observability](../17_observability/00_purpose.md) for the permission model). They are invoked over the same protocol as operations, just with different opcodes.

---

## 10. Summary table

| Primitive | Direction | Streaming | Mutates | Typical latency (CPU) |
|---|---|:-:|:-:|---|
| `ENCODE` | Write | No | Yes | ~12 ms |
| `RECALL` | Read | Yes | Salience only | ~8 ms |
| `PLAN` | Read | Yes | No | 10–1000 ms |
| `REASON` | Read | Yes | No | 50–1000 ms |
| `FORGET` | Write | No | Yes | ~3 ms |
| `SUBSCRIBE` | Read | Yes | No | ~1 ms (setup) |
| `TXN_*` | Both | No | Yes (on commit) | ~5 ms |
| `ADMIN_*` | Both | Varies | Varies | Varies |

---

*Continue to [`04_layers.md`](04_layers.md) for the architectural layers that implement these primitives.*
