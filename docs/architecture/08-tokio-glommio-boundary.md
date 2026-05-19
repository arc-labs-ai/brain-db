# 08 — Tokio / Glommio boundary

**Audience:** anyone touching the connection layer, adding a
shard request type, debugging a hang at shutdown, or asking
"could I just spawn a Tokio task inside the shard?"

**Goal:** by the end you should know exactly which types are
`Send` and which aren't, where the single channel between the
two runtimes lives, what happens to an in-flight request when a
client drops, and which patterns will cause subtle bugs if you
write them.

This chapter assumes [01 — System architecture](01-system-architecture.md)
— that's where the funnel-and-shelf picture comes from. This
chapter is the rules at the seam.

---

## The seam, in one sentence

Tokio tasks talk to Glommio shards through `flume` channels.
Nowhere else. Everything else is a violation of the rules below.

---

## Why two runtimes at all

[Chapter 01](01-system-architecture.md) makes the case. The
short version:

- **Tokio at the edge** wants `Send` futures, a work-stealing
  scheduler, and the broad async ecosystem (`tokio-rustls`,
  `hyper`, `tokio::signal`). The work it does is I/O-shaped:
  TCP accept, TLS, frame I/O, idle timers, signal handling, the
  HTTP control plane.
- **Glommio per shard** wants `!Send` futures, thread-per-core
  affinity, and `io_uring`. The work it does is cache-shaped:
  arena access, WAL append, redb writes, HNSW insert, embedding
  inference. Single-writer-per-shard is a *type-system* property
  here, not a runtime contract.

The cost is one boundary primitive. The next sections describe
it.

---

## The Send/!Send divide

The Rust type system enforces the boundary. The cheat sheet:

| What | `Send`? | Notes |
|---|---|---|
| `Frame`, `RequestBody`, `ResponseBody` | yes | Wire types are POD-shaped. Built on Tokio side, sent through `flume`, decoded into a `RequestBody` on Tokio, then *moved* across the channel. |
| `ShardHandle` | yes | `Clone`. Holds `flume::Sender<ShardRequest>` + `Arc<…>` knobs. Lives on the Tokio side and crosses freely between Tokio tasks. |
| `flume::Sender` / `flume::Receiver` | yes | Reactor-agnostic; `send_async` / `recv_async` natively `.await` under either runtime. |
| `Arc<dyn Dispatcher>` (the embedder) | yes | Embedder is shared *across* shards but its methods run on whichever thread calls them. |
| `Arc<dyn Summarizer>`, `Arc<dyn SnapshotSource>` | yes | Same shape — shared trait objects with `Send + Sync` bounds. |
| `OpsContext` | **no** | Intentionally `!Send` since sub-task 9.7. Held by `Arc<OpsContext>` inside the shard executor. The interior `Arc<…>` fields are kept rather than swapped to `Rc<…>` to avoid test churn — single-threaded usage is enforced by the executor, not the field types. |
| `MetadataDb` | **no** | `redb::Database` itself is `Send`, but the wrapper takes `&mut self` for writes so the borrow checker enforces single-writer-per-shard. |
| `HnswIndex<D>`, `IdMap` | **no** | Per-shard structures with internal `Rc<…>` and similar non-thread-safe pieces. |
| `Rc<RefCell<Wal>>`, `Rc<RefCell<ArenaFile>>` | **no** | The shard's main loop holds these. `Rc` deliberately — no thread-safety overhead is needed inside the executor. |

A handful of `#![allow(clippy::arc_with_non_send_sync)]` and
`clippy::await_holding_refcell_ref` attributes near the top of
`crates/brain-server/src/shard/mod.rs:42` keep clippy quiet
about patterns that are sound under the executor's discipline
but look wrong without that context.

### What the compiler does for us

Two patterns are *prevented at compile time* by this discipline:

- **You can't accidentally hold a shard-local lock across a
  Tokio `.await`.** The lock guard is `!Send`; any future that
  holds it is `!Send`; `tokio::spawn` requires `Send`. The build
  breaks.
