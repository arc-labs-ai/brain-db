# Sub-task 10.3 — Retry with exponential backoff + jitter

**Reads:**
- `spec/13_sdk_design/04_retries.md` §1, §2, §5, §6, §10, §13.
- Re-skim §3 (idempotency — needed for 10.4, not here),
  §9 (retry-after — depends on server's ERROR shape; defer),
  §17 (history — defer to 10.7).

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.3.

**Done when:** A `RetryConfig` + a generic
`retry_with_backoff(op, config) -> Result<T, ClientError>` helper
implement the spec §6 algorithm. Retries fire on retryable
errors (`is_retryable() == true`), respect `max_attempts`, sleep
between attempts with exponential backoff + ±10% jitter capped
at `max_delay`. `Client` exposes the helper internally so 10.5's
op methods get retries for free. The retry-exhausted error
preserves the last underlying error and attempt count.

---

## 1. What 10.3 delivers

The pieces needed to make 10.5's op methods correct from day one:

1. **`RetryConfig`** under `src/retry/`:
   - `max_attempts: u32` (default 3, spec §6)
   - `initial_delay: Duration` (default 100 ms)
   - `backoff_factor: f64` (default 2.0)
   - `max_delay: Duration` (default 30 s)
   - `jitter: f64` (default 0.1 — ±10 %)
   - `total_timeout: Option<Duration>` (default 60 s, spec §13).
     `None` disables the total-timeout check.
   - `RetryConfig::none()` — single-attempt fail-fast preset.

2. **`retry_with_backoff(op, config)`** — the generic loop in
   `retry/runner.rs`:
   - Calls `op()`. On `Ok(v)`, return.
   - On `Err(e)` where `!e.is_retryable() || attempt >= max`,
     return `Err(ClientError::RetryExhausted{...})` if any
     retries happened, else the raw error.
   - Sleeps `compute_delay(attempt, &config)` before the next
     attempt.
   - Tracks `total_started` and aborts with `RetryExhausted` if
     the total budget is exhausted (spec §13).

3. **`compute_delay`** — pure helper, easy to unit-test:
   - `base = initial_delay * backoff_factor.pow(attempt - 1)`
   - `jittered = base * (1.0 ± jitter)` using a deterministic PRNG
     injected for tests (we use `rand` for the production seed).
   - `min(jittered, max_delay)`.

4. **`ClientError::RetryExhausted`** variant:
   - `last_error: Box<ClientError>`
   - `attempts: u32`
   - `total_duration: Duration`
   - `is_retryable() = false` (user already saw the SDK exhaust).
   - `code()` delegates to `last_error.code()`.

5. **`ClientConfig.retry: RetryConfig`** replaces the
   placeholder `retries` + `backoff_initial` fields. Builder
   gets `with_retry(RetryConfig)`. `ClientConfig::default()`
   still produces the same observable timeout (30 s per attempt)
   so 10.1 / 10.2 tests pass unchanged.

6. **`Client` plumbing**:
   - Adds a `pub(crate) async fn run_op<F, T>(&self, op: F) ->
     Result<T, ClientError>` where `F: Fn() -> impl Future<Output = Result<T, ClientError>>`.
     Wraps `op` with the retry runner using `self.config.retry`.
   - 10.5's op methods will call this; 10.3 doesn't yet have
     any op methods, so the only consumer is a unit test that
     hands it a fake fallible closure.

What 10.3 does NOT deliver:

- Per-operation retry overrides (`encode().retry_config(...)`) —
  builder pattern lands with the op methods in 10.5.
- Idempotency-key generation (10.4).
- `retry_after` honoring (spec §9) — server-side ERROR shape
  doesn't expose it yet; defer until 11.x.
- Retry history accessor (`last_request_history`) — defer to
  10.7 observability.
- "Fail fast" client-wide flag (§16) — express via
  `RetryConfig::none()` instead.
- Distributed tracing spans on each attempt — 10.7.
- Per-attempt timeout (§13) — already exists as
  `ClientConfig::timeout`; 10.3 keeps the field, applies it via
  `tokio::time::timeout` around each `op()` call.

---

## 2. Module layout

```
crates/brain-sdk-rust/src/
├── lib.rs                 (+ pub mod retry; + re-exports)
├── client/mod.rs          (+ run_op helper)
├── config/mod.rs          (replaces `retries` / `backoff_initial`
│                           with `retry: RetryConfig`)
├── error/mod.rs           (+ RetryExhausted variant)
├── retry/                 NEW
│   ├── mod.rs             pub mod config; pub mod runner; re-exports
│   ├── config.rs          RetryConfig + presets + defaults
│   └── runner.rs          retry_with_backoff + compute_delay + jitter
├── pool/                  (unchanged)
└── proto/                 (unchanged)
```

LOC estimates: retry/runner.rs ~150, retry/config.rs ~80,
error/mod.rs +30, client/mod.rs +40, config/mod.rs +20.

---

## 3. Tests

### 3.1 Unit (`retry/config.rs`)
- `RetryConfig::default()` matches spec §6.
- `RetryConfig::none()` → max_attempts = 1.
- builder methods propagate.

### 3.2 Unit (`retry/runner.rs::compute_delay`)
- Exponential progression: with `initial=100ms factor=2`, attempt
  1 / 2 / 3 gives base 100ms / 200ms / 400ms (validate before
  jitter via a seeded RNG fixture).
- Jitter spread: 100 samples land within ±10%.
- Cap at `max_delay`: after enough attempts, delay is exactly
  `max_delay`.

### 3.3 Unit (`retry/runner.rs::retry_with_backoff`)
- Returns immediately on first-attempt `Ok`.
- Non-retryable error short-circuits — no sleep, no retry, the
  original error is returned (not `RetryExhausted`).
- Retryable error retries up to `max_attempts`, then returns
  `RetryExhausted { last_error, attempts, total_duration }`.
- Succeeds on second attempt — returns the `Ok` value, no
  RetryExhausted wrapper.
- Total-timeout exhaustion: with `total_timeout = 50ms` and a
  fast loop, runner aborts with `RetryExhausted` containing the
  last-seen error.

### 3.4 Integration (`tests/retry.rs`)
- Mock server that closes the connection mid-handshake on the
  first attempt and succeeds on the second. Drive via
  `Client::new_lazy + run_op` with a fake operation that opens
  a pool guard. Assert retry kicks in and the op succeeds.
- (We can hold this test until 10.5 lands real op methods; the
  unit tests in §3.3 already cover the runner's contract.)

---

## 4. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `rand` is a new dep | Already in the workspace dep set (used by brain-workers/scheduler jitter). Add `rand = { workspace = true }` to brain-sdk-rust. |
| Determinism in tests | The runner takes a `Box<dyn FnMut(...) -> f64>` for the jitter source so tests can inject a fixed value. Production wires `rand::random::<f64>()`. |
| Sleeps slow the test suite | All tests use `initial_delay = Duration::from_millis(1)` and `max_delay = Duration::from_millis(20)`. Total suite stays under 1 s. |
| `RetryConfig.total_timeout = Some(60s)` will keep a hung op alive — but the per-attempt timeout (30s) bounds each attempt | Document the relationship in the field doc. 10.5 will wire `tokio::time::timeout(self.config.timeout, op())` around each attempt. |
| Breaking change to `ClientConfig` (replaces `retries`/`backoff_initial` with `retry`) | Internal — 10.1/10.2 didn't expose the old fields in any test assertion. We renamed-in-place rather than keep the old fields as deprecated aliases (the SDK is pre-1.0). |
| The op closure must be re-runnable (`Fn`, not `FnOnce`) | Builder pattern in 10.5 will hand a freshly-built request per attempt. `retry_with_backoff`'s signature: `op: FnMut() -> Pin<Box<dyn Future<...>>>`. Documented. |

---

## 5. Done criteria

- [ ] `src/retry/` folder with `mod.rs`, `config.rs`, `runner.rs`.
- [ ] `ClientError::RetryExhausted` added; `is_retryable() == false`
  for it; `code()` delegates.
- [ ] `ClientConfig.retry: RetryConfig` replaces the placeholder
  fields. `Default` still produces spec defaults.
- [ ] `Client::run_op` (internal; `pub(crate)`) wraps an async op
  with `retry_with_backoff(...)`.
- [ ] All §3 unit tests pass. Pre-existing 18/18 from 10.1/10.2
  still pass.
- [ ] `just docker-verify` green.
- [ ] Sub-task 10.3 marked `[x]` in `docs/phases/phase-10-sdk-cli.md`.

---

## 6. What 10.3 explicitly defers

- Per-op `retry_config()` builder — lands with 10.5's op methods.
- Idempotency-key handling (`RequestId` generation) — 10.4.
- `retry_after` from server ERROR — needs server-side support;
  v2.
- Retry history accessor (§17) — 10.7.
- Tracing spans on each attempt — 10.7.
- "Fail fast" client-wide flag (§16) — express via
  `RetryConfig::none()`.
- Distributed-trace propagation — 10.7.

---

*Implement on approval.*
