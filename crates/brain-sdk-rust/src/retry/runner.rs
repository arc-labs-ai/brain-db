//! Retry runner: drives an async op through the
//! [`RetryConfig`] policy.
//!
//! The runner is generic over the op closure and the jitter
//! source. Production wires the jitter to a thread-local LCG
//! PRNG seeded from system time; tests inject a fixed value so
//! delay assertions are deterministic.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::ClientError;
use crate::retry::config::RetryConfig;

/// Compute the delay to sleep before attempt `attempt` (1-indexed,
/// where attempt=1 is the *first* call — but we only ever ask for
/// `attempt >= 2` since there's no delay before the first call).
///
/// Pure: takes a `jitter_factor` in `[1.0 - jitter, 1.0 + jitter]`
/// from the caller so tests can pin it.
#[must_use]
pub fn compute_delay(attempt: u32, config: &RetryConfig, jitter_factor: f64) -> Duration {
    debug_assert!(attempt >= 1, "attempt is 1-indexed");
    // `attempt - 1` is the number of completed retries; the
    // first retry uses `initial_delay * factor^0 = initial_delay`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let exponent = attempt.saturating_sub(1) as i32;
    let mult = config.backoff_factor.powi(exponent);
    let base_ms = config.initial_delay.as_secs_f64() * 1000.0 * mult;
    let with_jitter_ms = base_ms * jitter_factor;
    let max_ms = config.max_delay.as_secs_f64() * 1000.0;
    let capped_ms = with_jitter_ms.min(max_ms).max(0.0);
    Duration::from_secs_f64(capped_ms / 1000.0)
}

/// Trait abstracting the jitter source. `DefaultJitter` reads
/// from the SDK's small LCG; tests use `FixedJitter`.
pub trait JitterSource: Send + Sync + 'static {
    /// Return a multiplier in `[1.0 - jitter, 1.0 + jitter]`.
    fn factor(&self, jitter: f64) -> f64;
}

/// Thread-safe LCG seeded from `SystemTime` at construction.
/// Spec §13/04 §6 only requires "jitter prevents synchronized
/// retries"; cryptographic strength is not needed.
pub struct DefaultJitter {
    state: AtomicU64,
}

impl Default for DefaultJitter {
    fn default() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x517_cc1b_7274_4d2b);
        Self {
            state: AtomicU64::new(seed.wrapping_add(1)),
        }
    }
}

impl JitterSource for DefaultJitter {
    fn factor(&self, jitter: f64) -> f64 {
        // LCG params from Numerical Recipes; period 2^64.
        let prev = self.state.load(Ordering::Relaxed);
        let next = prev
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state.store(next, Ordering::Relaxed);
        // Map to [0.0, 1.0).
        let r = (next >> 32) as f64 / (u32::MAX as f64 + 1.0);
        // Map to [1.0 - jitter, 1.0 + jitter].
        1.0 + (2.0 * r - 1.0) * jitter
    }
}

/// Test jitter: always returns `1.0` (no jitter). Useful for
/// deterministic delay assertions.
#[cfg(test)]
pub(crate) struct FixedJitter;

#[cfg(test)]
impl JitterSource for FixedJitter {
    fn factor(&self, _jitter: f64) -> f64 {
        1.0
    }
}

/// Drive `op` through the retry policy. Returns the first
/// successful result, the original error if it isn't retryable,
/// or [`ClientError::RetryExhausted`] once attempts / total
/// timeout are exhausted.
pub async fn retry_with_backoff<F, Fut, T>(
    mut op: F,
    config: &RetryConfig,
    jitter: &dyn JitterSource,
) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    let started = Instant::now();
    let mut last_error: Option<ClientError> = None;

    for attempt in 1..=config.max_attempts {
        if let Some(budget) = config.total_timeout {
            if started.elapsed() >= budget {
                return Err(exhausted(last_error, attempt - 1, started.elapsed()));
            }
        }

        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !e.is_retryable() {
                    return Err(e);
                }
                if attempt == config.max_attempts {
                    return Err(exhausted(Some(e), attempt, started.elapsed()));
                }
                // Sleep before the next attempt. `attempt` is the
                // number we just finished; the next attempt is
                // `attempt + 1` and uses `compute_delay(attempt + 1, ...)`.
                let delay = compute_delay(attempt + 1, config, jitter.factor(config.jitter));
                // Respect the total budget for the sleep itself.
                if let Some(budget) = config.total_timeout {
                    let remaining = budget.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(exhausted(Some(e), attempt, started.elapsed()));
                    }
                    tokio::time::sleep(delay.min(remaining)).await;
                } else {
                    tokio::time::sleep(delay).await;
                }
                last_error = Some(e);
            }
        }
    }

    // `for` exited because `max_attempts` was 0 — shouldn't
    // happen (RetryConfig::with_max_attempts enforces >= 1).
    Err(exhausted(
        last_error,
        config.max_attempts,
        started.elapsed(),
    ))
}

