# 22 — Concurrency and async

A database needs to serve many requests at once without
stalling. Brain does this with **two async runtimes** in
the same process: **Tokio** at the edge (handling network
I/O) and **Glommio** per shard (handling storage).

This chapter explains what "async runtime" means, what
Tokio and Glommio do, what **thread-per-core** is, and why
Brain runs both. The systems vocabulary in this chapter is
the dense one; we'll lean heavily on sidebars.

---

## The problem

A server receives hundreds or thousands of requests per
second. Each request is mostly waiting — for the network,
for the disk, for the embedding model. Naive approaches:

- **One thread per request.** A thousand threads in
  flight. Each thread costs ~1 MB of stack. Context-
  switching between them is expensive. Scales badly.
- **Single-threaded blocking loop.** Process one request
  at a time. Throughput is dreadful.

Better: **asynchronous I/O** — a single thread (or a
small pool of threads) handles many in-flight requests
by *yielding* whenever it's about to wait, letting
another request make progress.

> **What is async I/O?**
>
> A model where operations that would otherwise block
> (network reads, file writes) instead return a *future*
> that completes later. The runtime drives many futures
> on a few threads, switching between them as they make
> progress. Pioneered in Erlang and Node.js; mainstream
> in Rust via tokio and async-std.
>
> See [Wikipedia: Asynchronous I/O](https://en.wikipedia.org/wiki/Asynchronous_I/O).

In Rust, you write code that *looks* like:

```rust
async fn handle_request(req: Request) -> Response {
    let data = read_from_disk(...).await;
    let result = process(data);
    write_to_disk(...).await;
    Response::ok(result)
}
```

The `.await` points are where the function might yield.
When the runtime sees `.await` on something that's not
ready (a disk read in flight, say), it parks this
function and runs something else. When the operation
finishes, the runtime resumes this function from the
`.await` point.

The result: one thread can be in the middle of
*thousands* of in-flight requests at once, as long as
most of them are waiting on I/O most of the time.

---

## What an async runtime is

An async runtime is the engine that drives async
functions. Two pieces:

1. **Executor.** Polls ready futures, runs them until
   they're stuck on an `.await`, then parks them.
2. **Reactor.** Watches for I/O completions (network
   reads, disk writes, timers); wakes up the parked
   futures when their I/O is ready.

In Rust the runtime isn't built into the language — you
pick one as a dependency. The big two:

- **[tokio](https://tokio.rs/)** — the most popular Rust
  async runtime. Work-stealing scheduler, mature
  ecosystem, broad library support.
- **[glommio](https://github.com/DataDog/glommio)** —
  thread-per-core, single-threaded executors,
  io_uring-based reactor.

Brain uses both. We'll cover what each one is good at and
why Brain uses each one in the role it does.

---

## Tokio: the edge

Tokio is what Brain uses at the **edge** — the
connection layer that accepts TCP, reads frames,
authenticates clients, and dispatches requests to shards.

Tokio's defining trait is **work-stealing**: it runs a
small thread pool, and any worker thread can pick up any
task. If one worker is idle and another is busy, the
idle one *steals* tasks from the busy one's queue.

> **What is work stealing?**
>
> A scheduling pattern where each worker thread has its
> own queue of tasks; when a thread runs out, it pulls
> work from another thread's queue. Common in parallel
> runtimes (Go, Rust's tokio, Java's ForkJoinPool).
>
> See [Wikipedia: Work stealing](https://en.wikipedia.org/wiki/Work_stealing).

Work-stealing is great for the edge because:

- Tasks are short-lived and varied — accept a connection,
  read a frame, parse, dispatch. Heterogeneous workloads
  benefit from balancing across workers.
- The work is mostly *I/O-bound*. Network reads and TLS
  handshakes spend most of their wall time waiting; the
  thread pool can drive many in flight without breaking
  a sweat.
- Tokio's broad library ecosystem (rustls, hyper, tower,
  tracing, …) gives Brain leverage at the edge with very
  little code.

The substrate's network layer is ~6000 lines of Rust
using Tokio. That's small. Most of the complexity is
elsewhere (storage, indexes, extractors); the edge is
"glue plus the protocol state machine."

---

## Thread-per-core

For the storage side, Brain wants the *opposite* of work-
stealing. Each shard's data — its arena, WAL, redb
metadata, HNSW index — lives in RAM on one specific NUMA
node. If a request migrates between threads, it suffers
cache misses (the CPU has to reload the shard's data into
the new core's cache).

The pattern is **thread-per-core**: pin one thread to one
CPU, give it its own private state, never touch state
across threads.

> **What is thread-per-core?**
>
> A concurrency model where each CPU core runs a single
> dedicated thread; the threads don't share mutable
> state. Requests are routed to whichever thread owns the
> data, not balanced across threads. Used in DPDK,
> Seastar, ScyllaDB.
>
> See [Seastar's "Asynchronous design" doc](https://seastar.io/seastar/)
> and [ScyllaDB on thread-per-core](https://www.scylladb.com/2020/05/19/seastar-thread-per-core/).

The trade-offs:

- **Win:** no cache-line ping-pong between threads. No
  locks on the hot path (each thread's data is private).
  Predictable latency.
- **Loss:** no automatic load balancing. If shard A is hot
  and shard B is idle, you can't migrate requests; you
  have to route correctly upstream.

For Brain, that's a great deal: shards already have
their own data anyway, and the upstream router (chapter
23) does the routing.

---

## Glommio: per-shard executor

[Glommio](https://github.com/DataDog/glommio) is a Rust
thread-per-core executor from DataDog. It runs one
single-threaded executor per OS thread, pins each thread
to a CPU, and uses **io_uring** for I/O.

Brain spawns one Glommio executor per shard. The
executor runs:

- The shard's main request loop.
- The WAL group-commit task.
- The twelve background workers (decay, consolidation,
  HNSW maintenance, etc., chapter 07).
- Anything else that touches shard state.

All on the same OS thread. Single-threaded means:

- No locks. The executor's task scheduler runs at most
  one task at a time, so accessing the shard's state is
  always serial.
- No `Send` / `Sync` worries. Glommio futures are
  intentionally `!Send` (can't move across threads),
  which is enforced by the Rust compiler.
- Deterministic ordering. Tasks run in submission order
  modulo `.await` points, which makes reasoning easier.

The trade-off: tasks can't migrate. If a shard's
executor is overwhelmed, it stays overwhelmed — Tokio's
work-stealing wouldn't help even if you wired it up.

---

## io_uring

> **What is io_uring?**
>
> A Linux kernel interface (introduced in kernel 5.1,
> 2019) for asynchronous I/O. Programs submit I/O
> operations into a *submission queue* and receive
> completions in a *completion queue* — both rings of
> memory shared with the kernel, so I/O happens with
> near-zero syscall overhead.
>
> See [Wikipedia: io_uring](https://en.wikipedia.org/wiki/Io_uring)
> and [Lord of the io_uring](https://unixism.net/loti/),
> the standard online introduction.

io_uring is fast because it amortises the cost of
crossing the user-kernel boundary. Submit 100 I/O
operations with one syscall (or zero, if you have
`IORING_SETUP_SQPOLL`); receive 100 completions the same
way.

Glommio uses io_uring for everything — file reads, file
writes, fsync, network I/O — so the shard's storage
operations are as efficient as Linux allows. Brain's
WAL group-commit pattern (chapter 18), which batches
many records into one fsync, leans on io_uring's
batched-submission model to amortise even further.

---

## Why two runtimes

You might ask: why not just one runtime, either Tokio or
Glommio, for everything?

**If everything were Tokio:**

- Shards would have to use locks (`Mutex`, `RwLock`) to
  protect their data, because Tokio tasks can migrate
  between threads. The single-writer-per-shard discipline
  (next section) becomes harder.
- Tokio's reactor uses `epoll` by default, not io_uring.
  Brain's storage performance — especially WAL fsync
  batching — depends on io_uring.
- Glommio's tight Linux integration (per-shard memory
  allocation, NUMA awareness) isn't part of Tokio.

**If everything were Glommio:**

- The edge (TCP accept, TLS) loses Tokio's broad
  ecosystem support. You'd port tokio-rustls or write
  your own.
- Multi-core scaling at the edge is harder. Tokio's
  work-stealing handles uneven load naturally; Glommio's
  thread-per-core doesn't.
- The runtime is younger and less mature for general
  network workloads.

So Brain runs both: Tokio at the edge (network, TLS,
HTTP/SSE, signal handling), Glommio per shard (storage,
indexes, workers). The seam between them is one channel
type — a `flume` MPMC channel, which works under either
runtime.

The architecture chapter (chapter 08 of the architecture
tier) covers this boundary in detail. The concepts version
is: there's a clean line between "stuff that talks to
clients" and "stuff that touches shards," and the line is
exactly where the runtime changes.

---

## Single-writer-per-shard

The most important invariant in Brain's concurrency model
is **single-writer-per-shard**: at any moment, only one
task is writing to a given shard's data structures.

This is *not* enforced with locks. It's enforced *by
construction*:

- Each shard has one Glommio executor.
- The executor is single-threaded.
- The executor's task scheduler runs at most one task at
  a time.
- Therefore, at most one task is writing to the shard's
  data at any moment.

Multiple *reads* can be in flight (because they're all
on the same thread, they can be interleaved at `.await`
points). But writes are naturally serialised.

What this buys:

- No lock contention on the write path. The arena,
  metadata, WAL, and HNSW writer all assume serial
  access; they don't take internal locks.
- No subtle ordering bugs. If task A writes a memory and
  then task B reads it, B always sees A's write — no
  cache coherency, no memory ordering puzzles.
- Simple recovery. The WAL records operations in the
  order they happened; replay produces the same state.

The trade-off: per-shard write throughput is limited by
how fast one thread can serialise writes. If your
workload is write-heavy on a single shard, that's the
bottleneck. The answer is to shard more (chapter 23).

---

## How the two runtimes communicate

The connection layer (Tokio) needs to send work to the
shards (Glommio). Brain uses a **channel** for this:

```
client → TCP read → Tokio task →
  flume channel →
    Glommio shard task → response
  ← flume channel ←
Tokio task → TCP write → client
```

A `flume::channel` is an MPMC queue (multi-producer,
multi-consumer) that works under both runtimes. The
Tokio task pushes a `Request` onto the channel; the
shard task pops it off, processes, and pushes a
`Response` onto a reply channel.

A few practical notes:

- The channel is bounded (typical sizes: 256 to 1024
  per shard). If a shard is overwhelmed, the channel
  fills, and the Tokio task waits — which provides
  natural backpressure to the client.
- The channel carries opaque payloads. The Tokio task
  doesn't know what the shard will do with the request;
  it just routes.
- Both runtimes are running simultaneously in the same
  process. They cooperate via this one channel.

There's a small Rust-specific subtlety: Glommio futures
are `!Send`, meaning they can't be moved across threads.
The flume channel handles the request *between* threads;
once it's inside Glommio, it stays.

---

## What an operator sees

The concurrency model has operational consequences:

- **Each shard pegs one CPU under load.** If you have 4
  shards on an 8-core machine, you have 4 cores
  potentially saturated. The other 4 are for Tokio (the
  edge), the embedding pool, OS bookkeeping.
- **Latency is stable.** Because there's no
  work-stealing across shards, tail latency is bounded.
  A slow shard doesn't slow down requests to other
  shards.
- **Memory is partitioned.** Each shard has its own
  arena, redb, indexes. Total memory is roughly N ×
  (per-shard size). No global shared cache.

These aren't accidental. The thread-per-core +
single-writer-per-shard model is *picked* for stable
latency and predictable resource usage.

---

## Recap

- Brain runs two async runtimes: **Tokio** at the edge,
  **Glommio** per shard.
- Tokio is work-stealing; good for I/O-heavy, varied
  network work.
- Glommio is thread-per-core; one single-threaded
  executor per shard, pinned to one CPU, using
  **io_uring** for everything.
- **Single-writer-per-shard** is enforced by the
  Glommio executor being single-threaded, not by locks.
- The two runtimes talk through a **flume channel** —
  one direction per shard, bounded for backpressure.
- The model gives stable tail latency at the cost of
  no automatic load balancing across shards.

---

## Where to go next

- **The Tokio/Glommio boundary, in detail:**
  [`../architecture/08-tokio-glommio-boundary.md`](../architecture/08-tokio-glommio-boundary.md).
- **The architecture-tier system view:**
  [`../architecture/01-system-architecture.md`](../architecture/01-system-architecture.md).
- **How shards are picked and partitioned:**
  [chapter 23](23-sharding-and-isolation.md).
- **The background workers that share each shard's
  executor:**
  [`../architecture/07-background-workers.md`](../architecture/07-background-workers.md).