- **You can't share `MetadataDb` between two writer tasks.**
  `write_txn(&mut self)` is the only path to a write
  transaction; the borrow checker refuses two simultaneous
  `&mut MetadataDb`. Same for `HnswIndex` and friends through
  `&mut self`-typed mutation methods.

The discipline is *structural*, not runtime. We don't need a
mutex hierarchy, and we don't need to remember which lock is
which.

---

## The boundary primitive: `flume`

Why `flume` and not `tokio::sync::mpsc`?

`tokio::sync::mpsc` is Tokio's reactor — its `recv()` registers
with a Tokio waker. A Glommio executor doesn't know about Tokio
wakers; `.await`ing a `tokio::mpsc::Receiver` from inside a
Glommio task would deadlock as soon as the Tokio runtime
shuts down its workers (or never resume, depending on the
runtime build).

`flume` is **reactor-agnostic**. Its `send_async` and
`recv_async` futures don't depend on which executor polls them.
The two sides of a `flume::bounded(n)` channel can live in
different async worlds and each side's `.await` just works.

(There's no MPMC vs MPSC issue worth worrying about here —
`flume` channels are MPMC by default. We use them as MPSC at the
shard boundary, and as one-shots for replies.)

### The two channels per request

Every cross-boundary request uses *two* channels:

```
Tokio side                       Glommio side

per-connection task              shard main loop
    │                                │
    │  send_async(ShardRequest)      │
    ├───────────────────────────────►│ flume::bounded(channel_capacity)
    │     (shard inbox)              │  (channel_capacity = 1024 default)
    │                                │
    │  reply_rx.recv_async().await   │
    │ ◄──────────────────────────────┤ flume::bounded(1)
    │     (per-call reply oneshot)   │  (one channel per request)
```

The **inbox** is created at `spawn_shard` time
(`crates/brain-server/src/shard/mod.rs:786`) with capacity
1024 (`crates/brain-server/src/shard/mod.rs:240`). It carries
`ShardRequest`s. Every `ShardHandle` clone shares the same
sender; the receiver lives inside the executor.

The **reply oneshot** is created per-call. Look at any handle
method (`crates/brain-server/src/shard/mod.rs:519` for
`dispatch_op`):

```rust
pub async fn dispatch_op(&self, req: RequestBody)
        -> Result<ResponseBody, DispatchError>
{
    let (reply_tx, reply_rx) = flume::bounded(1);
    self.tx.send_async(ShardRequest::DispatchOp {
        req: Box::new(req),
        reply_tx,
    }).await?;
    reply_rx.recv_async().await?
}
```

The reply sender travels *inside* the request — the shard pulls
the request out of the inbox, runs the op, and sends the result
back through the embedded `reply_tx`. A bounded(1) channel is
exactly a one-shot — one message at most, the receiver waits for
it.

### Why bounded, not unbounded

The 1024-slot inbox is what gives us backpressure. When 1024
requests are queued and a 1025th arrives, `send_async` *waits*
until the shard drains one off. The Tokio connection task is
the entity that waits; the TCP read loop pauses; the socket's
receive window closes; the client experiences flow control
end-to-end with no application-level signaling.

If the inbox were unbounded, a misbehaving shard (or a Glommio
executor lagging on a slow worker) would let memory grow without
bound. With 1024 slots and (say) 100 bytes per `ShardRequest`,
the worst-case backlog per shard is ~100 KB. Comfortable.

The reply channel is bounded(1) because there's exactly one
reply per request. Larger doesn't help; smaller is impossible.

---

## The full request lifecycle

Putting the boundary into the request flow:

