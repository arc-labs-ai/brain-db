# Phase 9 — `brain-server`: End-to-End Wire-Up

## Goal

A runnable substrate. TCP connection layer (Tokio) accepts clients; per-shard Glommio executors handle requests; cross-shard routing works; graceful shutdown is clean. After this phase, you can `cargo run --bin brain-server`, point a client at it, and exercise every operation end-to-end.

## Prerequisites

- [x] Phase 8 complete.

## Reading list

1. [`spec/01_system_architecture/00_purpose.md`](../../spec/01_system_architecture/00_purpose.md)
2. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
3. [`spec/01_system_architecture/03_primitives.md`](../../spec/01_system_architecture/03_primitives.md)
4. [`spec/01_system_architecture/05_hardware.md`](../../spec/01_system_architecture/05_hardware.md)
5. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
6. [`spec/01_system_architecture/04_layers.md`](../../spec/01_system_architecture/04_layers.md)
7. [`spec/12_sharding_clustering/00_purpose.md`](../../spec/12_sharding_clustering/00_purpose.md)
8. [`spec/12_sharding_clustering/01_shard_model.md`](../../spec/12_sharding_clustering/01_shard_model.md)
9. [`spec/12_sharding_clustering/02_routing.md`](../../spec/12_sharding_clustering/02_routing.md)
10. [`spec/10_concurrency_epochs/00_purpose.md`](../../spec/10_concurrency_epochs/00_purpose.md)
11. [`spec/10_concurrency_epochs/02_single_writer.md`](../../spec/10_concurrency_epochs/02_single_writer.md)
12. [`spec/10_concurrency_epochs/05_arc_swap.md`](../../spec/10_concurrency_epochs/05_arc_swap.md)
13. [`spec/10_concurrency_epochs/06_crossbeam_epoch.md`](../../spec/10_concurrency_epochs/06_crossbeam_epoch.md)

## Outputs

- `crates/brain-server` is a fully working binary.
- Config TOML loaded; multi-shard topology set up.
- Tokio connection layer + Glommio shard executors.
- Cross-shard routing via BLAKE3(agent_id) → shard.
- Graceful shutdown.
- Tag: `phase-9-complete`.

## Sub-tasks

### Task 9.1 — Config loading  [x]
**Reads:** `config/dev.toml`
**Writes:** `crates/brain-server/src/config.rs`
**Done when:** Config struct deserializes from TOML; env var overrides supported; missing required fields produce clear errors.

### Task 9.2 — Shard executor (Glommio)
**Reads:** `spec/01_system_architecture/05_hardware.md`
**Writes:** `crates/brain-server/src/shard.rs`
**What to build:**
- One `LocalExecutor` per shard, pinned to a CPU.
- The shard owns its `Arena`, `Wal`, `MetadataDb`, `HnswIndex`.
- Channels (mpsc) accept incoming requests from the connection layer.

### Task 9.3 — Connection layer (Tokio)
**Reads:** `spec/01_system_architecture/04_layers.md`
**Writes:** `crates/brain-server/src/connection.rs`
**What to build:**
- TCP listener (configurable port).
- Optional TLS via `rustls`.
- Per-connection task: read frames from socket, dispatch to shard, send responses back.

### Task 9.4 — Frame dispatcher
**Reads:** `spec/01_system_architecture/04_layers.md`
**Writes:** `crates/brain-server/src/dispatch.rs`
**Done when:** Frame → opcode → shard (via routing) → handler → response. Errors mapped to wire error frames.

### Task 9.5 — Cross-shard routing
**Reads:** `spec/12_sharding_clustering/02_routing.md`
**Writes:** `crates/brain-server/src/routing.rs`
**What to build:**
- `agent_id_to_shard(agent_id, num_shards) -> ShardId` via BLAKE3.
- `MemoryId.shard()` shortcuts routing for ops that already have a memory ID.

### Task 9.6 — `ArcSwap` shared state
**Reads:** `spec/10_concurrency_epochs/05_arc_swap.md`
**Writes:** `crates/brain-server/src/state.rs`
**Done when:** HNSW index and other read-mostly state is published via ArcSwap; readers don't block on writer.

### Task 9.7 — `crossbeam-epoch` for deferred reclamation
**Reads:** `spec/10_concurrency_epochs/06_crossbeam_epoch.md`
**Writes:** integrated into storage/index modules
**Done when:** Memory freed in writer is safely reclaimed only after readers done. No use-after-free in stress tests.

### Task 9.8 — Health and metrics endpoints
**Reads:** `spec/14_observability_ops/01_metrics.md`
**Writes:** `crates/brain-server/src/admin.rs`
**Done when:** Separate HTTP listener on metrics port serves `/healthz` and `/metrics` (Prometheus format, even if mostly stubs at this stage).

### Task 9.9 — Graceful shutdown
**Reads:** `spec/01_system_architecture/04_layers.md` (if present)
**Writes:** `crates/brain-server/src/shutdown.rs`
**Done when:** SIGINT/SIGTERM triggers: stop accepting; drain in-flight; flush WAL; close storage; exit clean.

### Task 9.10 — End-to-end smoke test
**Writes:** `crates/brain-server/tests/e2e.rs`
**What to build:**
- Test spins up the server in-process (or via subprocess).
- Uses `brain-sdk-rust` to drive: encode → recall → forget → recall.
- Verifies expected results.
**Done when:** Smoke passes reliably.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] `cargo run --bin brain-server` accepts a connection from a sample client.
- [ ] E2E smoke test passes 100 iterations.
- [ ] `just run-server` boots in < 5 seconds with empty data.
- [ ] Tag `phase-9-complete`.
