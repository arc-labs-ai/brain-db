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
    /// The provider completed and billed the call but returned no usable
    /// content — an empty completion, or a reply whose only blocks were
    /// non-text.
    ///
    /// The usage rides on the error because the call cost real money. A
    /// failure dropped on the floor is spend the tenant was charged for and
    /// that nothing in Brain ever recorded, and an empty reply is the failure
    /// most likely to repeat: the same prompt against the same budget returns
    /// empty again, and is billed again.
    ///
    /// `billed.content` is empty by construction; every other field is what the
    /// provider reported.
    #[error("provider billed the call but returned no usable content: {reason}")]
    EmptyContent {
        reason: String,
        billed: crate::types::LlmResponse,
    },
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

    /// The provider HTTP runtime could not be built, so no request was sent
    /// and nothing was billed.
    ///
    /// That runtime is built at most once per process and its outcome is
    /// cached, failure included: the first call that cannot build it settles
    /// the answer for every later call, so no retry inside this process can
    /// reach a provider.
    #[error("provider runtime unavailable: LLM completions are disabled for this process")]
    ProviderRuntimeUnavailable,

    /// The task carrying a provider request never produced a result: it
    /// panicked, or the runtime cancelled it on shutdown.
    ///
    /// A panic is a defect in the request path that repeats on the same
    /// input. The round trip may already have reached the provider and been
    /// billed, and that usage is unrecoverable, because the task's own result
    /// is what would have carried it back.
    #[error("provider request task failed: {reason}")]
    RequestTaskFailed { reason: String },

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
    /// A failure that never reached the provider is permanent rather than an
    /// unknown status: a runtime cached as unbuildable stays unbuildable for
    /// the process, and a panicked request path repeats while possibly having
    /// already billed a round trip.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport { .. } | Self::RateLimit { .. } | Self::Timeout => {
                FailureClass::Transient
            }
            // An empty billed completion is permanent for the same reason a
            // decode failure is: the identical prompt against the identical
            // budget produces the identical nothing, and each attempt is
            // billed. Retrying it burns money and hides the real cause, which
            // is a prompt or a budget the operator has to change.
            Self::Auth { .. }
            | Self::InvalidRequest { .. }
            | Self::OutputDecodeFailed { .. }
            | Self::EmptyContent { .. }
            // A failure that never reached the provider is not an unknown
            // provider status: a cached-unbuildable runtime stays unbuildable
            // for the process, and a panicked request path repeats while it
            // may already have billed a round trip.
            | Self::ProviderRuntimeUnavailable
            | Self::RequestTaskFailed { .. } => FailureClass::Permanent,
            Self::ProviderError { status, .. } => match status {
                408 | 425 | 429 | 500 | 502 | 503 | 504 => FailureClass::Transient,
                400..=499 => FailureClass::Permanent,
                _ => FailureClass::Transient,
            },
        }
    }

    /// The provider-reported usage of a call that was billed but produced
    /// nothing usable, for callers that record spend. `None` for every error
    /// that never reached a billed completion.
    #[must_use]
    pub fn billed_usage(&self) -> Option<&crate::types::LlmResponse> {
        match self {
            Self::EmptyContent { billed, .. } => Some(billed),
            _ => None,
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

    /// A runtime cached as unbuildable cannot become buildable, so retrying it
    /// is an unbounded loop that never reaches a provider.
    #[test]
    fn provider_runtime_unavailable_is_permanent() {
        let error = LlmError::ProviderRuntimeUnavailable;
        assert_eq!(error.failure_class(), FailureClass::Permanent);
        assert_ne!(error.failure_class(), FailureClass::Transient);
    }

    #[test]
    fn request_task_failure_is_permanent() {
        let error = LlmError::RequestTaskFailed {
            reason: "request task panicked".into(),
        };
        assert_eq!(error.failure_class(), FailureClass::Permanent);
        assert_ne!(error.failure_class(), FailureClass::Transient);
    }

    /// Usage from a provider round trip whose task died is unrecoverable, so
    /// there is nothing to report.
    #[test]
    fn provider_runtime_failures_have_no_billed_usage() {
        assert!(LlmError::ProviderRuntimeUnavailable
            .billed_usage()
            .is_none());
        assert!(LlmError::RequestTaskFailed {
            reason: "cancelled".into(),
        }
        .billed_usage()
        .is_none());
    }

    #[test]
    fn provider_runtime_failure_messages_include_the_condition() {
        assert!(LlmError::ProviderRuntimeUnavailable
            .to_string()
            .contains("runtime"));
        let reason = "request task panicked";
        assert!(LlmError::RequestTaskFailed {
            reason: reason.into(),
        }
        .to_string()
        .contains(reason));
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

    /// Empty billed output is terminal and exposes its usage, while timeout has none.
    #[test]
    fn empty_content_is_permanent_and_has_billed_usage() {
        let billed = crate::types::LlmResponse {
            content: String::new(),
            tokens_in: 1,
            tokens_out: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_micro_usd: 1,
            model_version: "m".into(),
            truncated: false,
        };
        let error = LlmError::EmptyContent {
            reason: "empty".into(),
            billed: billed.clone(),
        };
        assert_eq!(error.failure_class(), FailureClass::Permanent);
        assert_eq!(error.billed_usage(), Some(&billed));
        assert!(LlmError::Timeout.billed_usage().is_none());
    }
}
