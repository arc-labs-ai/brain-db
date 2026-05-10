---
name: brain-tokio-boundary
description: Police the Tokio↔Glommio boundary — connection-layer types are Send+Sync, no per-shard handles leak across, channels are bounded and carry plain messages.
when-to-use: |
  Triggers:
    - Diff touches both `tokio::` and `glommio::` symbols
    - New code that crosses the connection-layer ↔ shard-layer boundary
    - Adding a channel between layers
    - User asks "should this be Send?" or "where does this run?"
    - Per-shard types accidentally gaining Send+Sync
trigger-files:
  - crates/brain-server/**/*.rs
spec-refs:
  - spec/01_system_architecture/04_layers.md
---

# Tokio ↔ Glommio Boundary

## When to use

Code that crosses the connection layer (Tokio) and the shard layer (Glommio). The boundary is where both runtimes meet and where most of Brain's concurrency bugs hide.

## The architecture (CLAUDE.md §4)

```
            ┌──────────── Tokio (connection layer) ────────────┐
client ───► │  TCP accept → frame decode → dispatch → channel │ ───► shard 0  (Glommio)
            │  Many tasks, Send+Sync OK, full Tokio ecosystem  │ ───► shard 1  (Glommio)
            └──────────────────────────────────────────────────┘ ───► shard N  (Glommio)
                                                                          │
                                                                          ▼
                                                                  thread-per-core,
                                                                  io_uring, single-writer,
                                                                  !Send per-shard state
```

**The contract:**

1. The connection layer is Tokio-native. It can use `tokio::*`, `Arc<Mutex<…>>`, `tokio::sync::*`. It is NOT allowed to hold per-shard data directly.
2. The shard layer is Glommio-native. It cannot import `tokio::*`. Per-shard data is `!Send`.
3. The two layers communicate via **bounded channels** carrying **plain message structs** — not handles to per-shard state.

## Hard rules

- **No per-shard data with `Send + Sync`.** If a struct lives inside a shard, it must not be sent across threads. Adding `Send + Sync` is a design failure unless the struct is genuinely cross-shard.
- **No `Arc<Mutex<T>>` where `T` is per-shard.** Same reason.
- **Channels carry messages, not handles.** A `tokio::sync::mpsc::Sender<EncodeRequest>` is fine; a `tokio::sync::mpsc::Sender<Arc<ShardState>>` is wrong.
- **Channels are bounded.** Unbounded channels mask backpressure. The bound becomes a queue depth; size it deliberately.
- **Connection layer does not block.** No `tokio::fs::read` for shard data; the shard owns its files. No CPU-heavy work in the connection task; dispatch to a shard.
- **Shard layer does not call `tokio::*`.** Period. See `brain-glommio-rules`.

## Workflow

1. **Identify the layer.** Connection layer = `crates/brain-server/src/{server,connection,session}.rs`-ish. Shard layer = `crates/brain-server/src/shard/*` and the per-shard internals (`brain-storage`, `brain-ops`).
2. **Audit imports.** Connection layer code imports `tokio::*`. Shard layer code imports `glommio::*`. A file with both is suspicious — confirm it's a glue/boundary file.
3. **Check `Send + Sync`.** For each new type, ask: does it cross threads? If yes (channel-traversed message, connection-layer state), `Send + Sync` is fine. If no (per-shard data), `!Send`.
4. **Check channel shapes.** Channels at the boundary carry plain messages — `EncodeRequest`, `RecallRequest`, etc. Not `Arc<…>` of per-shard data.
5. **Backpressure.** Every channel is `bounded(N)` with a deliberate `N`. Document the choice in a comment if non-obvious.

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `Arc<Mutex<ShardState>>` shared with Tokio | Lock contention; defeats single-writer | Channel from connection → shard |
| `Sender<Arc<ShardState>>` | Per-shard data crossing threads | `Sender<EncodeRequest>` |
| `mpsc::unbounded_channel` | No backpressure | `mpsc::channel(N)` with explicit N |
| Connection-task does CPU-heavy work | Blocks the executor | Dispatch to shard via channel |
| Shard reads connection-layer state | Reverses the data flow | Connection asks; shard answers |

## Examples

### Golden — boundary message

```rust
// brain-server/src/connection/dispatch.rs (Tokio)
async fn dispatch(req: EncodeRequest, shard_tx: &Sender<ShardJob>) -> ... {
    let (resp_tx, resp_rx) = oneshot::channel();
    shard_tx.send(ShardJob::Encode { req, resp: resp_tx }).await?;
    resp_rx.await?
}

// brain-server/src/shard/loop.rs (Glommio)
fn shard_loop(state: ShardState, mut rx: glommio::channels::shared_channel::ConnectedReceiver<ShardJob>) {
    while let Some(job) = rx.recv().await {
        match job {
            ShardJob::Encode { req, resp } => {
                let r = handle_encode(&mut state, req);
                resp.send(r).ok();
            }
            ...
        }
    }
}
```

The boundary carries owned `EncodeRequest` (Send), not `&ShardState`. `state` lives in one task; `req` flows in; response flows out.

### Counter — per-shard handle escapes

```rust
struct Server {
    shards: Vec<Arc<Mutex<ShardState>>>,    // ← reject; per-shard data Mutex'd
}

async fn handle(server: Arc<Server>, req: EncodeRequest) {
    let mut shard = server.shards[0].lock().await;     // ← reject; locks shard from connection task
    shard.encode(req).await;
}
```

The shard's state has crossed back into a Tokio-typed handle. Single-writer is gone; the executor can be held up arbitrarily by lock contention. Replace with a channel-based dispatch.

## Cross-references

- `brain-glommio-rules` — what's allowed inside the shard.
- `rust-concurrency` — Send/Sync background.
- `brain-invariants` — single-writer-per-shard is invariant #2.

## Source / Adaptations

Project-local. Operationalizes CLAUDE.md §4.
