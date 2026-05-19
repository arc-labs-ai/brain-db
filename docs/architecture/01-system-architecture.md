# 01 — System architecture

**Audience:** engineers who need a working mental model of what runs
where, who talks to whom, and which boundary their next change is
about to cross.

**Goal:** end this chapter able to (a) name the components on a
request's path, (b) explain why Brain runs *two* async runtimes
instead of one, and (c) point at the right module before diving
into any of the chapters that follow.

This chapter is the load-bearing one. Every other chapter assumes
you know the layout introduced here, then specialises into a
single component (arena & WAL, wire codec, HNSW, …).

---

## What Brain is, in one paragraph

Brain is a single Rust server (`brain-server`) that exposes a binary
TCP protocol plus an HTTP control plane. Clients send cognitive
verbs — `encode`, `recall`, `plan`, `reason`, `forget`, plus a
knowledge-layer family — and the server stores embeddings, typed
metadata, and (optionally) structured statements derived from text.
There is no SQL, no JSON-on-the-fast-path, no general query
language: the verbs *are* the API. Internally it is built like a
single-node columnar database: an mmap'd vector arena, a write-ahead
log, an in-RAM HNSW index, and a B-tree metadata store, all
sharded by agent.

The rest of this chapter explains how those pieces are wired
together inside one process.

---

## Mental model

Think of the server as a **funnel with a shelf at the bottom**:

```
       inbound TCP / TLS                inbound HTTP
            (port 9090)                  (9091/9092)
                 │                            │
                 ▼                            ▼
        ┌──────────────────┐         ┌────────────────┐
        │ Tokio runtime    │         │ Tokio runtime  │
        │ ────────────     │         │ /healthz       │
        │ accept + TLS     │         │ /metrics       │
        │ frame I/O        │         │ /v1/admin/*    │
        │ HELLO→AUTH SM    │         └────────┬───────┘
        │ shard routing    │                  │ (admin RPCs to
        └────────┬─────────┘                  │  the same shard
                 │                            │  fan-out)
                 ▼                            │
    ┌────────────────────────────────────────▼─────────┐
    │            ShardHandle fan-out (Arc<Vec<…>>)      │
    └─┬────────────────┬───────────────┬───────────────┬┘
      │ flume          │ flume         │ flume         │ flume
      ▼                ▼               ▼               ▼
   ┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐
   │ shard 0  │    │ shard 1  │    │ shard 2  │    │ shard 3  │
   │ Glommio  │    │ Glommio  │    │ Glommio  │    │ Glommio  │
   │ executor │    │ executor │    │ executor │    │ executor │
   │  ─────   │    │  ─────   │    │  ─────   │    │  ─────   │
   │ arena    │    │ arena    │    │ arena    │    │ arena    │
   │ WAL      │    │ WAL      │    │ WAL      │    │ WAL      │
   │ redb     │    │ redb     │    │ redb     │    │ redb     │
   │ HNSW     │    │ HNSW     │    │ HNSW     │    │ HNSW     │
   │ workers  │    │ workers  │    │ workers  │    │ workers  │
   └──────────┘    └──────────┘    └──────────┘    └──────────┘
       one              one             one             one
       OS thread        OS thread       OS thread       OS thread
```

The funnel is **Tokio**: multi-threaded, work-stealing, handling
network I/O and dispatch. The shelf is one **Glommio** executor
per shard: single-threaded, pinned to one OS thread, owning all the
durable state for the agents that hash to that shard. The two
runtimes meet at a single primitive — a `flume` channel — and
nowhere else.

Pin this picture. The rest of the chapter is just unfolding what
sits in each box.

---

## The two-runtime split

Brain runs Tokio *and* Glommio in the same process, deliberately.
Other systems pick one. Why two?

**Tokio at the edge.** The connection layer needs to do many small
things concurrently: accept TCP, terminate TLS, read frames, parse
the handshake, time idle connections out, fan out admin HTTP, and
gracefully drain on SIGTERM. Tokio's work-stealing scheduler and
its mature ecosystem (`tokio-rustls`, `hyper`, `tokio::signal`) are
a good fit, and the workloads here are I/O-bound rather than
cache-sensitive. The Tokio runtime is built in `main.rs`:

