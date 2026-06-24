//! Confidence aggregation.
//!
//! Pure noisy-OR formula:
//!
//! ```text
//! confidence(S, now) = 1 - Π (1 - c_i · decay(age_i, kind))
//! ```
//!
//! Pure function — no I/O, no async, no state. Called by
//! `brain-metadata::statement_ops::statement_create` (and supersede)
//! when inline evidence carries per-entry metadata
//! (`confidence_milli > 0`). Wire callers without per-evidence
//! metadata keep their caller-supplied statement-level confidence
//! until the `STATEMENT_ADD_EVIDENCE` op lands.

use crate::nodes::{kinds::StatementKind, statement::EvidenceEntry};

// ---------------------------------------------------------------------------
// ConfidenceConfig.
// ---------------------------------------------------------------------------

/// Knobs for the per-kind decay used by [`aggregate_confidence`].
/// Defaults from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfidenceConfig {
    /// Fact half-life. Default 365 days (`31_536_000` s).
    pub fact_half_life_seconds: u64,
    /// Preference half-life. Default 60 days (`5_184_000` s).
    pub pref_half_life_seconds: u64,
    /// When `true`, Event evidence does not decay.
    pub event_decay_disabled: bool,
}

impl ConfidenceConfig {
    /// Defaults.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            fact_half_life_seconds: 31_536_000, // 365 days
            pref_half_life_seconds: 5_184_000,  // 60 days
            event_decay_disabled: true,
        }
    }
}

impl Default for ConfidenceConfig {
    fn default() -> Self {
        Self::default_v1()
    }
}

// ---------------------------------------------------------------------------
// aggregate_confidence.
// ---------------------------------------------------------------------------

/// Aggregate confidence.
///
/// - Empty evidence → `0.0`.
/// - Each evidence entry contributes its `confidence` (per spec
///   `c_i = confidence_milli / 1000`) weighted by the per-kind decay
///   over its age.
/// - Future timestamps (clock skew) saturate to age 0, no decay.
///
/// Result is clamped to `[0, 1]` defensively but should always lie
/// within bounds when inputs do.
#[must_use]
pub fn aggregate_confidence(
    evidence: &[EvidenceEntry],
    now_unix_nanos: u64,
    kind: StatementKind,
    config: &ConfidenceConfig,
) -> f32 {
    if evidence.is_empty() {
        return 0.0;
    }
    let mut product: f32 = 1.0;
    for e in evidence {
        let age_secs =
            (now_unix_nanos.saturating_sub(e.timestamp_unix_nanos) / 1_000_000_000) as f32;
        let decay = decay_for(kind, age_secs, config);
        let c_i = e.confidence();
        let weighted = (c_i * decay).clamp(0.0, 1.0);
        product *= 1.0 - weighted;
    }
    (1.0 - product).clamp(0.0, 1.0)
}

/// Per-kind decay function. Pulled out for unit-testability.
#[must_use]
pub fn decay_for(kind: StatementKind, age_secs: f32, config: &ConfidenceConfig) -> f32 {
    match kind {
        StatementKind::Event if config.event_decay_disabled => 1.0,
        StatementKind::Event => exp_decay(age_secs, config.fact_half_life_seconds),
        // Preferences are the slowest to decay — a stated like/dislike stays
        // true far longer than an episodic fact.
        StatementKind::Preference => exp_decay(age_secs, config.pref_half_life_seconds),
        // Everything else (Fact, Attribute, Relation, Directive, and
        // user-declared Custom kinds) decays on the fact half-life.
        StatementKind::Fact
        | StatementKind::Attribute
        | StatementKind::Relation
        | StatementKind::Directive
        | StatementKind::Custom(_) => exp_decay(age_secs, config.fact_half_life_seconds),
    }
}

