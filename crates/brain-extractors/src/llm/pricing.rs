//! Cost budget + per-model pricing + cost estimator.

use brain_llm::LlmRequest;

/// Per-call cost ceiling. v1 ships per-call only; the
/// per-deployment global budget is post-v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostBudget {
    pub per_call_micro_usd: u64,
}

/// Pricing for a single model in dollar micro-units per token.
/// Operator-overridable; v1 ships a small embedded default table
/// for the common models. Unknown models fall back
/// to the conservative default `100 µ$/1K input + 300 µ$/1K
/// output` ⇒ `0.1 µ$ / 0.3 µ$` per token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    pub input_micro_usd_per_token: f64,
    pub output_micro_usd_per_token: f64,
}

impl Pricing {
    /// Conservative default — used when no model-specific entry
    /// is registered.
    #[must_use]
    pub const fn conservative_default() -> Self {
        Self {
            input_micro_usd_per_token: 0.1,
            output_micro_usd_per_token: 0.3,
        }
    }

    /// Lookup pricing by model prefix. Embedded table covers the
    /// common models; operators override via the pricing config.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        if model.starts_with("claude-haiku") {
            Self {
                input_micro_usd_per_token: 1.0,
                output_micro_usd_per_token: 5.0,
            }
        } else if model.starts_with("claude-sonnet") {
            Self {
                input_micro_usd_per_token: 3.0,
                output_micro_usd_per_token: 15.0,
            }
        } else if model.starts_with("gpt-4.1-nano") {
            Self {
                input_micro_usd_per_token: 0.1,
                output_micro_usd_per_token: 0.4,
            }
        } else if model.starts_with("gpt-4.1-mini") {
            Self {
                input_micro_usd_per_token: 0.4,
                output_micro_usd_per_token: 1.6,
            }
        } else if model.starts_with("gpt-4.1") {
            Self {
                input_micro_usd_per_token: 2.0,
                output_micro_usd_per_token: 8.0,
            }
        } else if model.starts_with("gpt-4o-mini") {
            Self {
                input_micro_usd_per_token: 0.15,
                output_micro_usd_per_token: 0.6,
            }
        } else {
            Self::conservative_default()
        }
    }
}

/// Estimated dollar-micro cost of issuing `request` against
/// `pricing`. Uses `LlmRequest::approx_input_tokens()` for the
/// input side and `max_tokens` as the worst-case output.
#[must_use]
pub fn estimate_cost(request: &LlmRequest, pricing: &Pricing) -> u64 {
    let in_tokens = request.approx_input_tokens() as f64;
    let out_tokens = f64::from(request.max_tokens);
    (in_tokens * pricing.input_micro_usd_per_token
        + out_tokens * pricing.output_micro_usd_per_token)
        .round() as u64
}
