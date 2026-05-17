//! LLM transport errors. Spec §22/09 §9.

#[derive(thiserror::Error, Debug)]
pub enum LlmError {
    #[error("transport error: {source}")]
    Transport {
        #[from]
        source: reqwest::Error,
    },

    #[error("auth failed (provider={provider}): API key missing or rejected")]
    Auth { provider: &'static str },

    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimit { retry_after_ms: u64 },

    #[error("invalid request: {reason}")]
    InvalidRequest { reason: String },

    #[error("provider error (status {status}): {message}")]
    ProviderError { status: u16, message: String },

    #[error("provider request timed out")]
    Timeout,

    #[error("output decode failed: {reason}")]
    OutputDecodeFailed { reason: String },
}

impl LlmError {
    /// Retry-after hint surfaced to the caller for the audit
    /// row's `status_reason`. Returns `None` for non-rate-limit
    /// errors.
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimit { retry_after_ms } => Some(*retry_after_ms),
            _ => None,
        }
    }

    /// Provider name for diagnostic context. Returns `""` for
    /// transport / decode errors that don't carry a provider.
    #[must_use]
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Auth { provider } => provider,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_carries_retry_after() {
        let e = LlmError::RateLimit {
            retry_after_ms: 5_000,
        };
        assert_eq!(e.retry_after_ms(), Some(5_000));
    }

    #[test]
    fn non_rate_limit_has_no_retry_after() {
        let e = LlmError::Timeout;
        assert_eq!(e.retry_after_ms(), None);
    }

    #[test]
    fn provider_name_surfaces_on_auth() {
        let e = LlmError::Auth {
            provider: "anthropic",
        };
        assert_eq!(e.provider(), "anthropic");
    }

    #[test]
    fn error_messages_include_useful_context() {
        let e = LlmError::ProviderError {
            status: 503,
            message: "service down".into(),
        };
        let s = e.to_string();
        assert!(s.contains("503"));
        assert!(s.contains("service down"));
    }
}