```rust
// crates/brain-server/src/main.rs:187
let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .thread_name("brain-conn")
    .build()
```

**Glommio per shard.** Storage is the opposite problem. A shard's
arena, WAL, redb metadata, and HNSW index are all *cache-resident
state* that benefits from staying on one CPU's L1/L2. Cross-core
synchronisation on a hot path is exactly what we want to avoid.
Glommio's thread-per-core executor — single-threaded, `!Send`
futures, io_uring — gives us that without locks. One executor per
shard is spawned in `crates/brain-server/src/shard/mod.rs:804`:

```rust
let join_handle = LocalExecutorBuilder::new(placement)
    .name(&format!("brain-shard-{shard_id}"))
    .spawn(move || async move { … });
```

`placement` is `Placement::Fixed(cpu)` when the operator pinned
this shard to a specific core, or `Placement::Unbound` otherwise
(`crates/brain-server/src/shard/mod.rs:791`). Either way, one
shard = one OS thread = one executor = one set of files on disk.

**Why not just use Tokio everywhere?** Inside a shard we *want*
the types to be `!Send`. The single-writer-per-shard discipline
(see [03 — arena and WAL](03-arena-and-wal.md)) relies on the
compiler refusing to let shard-owned state move across threads.
Tokio's `Send` futures would force `Arc<Mutex<…>>` around
everything; Glommio's `!Send` futures don't.

**Why not just use Glommio everywhere?** The accept path doesn't
benefit from thread-per-core (a TCP connection's lifetime is
unrelated to which shard it ends up talking to), and the HTTP
control plane wants the broader Tokio ecosystem. Putting Glommio
at the edge would also mean teaching every Tokio dependency
(`tokio-rustls`, `hyper`, `tower`) to live without its runtime.

The cost is one boundary primitive, described next.

---

## The boundary: `flume` channels

Tokio tasks and Glommio tasks never run together; they communicate
exclusively through `flume` channels. `flume` is reactor-agnostic —
`send_async` / `recv_async` work natively under either runtime —
which is exactly what we need at this seam.

The shape:

- Each shard owns the **receive end** of a `flume::Receiver<ShardRequest>`
  (`crates/brain-server/src/shard/mod.rs:786`).
- Each `ShardHandle` (held by the Tokio connection task) owns a
  cloneable **send end** to that channel.
- Per-call replies travel back on a fresh `flume::bounded(1)`
  oneshot whose sender is carried *inside* the request itself.

Every variant of `ShardRequest`
(`crates/brain-server/src/shard/mod.rs:100`) follows the same
two-channel pattern:

```rust
ShardRequest::DispatchOp {
    req: Box<RequestBody>,
    reply_tx: Sender<Result<ResponseBody, OpError>>,
}
```

The connection task does:

```
// crates/brain-server/src/shard/mod.rs:519
shard_handle.tx.send_async(ShardRequest::DispatchOp { … }).await?;
reply_rx.recv_async().await
```

The shard's main loop pulls the request off its receiver, runs
`brain_ops::dispatch` on the Glommio executor, and sends the
`ResponseBody` back through `reply_tx`. The Tokio task `.await`s
the reply, then writes the response frame out to the socket.

**This is the only place ownership crosses runtimes.** Every other
piece of shard state stays `!Send` and stays on its executor —
`OpsContext` is intentionally not `Send` post sub-task 9.7
(`crates/brain-ops/src/context.rs:7`).

---

## A request, end to end

Let's walk one `RECALL` from packet to durable read. Assume the
client is already past the handshake; the connection is in the
`Established` phase.

```
        ┌───────────────────────────────────────────┐
   (1)  │ client → TCP → tokio_rustls → Tokio task  │
        └─────────────────────┬─────────────────────┘
                              │ frame bytes
        ┌─────────────────────▼─────────────────────┐
   (2)  │ Frame::decode_with_max → RequestBody       │
        │  (crates/brain-protocol/...)               │
        └─────────────────────┬─────────────────────┘
                              │ Action::OpDispatch
        ┌─────────────────────▼─────────────────────┐
   (3)  │ pick shard:                                │
        │   MemoryId-bearing op → memory_id.shard()  │
        │   else → BLAKE3(agent_id) % shard_count    │
        └─────────────────────┬─────────────────────┘
                              │ flume send
        ┌─────────────────────▼─────────────────────┐
   (4)  │ Glommio executor: brain_ops::dispatch      │
        │   plan → execute → write → publish epoch   │
        └─────────────────────┬─────────────────────┘
                              │ flume reply
        ┌─────────────────────▼─────────────────────┐
   (5)  │ Tokio task: ResponseBody → Frame → socket  │
        └───────────────────────────────────────────┘
```