```
Tokio side                                        Glommio side

1. accept TCP → per-conn task
2. read frame, decode, dispatch_frame()
3. Action::OpDispatch → tokio::spawn(run_op_dispatch)
                                                  
4. shard.dispatch_op(req).await
        │
        │  flume::bounded(1) — reply channel
        │  flume::send_async(ShardRequest::DispatchOp{req, reply_tx})
        ├────────────────────────────────────────►
        │                                         5. rx.recv_async() returns
        │                                         6. brain_ops::dispatch(req, &ctx)
        │                                         7. reply_tx.send(Ok(resp))
        │ ◄────────────────────────────────────────
8. reply_rx.recv_async() returns
9. build response frame, send to client over TCP
```

A few things to notice:

- **The per-op work is spawned as a new Tokio task** in step 3.
  The connection's read loop doesn't await `dispatch_op`; it
  spawns it and goes back to reading. Concurrency on one
  connection comes from many outstanding `tokio::spawn`ed
  sub-tasks.
- **The shard's main loop is sequential.** It pulls one request
  off the inbox, runs the op (potentially `.await`ing on the
  WAL fsync), then pulls the next. Concurrency *within* a shard
  comes from Glommio's cooperative multitasking — a handler
  that yields on an `.await` lets the executor run something
  else (a background worker, another request).
- **The reply travels back through a fresh channel** that the
  spawned Tokio task owns. The shard's main loop doesn't keep
  state about outstanding replies — once it sends the reply,
  it's done with that request.

---

## What lives on each side

A more complete inventory than chapter 01's table:

### Tokio side
- `TcpListener` + per-connection tasks (`crates/brain-server/src/network/connection.rs:470`).
- `tokio_rustls::TlsAcceptor`.
- `ConnState` per connection (HELLO → AUTH → Established state machine).
- `Topology { shards, routing, server_caps, request_metrics }`
  (`crates/brain-server/src/network/dispatch.rs:89`).
- HTTP listeners (`admin`, `metrics`) on separate `SocketAddr`s.
- `ShutdownSignal` (`tokio::sync::watch`).
- `tokio::signal::unix` for SIGINT/SIGTERM.

### The boundary
- `flume::Sender<ShardRequest>` (1 per shard, cloned freely on
  the Tokio side).
- `flume::Receiver<ShardRequest>` (1 per shard, lives inside the
  executor).
- `flume::bounded(1)` reply channels created per call.

### Glommio side (per shard)
- `LocalExecutor` on its own OS thread.
- `Rc<RefCell<ArenaFile>>`.
- `Rc<RefCell<Option<Wal>>>`.
- `Arc<OpsContext>` (`!Send`, but `Arc`-shared inside the
  executor for ergonomic reasons — see the
  `#[allow(clippy::arc_with_non_send_sync)]` at
  `crates/brain-server/src/shard/mod.rs:42`).
- `SharedHnsw<384>` and its `Writer<384>` companion.
- `WorkerScheduler` + the twelve workers.
- Tantivy indexes (when active).
- LLM cache (when active).

### Crosses the boundary as data
- `RequestBody`, `ResponseBody` — `Send`, owned, moved across
  the channel by value (boxed in `ShardRequest::DispatchOp` to
  keep the enum size small).
- `Result<…, OpError>` — error taxonomy is a pure enum, `Send`.
- Scalar reply types for admin paths (`SnapshotInfo`, `HnswCounts`,
  `Vec<…>`-of-PODs).

---

## Backpressure, in detail

Three places exert backpressure, in order from outside to in:

1. **TCP receive window.** When the Tokio task stops reading,
   the kernel's TCP window narrows and the client sees flow
   control at the wire layer. This is the *outermost* signal.
2. **The shard inbox** (`flume::bounded(1024)`). When the Tokio
   task tries to `send_async` and the inbox is full, the future
   `.await`s. The connection task blocks, the read loop pauses,
   and TCP backs up.
3. **WAL group commit's batch window.** Inside the shard, the
   group committer batches up to 60 KiB or 100 µs
   ([chapter 03](03-arena-and-wal.md)). A burst of `ENCODE`s on
   one shard gets coalesced into a single fsync.

Three places that *don't* offer backpressure:

