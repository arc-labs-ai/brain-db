//! Precision decision — calibrated selective shaping over the RECALL membership set.
//!
//! Runs at the end of the read pipeline's shaping stage, after `build_membership`
//! has assembled and ordered the members and (optionally) committed a grounded
//! lead. It answers one question: *given how strongly each member is corroborated,
//! should the DB commit a `Single`/`Many` answer, and how much of the set is the
//! answer — or should it abstain with an honest `None`?*
//!
//! The confidence signal is **cross-lane consensus** — the `support` count from
//! the fan-out (strong-semantic + lexical + graph + grounded + strong-HyPE). This
//! is the one signal shown to separate correct from wrong answers (research:
//! AUROC ~0.66); the retrieval cosine / fused score does not (~0.57), so neither
//! is an input here. See `spec/13_retrievers/07_precision_engine.md`.
//!
//! The stage is always-on (C0), but its thresholds
//! ([`PrecisionTuning`](brain_core::PrecisionTuning)) default to no-ops, so an
//! uncalibrated deploy reproduces the pre-precision behaviour exactly. Everything
//! here is pure (no `ctx`, no I/O) and unit-tested.

use brain_core::{PrecisionTuning, MAX_SUPPORT};
use brain_protocol::ops::memory::AnswerKindWire;

/// The precision decision for one RECALL: the committed answer shape and how many
/// leading members constitute the answer (the rest are retained context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecisionDecision {
    /// The answer shape after the selective decision.
    pub shape: AnswerKindWire,
    /// How many leading members are the ANSWER. `0` for `None`; `1` for `Single`;
    /// the committed-set size for `Many`. Members beyond this are retained context
    /// (never dropped from the response — the standing grounded-first guardrail).
    pub lead_count: usize,
    /// `true` when the engine turned a would-be answer into `None` on the
    /// consensus gate (a weak, uncommitted lead). Observability only.
    pub abstained: bool,
}

/// Inputs the decision needs, all already computed by `build_membership`.
///
/// Note the "nothing belongs" abstention (a set of lone-cosine topical noise) is
/// deliberately NOT handled here — the RECALL handler owns it, with its own
/// anchor-state preconditions and its read-your-writes txn exemption. This stage
/// adds only the calibrated consensus gate and the `Many` trim on top.
pub struct DecisionInput<'a> {
    /// Per-member `support` (cross-lane consensus, `0..=MAX_SUPPORT`), in the
    /// FINAL member order (leads first). Empty ⇒ nothing to answer with.
    pub supports: &'a [u8],
    /// `Some(shape)` when a grounded commit fired (`Single` or `Many`); `None`
    /// when the set is uncommitted episodic membership.
    pub committed_shape: Option<AnswerKindWire>,
    /// Number of leading members the grounded commit put in front (the committed
    /// lead size). Ignored when `committed_shape` is `None`.
    pub committed_lead_count: usize,
}

