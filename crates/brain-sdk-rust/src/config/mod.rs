//! `ClientConfig` — typed constructor knobs for `Client`.
//!
//! Spec §13/02 §14 lists the default values; we encode them as
//! `Default` impl. The auth surface mirrors spec §03/06 `AuthMethod`
//! (re-exported from `brain-protocol`).

use std::time::Duration;

pub use brain_protocol::handshake::AuthMethod;

use crate::pool::PoolConfig;

/// Spec §13/02 §14 default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Spec §13/02 §14 default retry attempts. 10.3 wires the actual
/// retry loop; 10.1 only stores the value.
pub const DEFAULT_RETRIES: u32 = 3;
/// Spec §13/04 §1 default initial backoff. 10.3 wires backoff.
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(100);

/// Construction-time configuration for a `Client`.
///
/// Use the `Default` impl for spec-defaults; the builder methods
/// override individual knobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// Authentication scheme the client should propose at AUTH
    /// time. Default `AuthMethod::None` matches v1 dev policy
    /// (spec §03/06 §3.1 / brain-server's `linux_main`).
    pub auth: AuthMethod,
    /// Per-request wall-clock budget. Applied by 10.5+; 10.1
    /// stores it on the client for handshake completion.
    pub timeout: Duration,
    /// Max retry attempts for retryable errors. 10.3 enforces it.
    pub retries: u32,
    /// Initial backoff before the first retry. 10.3 enforces it.
    pub backoff_initial: Duration,
    /// Connection-pool sizing + idle reaping. See [`PoolConfig`].
    pub pool: PoolConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            auth: AuthMethod::None,
            timeout: DEFAULT_TIMEOUT,
            retries: DEFAULT_RETRIES,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            pool: PoolConfig::default(),
        }
    }
}

impl ClientConfig {
    /// Construct with spec defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the auth method.
    #[must_use]
    pub fn with_auth(mut self, auth: AuthMethod) -> Self {
        self.auth = auth;
        self
    }

    /// Override the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the max retries.
    #[must_use]
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// Override the initial backoff.
    #[must_use]
    pub fn with_backoff_initial(mut self, backoff_initial: Duration) -> Self {
        self.backoff_initial = backoff_initial;
        self
    }

    /// Override the pool configuration.
    #[must_use]
    pub fn with_pool(mut self, pool: PoolConfig) -> Self {
        self.pool = pool;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_13_02_14() {
        let cfg = ClientConfig::default();
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.retries, 3);
        assert_eq!(cfg.backoff_initial, Duration::from_millis(100));
        assert_eq!(cfg.auth, AuthMethod::None);
    }

    #[test]
    fn builder_overrides_propagate() {
        let cfg = ClientConfig::new()
            .with_timeout(Duration::from_secs(5))
            .with_retries(7)
            .with_backoff_initial(Duration::from_millis(50));
        assert_eq!(cfg.timeout, Duration::from_secs(5));
        assert_eq!(cfg.retries, 7);
        assert_eq!(cfg.backoff_initial, Duration::from_millis(50));
    }
}
