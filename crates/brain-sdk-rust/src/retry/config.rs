//! `RetryConfig` — knobs for the exponential-backoff retry loop.
//!
//! Defaults come from spec §13/04 §6:
//! - `max_attempts = 3`
//! - `initial_delay = 100 ms`
//! - `backoff_factor = 2.0`
//! - `max_delay = 30 s`
//! - `jitter = 0.1` (±10 %)
//! - `total_timeout = 60 s` (spec §13)

use std::time::Duration;

pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_INITIAL_DELAY: Duration = Duration::from_millis(100);
pub const DEFAULT_BACKOFF_FACTOR: f64 = 2.0;
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(30);
pub const DEFAULT_JITTER: f64 = 0.1;
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Retry policy applied by [`crate::retry::retry_with_backoff`].
///
/// 10.3 ships this as a single client-wide policy carried on
/// [`crate::ClientConfig`]. 10.5 will add per-operation overrides
/// via the op-method builders.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryConfig {
    /// Inclusive cap on attempts (1 = no retries, just the
    /// initial call). Spec §6 default 3.
    pub max_attempts: u32,
    /// Base for the exponential backoff. Spec §6 default 100 ms.
    pub initial_delay: Duration,
    /// Multiplier applied per attempt. Spec §6 default 2.0.
    pub backoff_factor: f64,
    /// Cap on any individual sleep between attempts. Spec §6
    /// default 30 s.
    pub max_delay: Duration,
    /// Symmetric jitter factor (`0.1` = ±10 %). Spec §6 default
    /// 0.1.
    pub jitter: f64,
    /// Wall-clock budget covering the whole retry chain (all
    /// attempts + sleeps). `None` disables the total cap; only
    /// `max_attempts` bounds the loop. Spec §13 default 60 s.
    pub total_timeout: Option<Duration>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            initial_delay: DEFAULT_INITIAL_DELAY,
            backoff_factor: DEFAULT_BACKOFF_FACTOR,
            max_delay: DEFAULT_MAX_DELAY,
            jitter: DEFAULT_JITTER,
            total_timeout: Some(DEFAULT_TOTAL_TIMEOUT),
        }
    }
}

impl RetryConfig {
    /// Spec defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// "No retries" preset — single attempt, fail fast. Spec
    /// §13/04 §11 / §16.
    #[must_use]
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// Aggressive preset: 5 attempts, faster backoff. Spec §8 —
    /// useful for important writes the caller will not retry
    /// itself.
    #[must_use]
    pub fn aggressive() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(50),
            backoff_factor: 1.7,
            max_delay: Duration::from_secs(10),
            jitter: 0.2,
            total_timeout: Some(Duration::from_secs(60)),
        }
    }

    #[must_use]
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        assert!(n >= 1, "max_attempts must be >= 1");
        self.max_attempts = n;
        self
    }

    #[must_use]
    pub fn with_initial_delay(mut self, d: Duration) -> Self {
        self.initial_delay = d;
        self
    }

    #[must_use]
    pub fn with_backoff_factor(mut self, f: f64) -> Self {
        assert!(f >= 1.0, "backoff_factor must be >= 1.0");
        self.backoff_factor = f;
        self
    }

    #[must_use]
    pub fn with_max_delay(mut self, d: Duration) -> Self {
        self.max_delay = d;
        self
    }

    #[must_use]
    pub fn with_jitter(mut self, j: f64) -> Self {
        assert!((0.0..=1.0).contains(&j), "jitter must be in [0.0, 1.0]");
        self.jitter = j;
        self
    }

    #[must_use]
    pub fn with_total_timeout(mut self, t: Option<Duration>) -> Self {
        self.total_timeout = t;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_13_04_6() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.initial_delay, Duration::from_millis(100));
        assert_eq!(cfg.backoff_factor, 2.0);
        assert_eq!(cfg.max_delay, Duration::from_secs(30));
        assert_eq!(cfg.jitter, 0.1);
        assert_eq!(cfg.total_timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn none_is_single_attempt() {
        let cfg = RetryConfig::none();
        assert_eq!(cfg.max_attempts, 1);
    }

    #[test]
    fn aggressive_has_more_attempts() {
        let cfg = RetryConfig::aggressive();
        assert!(cfg.max_attempts > RetryConfig::default().max_attempts);
    }

    #[test]
    fn builder_overrides_propagate() {
        let cfg = RetryConfig::new()
            .with_max_attempts(7)
            .with_initial_delay(Duration::from_millis(20))
            .with_backoff_factor(1.5)
            .with_max_delay(Duration::from_secs(5))
            .with_jitter(0.0)
            .with_total_timeout(None);
        assert_eq!(cfg.max_attempts, 7);
        assert_eq!(cfg.initial_delay, Duration::from_millis(20));
        assert!((cfg.backoff_factor - 1.5).abs() < f64::EPSILON);
        assert_eq!(cfg.max_delay, Duration::from_secs(5));
        assert!(cfg.jitter.abs() < f64::EPSILON);
        assert!(cfg.total_timeout.is_none());
    }

    #[test]
    #[should_panic(expected = "max_attempts must be >= 1")]
    fn with_max_attempts_rejects_zero() {
        let _ = RetryConfig::new().with_max_attempts(0);
    }

    #[test]
    #[should_panic(expected = "jitter must be in")]
    fn with_jitter_rejects_negative() {
        let _ = RetryConfig::new().with_jitter(-0.5);
    }
}
