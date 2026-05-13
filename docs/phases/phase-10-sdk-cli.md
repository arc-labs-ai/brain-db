# Phase 10 — Rust SDK & CLI

## Goal

A polished Rust SDK (`brain-sdk-rust`) and the admin CLI (`brain-cli`). After this phase, application developers can `use brain_sdk::Client` to drive every operation; operators can use `brain-cli` to manage the substrate.

Other-language SDKs (Python, TypeScript, Go) are deferred to v1.x.

## Prerequisites

- [x] Phase 9 complete (server is runnable).

## Reading list

1. [`spec/13_sdk_design/00_purpose.md`](../../spec/13_sdk_design/00_purpose.md)
2. [`spec/13_sdk_design/01_principles.md`](../../spec/13_sdk_design/01_principles.md)
3. [`spec/13_sdk_design/02_core_api.md`](../../spec/13_sdk_design/02_core_api.md)
4. [`spec/13_sdk_design/03_connection.md`](../../spec/13_sdk_design/03_connection.md)
5. [`spec/13_sdk_design/04_retries.md`](../../spec/13_sdk_design/04_retries.md)
6. [`spec/13_sdk_design/04_retries.md`](../../spec/13_sdk_design/04_retries.md)
7. [`spec/13_sdk_design/05_streams.md`](../../spec/13_sdk_design/05_streams.md)
8. [`spec/13_sdk_design/07_observability.md`](../../spec/13_sdk_design/07_observability.md)
9. [`spec/14_observability_ops/06_admin_ops.md`](../../spec/14_observability_ops/06_admin_ops.md) — CLI surface.

## Outputs

- `crates/brain-sdk-rust` exports `Client` with all operations.
- `crates/brain-cli` implements every spec'd admin command.
- Tag: `phase-10-complete`.

## Sub-tasks

### Task 10.1 — `Client` skeleton  [x]
**Reads:** `spec/13_sdk_design/02_core_api.md`, `03_connection.md`,
  `spec/03_wire_protocol/06_handshake.md`. Plan
  `.claude/plans/phase-10-task-01.md`.
**Writes:** `crates/brain-sdk-rust/src/{client,config,error,proto}/`
  (folder-per-concern; only `lib.rs` at src root). Integration
  test `tests/handshake.rs` uses a hand-rolled mock server (no
  cross-crate dep on brain-server).
**Done when:** `Client::connect(addr).await?` opens TCP, drives
  spec §03/06 handshake (HELLO → WELCOME → AUTH → AUTH_OK), and
  returns a usable client. `Client::bye(self)` performs the
  spec §03/05 §1.1 echo-and-close. 8/8 tests pass (6 unit +
  2 integration); docker-verify green.

### Task 10.2 — Connection pool  [x]
**Reads:** `spec/13_sdk_design/03_connection.md` §1, §2, §4, §5,
  §13, §14. Plan `.claude/plans/phase-10-task-02.md`.
**Writes:** `crates/brain-sdk-rust/src/pool/`
  (`mod.rs` Pool + acquire + reaper, `connection.rs` extracted
  from `client/mod.rs`, `config.rs` PoolConfig, `guard.rs` RAII
  PoolGuard). `client/mod.rs` reshaped as a thin `Arc<Pool>`
  wrapper preserving 10.1's `connect/bye` surface.
**Done when:** Pool keeps `min..=max` connections per server,
  reaps idle past `idle_timeout`, exposes `warm_up()`, returns
  `ClientError::Overloaded` once `acquire_timeout` fires at cap,
  and `ClientError::PoolClosed` after `close()`. 18/18 tests
  pass: 9 unit (config, error mapping, stream-id allocator,
  pool defaults) + 2 handshake + 7 pool (warm_up, idle-reuse,
  blocks-then-succeeds, Overloaded, reaper, close, 10.1 compat).
  docker-verify green.

### Task 10.3 — Retry with exponential backoff + jitter
**Reads:** `spec/13_sdk_design/04_retries.md`
**Writes:** `crates/brain-sdk-rust/src/retry.rs`
**Done when:** Retries on `Overloaded` / network errors with exponential backoff capped at spec max; jitter applied; max attempts respected.

### Task 10.4 — Auto-generated UUIDv7 RequestIds
**Reads:** `spec/13_sdk_design/04_retries.md`
**Writes:** integrated into `Client`
**Done when:** Every write op gets a fresh RequestId by default; user can override per-call.

### Task 10.5 — All op methods on `Client`
**Reads:** `spec/13_sdk_design/02_core_api.md`
**Writes:** `crates/brain-sdk-rust/src/ops.rs`
**Done when:** `client.encode(...)`, `recall(...)`, `plan(...)`, `reason(...)`, `forget(...)`, `link(...)`, `unlink(...)`, `txn(...)`, `subscribe(...)` all work.

### Task 10.6 — Streaming via async iterators
**Reads:** `spec/13_sdk_design/05_streams.md`
**Writes:** `crates/brain-sdk-rust/src/stream.rs`
**Done when:** `subscribe(...)` returns `impl Stream<Item = Memory>`; backpressure works.

### Task 10.7 — SDK observability
**Reads:** `spec/13_sdk_design/07_observability.md`
**Writes:** `crates/brain-sdk-rust/src/tracing.rs`
**Done when:** `tracing` spans on every call; OpenTelemetry-compatible attributes.

### Task 10.8 — `brain-cli stats` and `health`
**Reads:** `spec/14_observability_ops/06_admin_ops.md`
**Writes:** `crates/brain-cli/src/stats.rs`
**Done when:** `brain-cli stats` and `health` output JSON or human-readable.

### Task 10.9 — `brain-cli snapshot` family
**Writes:** `crates/brain-cli/src/snapshot.rs`
**Done when:** `snapshot create/list/restore/delete` all work end-to-end.

### Task 10.10 — `brain-cli rebuild-ann`
**Writes:** `crates/brain-cli/src/rebuild.rs`
**Done when:** Triggers an immediate rebuild via admin API; reports progress.

### Task 10.11 — `brain-cli worker`, `config`, `audit`, `agent`, `shard`
**Writes:** `crates/brain-cli/src/{worker,config,audit,agent,shard}.rs`
**Done when:** All spec'd subcommands work. (Stubs from Phase 0 are now real.)

### Task 10.12 — `brain-cli profile`, `debug-snapshot`
**Writes:** `crates/brain-cli/src/diagnostics.rs`
**Done when:** Profile capture works (pprof format); debug snapshot writes JSON.

### Task 10.13 — SDK + CLI integration tests
**Writes:** `tests/cli_e2e.rs` and `tests/sdk_e2e.rs` (workspace-level fixture project)
**Done when:** Test harness spins up server, drives via SDK + CLI, asserts outputs.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] SDK can drive every operation per spec.
- [ ] CLI covers every command in spec §14/06.
- [ ] Tag `phase-10-complete`.