(1) The Tokio runtime's accept loop spawned a per-connection task
on first accept (`crates/brain-server/src/network/connection.rs:470`).
That task reads frame-shaped bytes with a per-frame timeout.

(2) Each frame is validated by
`brain_protocol::Frame::decode_with_max`. The connection task hands
the decoded frame to a pure state machine — `dispatch_frame`
(`crates/brain-server/src/network/dispatch.rs:158`) — which returns
one of `Action::Inline` (built right here), `Action::OpDispatch`
(needs a shard), or a few stream-management variants.

(3) For `OpDispatch`, the state machine picks the shard. Requests
that already carry a `MemoryId` use the embedded shard bits
(`crates/brain-server/src/network/routing.rs:120`); everything else
hashes the agent's UUID with BLAKE3 modulo `shard_count`
(`crates/brain-server/src/network/routing.rs:105`). The routing
table is published behind an `ArcSwap` so a future admin RPC can
swap it without restarting connections
(`crates/brain-server/src/main.rs:158`).

(4) Cross the boundary: the connection task sends the request
through `flume`. The Glommio executor pulls it off and calls
`brain_ops::dispatch` (`crates/brain-ops/src/dispatch.rs:19`),
which is an exhaustive `match` over every wire opcode. The match
is intentionally exhaustive — adding a new opcode to the wire
shape fails to compile here until you wire the handler.

For `RECALL`, the handler embeds the cue text (or accepts a vector
directly), runs the HNSW search against the shard's index, applies
filters, optionally re-ranks, and assembles the response. The
storage layer is read-only on a `RECALL` path; no WAL traffic, no
locks, no allocations after warm-up.

(5) The reply comes back through `flume`, the Tokio task encodes
the `ResponseBody` back into a frame, writes it to the socket, and
goes back to reading. The whole round trip lives inside one
per-connection Tokio task plus one shard executor — no thread pool,
no inter-shard chatter, no global locks.

A `RECALL` is the simple case. An `ENCODE` adds the WAL fsync
step (the only thing that gates the response) and a publish step
that makes the new memory visible to readers — covered in detail
in [03 — arena and WAL](03-arena-and-wal.md).

---

## Layering

The runtime split corresponds to a strict layering. From the wire
inwards:

| Layer | Crate(s) | Runtime | What it owns |
|---|---|---|---|
| Wire codec | `brain-protocol` | n/a (pure) | Frames, opcodes, rkyv types |
| Connection | `brain-server/src/network/` | Tokio | TCP accept, TLS, frame I/O, handshake, routing |
| Boundary | `brain-server/src/shard/` (`ShardHandle`) | Tokio↔Glommio | `flume` channels |
| Cognitive ops | `brain-ops` | Glommio | Dispatch on `RequestBody`, transactions, subscribe |
| Planning | `brain-planner` | Glommio | Execution plans, hybrid query routing |
| Storage | `brain-storage` | Glommio | mmap'd arena, WAL, recovery |
| Metadata | `brain-metadata` | Glommio | redb tables (substrate + knowledge) |
| Index | `brain-index` | Glommio | HNSW, tantivy (when activated) |
| Embedding | `brain-embed` | Glommio | BGE inference + cache |
| Workers | `brain-workers` | Glommio | Twelve periodic sweepers, all per shard |

Two crosscutting crates sit outside the layer stack:

- `brain-core` — shared `Copy`/`Clone` types (`MemoryId`, `AgentId`,
  `ShardId`, `SlotVersion`, error taxonomy). Depended on by
  everyone; depends on nothing.
- `brain-llm` — only used when the knowledge layer is active and
  an LLM extractor tier is configured. See
  [10 — extractors](10-extractors.md).

