# Sub-task 10.7 — SDK observability

**Reads:**
- `spec/13_sdk_design/07_observability.md` §1, §2, §3, §6, §17.
- `crates/brain-server/src/admin/` — server-side metric naming
  convention (for consistency).
- Skim §4, §11-§13, §15 (metric surface) — most of these are
  v2 / `metrics` crate integrations that need a registry choice
  we haven't made yet.

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.7.

**Done when:** Every Client method emits a `tracing::Span` with
the operation name and OpenTelemetry-compatible attributes
(`brain.operation`, `brain.request_id`, `brain.agent_id`,
`brain.attempt`). Retries log at WARN; non-retryable errors at
ERROR. The Client lifecycle (connect / bye / pool acquire) logs
at DEBUG. Internal counters (request total, retry count,
in-flight) live on the Client and are pub-accessible via
`Client::metrics_snapshot()`. The surface is OTLP-friendly but
doesn't depend on the `opentelemetry` or `prometheus` crates —
those are application-level choices.

---

## 1. What 10.7 actually delivers

Spec §13/07 lays out a broad observability surface (logs,
metrics, traces, hooks, audit, circuit-breaker metrics, stream
metrics, debug snapshot, custom tags). 10.7 ships the
**minimum that matters for v1**:

- **Tracing spans** on every op + handshake + retry attempt,
  with OpenTelemetry-style attributes.
- **Internal counters** maintained by the Client — request
  total per op, retry total, in-flight gauge, last-error code.
- **`Client::metrics_snapshot()`** returns a copy of those
  counters as a typed struct. Applications can poll this and
  publish to whatever registry they use.

What's NOT in 10.7:

- A direct integration with `metrics` / `prometheus_client` /
  `opentelemetry-otlp` exporters. Those are application
  choices; the SDK gives the data, the app decides where it
  goes (spec §13/07 §3 / §5 / §14 land in later sub-tasks once
  we pick a registry shape).
- Per-request `.trace(true)` opt-in dump (spec §10) — debug
  feature; defer.
- Audit-log mode (spec §9) — compliance feature; defer.
- Hooks (spec §8) — extensibility; defer until a real user
  asks. (`tracing` events already serve the most common case.)
- Stream metrics (spec §13). The `FrameStream` is short-lived
  enough that span-per-stream covers the dominant case.
- Circuit-breaker metrics (spec §11) — we don't have a
  circuit breaker.
- Debug snapshot accessor (spec §16) — defer to 10.12's
  `brain-cli debug-snapshot` (Phase 10 CLI work).

---

## 2. Module layout (folder-per-concern)

```
crates/brain-sdk-rust/src/
├── observability/             NEW
│   ├── mod.rs                 re-exports
│   ├── attributes.rs          OTel attribute keys + helpers
│   └── metrics.rs             MetricsSnapshot + atomic counters
├── client/mod.rs              + tracing macros around each op call
└── ops/                       per-op `send()` wraps op body in
                               `tracing::info_span!` + records
                               retry attempt counts
```

LOC estimates: observability/mod.rs ~20, attributes.rs ~80,
metrics.rs ~150, client + ops sprinkles ~120.

---

## 3. Attribute key conventions

OpenTelemetry semantic convention names where they exist; SDK-
specific names prefixed `brain.*`. All defined as `pub const`
in `attributes.rs` so callers can match against them:

```rust
pub const OP: &str = "brain.operation";        // "encode" / "recall" / …
pub const REQUEST_ID: &str = "brain.request_id";
pub const AGENT_ID: &str = "brain.agent_id";
pub const ATTEMPT: &str = "brain.attempt";     // 1-indexed
pub const SERVER_ADDR: &str = "server.address"; // OTel convention
pub const ERROR_CODE: &str = "brain.error_code";
pub const ERROR_KIND: &str = "error.type";     // OTel
```

These mirror what the brain-server's tracing already emits, so
end-to-end traces stitch up cleanly (spec §13/07 §7).

---

## 4. Metric counters

`MetricsSnapshot` (returned by `Client::metrics_snapshot()`):

```rust
pub struct MetricsSnapshot {
    pub requests_total: u64,                 // sum across ops
    pub retries_total: u64,                  // count of attempts > 1
    pub errors_total: u64,                   // ops that returned Err
    pub by_op: BTreeMap<&'static str, OpMetrics>,
    pub connections_opened_total: u64,       // from Pool internals
    pub in_flight_gauge: u64,                // current in-flight ops
}

pub struct OpMetrics {
    pub requests_total: u64,
    pub errors_total: u64,
    pub retries_total: u64,
}
```

Backed by atomics on the Client (`Arc<MetricsState>`).
Cloned Clients share the state, so a snapshot from any clone
reflects the whole process's activity.

All counters are monotonically increasing; users compute deltas
client-side. Histograms / percentiles are NOT in 10.7 (deferred
to a real metrics-registry integration).

