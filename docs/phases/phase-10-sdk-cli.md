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

### Task 10.3 — Retry with exponential backoff + jitter  [x]
**Reads:** `spec/13_sdk_design/04_retries.md` §1, §2, §5, §6, §10,
  §13. Plan `.claude/plans/phase-10-task-03.md`.
**Writes:** `crates/brain-sdk-rust/src/retry/`
  (`mod.rs`, `config.rs` RetryConfig + presets, `runner.rs`
  retry_with_backoff + compute_delay + LCG-based JitterSource).
  `ClientConfig.retry: RetryConfig` replaces the 10.1 placeholder
  fields. `ClientError::RetryExhausted` variant added.
  `Client::run_op` (`pub(crate)`) wraps any async op through the
  policy — 10.5 will use it on every op method.
**Done when:** Exponential backoff with ±10% jitter respects
  spec §6 defaults (max=3, initial=100ms, factor=2.0, cap=30s);
  total_timeout aborts the loop early per spec §13;
  non-retryable errors short-circuit; first-attempt successes
  bypass the retry path. 31/31 tests pass (22 lib unit + 9
  integration). docker-verify green.

### Task 10.4 — Auto-generated UUIDv7 RequestIds  [x]
**Reads:** `spec/13_sdk_design/04_retries.md` §3, §15.
  Plan `.claude/plans/phase-10-task-04.md`.
**Writes:** `crates/brain-sdk-rust/src/request_id/mod.rs` —
  `RequestIdSource` trait + `DefaultRequestIdSource` (production,
  wraps `RequestId::new()` = UUIDv7) + `FixedRequestIdSource`
  (test-only canned sequence). `Client` carries
  `Arc<dyn RequestIdSource>` and exposes `Client::next_request_id()`.
  `brain_core::RequestId` re-exported from the SDK root.
**Done when:** Per-call ids are fresh UUIDv7s; cloned `Client`s
  share the same source so concurrent ops see distinct ids; the
  retry-reuses-same-id contract is documented for 10.5. 36/36
  tests pass (27 lib unit + 9 integration). docker-verify green.

### Task 10.5 — All op methods on `Client`  [x]
**Reads:** `spec/13_sdk_design/02_core_api.md` §3-§11. Plan
  `.claude/plans/phase-10-task-05.md`.
**Writes:** `crates/brain-sdk-rust/src/ops/` (folder-per-concern:
  `common.rs` + 9 op files: `encode/recall/plan/reason/forget/`
  `link/unlink/subscribe/txn.rs`). `Client` gains 11 methods
  (encode, recall, plan, reason, forget, link, unlink,
  subscribe, txn_begin, txn_commit, txn_abort). Shared mock
  fixture in `tests/common/mod.rs`.
**Done when:** Every op method exists, builds a typed request,
  goes through `Client::run_op` (retries on retryable errors
  with the same auto-minted `RequestId`), reads the response,
  and returns the typed result. Streaming ops collect into
  `Vec<T>` for now — 10.6 adds the async-iterator surface.
  ERROR-frame mapping wired (`ClientError::Server`). All op
  tests pass; pre-10.5 tests still pass. docker-verify green.

  Deferred: ENCODE_VECTOR_DIRECT, async-iterator streaming
  (10.6), nested `txn.encode(...)` sugar, FORGET batch/filter,
  ADMIN ops (10.8+), per-op retry overrides, cancellation
  tokens, `retry_after` honoring.

### Task 10.6 — Streaming via async iterators  [x]
**Reads:** `spec/13_sdk_design/05_streams.md` §1-§3, §5, §10-§12.
  Plan `.claude/plans/phase-10-task-06.md`.
**Writes:** `crates/brain-sdk-rust/src/ops/stream.rs` —
  generic `FrameStream<T>` impls `futures_lite::Stream`; owns
  the `PoolGuard` for lifetime so back-pressure is
  demand-driven (one socket read per `.next()` poll).
  `RecallBuilder`, `PlanBuilder`, `ReasonBuilder`,
  `SubscribeBuilder` gain `.send_stream() -> FrameStream<…>`
  alongside the 10.5 `.send()` / `.collect()` forms.
**Done when:** `subscribe().send_stream()` and the three
  cognitive streamers yield items one-at-a-time, drop releases
  the connection, ERROR frames surface via the stream as
  `Some(Err(ClientError::Server))`, EOS terminates the stream
  with `Ready(None)`. 48/48 tests pass (27 lib unit + 21
  integration including new `ops_recall_stream.rs` and
  `ops_subscribe_stream.rs`). docker-verify green.
  Deferred: reconnect/resume (11.x), keep-alive on streams
  (server-side prerequisite), stream metrics (10.7), multi-
  shard fan-out (v2), `STREAM_CLOSE` on drop (the SDK
  drop-and-pool path is best-effort).

### Task 10.7 — SDK observability  [x]
**Reads:** `spec/13_sdk_design/07_observability.md` §1, §2, §3,
  §6, §17. Plan `.claude/plans/phase-10-task-07.md`.
**Writes:** `crates/brain-sdk-rust/src/observability/`
  (folder-per-concern: `mod.rs`, `attributes.rs` OTel-style
  attribute keys, `metrics.rs` `MetricsState` + atomic counters
  + `MetricsSnapshot`). `Client` gains an
  `Arc<MetricsState>` field, exposes `metrics_snapshot()`. The
  `run_op` helper takes an op_name parameter, records per-op
  request totals, retries, errors, and the in-flight gauge,
  and emits `tracing::warn!` on retries / `tracing::error!`
  on terminal failures.
**Done when:** `Client::metrics_snapshot()` returns a
  point-in-time view of the counters; cloned clients share
  state. Spans + retry/error tracing emit. Direct
  `prometheus_client` / OTLP integrations stay deferred to
  application choice. 50/50 tests pass (32 lib unit + 18
  integration). docker-verify green.
  Deferred: per-request `.trace(true)` opt-in dump, audit-log
  mode, hooks, stream metrics, circuit-breaker metrics,
  `client.debug_snapshot()` (10.12), custom default tags,
  histogram/percentile surfaces.

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
