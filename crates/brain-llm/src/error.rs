//! LLM transport errors.

/// Whether a failed LLM call is worth retrying.
///
/// The extractor pipeline uses this to decide a failed extraction's fate:
/// a **transient** failure (the provider blipped — timeout, rate-limit,
/// network, 5xx) keeps the memory queued and is retried with backoff until
/// it succeeds, so a passing outage never permanently strips a memory of its
/// typed-graph grounding. A **permanent** failure (the request can't ever
/// succeed as-is — bad/absent key, no balance, malformed request) is
/// terminal immediately: retrying only burns money and hides the real
/// problem from the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Transient,
    Permanent,
}

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

    /// Classify this error as retryable (transient) or terminal (permanent).
    ///
    /// Transient = the same request could succeed later: network/transport
    /// faults, timeouts, rate limits, and server-side 5xx / 408 / 425 / 429.
    /// Permanent = the request can't succeed as-is: missing/rejected key,
    /// payment required, or a malformed request the provider refuses (4xx
    /// other than the throttling codes), plus a decode failure (a prompt or
    /// schema bug, not a blip). An unknown provider status defaults to
    /// transient: retry-with-backoff is bounded and cheap, while a wrong
    /// "permanent" verdict would silently drop a memory's grounding forever.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport { .. } | Self::RateLimit { .. } | Self::Timeout => {
                FailureClass::Transient
            }
            Self::Auth { .. } | Self::InvalidRequest { .. } | Self::OutputDecodeFailed { .. } => {
                FailureClass::Permanent
            }
            Self::ProviderError { status, .. } => match status {
                408 | 425 | 429 | 500 | 502 | 503 | 504 => FailureClass::Transient,
                400..=499 => FailureClass::Permanent,
                _ => FailureClass::Transient,
            },
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
    fn transient_errors_are_retryable() {
        assert_eq!(LlmError::Timeout.failure_class(), FailureClass::Transient);
        assert_eq!(
            LlmError::RateLimit { retry_after_ms: 1 }.failure_class(),
            FailureClass::Transient
        );
        for status in [408u16, 425, 429, 500, 502, 503, 504] {
            assert_eq!(
                LlmError::ProviderError {
                    status,
                    message: String::new()
                }
                .failure_class(),
                FailureClass::Transient,
                "status {status} should be transient",
            );
        }
    }

    #[test]
    fn permanent_errors_are_terminal() {
        assert_eq!(
            LlmError::Auth { provider: "openai" }.failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            LlmError::InvalidRequest {
                reason: "bad".into()
            }
            .failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            LlmError::OutputDecodeFailed {
                reason: "bad".into()
            }
            .failure_class(),
            FailureClass::Permanent
        );
        // 401 unauthorized, 402 payment-required, 400 bad-request → permanent.
        for status in [400u16, 401, 402, 403, 404, 422] {
            assert_eq!(
                LlmError::ProviderError {
                    status,
                    message: String::new()
                }
                .failure_class(),
                FailureClass::Permanent,
                "status {status} should be permanent",
            );
        }
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