---

## 5. Tracing pattern (per op)

```rust
let span = tracing::info_span!(
    target: "brain_sdk_rust::ops",
    "brain.encode",
    {OP} = "encode",
    {REQUEST_ID} = %request_id,
    {AGENT_ID} = %agent_id,
);
let _enter = span.enter();
// ... run_op invokes the closure, which captures the span via
//     tracing::Instrument
```

Inside `Client::run_op`:
- Before each `op()` call, set `brain.attempt = N` on the span.
- After failures, emit `tracing::warn!` (retryable) or
  `tracing::error!` (non-retryable / exhausted) with the
  underlying error code.
- On success, emit `tracing::debug!("success")`.

`run_op` reads from `self.config.retry`; we already have the
attempts count. Adding the span requires minor refactoring to
thread the op name through.

---

## 6. Op-name plumbing

`Client::run_op` needs to know the op name for span naming.
Two options:

**A.** Pass `op_name: &'static str` as the first arg:
```rust
client.run_op("encode", || async { ... }).await
```

**B.** Push spans from each builder's `send()` and let `run_op`
inherit them:
```rust
async {
    let _span = info_span!("brain.encode", …).entered();
    client.run_op(|| async { ... }).await
}
```

Option B keeps `run_op` signature stable; each builder controls
its own span. Going with B.

In `run_op` we still log retry attempts via `tracing::warn!` —
those log lines will inherit the active span.

---

## 7. Tests

### 7.1 Unit (`observability/metrics.rs::tests`)
- Snapshot reflects zero state after `MetricsState::default()`.
- `MetricsState::record_request("encode")` increments
  `by_op["encode"].requests_total` and the global counter.
- `record_retry()` and `record_error()` similar.
- Clone shares state (mutation through one clone is visible in
  the other).

### 7.2 Integration (`tests/observability.rs`)
- Run a single ENCODE op against the mock server.
- Snapshot before / after; assert `requests_total == 1`,
  `by_op["encode"].requests_total == 1`.
- Run a retry scenario (server returns ERROR Overloaded once,
  then EncodeResp). Snapshot: `retries_total == 1`,
  `requests_total == 1`, no errors.
- (Tracing spans aren't easily test-able without a custom
  tracing subscriber; rely on the contract: doc says spans are
  emitted, code emits them. A future test sub-task can wire
  `tracing-test` to assert.)

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Atomic counters add overhead per op | `AtomicU64::fetch_add(_, Relaxed)` is ~2 ns; negligible vs the op's network latency. |
| Per-op span allocation has cost when `tracing::Level::INFO` is disabled | `tracing`'s spans are cheap when the subscriber filters them out (zero-cost in the disabled path). Verified by tracing's design. |
| Adding `metrics` field to `Client` breaks `Clone` semantics | We wrap it in `Arc<MetricsState>` like the other shared state — clones already share an Arc per `JitterSource` / `RequestIdSource`. |
| Op-name strings must be stable | Pin them as `pub const OP_ENCODE: &str = "encode"` etc. in `observability::attributes`. |
| BTreeMap<&'static str, OpMetrics> isn't atomically updated | Inserted lazily on first record; subsequent updates use the existing entry. Use `parking_lot::Mutex<BTreeMap<...>>` for the slot map; per-op counters are AtomicU64 (lock-free reads/writes). |

---

## 9. Done criteria

- [ ] `src/observability/` folder with `mod.rs`, `attributes.rs`,
  `metrics.rs`.
- [ ] `MetricsSnapshot` returned by `Client::metrics_snapshot()`.
- [ ] All 11 Client op methods + handshake + bye emit tracing
  spans.
- [ ] `Client::run_op` records retry attempts via
  `tracing::warn!` and updates metrics state.
- [ ] ERROR-frame ops log at `tracing::error!`.
- [ ] 4+ new unit tests in `observability/metrics.rs::tests`.
- [ ] 1+ integration test exercising metric snapshots before /
  after a real-ish op + retry scenario.
- [ ] All 48 pre-10.7 tests still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.7 marked `[x]` in the phase doc.

---

## 10. What 10.7 explicitly defers

- `metrics` / `prometheus_client` / `opentelemetry` direct
  integrations — application-level choice; defer until a
  user-facing reason arrives.
- Per-request `.trace(true)` opt-in dump (spec §10).
- Audit-log mode (spec §9).
- Hooks (`on_request`, `on_response`, `on_error`) (spec §8).
- Stream metrics (spec §13).
- Circuit breaker (spec §18 + §11).
- `client.debug_snapshot()` accessor (spec §16) — 10.12's
  CLI work has the production-shaped version.
- Custom default tags (spec §14) — user code can wrap spans.
- Percentile histograms (spec §4 / §15).

---

*Implement on approval.*
