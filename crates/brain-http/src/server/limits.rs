//! Per-server request and connection limits.
//!
//! Values land at hyper through the builder (max-buf-size) and at the
//! Brain dispatch path through explicit checks (body size,
//! per-request timeout).

use std::time::Duration;

/// Default per-request header block ceiling. 16 KiB matches the
/// implicit limit in the existing `brain-server::admin` hand-roll.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 16 * 1024;

/// Default per-request body ceiling. 16 MiB matches
/// [`crate::body::MAX_BODY_BYTES`].
pub const DEFAULT_MAX_BODY_BYTES: u64 = crate::body::MAX_BODY_BYTES;

/// Default per-request wall-clock timeout (head + body + handler).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default per-connection idle timeout.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// All knobs the brain-http server exposes for limiting work per
/// request and per connection. The hyper layer learns about
/// `max_header_bytes` (via `max_buf_size`); the Brain dispatch path
/// enforces the body and timeout limits explicitly.
#[derive(Debug, Clone)]
pub struct ServerLimits {
    /// Maximum bytes for the request head (request line + headers).
    pub max_header_bytes: usize,
    /// Maximum inbound body size.
    pub max_body_bytes: u64,
    /// Per-request wall-clock timeout (head + body + handler).
    pub request_timeout: Duration,
    /// Per-connection idle timeout — applied between requests on a
    /// keep-alive connection.
    pub idle_timeout: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

impl ServerLimits {
    /// Spec defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override `max_header_bytes`.
    #[must_use]
    pub fn with_max_header_bytes(mut self, bytes: usize) -> Self {
        self.max_header_bytes = bytes;
        self
    }

    /// Override `max_body_bytes`.
    #[must_use]
    pub fn with_max_body_bytes(mut self, bytes: u64) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Override `request_timeout`.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override `idle_timeout`.
    #[must_use]
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_chain_overrides() {
        let l = ServerLimits::new()
            .with_max_header_bytes(1024)
            .with_max_body_bytes(2048)
            .with_request_timeout(Duration::from_millis(500))
            .with_idle_timeout(Duration::from_secs(5));
        assert_eq!(l.max_header_bytes, 1024);
        assert_eq!(l.max_body_bytes, 2048);
        assert_eq!(l.request_timeout, Duration::from_millis(500));
        assert_eq!(l.idle_timeout, Duration::from_secs(5));
    }
}