/// Decide the answer shape and committed lead size.
///
/// Precedence:
/// 1. **Empty set** → `None`.
/// 2. **Consensus abstention** → `None` when the lead's support is below
///    `commit_min_support` AND no grounded commit fired. A grounded commit is
///    exempt: it already cleared its own corroboration gate
///    (`SUPPORT_CORROBORATED`). With the default `commit_min_support = 0` this
///    branch never fires — the pre-precision behaviour.
/// 3. **Committed shape** is honored (`Single` → 1 lead; `Many` → the committed
///    lead count, floored at 1).
/// 4. **Uncommitted** → shape by the trimmed answer set: the leading run of
///    members whose support clears `many_min_support`, always at least the lead
///    (index 0). One answer member ⇒ `Single`, several ⇒ `Many`. With the default
///    `many_min_support = 0` every member qualifies, reproducing the prior
///    pure-cardinality shape.
///
/// Pure and total; never panics. `supports` values above `MAX_SUPPORT` are treated
/// as `MAX_SUPPORT`. With the all-zero default tuning this returns the same shape
/// the pre-precision cardinality rule would, and never empties a non-empty set.
#[must_use]
pub fn decide(input: &DecisionInput, tuning: &PrecisionTuning) -> PrecisionDecision {
    let none = |abstained| PrecisionDecision {
        shape: AnswerKindWire::None,
        lead_count: 0,
        abstained,
    };

    // 1. Nothing to answer with → None (the handler's own abstention has already
    //    run; an empty set here just maps to the None shape).
    if input.supports.is_empty() {
        return none(false);
    }

    let clamp = |s: u8| s.min(MAX_SUPPORT);
    let lead_support = clamp(input.supports[0]);
    let commit_min = tuning.commit_min_support_clamped();
    let many_min = tuning.many_min_support_clamped();

    // 2. Consensus abstention — a weak, uncommitted lead becomes None.
    if input.committed_shape.is_none() && lead_support < commit_min {
        return none(true);
    }

    // 3. Grounded commit is authoritative on the shape.
    match input.committed_shape {
        Some(AnswerKindWire::Single) => PrecisionDecision {
            shape: AnswerKindWire::Single,
            lead_count: 1,
            abstained: false,
        },
        Some(AnswerKindWire::Many) => PrecisionDecision {
            shape: AnswerKindWire::Many,
            lead_count: input.committed_lead_count.max(1),
            abstained: false,
        },
        Some(AnswerKindWire::None) | None => {
            // 4. Uncommitted: shape from the trimmed answer set. Count the leading
            //    run whose support clears `many_min` (the lead always counts).
            let mut answer_n = 1usize;
            for &s in &input.supports[1..] {
                if clamp(s) >= many_min {
                    answer_n += 1;
                } else {
                    break;
                }
            }
            let shape = if answer_n <= 1 {
                AnswerKindWire::Single
            } else {
                AnswerKindWire::Many
            };
            PrecisionDecision {
                shape,
                lead_count: answer_n,
                abstained: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning(commit: u8, many: u8) -> PrecisionTuning {
        PrecisionTuning {
            commit_min_support: commit,
            many_min_support: many,
        }
    }
    fn inp<'a>(
        supports: &'a [u8],
        committed: Option<AnswerKindWire>,
        lead: usize,
    ) -> DecisionInput<'a> {
        DecisionInput {
            supports,
            committed_shape: committed,
            committed_lead_count: lead,
        }
    }

    // ── Default (no-op) tuning reproduces the prior behaviour ────────────────

    #[test]
    fn default_tuning_never_abstains_on_consensus() {
        // A lone weak lead (support 1): default keeps it (Single).
        let d = decide(&inp(&[1], None, 0), &PrecisionTuning::default());
        assert_eq!(d.shape, AnswerKindWire::Single);
        assert!(!d.abstained);
    }

    #[test]
    fn default_tuning_keeps_full_many() {
        // Three members, mixed support: default many_min=0 keeps all three.
        let d = decide(&inp(&[3, 1, 0], None, 0), &PrecisionTuning::default());
        assert_eq!(d.shape, AnswerKindWire::Many);
        assert_eq!(d.lead_count, 3);
    }

    // ── Empty → None ─────────────────────────────────────────────────────────

    #[test]
    fn empty_set_is_none() {
        let d = decide(&inp(&[], None, 0), &PrecisionTuning::default());
        assert_eq!(d.shape, AnswerKindWire::None);
        assert_eq!(d.lead_count, 0);
        assert!(!d.abstained, "empty is not a consensus abstention");
    }

    // ── Consensus abstention (calibrated) ────────────────────────────────────

    #[test]
    fn weak_uncommitted_lead_abstains_when_gated() {
        // commit_min=2, lead support 1 → None (abstained).
        let d = decide(&inp(&[1, 1], None, 0), &tuning(2, 0));
        assert_eq!(d.shape, AnswerKindWire::None);
        assert!(d.abstained);
    }

    #[test]
    fn strong_uncommitted_lead_commits_when_gated() {
        let d = decide(&inp(&[3, 1], None, 0), &tuning(2, 0));
        assert_ne!(d.shape, AnswerKindWire::None);
        assert!(!d.abstained);
    }

    #[test]
    fn grounded_commit_is_exempt_from_consensus_gate() {
        // Even with a high commit_min and a low lead support, a grounded Single
        // commit is honored — it cleared its own corroboration gate upstream.
        let d = decide(&inp(&[1], Some(AnswerKindWire::Single), 1), &tuning(5, 0));
        assert_eq!(d.shape, AnswerKindWire::Single);
        assert!(!d.abstained);
    }

    // ── Minimal-Many trimming ────────────────────────────────────────────────

    #[test]
    fn many_trims_to_leading_high_support_run() {
        // supports 3,2,0,2 with many_min=2 → answer = [3,2], the third breaks the run.
        let d = decide(&inp(&[3, 2, 0, 2], None, 0), &tuning(0, 2));
        assert_eq!(d.shape, AnswerKindWire::Many);
        assert_eq!(
            d.lead_count, 2,
            "trim stops at the first sub-threshold member"
        );
    }

    #[test]
    fn many_trims_to_single_when_only_lead_qualifies() {
        let d = decide(&inp(&[3, 1, 1], None, 0), &tuning(0, 2));
        assert_eq!(d.shape, AnswerKindWire::Single);
        assert_eq!(d.lead_count, 1);
    }

    #[test]
    fn lead_always_counts_even_below_many_min() {
        // Lead support 1 < many_min 3, but the lead is always the answer → Single.
        let d = decide(&inp(&[1], None, 0), &tuning(0, 3));
        assert_eq!(d.shape, AnswerKindWire::Single);
        assert_eq!(d.lead_count, 1);
    }

    // ── Committed Many keeps its committed lead count ────────────────────────

    #[test]
    fn committed_many_uses_committed_lead_count() {
        let d = decide(
            &inp(&[3, 3, 2], Some(AnswerKindWire::Many), 2),
            &tuning(0, 5),
        );
        assert_eq!(d.shape, AnswerKindWire::Many);
        assert_eq!(d.lead_count, 2, "committed set size wins over the trim");
    }

    #[test]
    fn committed_many_floors_lead_count_at_one() {
        let d = decide(&inp(&[2], Some(AnswerKindWire::Many), 0), &tuning(0, 0));
        assert_eq!(d.lead_count, 1);
    }

    // ── Support values above MAX_SUPPORT are clamped, never panic ─────────────

    #[test]
    fn oversized_support_is_clamped() {
        let d = decide(&inp(&[250], None, 0), &tuning(5, 0));
        // 250 clamps to MAX_SUPPORT (5) >= commit_min 5 → commits.
        assert_ne!(d.shape, AnswerKindWire::None);
    }
}