- **HNSW insert.** Synchronous, in-RAM. Not a backpressure
  surface; if you're inserting faster than 1–3 ms per node, the
  shard's main loop just stalls until each insert returns.
- **Embedding (CPU).** Same — synchronous, ~5–10 ms per inference.
- **The reply channel** (`flume::bounded(1)`). Never full by
  construction; one request, one reply.

The whole stack composes: a slow shard means a full inbox means
TCP window narrows means client slows down. No application-level
signaling needed.

---

## Cancellation and dropped clients

What happens if a client disconnects mid-`ENCODE`?

The Tokio connection task notices the TCP read failing and
returns from `serve_connection`. The per-connection task drops.
Any `tokio::spawn`ed sub-tasks for in-flight ops it owned are
still alive — `tokio::spawn` is detached by default.

For an in-flight `dispatch_op`:

- The spawned Tokio task is still `.await`ing
  `reply_rx.recv_async()`.
- The shard's main loop hasn't noticed anything; it'll finish
  the op and `reply_tx.send(Ok(resp))`.
- The reply comes back; the spawned task receives it; the
  spawned task tries to send the response frame out via
  `frame_tx.send_async(...).await`
  (`crates/brain-server/src/network/connection.rs:656`).
- `frame_tx` was the connection's outgoing-frame channel. Its
  receiver lived in the per-connection task. **That task is
  gone** — `frame_tx.send_async` returns an error; the spawned
  task does `let _ = ...` and exits silently.

The net effect: **the shard always finishes the operation**.
WAL is committed, arena is written, HNSW updated. The client
doesn't get the response, but the durable side reflects the
work. This is intentional: an `ENCODE` ack is just an
acknowledgement; the durability barrier was crossed before the
client could even have observed the response.

For idempotency this is exactly what we want — a client that
retries on disconnect will get an `IdempotencyConflict` (if its
hash matches) or a fresh execution. The wire protocol's
idempotency table ([chapter 05](05-redb-metadata.md)) is what
makes the retry safe.

### What we don't have

There's no cancellation token threading from the Tokio side
into the shard. If you killed the per-connection task before
the spawned sub-task even ran the dispatch, *that* would
cancel the boundary crossing — but only because the spawned
task hadn't sent the request yet. Once `send_async` succeeds,
the work is committed to running.

A future iteration could carry a cancellation channel inside
each request and have the shard's main loop check it before
expensive work, but the current design is "the shard always
finishes." This is the simpler invariant and the one the
durability story relies on.

---

## Graceful shutdown across the boundary

Shutdown is where the two runtimes interlock the most.
`graceful_shutdown_shards`
(`crates/brain-server/src/bootstrap/shutdown.rs:51`) coordinates
it:

1. **SIGINT/SIGTERM fires `ShutdownSignal`** (a `watch::channel`
   so late observers don't miss the edge,
   `crates/brain-server/src/network/connection.rs:73`).
2. **The accept loop stops accepting** new TCP connections.
3. **Existing per-connection tasks** finish their current frames
   and exit (their `serve_connection` futures observe the
   shutdown signal in their `select!`).
4. **The admin and metrics HTTP listeners drain** (each on a
   bounded 2 s budget).
5. **Tokio runtime's `block_on` returns.** All Tokio-side
   resources are dropped — including every clone of `Arc<Vec<ShardHandle>>`
   the topology/admin/event-hub held.
6. **`graceful_shutdown_shards` runs *outside* any runtime**
   (`std::thread::spawn`, `mpsc::recv_timeout`). It drops the
   last `Arc<Vec<ShardHandle>>`, which drops every shard's
   `flume::Sender<ShardRequest>`.
7. **Each shard's main loop** sees `rx.recv_async()` return
   `Err` (channel closed). It runs the in-shard drain: flush
   WAL, shutdown workers, take the `Wal` out of its `Option`,
   await `Wal::shutdown` (the group committer flushes its last
   batch).
8. **The Glommio executor exits** cleanly.
9. **`ShardJoiner::join()`** blocks on the OS thread and returns
   `Ok(())`.