**The rules:**

1. **No skipping.** `brain-ops` does not reach into `brain-storage`
   directly; it goes through `brain-planner`'s executor context.
2. **No reverse calls.** `brain-storage` does not call back into
   `brain-ops`.
3. **Async boundaries between layers.** Storage and metadata return
   futures that the executor can yield on.
4. **No shared mutable state across crates.** Channels and
   `Arc<dyn …>` handles, not shared `Mutex`s.

These are enforced socially, not by the compiler. The compiler
enforces module visibility; the layering is a code-review rule.

---

## What each shard owns

A shard is the unit of *isolation*. Inside one Glommio executor,
this set of state is co-located and never shared:

- **Arena file** — `<data_dir>/<shard_id>/arena.bin`, a sparse
  mmap'd file with one fixed-size slot per memory. See
  [03 — arena and WAL](03-arena-and-wal.md).
- **WAL segments** — `<data_dir>/<shard_id>/wal/*.wal`, append-only
  with `O_DIRECT` + group commit.
- **redb metadata** — `<data_dir>/<shard_id>/metadata.redb`, a
  copy-on-write B-tree. See [05 — redb metadata](05-redb-metadata.md).
- **HNSW index** — in-RAM, rebuilt from arena on startup if no
  snapshot is found. See [04 — HNSW index](04-hnsw-index.md).
- **Embedder** — one `Arc<dyn Dispatcher>` (BGE-small via candle).
  Shared by every handler on this executor; not shared across
  executors.
- **OpsContext** — the per-shard handle bag handlers consume
  (`crates/brain-ops/src/context.rs:34`).
- **Worker scheduler** — twelve background workers run on this
  same executor, time-sliced cooperatively with request work. See
  [07 — background workers](07-background-workers.md).
- **Knowledge-layer state** (only when a schema is declared) —
  entity HNSW, statement HNSW, tantivy indexes, LLM extractor cache.

What a shard does *not* own:

- TCP connections. Those live in Tokio tasks and may talk to any
  shard over the course of a session.
- The routing table.
- The HTTP control plane.
- Other shards.

Two shards never call each other on a request path. The closest
thing to a cross-shard read is the connection-layer SUBSCRIBE
fan-out, which drains each shard's local event bus into a single
client stream (`crates/brain-server/src/network/subscribe.rs`),
and even that is per-shard reads, not cross-shard joins.

---

## Routing

The wire protocol guarantees that any request a shard receives is
*for* that shard. Routing happens once, in the connection task,
before the boundary crossing.

Two routing modes:

- **By `MemoryId`.** Every `MemoryId` encodes its shard in the
  top 16 bits. Operations that already carry one (`FORGET`,
  `LINK`, `UNLINK`, idempotency lookups against an existing
  memory) extract it directly — O(1) bit shift, no hash, no map
  lookup (`crates/brain-server/src/network/routing.rs:120`).
- **By `AgentId`.** Everything else (`ENCODE`, the first `RECALL`
  of a session, schema ops, admin ops) routes on
  `BLAKE3(agent_uuid_bytes) % shard_count`
  (`crates/brain-server/src/network/routing.rs:105`).

BLAKE3 was chosen over the obvious xxhash for two reasons: the
cluster already uses BLAKE3 for content addressing (chapter 05),
so taking the dependency costs nothing, and BLAKE3's
distribution-uniformity tests pass at much tighter bounds than the
test suite needs (`crates/brain-server/src/network/routing.rs:173`).
A 10k-agent sweep across 16 shards comes well inside the ±50% band
the test enforces.

A startup-time *override map* lets operators pin specific agent
IDs to specific shards — useful for VIP agents and for shifting
load off a hot shard (`crates/brain-server/src/network/routing.rs:48`).
Overrides are validated against `shard_count` at construction
(no foot-guns on shard-count reductions).

**Hot reload.** The connection layer reads the routing table
through `arc_swap::ArcSwap<RoutingTable>`. An admin RPC can
publish a new table atomically; in-flight requests on the previous
table keep a reference, so they see a coherent snapshot until they
drop it. There's no global lock and no quiescence step.

