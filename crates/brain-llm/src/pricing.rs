//! Published per-model provider rates, and the one place a billed call is
//! turned into a dollar figure.
//!
//! ## Why nano-USD
//!
//! Every downstream field Brain stores spend in is micro-USD (1e-6 USD) — the
//! `cost_micro_usd` column on an audit row, the `llm_micro_usd_spent_total`
//! metric, the durable `llm_spend` ledger. A **rate** cannot live in that unit:
//! the cheapest models are priced below one dollar per million tokens, and
//! gpt-4o-mini input at $0.15/MTok is 0.15 micro-USD per token, which rounds to
//! either nothing or to more than four times the real price. Rates are
//! therefore nano-USD (1e-9 USD) per token, which is exactly the published
//! dollars-per-million-tokens figure times one thousand, and a call's total is
//! converted to micro-USD once, at the end.
//!
//! ## Why an unknown model has no price
//!
//! [`ModelPricing::for_model`] returns `None` rather than a house default. A
//! plausible-looking fallback rate is indistinguishable from a real one once it
//! is summed into a ledger, so it corrupts every total downstream of it
//! silently. Recording the token counts with a zero cost and a warning is wrong
//! in exactly one visible place.
//!
//! ## Where a priced call lands
//!
//! Brain keeps durable spend in two stores, and a total that reads one of them
//! is not a total:
//!
//! - The extraction tiers record `cost_micro_usd` on their `ExtractionAudit`
//!   row in `EXTRACTOR_AUDIT_TABLE`, keyed by audit id and tied to the one
//!   memory and extractor that spent it.
//! - HyPE generation, the HyPE refresh sweep and entity disambiguation produce
//!   no audit row, so they accumulate into the `llm_spend` ledger, keyed
//!   `(namespace, stage)`.
//!
//! Durable total spend is the sum of the two. Summing only the ledger misses
//! every extraction; adding extraction costs into the ledger double counts them
//! against the audit rows that already hold them.
//!
//! The two are not interchangeable and neither subsumes the other. An audit row
//! answers "what did extracting this memory cost", which a `(namespace, stage)`
//! counter cannot express. The ledger answers "what has this tenant spent on
//! this stage", which the audit rows cannot answer: an audit row exists only
//! for an extraction tier run, so every stage that writes no audit row is
//! absent from a total assembled from them alone.
//!
//! Sources, both fetched 2026-11-15:
//! - Anthropic: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - OpenAI: <https://developers.openai.com/api/docs/pricing> and the per-model
//!   pages under <https://developers.openai.com/api/docs/models/>

use crate::types::LlmRequest;

/// Published provider rates for one model, in nano-USD (1e-9 USD) per token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPricing {
    /// Live (uncached) input tokens.
    pub input_nano_usd_per_token: u64,
    /// Generated output tokens.
    pub output_nano_usd_per_token: u64,
    /// Tokens billed at the cache-WRITE price — Anthropic's 5-minute ephemeral
    /// breakpoint. Equal to the input rate on a provider that does not bill
    /// cache writes separately, which is how OpenAI's rows are filled.
    pub cache_write_nano_usd_per_token: u64,
    /// Tokens served from a server-side cache. Materially cheaper than the live
    /// input rate on both providers — a tenth of it on Anthropic, a quarter on
    /// gpt-4.1 — so billing them at the input rate overstates the cost of every
    /// cache hit, which is the steady state for Brain's prompts.
    pub cache_read_nano_usd_per_token: u64,
}