If a shard takes longer than the 30 s drain budget
(`crates/brain-server/src/bootstrap/shutdown.rs:33`), the
joiner is `mem::forget`'d and the process exits with
`ExitCode::FAILURE`. We'd rather force-exit than wait forever;
the WAL durability story means we can't lose data even on
process abort.

The order *matters*:

- **Stop accepting → drain in-flight → close shard channels →
  wait for shard threads.** Each step waits for the previous to
  reach steady state. Closing shard channels before all
  Tokio-side clones are dropped wouldn't actually close them
  (the channel stays open as long as any sender lives), so the
  Tokio drain must complete first.

---

## Concurrency anti-patterns we explicitly prevent

The discipline forbids these patterns, and the type system
enforces all of them at compile time:

| Anti-pattern | What the compiler says |
|---|---|
| `tokio::spawn` inside a Glommio executor | Future is `!Send` (it borrows shard-local state); refuses to compile. |
| `tokio::fs::*` or `tokio::time::*` inside a shard handler | Same — Tokio's primitives register with the Tokio reactor that isn't there. Future doesn't poll. |
| Sharing `OpsContext` between threads | `!Send`; can't move into a `tokio::spawn`ed task. |
| Two writer tasks per shard | `MetadataDb::write_txn(&mut self)` and `Writer<D>::insert(&mut self)` both require `&mut`; the borrow checker refuses to give you two simultaneously. |
| Holding a lock across an `.await` | `parking_lot::RwLockReadGuard` is `!Send`; the future containing it is `!Send`; `tokio::spawn` refuses. |
| `Box<dyn …>` without `+ Send` | Won't satisfy `tokio::spawn`'s `Send` bound. |
| Per-shard `Arc<Mutex<…>>` everywhere | Not prevented, but not needed: nothing shares mutable state across threads inside a shard. If you find yourself wanting one, you're probably breaking the discipline somewhere upstream. |

The cost of each "violation" is also at compile time. You don't
ship a bug; the build refuses.

---

## A short list of patterns that **are** OK

- **`tokio::spawn` on the Tokio side.** Per-op tasks, signal
  handlers, the HTTP listeners.
- **`glommio::spawn_local` inside the shard.** Background
  workers, the group committer task. `spawn_local` futures are
  `!Send` and stay on the shard's executor.
- **`Arc<dyn Send + Sync>` for things shared across shards.**
  The embedder, the summarizer, the LLM cache file handle. Each
  shard pulls from the same `Arc` but the methods run on the
  calling thread.
- **`flume` for shard ↔ Tokio.** And only for that. Inside a
  shard, prefer `glommio::channels::*` or simple `Rc<RefCell<…>>`
  state machines.
- **A single `tokio::sync::watch` for shutdown.** Late
  observers see the edge — important for tasks that haven't
  started yet at shutdown signal time.
- **Bounded channels everywhere across the boundary.** 1024 for
  the inbox, 1 for the reply oneshots. Anywhere else needs a
  reason to be unbounded.

---

## Failure modes

**Shard panics.** The Glommio executor aborts the thread. Every
queued request fails with `DispatchError::ShardDisconnected`
when its `reply_rx` returns an error
(`crates/brain-server/src/shard/mod.rs:555`). The Tokio
connection layer keeps running and surfaces a `ShardUnavailable`
wire error to clients
(`crates/brain-server/src/network/dispatch.rs:464`). The shard
is gone; only a process restart brings it back.

**Tokio task panics.** Tokio kills that task; nothing else is
affected. If it was a per-connection task, the TCP socket
closes. If it was a per-op `tokio::spawn`, the spawned task is
gone and (as covered above) the shard still finishes the op —
the reply just gets dropped on the floor.

**Inbox full (backpressure).** `send_async` waits. The Tokio
connection task blocks at that line. Eventually the shard
drains and the send succeeds. No data loss, just latency.