What's *not* implemented in v1: multi-shard agents, consistent
hashing for elastic shard counts, and `WrongShard` redirects from
the server. The shard count is fixed at startup.

---

## Lifecycle: starting up and shutting down

**Boot order** (`crates/brain-server/src/main.rs`):

1. Parse argv, load TOML config (`Config::load`).
2. Initialise logging (`bootstrap::logging::init_pre_config`).
3. Build the optional summarizer (knowledge-layer LLM tier).
4. **For each shard** (sequential, in `spawn_shards`):
   a. `mkdir -p` the shard's data dir.
   b. Open (or generate) the shard UUID file.
   c. mmap the arena.
   d. Open redb metadata.
   e. Replay WAL segments (`brain_storage::recover`).
   f. Spawn a `LocalExecutor` on a dedicated OS thread.
   g. Inside the executor: build HNSW, attach embedder, open
      tantivy (if schema active), materialise extractor registry,
      start the worker scheduler.
5. Build the Tokio runtime.
6. Bind the admin HTTP listener (`admin_addr`, default
   `127.0.0.1:9092` — loopback).
7. Bind the public metrics listener (`metrics_addr`, default
   `127.0.0.1:9091`).
8. Bind the wire-protocol listener (`listen_addr`, default
   `127.0.0.1:9090`).
9. Install SIGINT + SIGTERM handlers.
10. `await` the accept loop until shutdown is signalled.

Step 4 is the slow part on a cold start — WAL replay scales with
the durable segment count. On a warm start (snapshots present, no
unreplayed segments) each shard opens in single-digit milliseconds.

**Shutdown** (`crates/brain-server/src/main.rs:382`,
`crates/brain-server/src/bootstrap/shutdown.rs`):

