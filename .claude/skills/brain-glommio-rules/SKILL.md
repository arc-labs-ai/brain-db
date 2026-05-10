---
name: brain-glommio-rules
description: Enforce Glommio shard-runtime rules — no Tokio, no thread pool, types are !Send, no tokio::fs, no shared mutex on per-shard data. Fires on diffs in shard executors and per-shard data.
when-to-use: |
  Triggers:
    - Diff touches per-shard runtime code (will land under crates/brain-server/src/shard/ once Phase 9 starts)
    - Diff touches code that runs inside a glommio executor
    - Anything imports `glommio::`
    - User adds `tokio::*`, `std::thread::spawn`, or `Mutex<T>` near per-shard state
    - User mentions "shard executor", "thread-per-core", "io_uring"
trigger-files:
  - crates/brain-server/**/shard/**/*.rs
  - crates/brain-storage/**/*.rs
  - crates/brain-ops/**/*.rs
  - crates/brain-workers/**/*.rs
spec-refs:
  - spec/01_system_architecture/04_layers.md
  - spec/01_system_architecture/05_hardware.md
---

# Glommio Shard Rules

## When to use

Code that runs *inside* a Glommio shard executor — per-shard data, per-shard tasks, the per-shard async event loop. Brain's architecture (CLAUDE.md §4) has a strict split:

- **Connection layer** = Tokio. Many tasks, lots of `Send + Sync`, OK to use Tokio's full ecosystem.
- **Shard layer** = Glommio. Thread-per-core, io_uring, single-task-per-shard for the writer, no Tokio. **This skill polices the shard layer.**

If the file lives in the connection layer, this skill is the wrong tool — see `brain-tokio-boundary` instead.

## Hard rules (CLAUDE.md §9)

- **Don't add Tokio inside a shard.** No `tokio::*` import. Mixing blocks the executor.
- **Don't introduce a thread pool** for parallel work. Sharding *is* the parallelism.
- **Don't allocate in the hot path** (encode/recall serving). Use object pools and pre-sized scratch.
- **Don't add `Send + Sync`** to per-shard types. They're explicitly `!Send`.
- **Don't use `tokio::fs`** in shard code. Use Glommio's I/O.
- **Don't hold a lock across `.await`.**
- **No `Mutex` on per-shard data.** The single-writer discipline replaces locks.

## Workflow

1. **Confirm the file is shard-layer.** Check the path against `trigger-files`. If unsure, look for a `glommio::` import or a `glommio::LocalExecutor` runtime.
2. **Banned imports.** `grep -nE 'use tokio::|use std::thread::|tokio::fs|Arc<Mutex|Arc<RwLock' <files>` — every hit is a hard reject unless it's clearly a `Send`-typed channel for crossing into the connection layer (see step 5).
3. **`!Send` discipline.** Per-shard structs must NOT derive `Send` or `Sync` explicitly, and shouldn't be `Send` by structural inheritance unless they cross shards. If a struct *is* `Send + Sync`, ask: should it be? If not, restructure (use `Rc<T>` over `Arc<T>` for intra-shard sharing; use `RefCell<T>` for interior mutability).
4. **Allocation audit.** In hot-path functions (encode, recall, plan, reason, forget), no `Vec::new()`, `String::from`, `Box::new` per request. Pre-allocate at startup or lease from an object pool.
5. **Cross-layer channels.** The shard layer talks to the connection layer through bounded channels. The channel itself can be `Send + Sync` (it has to cross threads); the *messages* on it should be plain structs, not handles to per-shard data.
6. **I/O.** Reads/writes use `glommio::io::*`. No `tokio::fs`, no `std::fs::File::read` in async code.

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `tokio::spawn(...)` inside shard | Mixing runtimes | `glommio::spawn_local(...)` |
| `Arc<Mutex<ShardState>>` | Defeats single-writer | Single-task ownership; cross-task via channels |
| `Arc<RwLock<HnswIndex>>` for hot reads | Lock contention | `ArcSwap<HnswIndex>` + crossbeam-epoch (CLAUDE.md §4) |
| `let _g = lock.await; foo().await` | Lock across await | Drop the guard before `.await` |
| `let v = Vec::with_capacity(n); ... ;` per request | Hot-path alloc | Lease from a per-shard object pool |
| `tokio::fs::read(...)` in shard | Wrong runtime | Glommio's `Buffered`/`Direct` I/O |
| `std::thread::spawn` for parallelism | Defeats sharding | Don't — work goes to the right shard |

## Cross-references

- `brain-tokio-boundary` — connection-layer rules; complementary.
- `brain-invariants` — single-writer-per-shard is invariant #2.
- `rust-concurrency` — generic Send/Sync background.

## Examples

### Golden — shard handler

```rust
// brain-server/src/shard/encode.rs
pub(crate) fn handle_encode(
    state: &mut ShardState,           // owned by this task; !Send
    req: EncodeRequest,
    scratch: &mut EncodeScratch,      // pool-leased; reused
) -> Result<EncodeResponse, ProtocolError> {
    // No allocation; no lock; no tokio::*; no .await across borrow.
    state.wal.append(&req.text, scratch)?;
    state.wal.fsync()?;                // WAL-before-ack
    state.arena.write_slot(...)?;
    Ok(EncodeResponse { ... })
}
```

### Counter — Tokio in a shard

```rust
use tokio::sync::Mutex;       // ← reject, mixing runtimes
use tokio::fs;                // ← reject, use glommio I/O

let state = Arc::new(Mutex::new(ShardState::new()));   // ← reject, defeats single-writer

tokio::spawn(async move {                              // ← reject, wrong runtime
    let bytes = fs::read("wal.log").await?;
    ...
});
```

Reject all four lines; surface to the user with the right Glommio replacements.

## Source / Adaptations

Project-local. Operationalizes CLAUDE.md §4 + §9.
