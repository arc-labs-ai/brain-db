---
name: rust-concurrency
description: General Rust concurrency patterns (Send/Sync, channels, atomics, async vs threads). Brain-specific Glommio/Tokio rules live in brain-glommio-rules and brain-tokio-boundary.
when-to-use: |
  Triggers:
    - User asks "thread or async?" / "Mutex or channel?" / "deadlock?"
    - Compiler errors mentioning E0277 with `Send` or `Sync`
    - Adding a new concurrent primitive: thread, channel, Mutex, RwLock, Atomic
    - Reviewing async code for Send-across-await issues
    - User mentions race condition, future is not Send, lock contention
spec-refs:
  - spec/01_system_architecture/04_layers.md
license: MIT
source: https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m07-concurrency
---

# Concurrency

## When to use

Picking concurrency primitives or debugging an `E0277 Send/Sync` error. For Brain's specific rules (Glommio shards, Tokio connection layer, single-writer-per-shard, ArcSwap + crossbeam-epoch, no thread pool for parallel work) see the project-specific `brain-glommio-rules` and `brain-tokio-boundary` skills.

## Core question

**Is this CPU-bound or I/O-bound, and what's the sharing model?**

1. **Workload type:** CPU-bound → threads (often: don't, since sharding is the parallelism). I/O-bound → async.
2. **Sharing model:** none → channels; immutable → `Arc<T>`; mutable → `Arc<Mutex<T>>` / `Arc<RwLock<T>>`; lock-free → `ArcSwap` + epoch GC.
3. **Send/Sync:** crossing threads → `Send`; cross-thread refs → `Sync`. Per-shard types in Brain are intentionally `!Send`.

## Brain context (cross-link)

- **Glommio** runs the shard layer (thread-per-core, io_uring). Inside a shard, concurrency is single-task — no Mutex needed for owned data.
- **Tokio** runs the connection layer. Many tasks; locks-across-await is forbidden.
- **Cross-shard** sharing uses `ArcSwap` + `crossbeam-epoch` (lock-free) — see CLAUDE.md §4.

## Workflow

1. Determine workload: CPU vs I/O.
2. Pick the model (Brain-specific overrides above).
3. If you reach for a Mutex, ask first: can a channel + ownership transfer do it? In Brain: can shard discipline avoid the lock entirely?
4. If async, scope locks tightly — never across `.await`.
5. Verify Send/Sync expectations match the runtime: shard code is `!Send`, connection code is `Send`.

## Send/Sync markers

| Marker | Meaning | Example |
|--------|---------|---------|
| `Send` | Can transfer ownership between threads | Most types |
| `Sync` | Can share `&T` between threads | `Arc<T>` |
| `!Send` | Must stay on one thread | `Rc<T>`, Brain shard state |
| `!Sync` | No shared refs across threads | `RefCell<T>` |

## Quick reference

| Pattern | Thread-safe | Blocking | Use when |
|---------|-------------|----------|----------|
| `std::thread` | yes | yes | CPU-bound parallelism (rare in Brain — sharding is the parallelism) |
| `async/await` | yes | no | I/O-bound concurrency |
| `Mutex<T>` | yes | yes | Shared mutable state (avoid in shards; OK at connection layer with strict scoping) |
| `RwLock<T>` | yes | yes | Read-heavy shared state |
| `mpsc::channel` / `tokio::sync::mpsc` | yes | optional | Message passing |
| `Arc<Mutex<T>>` | yes | yes | Cross-thread shared mutable (last resort) |
| `ArcSwap` + epoch | yes | no | Cross-shard lock-free reads (Brain default for hot reads) |
| `Atomic*` | yes | no | Counters, flags |

## Decision flowchart

```
What type of work?
├─ CPU-bound  → threads (usually shard discipline replaces this in Brain)
├─ I/O-bound  → async/await
└─ Mixed      → spawn_blocking from async, or split layers

Need to share data?
├─ No        → message passing (channels)
├─ Immutable → Arc<T>
└─ Mutable   →
   ├─ Read-heavy   → Arc<RwLock<T>>
   ├─ Write-heavy  → Arc<Mutex<T>>
   ├─ Atomic op    → AtomicUsize / AtomicBool
   └─ Hot read     → ArcSwap + epoch GC (Brain pattern)

Async context?
├─ Type is Send       → tokio::spawn
├─ Type is !Send      → spawn_local OR keep in a Glommio shard
└─ Blocking code      → spawn_blocking (Tokio only; never in Glommio)
```

## Common errors

| Error | Cause | Fix |
|-------|-------|-----|
| E0277 `Send` not satisfied | Non-Send in async | `Arc<T>` instead of `Rc<T>`, or scope to single-thread runtime |
| E0277 `Sync` not satisfied | Non-Sync shared | Wrap in Mutex, or rethink ownership |
| Future not `Send` | `.await` while holding `!Send` | Drop the `!Send` value before `.await` |
| Deadlock | Lock ordering | Consistent global order, or `try_lock` with timeout |
| `MutexGuard` across await | Guard held during suspend | Scope the guard tightly |

## Async-specific patterns

### Avoid `MutexGuard` across `.await`

```rust
// Bad: guard held across await
let guard = mutex.lock().await;
do_async().await;  // guard still held!

// Good: scope the lock
let value = {
    let guard = mutex.lock().await;
    guard.value()
};
do_async(value).await;
```

### Non-Send types in async

- `Rc<T>` is `!Send`; can't cross `.await` in a multi-thread spawned task.
- Either use `Arc<T>`, run on a single-thread executor, or drop the `Rc` before `.await`.

## Anti-patterns

| Anti-Pattern | Why bad | Better |
|--------------|---------|--------|
| `Arc<Mutex<T>>` everywhere | Contention, complexity | Channels, sharding, `ArcSwap` |
| `thread::sleep` in async | Blocks executor | `tokio::time::sleep` |
| Holding locks across await | Blocks the executor task | Scope locks tightly |
| Ignoring deadlock risk | Hard to debug | Lock ordering, `try_lock` |
| Adding `Send + Sync` to a per-shard type | Defeats single-writer model | Keep `!Send`; channel between shards |

## Source / Adaptations

- **Source:** [`actionbook/rust-skills@1f4becd`](https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/m07-concurrency)
- **License:** MIT
- **Adaptations:**
  - Renamed `m07-concurrency` → `rust-concurrency`.
  - Cross-linked Brain's Glommio + Tokio split (CLAUDE.md §4) and pointed to project-specific skills for shard rules.
  - Added `ArcSwap` + epoch GC as a primary pattern for cross-shard lock-free reads.
  - Tightened the "Per-shard types are `!Send`" rule per CLAUDE.md §9.
  - Removed upstream "Trace Up / Trace Down" / `domain-*` cross-references.