1. SIGINT or SIGTERM fires the `ShutdownSignal` (a
   `tokio::sync::watch` so late observers don't miss the edge).
2. The connection accept loop stops accepting; the per-connection
   tasks finish their current frames and exit.
3. The admin and metrics HTTP listeners drain (bounded 2s budget).
4. Outside the Tokio runtime, `graceful_shutdown_shards` drops the
   `Arc<Vec<ShardHandle>>` so every shard's request channel closes.
5. Each shard's main loop sees the channel close, flushes its WAL,
   takes the WAL out of its `Option`, awaits `Wal::shutdown`, and
   the executor exits.
6. `ShardJoiner::join()` blocks on the OS thread and returns.

The order matters. We stop *accepting* before we close the shard
channels, so no client gets an in-flight `flume::SendError`. We
close the shard channels before joining, so each shard sees a
clean termination and not a panic.

SIGTERM is what supervisors (systemd, k8s, docker stop) send. If
SIGTERM install fails (restricted containers), the server falls
back to SIGINT-only and warns
(`crates/brain-server/src/main.rs:408`).

---

## Failure modes

What can go wrong at this layer, and what the operator sees.

**Shard spawn fails on boot.** A bad arena file, missing
permissions on `data_dir`, or a redb that won't open. The server
logs the failed shard id and exits non-zero before opening any
listener — half-started states are never published
(`crates/brain-server/src/main.rs:332`).

**WAL replay finds a torn record.** Recovery treats the torn record
as a transactional boundary: everything before it is committed,
everything at or after is discarded. The recovery report (records
replayed / skipped / discarded) is logged
(`crates/brain-server/src/shard/mod.rs:769`). The handler-visible
state is identical to "the truncated operation never returned an
ack" — see chapter 03 for the proof.

**A shard panics under load.** Glommio aborts the executor thread.
The shard's `flume::Receiver` is dropped; every queued request
fails with `DispatchError::ShardDisconnected`, which surfaces on
the wire as the structured "shard unavailable" error. The
connection layer keeps running. The only fix is process restart;
v1 has no shard auto-respawn.

**The connection layer panics in one task.** Tokio kills that
task. The TCP socket closes. Other connections are unaffected. The
shard the dead task was talking to is unaffected — its main loop
notices the reply channel was dropped (because the Tokio task is
gone) and discards the in-flight reply.

**A client opens many connections from one IP.** Connection limits
are enforced by `ConnectionLimits`
(`crates/brain-server/src/network/connection.rs:117`). Beyond the
configured ceiling, the accept loop refuses new sockets.

**Auth is unset and the listener is public.** This is a deployment
mistake, not a failure mode. The server ships with `auth = none`
by default — fine for loopback dev, not fine for an exposed port.
The deployment guide spells this out; the server itself only logs
that AUTH_NONE is in effect.

**SIGKILL.** Brain is a fail-stop database. SIGKILL leaves the
durable side (WAL + arena + redb) in whatever state was last
fsynced. On restart, WAL replay restores a consistent snapshot of
acknowledged operations. Anything that was in flight but not
acknowledged is gone; that's the invariant the wire protocol
exposes (chapter 03).

---

## Configuration & tuning

Defaults that matter for this layer (from `config/dev.toml`; full
field reference is in `docs/reference/configuration.md`):

| Field | Default | Notes |
|---|---|---|
| `server.listen_addr` | `127.0.0.1:9090` | Wire protocol (TCP, optionally TLS). |
| `server.metrics_addr` | `127.0.0.1:9091` | Public HTTP: `/healthz`, `/metrics`. |
| `server.admin_addr` | `127.0.0.1:9092` | Admin HTTP: `/v1/admin/*`. Loopback by default. |
| `storage.shard_count` | `4` | Fixed at startup. Pick before first write. |
| `storage.data_dir` | `./data` | One subdirectory per shard. |
| `shard.arena_capacity_bytes` | `1GiB` | Initial sparse mmap size; grows. |
| `shard.wal_segment_size_bytes` | `256MiB` | One WAL segment file at a time. |

A few tuning rules of thumb:

- **`shard_count` is the most consequential knob.** Set it once,
  based on expected core count + working-set size, and don't
  change it. Reducing it later requires a rebalance that is not
  in v1.
- **One shard per physical core is the upper bound.** Anything
  beyond that means executors competing for L1, and you've lost
  the reason for going thread-per-core in the first place. Two
  shards per core is acceptable if the second shard is mostly
  idle (a cold tenant). Four or more shards per core is a
  misconfiguration.
- **Leave `metrics_addr` and `admin_addr` on loopback** unless
  you have something else fronting them (a sidecar, an nginx).
  Both expose unauthenticated endpoints in v1.
- **TLS is opt-in.** `server.tls.enabled = true` plus `cert` and
  `key` paths. The server fails to start if `enabled = true` and
  either path is unset (`crates/brain-server/src/main.rs:430`).

Environment variables override TOML via `BRAIN__SECTION__FIELD`
(double underscore separates nesting); `RUST_LOG` controls
tracing filter. See the `--help` output for the full list.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Process entry, runtime build | `crates/brain-server/src/main.rs` |
| TCP accept + per-connection task | `crates/brain-server/src/network/connection.rs` |
| Frame state machine + routing decisions | `crates/brain-server/src/network/dispatch.rs` |
| BLAKE3 / MemoryId-bit routing | `crates/brain-server/src/network/routing.rs` |
| `ShardHandle`, `ShardRequest`, `flume` boundary | `crates/brain-server/src/shard/mod.rs` |
| Shard executor body | `crates/brain-server/src/shard/mod.rs` (inside `spawn_shard`) |
| Admin / metrics HTTP listeners | `crates/brain-server/src/admin/`, `crates/brain-server/src/metrics/` |
| Shutdown signal + drain | `crates/brain-server/src/network/connection.rs`, `crates/brain-server/src/bootstrap/shutdown.rs` |
| TOML config schema | `crates/brain-server/src/config/mod.rs` |
| Top-level op dispatch | `crates/brain-ops/src/dispatch.rs` |
| Per-shard state bag | `crates/brain-ops/src/context.rs` |

---

## Further reading

- [02 — Wire protocol](02-wire-protocol.md) — how the bytes on
  the TCP stream become a `RequestBody`.
- [03 — Arena and WAL](03-arena-and-wal.md) — what happens after
  `brain_ops::dispatch` decides to write.
- [08 — Tokio/Glommio boundary](08-tokio-glommio-boundary.md) — the
  channel discipline, in detail, with the failure cases.
- [07 — Background workers](07-background-workers.md) — what runs
  inside the shard executor besides request handlers.
