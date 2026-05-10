# 13.04 Retries

Retry policy and idempotency in the SDK.

## 1. The retry decision

For each operation, the SDK decides:

- Is this error retryable?
- Has the retry budget been exhausted?
- Should we wait before retrying?

```rust
fn should_retry(err: &BrainError, attempt: u32, config: &RetryConfig) -> bool {
    err.is_retryable() && attempt < config.max_attempts
}

fn retry_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let base = config.initial_delay;
    let factor = config.backoff_factor.pow(attempt - 1);
    let with_jitter = base * factor * jitter();
    with_jitter.min(config.max_delay)
}
```

## 2. Retryable errors

Per [09. Cognitive Operations](../09_cognitive_operations/) §error model:

| Error code | Retryable |
|---|---|
| InvalidRequest | No |
| NotFound | No |
| Unauthorized | No |
| QuotaExceeded | No |
| Conflict (idempotency mismatch) | No |
| Overloaded | Yes |
| Timeout | Yes |
| NetworkError | Yes |
| InternalError | Yes (carefully) |
| EmbedderUnavailable | Yes |

The substrate's responses include the `retryable` flag explicitly. The SDK respects it.

## 3. Idempotency

State-mutating operations require idempotency:

- ENCODE
- FORGET
- LINK / UNLINK
- TXN_COMMIT

Each of these requires a `RequestId`. If not provided, the SDK generates one:

```rust
let request_id = RequestId::generate();    // UUIDv7
client.encode("text").request_id(request_id).send().await?;
```

The auto-generated RequestId is stable for the lifetime of the operation — retries use the same RequestId. The substrate deduplicates.

## 4. Read operations and retries

Read operations (RECALL, PLAN, REASON, ADMIN_STATS) are idempotent by nature:

- The substrate doesn't change state when serving them.
- A retry just re-runs the read.
- No RequestId needed.

Retries on reads are simpler — just resend.

## 5. The retry loop

```rust
async fn execute_with_retry(
    op: impl Fn() -> impl Future<Output = Result<R, BrainError>>,
    config: &RetryConfig,
) -> Result<R, BrainError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(r) => return Ok(r),
            Err(e) if !should_retry(&e, attempt, config) => return Err(e),
            Err(e) => {
                let delay = retry_delay(attempt, config);
                tracing::warn!("Retry attempt {}, error: {}", attempt, e);
                tokio::time::sleep(delay).await;
            }
        }
    }
}
```

The SDK wraps each operation with this retry logic.

## 6. Exponential backoff with jitter

Default config:

```
max_attempts = 3
initial_delay = 100ms
backoff_factor = 2.0
max_delay = 30s
jitter = 0.1 (±10%)
```

So delays: 100ms, 200ms, 400ms (with jitter).

Jitter prevents synchronized retries from many clients ("thundering herd").

## 7. The retry budget

Beyond `max_attempts`, the SDK gives up. The error is returned to the caller.

The caller can:
- Implement application-level retry on top.
- Treat the failure as terminal.

The SDK's retries are first-line defense; not unlimited.

## 8. Per-operation retry config

Different operations may want different retry configs:

```rust
client.encode("text")
    .retry_config(RetryConfig::aggressive())    // More retries for important data
    .send().await?;

client.recall("cue")
    .retry_config(RetryConfig::fast_fail())      // Don't retry; user can re-cue
    .send().await?;
```

Defaults are conservative; per-op overrides for special cases.

## 9. The "retry-after" header

If the substrate's Overloaded response includes a "retry after" duration, the SDK respects it:

```rust
match err {
    Error::Overloaded { retry_after: Some(d) } => sleep(d).await,
    _ => sleep(retry_delay(attempt, config)).await,
}
```

This lets the substrate signal "I'll be ready in 5 seconds; back off until then".

## 10. The "retry exhausted" error

When all retries fail, the SDK returns the last error with retry context:

```rust
Err(BrainError::RetryExhausted {
    last_error: Box::new(actual_err),
    attempts: 3,
    total_duration: Duration::from_millis(700),
})
```

The caller knows that retries were attempted. They can choose to retry further or treat as failure.

## 11. The "no retry" option

For applications that prefer manual retry control:

```rust
let client = Client::builder()
    .retry_config(RetryConfig::none())    // No automatic retries
    .build();
```

Or per-operation:

```rust
client.encode("text").no_retry().send().await?;
```

The application is responsible for retries. Useful for systems that already have retry logic.

## 12. Retries and tracing

Each retry is logged / traced:

```
encode op started [request_id=abc]
  attempt 1: NetworkError after 50ms
  attempt 2: succeeded after 120ms
encode op completed [duration=170ms, attempts=2]
```

This visibility helps debugging.

## 13. Retries and timeouts

There are two timeouts:

- **Per-attempt timeout** (default 30s): each individual request can take up to this long.
- **Total timeout** (default 60s): retries plus delays must finish within this.

If total exceeds, the SDK gives up regardless of attempts.

```rust
client.encode("text")
    .per_attempt_timeout(Duration::from_secs(10))
    .total_timeout(Duration::from_secs(60))
    .send().await?;
```

## 14. The "first attempt's error is the user's error" rule

If the first attempt fails with InvalidRequest (non-retryable), the SDK doesn't retry — the error is returned. The user fixes the request.

If it fails with Overloaded, retries kick in. The user shouldn't see Overloaded unless retries fail.

The SDK handles transient errors transparently; only persistent errors surface to the user.

## 15. Retries and side effects

For idempotent operations, retries are safe.

For non-idempotent operations (rare in Brain — the substrate enforces idempotency for state-mutating operations), retries may cause duplication.

The SDK's RequestId mechanism makes all state-mutating operations idempotent. So retries are always safe for our operations.

## 16. The "fail fast" mode

For low-latency applications, retries add latency. An option:

```rust
let client = Client::builder()
    .fail_fast(true)
    .build();
```

In fail-fast mode:
- Single attempt.
- No retries.
- Immediate failure on any error.

The application implements its own retry strategy at a higher layer.

## 17. The retry history

For debugging, the SDK can record per-request retry history:

```rust
let result = client.encode("text").send().await;
let history = client.last_request_history();    // e.g., ["NetworkError", "Success"]
```

Useful in tests and during development.

## 18. The retries-and-timeout interaction

If the per-attempt timeout fires during a retry attempt:

- The attempt is canceled.
- The error is `Timeout`.
- If retryable (yes for Timeout), retry continues until exhausted.

If the total timeout fires:

- The whole operation is canceled.
- The error is `Timeout` (with "total" context).
- No more retries.

The two timeouts interact: per-attempt for individual calls, total for the whole operation.

---

*Continue to [`05_streams.md`](05_streams.md) for streaming.*