/// `(model-id prefix, rates)`, ordered so a longer prefix is tested before any
/// prefix of it. `claude-sonnet-5-5` ahead of `claude-sonnet-5`, `gpt-4o-mini`
/// ahead of `gpt-4o`: reversing either pair silently prices a model at its
/// larger sibling's rate, which is the exact failure this table replaces.
///
/// Matching is on a prefix because providers append a dated snapshot to the
/// model string they report back (`claude-haiku-4-5-20240307`,
/// `gpt-4o-mini-2024-07-18`) and the rate is the same across snapshots.
const RATES: &[(&str, ModelPricing)] = &[
    (
        "claude-haiku-4-5",
        ModelPricing {
            input_nano_usd_per_token: 1_000,
            output_nano_usd_per_token: 5_000,
            cache_write_nano_usd_per_token: 1_250,
            cache_read_nano_usd_per_token: 100,
        },
    ),
    // These are the rates for prompts up to 100,000 tokens. Anthropic bills a
    // prompt over that at five times every figure here, which this table does
    // not model: a >100K-token Haiku 5.5 prompt is under-reported by 5x.
    (
        "claude-haiku-5-5",
        ModelPricing {
            input_nano_usd_per_token: 100,
            output_nano_usd_per_token: 500,
            cache_write_nano_usd_per_token: 125,
            cache_read_nano_usd_per_token: 10,
        },
    ),
    (
        "claude-sonnet-4-5",
        ModelPricing {
            input_nano_usd_per_token: 3_000,
            output_nano_usd_per_token: 15_000,
            cache_write_nano_usd_per_token: 3_750,
            cache_read_nano_usd_per_token: 300,
        },
    ),
    (
        "claude-sonnet-4-6",
        ModelPricing {
            input_nano_usd_per_token: 3_000,
            output_nano_usd_per_token: 15_000,
            cache_write_nano_usd_per_token: 3_750,
            cache_read_nano_usd_per_token: 300,
        },
    ),
    // Sonnet 5.5 reads a cache at 0.05x its base input rate; Sonnet 5 at 0.1x.
    // The two models are otherwise identically priced, which is why the cache
    // read is the only column that separates these rows.
    (
        "claude-sonnet-5-5",
        ModelPricing {
            input_nano_usd_per_token: 2_000,
            output_nano_usd_per_token: 10_000,
            cache_write_nano_usd_per_token: 2_500,
            cache_read_nano_usd_per_token: 100,
        },
    ),
    (
        "claude-sonnet-5",
        ModelPricing {
            input_nano_usd_per_token: 2_000,
            output_nano_usd_per_token: 10_000,
            cache_write_nano_usd_per_token: 2_500,
            cache_read_nano_usd_per_token: 200,
        },
    ),
    (
        "claude-opus-4-5",
        ModelPricing {
            input_nano_usd_per_token: 5_000,
            output_nano_usd_per_token: 25_000,
            cache_write_nano_usd_per_token: 6_250,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    (
        "claude-opus-4-6",
        ModelPricing {
            input_nano_usd_per_token: 5_000,
            output_nano_usd_per_token: 25_000,
            cache_write_nano_usd_per_token: 6_250,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    (
        "claude-opus-4-7",
        ModelPricing {
            input_nano_usd_per_token: 5_000,
            output_nano_usd_per_token: 25_000,
            cache_write_nano_usd_per_token: 6_250,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    (
        "claude-opus-4-8",
        ModelPricing {
            input_nano_usd_per_token: 5_000,
            output_nano_usd_per_token: 25_000,
            cache_write_nano_usd_per_token: 6_250,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    (
        "claude-opus-5-5",
        ModelPricing {
            input_nano_usd_per_token: 4_000,
            output_nano_usd_per_token: 20_000,
            cache_write_nano_usd_per_token: 5_000,
            cache_read_nano_usd_per_token: 200,
        },
    ),
    (
        "claude-opus-5",
        ModelPricing {
            input_nano_usd_per_token: 5_000,
            output_nano_usd_per_token: 25_000,
            cache_write_nano_usd_per_token: 6_250,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    // OpenAI lists this model as shut down. Its rate stays so a deployment
    // still naming it costs out correctly instead of silently at zero.
    (
        "gpt-4.1-nano",
        ModelPricing {
            input_nano_usd_per_token: 100,
            output_nano_usd_per_token: 400,
            cache_write_nano_usd_per_token: 100,
            cache_read_nano_usd_per_token: 25,
        },
    ),
    (
        "gpt-4.1-mini",
        ModelPricing {
            input_nano_usd_per_token: 400,
            output_nano_usd_per_token: 1_600,
            cache_write_nano_usd_per_token: 400,
            cache_read_nano_usd_per_token: 100,
        },
    ),
    (
        "gpt-4.1",
        ModelPricing {
            input_nano_usd_per_token: 2_000,
            output_nano_usd_per_token: 8_000,
            cache_write_nano_usd_per_token: 2_000,
            cache_read_nano_usd_per_token: 500,
        },
    ),
    (
        "gpt-4o-mini",
        ModelPricing {
            input_nano_usd_per_token: 150,
            output_nano_usd_per_token: 600,
            cache_write_nano_usd_per_token: 150,
            cache_read_nano_usd_per_token: 75,
        },
    ),
    (
        "gpt-4o",
        ModelPricing {
            input_nano_usd_per_token: 2_500,
            output_nano_usd_per_token: 10_000,
            cache_write_nano_usd_per_token: 2_500,
            cache_read_nano_usd_per_token: 1_250,
        },
    ),
];

impl ModelPricing {
    /// Rates for `model`, or `None` when no published rate for it is recorded
    /// here. A caller that gets `None` records the token counts and reports a
    /// zero cost rather than guessing — see the module docs.
    #[must_use]
    pub fn for_model(model: &str) -> Option<Self> {
        RATES
            .iter()
            .find(|(prefix, _)| model.starts_with(prefix))
            .map(|(_, pricing)| *pricing)
    }

    /// Micro-USD cost of one billed call.
    ///
    /// Every token class is summed in nano-USD and converted once, rounding to
    /// nearest, so a call's rounding error is bounded by half a micro-dollar
    /// instead of accumulating a separate rounding per class. Saturating
    /// throughout: an implausible token count pins the figure rather than
    /// wrapping it to a small number and understating the spend.
    #[must_use]
    pub fn cost_micro_usd(
        &self,
        tokens_in: u64,
        tokens_out: u64,
        cache_write_tokens: u64,
        cache_read_tokens: u64,
    ) -> u64 {
        let nano = tokens_in
            .saturating_mul(self.input_nano_usd_per_token)
            .saturating_add(tokens_out.saturating_mul(self.output_nano_usd_per_token))
            .saturating_add(cache_write_tokens.saturating_mul(self.cache_write_nano_usd_per_token))
            .saturating_add(cache_read_tokens.saturating_mul(self.cache_read_nano_usd_per_token));
        nano.saturating_add(500) / 1_000
    }

    /// Micro-USD a request is expected to cost, priced before it is issued.
    ///
    /// The response does not exist yet, so the figure comes from the request
    /// itself: [`LlmRequest::approx_input_tokens`] for the input side and
    /// `max_tokens` for the output, which is the most the provider can bill for
    /// generation. Every input token is priced at the live input rate, so a call
    /// whose prompt is served from cache costs less than this — the estimate is
    /// an upper bound and never reads low.
    ///
    /// Distinct from [`ModelPricing::cost_micro_usd`], which prices the usage a
    /// provider reported after the fact. Both read the same rate row, so an
    /// estimate and the recorded cost of one call cannot disagree about the rate.
    #[must_use]
    pub fn estimate_request_micro_usd(&self, request: &LlmRequest) -> u64 {
        let nano = request
            .approx_input_tokens()
            .saturating_mul(self.input_nano_usd_per_token)
            .saturating_add(
                u64::from(request.max_tokens).saturating_mul(self.output_nano_usd_per_token),
            );
        nano.saturating_add(500) / 1_000
    }
}

#[cfg(test)]
mod tests {
    use super::ModelPricing;
    use crate::types::LlmRequest;

    /// A request is priced from its own token counts, at the same rates the
    /// billed figure uses.
    #[test]
    fn a_request_is_estimated_from_its_input_and_output_ceiling() {
        let mut request = LlmRequest::new("claude-haiku-4-5", "abcdefgh");
        request.max_tokens = 100;
        let pricing = ModelPricing::for_model("claude-haiku-4-5").unwrap();
        // 8 chars / 4 = 2 input tokens at 1_000 nano, 100 output tokens at 5_000
        // nano = 2_000 + 500_000 = 502_000 nano = 502 micro-USD.
        assert_eq!(pricing.estimate_request_micro_usd(&request), 502);
    }

    /// The estimate prices every input token live, so the billed figure for the
    /// same call can only come in at or under it.
    #[test]
    fn an_estimate_is_an_upper_bound_on_the_billed_figure() {
        let mut request = LlmRequest::new("claude-haiku-4-5", "a".repeat(4_000));
        request.max_tokens = 500;
        let pricing = ModelPricing::for_model("claude-haiku-4-5").unwrap();
        let estimate = pricing.estimate_request_micro_usd(&request);
        // The same 1_000 input tokens served from cache, generating half the
        // output ceiling.
        let billed = pricing.cost_micro_usd(0, 250, 0, 1_000);
        assert!(
            billed < estimate,
            "billed {billed} should be under estimate {estimate}"
        );
    }

    /// A dated snapshot suffix resolves to its base model's rates.
    #[test]
    fn dated_snapshots_match_their_base_model() {
        assert_eq!(
            ModelPricing::for_model("claude-haiku-4-5-20240307"),
            ModelPricing::for_model("claude-haiku-4-5"),
        );
        assert_eq!(
            ModelPricing::for_model("gpt-4o-mini-2024-07-18"),
            ModelPricing::for_model("gpt-4o-mini"),
        );
    }

    /// The table is ordered longest-prefix-first, so a model never picks up a
    /// shorter sibling's rate. These are the two pairs that can collide.
    #[test]
    fn a_longer_prefix_is_not_swallowed_by_a_shorter_one() {
        // Sonnet 5.5 and Sonnet 5 differ only in the cache-read rate.
        assert_eq!(
            ModelPricing::for_model("claude-sonnet-5-5")
                .unwrap()
                .cache_read_nano_usd_per_token,
            100,
        );
        assert_eq!(
            ModelPricing::for_model("claude-sonnet-5")
                .unwrap()
                .cache_read_nano_usd_per_token,
            200,
        );
        assert_eq!(
            ModelPricing::for_model("claude-opus-5-5")
                .unwrap()
                .input_nano_usd_per_token,
            4_000,
        );
        assert_eq!(
            ModelPricing::for_model("claude-opus-5")
                .unwrap()
                .input_nano_usd_per_token,
            5_000,
        );
        assert_eq!(
            ModelPricing::for_model("gpt-4o-mini")
                .unwrap()
                .output_nano_usd_per_token,
            600,
        );
        assert_eq!(
            ModelPricing::for_model("gpt-4o")
                .unwrap()
                .output_nano_usd_per_token,
            10_000,
        );
    }

    /// An unpriced model yields no rates at all — never a house default.
    #[test]
    fn an_unknown_model_has_no_rates() {
        assert!(ModelPricing::for_model("llama-3.1-8b").is_none());
        assert!(ModelPricing::for_model("").is_none());
    }

    /// The case whole-micro-USD rates could not express: gpt-4o-mini input is
    /// $0.15 per million tokens, which is 0.15 micro-USD per token.
    #[test]
    fn a_sub_micro_usd_rate_is_exactly_representable() {
        let pricing = ModelPricing::for_model("gpt-4o-mini").unwrap();
        // 1_000_000 * 150 nano = 150_000_000 nano = 150_000 micro-USD = $0.15.
        assert_eq!(pricing.cost_micro_usd(1_000_000, 0, 0, 0), 150_000);
    }

    /// Each token class is priced at its own rate and the total converts once.
    #[test]
    fn every_token_class_is_priced_at_its_own_rate() {
        let pricing = ModelPricing::for_model("claude-haiku-4-5").unwrap();
        // 100*1000 + 50*5000 + 1500*1250 + 2000*100 nano
        //  = 100_000 + 250_000 + 1_875_000 + 200_000 = 2_425_000 nano
        //  = 2425 micro-USD.
        assert_eq!(pricing.cost_micro_usd(100, 50, 1_500, 2_000), 2_425);
    }

    /// Rounding is to nearest, once, on the summed figure.
    #[test]
    fn a_sub_micro_usd_total_rounds_to_nearest() {
        let pricing = ModelPricing::for_model("gpt-4o-mini").unwrap();
        // 3 * 150 = 450 nano → 0 micro-USD.
        assert_eq!(pricing.cost_micro_usd(3, 0, 0, 0), 0);
        // 4 * 150 = 600 nano → 1 micro-USD.
        assert_eq!(pricing.cost_micro_usd(4, 0, 0, 0), 1);
    }
}
