//! Process-global precision-decision tuning, sourced from deploy config.
//!
//! The read path's shaping stage (RECALL membership → answer shape) applies a
//! calibrated *selective* decision: commit a `Single`/`Many` answer only when the
//! lead is confident enough, otherwise abstain with an honest `None`; and keep a
//! `Many` answer's committed set minimal. The confidence signal is cross-lane
//! consensus (the `support` count already computed in the fan-out — the one signal
//! that empirically separates correct from wrong answers; retrieval score does
//! not). See `spec/13_retrievers/07_precision_engine.md`.
//!
//! These are deploy-time knobs, never per-request or per-shard, so — like
//! [`RetrievalTuning`](crate::RetrievalTuning) — they live here as a read-only
//! value installed once at boot from the `[precision]` TOML section and read at
//! the RECALL shaping call site.
//!
//! **Defaults reproduce the pre-precision behaviour exactly.** With
//! `commit_min_support = 0` the engine never abstains on the consensus signal
//! (the prior `any_belongs` gate still applies), and with `many_min_support = 0`
//! it never trims a `Many`. Raising either trades coverage for committed
//! precision. This is why the stage is always-on (C0) yet safe to ship before a
//! calibration is fitted: an uncalibrated deploy behaves as it did before.

use std::sync::OnceLock;

/// The maximum value the `support` consensus count can take (strong-semantic +
/// lexical + graph + grounded + strong-HyPE lanes). Thresholds above this can
/// never be met, so a value here is clamped to it on read.
pub const MAX_SUPPORT: u8 = 5;

/// Deploy-time precision-decision tuning. Read-only after [`install`](PrecisionTuning::install).
///
/// The derived `Default` is all-zero — the no-op that reproduces the pre-precision
/// behaviour (never abstain on consensus, never trim a `Many`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrecisionTuning {
    /// Minimum cross-lane `support` the lead answer must carry to COMMIT rather
    /// than abstain with `None`. `0` (default) never abstains on this signal,
    /// reproducing the prior behaviour. Higher ⇒ more `None`, higher committed
    /// precision. A grounded commit is exempt (it clears its own corroboration
    /// gate); this gates only the uncommitted episodic lead.
    pub commit_min_support: u8,
    /// Minimum `support` a member needs to be part of a `Many` answer's committed
    /// SET (as opposed to the retained context tail). `0` (default) keeps every
    /// band member in the answer, reproducing the prior behaviour. Higher ⇒
    /// tighter, less-padded `Many`. The lead member is always in the answer even
    /// if it does not clear this bar, so a `Many` never trims to empty.
    pub many_min_support: u8,
}

static PRECISION_TUNING: OnceLock<PrecisionTuning> = OnceLock::new();

impl PrecisionTuning {
    /// Install the process-wide tuning from parsed deploy config. First call
    /// wins (the server calls it once at boot); later calls are ignored, so a
    /// stray second install can never mutate live tuning. Returns whether this
    /// call was the one that installed the value.
    pub fn install(self) -> bool {
        PRECISION_TUNING.set(self).is_ok()
    }

    /// The active tuning, or defaults if none was installed (tests / non-server
    /// callers). The default reproduces the pre-precision behaviour.
    #[must_use]
    pub fn active() -> &'static PrecisionTuning {
        PRECISION_TUNING.get_or_init(PrecisionTuning::default)
    }

    /// `commit_min_support` clamped to the reachable range `0..=MAX_SUPPORT`.
    #[must_use]
    pub fn commit_min_support_clamped(&self) -> u8 {
        self.commit_min_support.min(MAX_SUPPORT)
    }

    /// `many_min_support` clamped to the reachable range `0..=MAX_SUPPORT`.
    #[must_use]
    pub fn many_min_support_clamped(&self) -> u8 {
        self.many_min_support.min(MAX_SUPPORT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_noop() {
        let t = PrecisionTuning::default();
        assert_eq!(
            t.commit_min_support, 0,
            "default must never abstain on consensus"
        );
        assert_eq!(t.many_min_support, 0, "default must never trim a Many");
    }

    #[test]
    fn thresholds_clamp_to_reachable_support() {
        let t = PrecisionTuning {
            commit_min_support: 99,
            many_min_support: 42,
        };
        assert_eq!(t.commit_min_support_clamped(), MAX_SUPPORT);
        assert_eq!(t.many_min_support_clamped(), MAX_SUPPORT);
    }
}
