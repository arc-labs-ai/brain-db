//! Runtime options for `run_extractor`. Spec §22/06 §3.

/// Caller-side knobs for an extractor dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractorRunOptions {
    /// Force re-execution even if the audit-row idempotency probe
    /// hits. Used by admin "re-extract" flows.
    pub replay: bool,
    /// On idempotency-probe cache hit, also re-emit the cached
    /// outputs (rather than just writing a `SkippedDuplicate` audit
    /// row and returning empty). Read-after-write flows that re-run
    /// extraction on cache-warmed paths set this.
    pub include_cached_outputs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_no_replay_no_cached() {
        let o = ExtractorRunOptions::default();
        assert!(!o.replay);
        assert!(!o.include_cached_outputs);
    }

    #[test]
    fn options_equality() {
        let a = ExtractorRunOptions {
            replay: true,
            include_cached_outputs: false,
        };
        let b = ExtractorRunOptions {
            replay: true,
            include_cached_outputs: false,
        };
        assert_eq!(a, b);
    }
}
