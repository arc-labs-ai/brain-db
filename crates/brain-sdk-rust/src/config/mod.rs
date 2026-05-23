//! `ClientConfig` — typed constructor knobs for `Client`.
//!
//! lists the default values; we encode them as
//! `Default` impl. The auth surface mirrors `AuthMethod`
//! (re-exported from `brain-protocol`).

use std::time::Duration;

pub use brain_protocol::connection::handshake::AuthMethod;

use crate::pool::PoolConfig;
use crate::retry::RetryConfig;

/// default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Construction-time configuration for a `Client`.
///
/// Use the `Default` impl for spec-defaults; the builder methods
/// override individual knobs.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientConfig {
    /// Authentication scheme the client should propose at AUTH
    /// time. Default `AuthMethod::None` matches v1 dev policy
    /// (/ brain-server's `linux_main`).
    pub auth: AuthMethod,
    /// Per-request wall-clock budget. Applied by 10.5+; 10.1
    /// stores it on the client for handshake completion.
    pub timeout: Duration,
    /// Connection-pool sizing + idle reaping. See [`PoolConfig`].
    pub pool: PoolConfig,
    /// Retry policy applied by [`crate::retry::retry_with_backoff`].
    pub retry: RetryConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            auth: AuthMethod::None,
            timeout: DEFAULT_TIMEOUT,
            pool: PoolConfig::default(),
            retry: RetryConfig::default(),
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

    /// Override the pool configuration.
    #[must_use]
    pub fn with_pool(mut self, pool: PoolConfig) -> Self {
        self.pool = pool;
        self
    }

    /// Override the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
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
        assert_eq!(cfg.auth, AuthMethod::None);
        // defaults are validated in RetryConfig's
        // own tests; here we just check the field is set.
        assert_eq!(cfg.retry.max_attempts, 3);
        assert_eq!(cfg.retry.initial_delay, Duration::from_millis(100));
    }

    #[test]
    fn builder_overrides_propagate() {
        let cfg = ClientConfig::new()
            .with_timeout(Duration::from_secs(5))
            .with_retry(RetryConfig::none());
        assert_eq!(cfg.timeout, Duration::from_secs(5));
        assert_eq!(cfg.retry.max_attempts, 1);
    }
}