fn exhausted(
    last_error: Option<ClientError>,
    attempts: u32,
    total_duration: Duration,
) -> ClientError {
    let last = last_error.unwrap_or(ClientError::Internal(
        "retry runner exhausted without an underlying error".into(),
    ));
    ClientError::RetryExhausted {
        last_error: Box::new(last),
        attempts,
        total_duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn cfg_fast() -> RetryConfig {
        RetryConfig::new()
            .with_initial_delay(Duration::from_millis(1))
            .with_max_delay(Duration::from_millis(20))
            .with_jitter(0.0)
            .with_total_timeout(Some(Duration::from_secs(5)))
    }

    #[test]
    fn compute_delay_exponential_progression() {
        let cfg = RetryConfig::new()
            .with_initial_delay(Duration::from_millis(100))
            .with_backoff_factor(2.0)
            .with_max_delay(Duration::from_secs(60));
        // attempt 2 (first retry): initial_delay * 2^1
        // attempt 3: initial_delay * 2^2
        // attempt 4: initial_delay * 2^3
        let d2 = compute_delay(2, &cfg, 1.0);
        let d3 = compute_delay(3, &cfg, 1.0);
        let d4 = compute_delay(4, &cfg, 1.0);
        assert_eq!(d2, Duration::from_millis(200));
        assert_eq!(d3, Duration::from_millis(400));
        assert_eq!(d4, Duration::from_millis(800));
    }

    #[test]
    fn compute_delay_caps_at_max() {
        let cfg = RetryConfig::new()
            .with_initial_delay(Duration::from_millis(100))
            .with_backoff_factor(2.0)
            .with_max_delay(Duration::from_millis(500));
        // Without cap, attempt 5 would be 100 * 2^4 = 1600 ms.
        let d5 = compute_delay(5, &cfg, 1.0);
        assert_eq!(d5, Duration::from_millis(500));
    }

    #[test]
    fn jitter_factor_stays_in_band() {
        let j = DefaultJitter::default();
        for _ in 0..256 {
            let f = j.factor(0.1);
            assert!(
                (0.9..=1.1).contains(&f),
                "jitter factor {f} out of ±10% band"
            );
        }
    }

    #[tokio::test]
    async fn retries_on_retryable_then_succeeds() {
        let attempts = AtomicU32::new(0);
        let cfg = cfg_fast();
        let result: Result<u32, ClientError> = retry_with_backoff(
            || async {
                let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
                if n < 2 {
                    Err(ClientError::Overloaded { detail: "x".into() })
                } else {
                    Ok(42)
                }
            },
            &cfg,
            &FixedJitter,
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn non_retryable_error_short_circuits() {
        let attempts = AtomicU32::new(0);
        let cfg = cfg_fast();
        let result: Result<(), ClientError> = retry_with_backoff(
            || async {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(ClientError::Closed)
            },
            &cfg,
            &FixedJitter,
        )
        .await;
        match result {
            Err(ClientError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn exhausts_after_max_attempts() {
        let attempts = AtomicU32::new(0);
        let cfg = cfg_fast().with_max_attempts(3);
        let result: Result<(), ClientError> = retry_with_backoff(
            || async {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(ClientError::Overloaded { detail: "x".into() })
            },
            &cfg,
            &FixedJitter,
        )
        .await;
        match result {
            Err(ClientError::RetryExhausted {
                attempts: a,
                last_error,
                ..
            }) => {
                assert_eq!(a, 3);
                assert!(matches!(*last_error, ClientError::Overloaded { .. }));
            }
            other => panic!("expected RetryExhausted, got {other:?}"),
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn total_timeout_aborts_loop() {
        let cfg = RetryConfig::new()
            .with_initial_delay(Duration::from_millis(30))
            .with_max_delay(Duration::from_millis(60))
            .with_jitter(0.0)
            .with_max_attempts(100)
            .with_total_timeout(Some(Duration::from_millis(80)));

        let result: Result<(), ClientError> = retry_with_backoff(
            || async { Err(ClientError::Overloaded { detail: "x".into() }) },
            &cfg,
            &FixedJitter,
        )
        .await;
        match result {
            Err(ClientError::RetryExhausted { attempts: a, .. }) => {
                assert!(
                    a < 100,
                    "should have aborted before reaching max_attempts; attempts = {a}"
                );
            }
            other => panic!("expected RetryExhausted, got {other:?}"),
        }
    }
}