**Reply channel sender dropped before sending.** Either the
shard panicked or the main loop exited mid-request. `recv_async`
returns an error; the call surfaces as `ShardDisconnected`.

**Shutdown signal observed by zero tasks.** Can't happen — the
`tokio::sync::watch` channel carries an edge that any future
observer sees on `.borrow()`
(`crates/brain-server/src/network/connection.rs:88`). The
previous implementation used `tokio::sync::Notify` and *did*
have this bug; the migration to `watch` is the fix.

**Drain budget exhausted.** A shard is taking too long to flush
its WAL or join its thread. After 30 s the joiner is `mem::forget`'d
and the process exits with `FAILURE`. The WAL is still durable
through wherever the last fdatasync got to.

---

## Configuration & tuning

| Knob | Where | Default | Notes |
|---|---|---|---|
| `channel_capacity` | `ShardSpawnConfig` (set in code, not TOML) | 1024 | The shard inbox size. Larger = more buffered backpressure, more worst-case memory. |
| `DEFAULT_SHARD_DRAIN_BUDGET` | `crates/brain-server/src/bootstrap/shutdown.rs:33` | 30 s | Per-process budget for graceful shutdown. Beyond this, threads are leaked and the process exits non-zero. |
| Tokio runtime threads | `tokio::runtime::Builder::new_multi_thread()` defaults | num CPUs | Set by Tokio's default. The connection layer is I/O-bound; the threads share work. |
| Glommio executors | one per shard | — | Set by `storage.shard_count`. One OS thread per shard; one `LocalExecutor` per OS thread. |

Operational rules:

- **Pick `shard_count` first, then run.** Tokio scales with
  CPUs automatically; the *interesting* choice is how many
  shards. See [chapter 01](01-system-architecture.md).
- **Don't oversize `channel_capacity`.** 1024 already gives ~1 s
  of headroom at thousand-req/s shard throughput. Larger trades
  worst-case memory for marginal extra burst absorption.
- **Watch for shutdown timeouts.** A 30 s timeout is enough for
  a clean drain; a deployment hitting it has either a stuck
  worker or a 25-minute WAL replay. The process exits FAILURE
  in this case; alert on it.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Tokio runtime build | `crates/brain-server/src/main.rs` |
| Per-connection task, `select!` loop | `crates/brain-server/src/network/connection.rs` |
| Frame state machine, `Topology` | `crates/brain-server/src/network/dispatch.rs` |
| `run_op_dispatch`, per-op `tokio::spawn` | `crates/brain-server/src/network/dispatch.rs` |
| `ShardRequest`, `ShardHandle`, `flume` inbox | `crates/brain-server/src/shard/mod.rs` |
| `ShardSpawnConfig`, `channel_capacity` default | `crates/brain-server/src/shard/mod.rs` |
| `Wal`, `OpsContext`, `!Send` types | `crates/brain-storage`, `crates/brain-ops` |
| `MetadataDb::write_txn(&mut self)` | `crates/brain-metadata/src/db.rs` |
| `HnswIndex` / `Writer` `&mut self` | `crates/brain-index/src/shared.rs` |
| `graceful_shutdown_shards` | `crates/brain-server/src/bootstrap/shutdown.rs` |
| `ShutdownSignal` (`watch::channel`) | `crates/brain-server/src/network/connection.rs` |
| Glommio executor spawn | `crates/brain-server/src/shard/mod.rs` |

---

## Further reading

- [01 — System architecture](01-system-architecture.md) for the
  funnel-and-shelf picture, the full request lifecycle, and why
  there are two runtimes.
- [03 — Arena and WAL](03-arena-and-wal.md) for what makes
  "always finish the operation" safe — the WAL is the
  durability boundary, not the client's ack.
- [05 — redb metadata](05-redb-metadata.md) for the
  `write_txn(&mut self)` pattern that makes single-writer
  structural.
- [07 — Background workers](07-background-workers.md) for what
  runs inside the shard executor alongside request handlers
  (and shares its concurrency budget).