/// `exp(-age / half_life)` — the half-life is the time at which the
/// decay output equals `0.5`.
#[must_use]
fn exp_decay(age_secs: f32, half_life_seconds: u64) -> f32 {
    if half_life_seconds == 0 {
        // Guard against div-by-zero — treat as immediate decay.
        return 0.0;
    }
    // `exp(-age / half_life)` does NOT yield 0.5 at age = half_life;
    // that needs `exp(-ln(2) * age / half_life)`.1 uses the
    // `exp(-age / half_life)` form, which gives `1/e ≈ 0.368` at one
    // half-life (more like a "characteristic time" than a half-life).
    // We follow the spec verbatim; tests verify the published numbers.
    (-age_secs / half_life_seconds as f32).exp()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ExtractorId;
    use crate::{ContextId, MemoryId};

    fn cfg() -> ConfidenceConfig {
        ConfidenceConfig::default_v1()
    }

    fn evi(confidence: f32, timestamp_unix_nanos: u64) -> EvidenceEntry {
        EvidenceEntry::from_parts(
            MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            confidence,
            timestamp_unix_nanos,
            ExtractorId::from(0),
        )
    }

    const NOW: u64 = 1_700_000_000_000_000_000;
    const ONE_YEAR_NS: u64 = 365 * 24 * 60 * 60 * 1_000_000_000;
    const SIXTY_DAYS_NS: u64 = 60 * 24 * 60 * 60 * 1_000_000_000;
    const FIVE_YEARS_NS: u64 = 5 * 365 * 24 * 60 * 60 * 1_000_000_000;

    #[test]
    fn empty_evidence_zero() {
        let r = aggregate_confidence(&[], NOW, StatementKind::Fact, &cfg());
        assert_eq!(r, 0.0);
    }

    #[test]
    fn single_evidence_full_confidence_fact_zero_age() {
        let e = [evi(1.0, NOW)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Fact, &cfg());
        assert!((r - 1.0).abs() < 1e-3, "got {r}");
    }

    #[test]
    fn two_evidence_each_0_9_no_decay() {
        // Use Event kind so decay = 1.0 across the board.
        let e = [evi(0.9, NOW), evi(0.9, NOW)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Event, &cfg());
        // 1 - (0.1)^2 = 0.99
        assert!((r - 0.99).abs() < 1e-3, "got {r}");
    }

    #[test]
    fn fact_at_one_year_age() {
        // age = one half-life → decay = 1/e ≈ 0.3679 per the spec
        // form `exp(-age / half_life)`. Single evidence c=0.9 yields
        // weighted ≈ 0.331, confidence = 1 - (1 - 0.331) = 0.331.
        let ts = NOW.saturating_sub(ONE_YEAR_NS);
        let e = [evi(0.9, ts)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Fact, &cfg());
        let expected = 0.9 * (-1.0f32).exp(); // 0.9 / e
        assert!((r - expected).abs() < 5e-3, "got {r}, expected {expected}");
    }

    #[test]
    fn preference_at_60_day_age() {
        let ts = NOW.saturating_sub(SIXTY_DAYS_NS);
        let e = [evi(0.9, ts)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Preference, &cfg());
        let expected = 0.9 * (-1.0f32).exp();
        assert!((r - expected).abs() < 5e-3, "got {r}, expected {expected}");
    }

    #[test]
    fn event_no_decay_at_five_year_age() {
        let ts = NOW.saturating_sub(FIVE_YEARS_NS);
        let e = [evi(0.9, ts)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Event, &cfg());
        assert!((r - 0.9).abs() < 1e-3, "got {r}");
    }

    #[test]
    fn hundred_evidence_each_0_1_no_decay() {
        let e: Vec<EvidenceEntry> = (0..100).map(|_| evi(0.1, NOW)).collect();
        let r = aggregate_confidence(&e, NOW, StatementKind::Event, &cfg());
        // 1 - (0.9)^100 ≈ 0.99997
        assert!(r >= 0.99, "got {r}");
    }

    #[test]
    fn future_timestamp_clamps_to_zero_age() {
        // Evidence "from the future" — clock skew. saturating_sub
        // pins age to 0; decay = 1.0; result = c_i.
        let ts = NOW.saturating_add(60 * 60 * 1_000_000_000); // 1 hour ahead
        let e = [evi(0.5, ts)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Fact, &cfg());
        assert!((r - 0.5).abs() < 1e-3, "got {r}");
    }

    #[test]
    fn single_evidence_zero_confidence() {
        let e = [evi(0.0, NOW)];
        let r = aggregate_confidence(&e, NOW, StatementKind::Fact, &cfg());
        assert_eq!(r, 0.0);
    }

}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::ids::ExtractorId;
    use crate::{ContextId, MemoryId};
    use proptest::prelude::*;

    fn evi_strategy() -> impl Strategy<Value = EvidenceEntry> {
        (0.0f32..=1.0f32, 0u64..2_000_000_000_000_000_000u64).prop_map(|(c, ts)| {
            EvidenceEntry::from_parts(
                MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
                c,
                ts,
                ExtractorId::from(0),
            )
        })
    }

    proptest! {
        #[test]
        fn confidence_in_unit_interval(
            evidence in proptest::collection::vec(evi_strategy(), 0..50),
            now in 0u64..2_500_000_000_000_000_000u64,
            kind_byte in 0u8..6u8,
        ) {
            let kind = StatementKind::from_u8(kind_byte);
            let r = aggregate_confidence(&evidence, now, kind, &ConfidenceConfig::default_v1());
            prop_assert!((0.0..=1.0).contains(&r) && !r.is_nan(), "got {r}");
        }

        #[test]
        fn confidence_monotonic_in_evidence(
            mut evidence in proptest::collection::vec(evi_strategy(), 1..20),
            now in 1_500_000_000_000_000_000u64..2_500_000_000_000_000_000u64,
            kind_byte in 0u8..6u8,
        ) {
            let kind = StatementKind::from_u8(kind_byte);
            let cfg = ConfidenceConfig::default_v1();
            let base = aggregate_confidence(&evidence, now, kind, &cfg);
            // Append a duplicate of the first entry; confidence must
            // not decrease.
            let dup = evidence[0];
            evidence.push(dup);
            let extended = aggregate_confidence(&evidence, now, kind, &cfg);
            prop_assert!(extended + 1e-5 >= base, "extended {extended} < base {base}");
        }
    }
}
