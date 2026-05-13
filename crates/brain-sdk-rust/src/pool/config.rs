//! `PoolConfig` — per-pool sizing and lifetime knobs.
//!
//! Defaults derive from spec §13/03 §1 (1 min / 8 max), §5
//! (5 min idle, 30 s keep-alive), §13 (overloaded threshold).

use std::time::Duration;

/// Spec §13/03 §1 — default minimum connections per pool.
pub const DEFAULT_MIN_CONNECTIONS: u32 = 1;
/// Spec §13/03 §1 — default maximum connections per pool.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 8;
/// Spec §13/03 §5 — close idle connections after this duration.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long [`super::Pool::acquire`] waits for a free slot before
/// returning `ClientError::Overloaded`. Matches `ClientConfig::timeout`'s
/// 30 s default so callers don't see two competing budgets.
pub const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// Spec §13/03 §5 — periodic keepalive interval. Reserved for 10.6;
/// 10.2 stores it but does not yet emit `SERVER_PING`-style frames.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Construction-time knobs for [`super::Pool`].
///
/// Use `PoolConfig::default()` for spec-defaults; builder methods
/// override individual knobs. The defaults collapse to
/// "single-connection mode" — `min = 1`, `max = 1` — when set via
/// [`PoolConfig::single`] so `Client::connect(addr)` keeps its
/// 10.1 contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// Minimum connections kept alive. The idle reaper won't drop
    /// the pool below this.
    pub min_connections: u32,
    /// Maximum connections the pool will open. New requests after
    /// this cap is hit either wait up to `acquire_timeout` for a
    /// release or return `ClientError::Overloaded`.
    pub max_connections: u32,
    /// Close a connection once it's been idle for at least this
    /// long. Spec §13/03 §5.
    pub idle_timeout: Duration,
    /// How long [`super::Pool::acquire`] waits at capacity.
    pub acquire_timeout: Duration,
    /// Reserved — 10.6 wires keepalive PINGs.
    pub keepalive_interval: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_connections: DEFAULT_MIN_CONNECTIONS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
        }
    }
}

impl PoolConfig {
    /// Spec defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The "single connection" preset: `min = 1`, `max = 1`. Used
    /// by `Client::connect(addr)` so 10.1's contract is preserved
    /// (one connection, no pooling).
    #[must_use]
    pub fn single() -> Self {
        Self {
            min_connections: 1,
            max_connections: 1,
            ..Self::default()
        }
    }

    /// Override the minimum.
    ///
    /// # Panics
    /// Panics if `min > max_connections`. Use `with_max` first.
    #[must_use]
    pub fn with_min(mut self, min: u32) -> Self {
        assert!(min <= self.max_connections, "min must be <= max");
        self.min_connections = min;
        self
    }

    /// Override the maximum.
    ///
    /// # Panics
    /// Panics if `max == 0` or `max < min_connections`.
    #[must_use]
    pub fn with_max(mut self, max: u32) -> Self {
        assert!(max > 0, "max must be > 0");
        assert!(
            max >= self.min_connections,
            "max must be >= min ({})",
            self.min_connections
        );
        self.max_connections = max;
        self
    }

    /// Override the idle timeout.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// Override the acquire timeout.
    #[must_use]
    pub fn with_acquire_timeout(mut self, acquire_timeout: Duration) -> Self {
        self.acquire_timeout = acquire_timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.min_connections, 1);
        assert_eq!(cfg.max_connections, 8);
        assert_eq!(cfg.idle_timeout, Duration::from_secs(300));
        assert_eq!(cfg.keepalive_interval, Duration::from_secs(30));
    }

    #[test]
    fn single_preset_is_one_one() {
        let cfg = PoolConfig::single();
        assert_eq!(cfg.min_connections, 1);
        assert_eq!(cfg.max_connections, 1);
    }

    #[test]
    #[should_panic(expected = "max must be > 0")]
    fn with_max_rejects_zero() {
        let _ = PoolConfig::new().with_max(0);
    }

    #[test]
    #[should_panic(expected = "min must be <= max")]
    fn with_min_rejects_above_max() {
        let _ = PoolConfig::new().with_max(2).with_min(3);
    }
}
