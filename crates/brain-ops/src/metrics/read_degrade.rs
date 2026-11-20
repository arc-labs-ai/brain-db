//! Rows a read path returned in degraded form.
//!
//! Two projections name a row's vocabulary from a registry, and one
//! resolves a statement's citations from an overflow row. When the thing
//! they read is gone, the read does not fail: it returns the row with the
//! missing part blanked, because one broken row must not deny a caller the
//! healthy rows beside it on the same page.
//!
//! That choice trades a denied page for a quiet lie. The degraded row is
//! shaped like a healthy one, so nothing downstream can tell them apart —
//! an empty evidence list is an ordinary value for a statement that cites
//! nothing, and while an empty qname is a value no write can produce, a
//! client has no way to know that. This family is the only signal that it
//! happened: a rate an operator can alert on, split by which registry
//! broke, because the remedies do not match. A missing predicate or
//! relation-type row is a schema-catalog problem; a missing overflow row is
//! an evidence-storage problem.
//!
//! Process-global on purpose. The counts answer "is the store rotting, and
//! how fast", which is a property of the process an operator scrapes, and
//! the projections that feed it are free functions with no shard handle in
//! scope. The paired `tracing::warn!` at each site carries the identifier
//! of the specific broken row, which is what an operator chases once the
//! rate tells them to look.

use std::sync::atomic::{AtomicU64, Ordering};

/// Which part of a row a read path could not resolve. Discriminants are
/// append-only — they index counter slots the `/metrics` exposition reads
/// by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The shared `Missing` postfix is the point: each variant names a thing that
// was absent, and the names mirror the label strings in
// `READ_DEGRADE_REASON_LABELS` one for one, which is what an operator reads
// off `/metrics`. Dropping the postfix would break that correspondence.
#[allow(clippy::enum_variant_names)]
pub enum ReadDegradeReason {
    /// A statement's predicate has no row in the predicate registry, so the
    /// statement projects with an empty predicate qname.
    PredicateQnameMissing = 0,
    /// A relation's type has no row in the relation-type registry, so the
    /// edge projects with an empty relation-type qname.
    RelationTypeQnameMissing = 1,
    /// A statement points at an evidence-overflow row the store no longer
    /// has, so the statement projects citing nothing.
    EvidenceOverflowMissing = 2,
}

/// Label values published by the degraded-row counter family, indexed by
/// [`ReadDegradeReason`].
pub const READ_DEGRADE_REASON_LABELS: &[&str] = &[
    "predicate_qname_missing",
    "relation_type_qname_missing",
    "evidence_overflow_missing",
];

/// Counter slots in the family, one per [`ReadDegradeReason`].
pub const READ_DEGRADE_REASON_COUNT: usize = READ_DEGRADE_REASON_LABELS.len();

impl ReadDegradeReason {
    fn idx(self) -> usize {
        self as usize
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        READ_DEGRADE_REASON_LABELS[self.idx()]
    }
}

/// Degraded rows served, by reason. One row counted per projection, so a
/// page of ten orphans counts ten — the rate is in rows served to clients,
/// not in distinct broken registry entries.
#[derive(Debug)]
pub struct ReadDegradeMetrics {
    rows_total: [AtomicU64; READ_DEGRADE_REASON_COUNT],
}

impl ReadDegradeMetrics {
    /// Construct a zeroed instance.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows_total: [const { AtomicU64::new(0) }; READ_DEGRADE_REASON_COUNT],
        }
    }

    /// Record one row served in degraded form.
    pub fn inc(&self, reason: ReadDegradeReason) {
        self.rows_total[reason.idx()].fetch_add(1, Ordering::Relaxed);
    }

    /// Read-only snapshot for `/metrics`.
    #[must_use]
    pub fn snapshot(&self) -> ReadDegradeMetricsSnapshot {
        let mut rows_total = [0u64; READ_DEGRADE_REASON_COUNT];
        for (slot, counter) in rows_total.iter_mut().zip(self.rows_total.iter()) {
            *slot = counter.load(Ordering::Relaxed);
        }
        ReadDegradeMetricsSnapshot { rows_total }
    }
}

impl Default for ReadDegradeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-data snapshot of [`ReadDegradeMetrics`]. `rows_total` is indexed
/// in [`READ_DEGRADE_REASON_LABELS`] order.
#[derive(Debug, Clone)]
pub struct ReadDegradeMetricsSnapshot {
    pub rows_total: [u64; READ_DEGRADE_REASON_COUNT],
}

impl ReadDegradeMetricsSnapshot {
    /// The count for one reason.
    #[must_use]
    pub fn rows(&self, reason: ReadDegradeReason) -> u64 {
        self.rows_total[reason as usize]
    }
}

static GLOBAL: ReadDegradeMetrics = ReadDegradeMetrics::new();

/// The process-wide degraded-row counters. Read projections bump these;
/// `/metrics` exposition snapshots them.
#[must_use]
pub fn global() -> &'static ReadDegradeMetrics {
    &GLOBAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_degrade_counters_round_trip() {
        let m = ReadDegradeMetrics::new();
        m.inc(ReadDegradeReason::PredicateQnameMissing);
        m.inc(ReadDegradeReason::EvidenceOverflowMissing);
        m.inc(ReadDegradeReason::EvidenceOverflowMissing);
        let s = m.snapshot();
        assert_eq!(s.rows(ReadDegradeReason::PredicateQnameMissing), 1);
        assert_eq!(s.rows(ReadDegradeReason::RelationTypeQnameMissing), 0);
        assert_eq!(s.rows(ReadDegradeReason::EvidenceOverflowMissing), 2);
    }

    #[test]
    fn read_degrade_reason_labels_match_discriminants() {
        assert_eq!(
            ReadDegradeReason::PredicateQnameMissing.label(),
            "predicate_qname_missing"
        );
        assert_eq!(
            ReadDegradeReason::RelationTypeQnameMissing.label(),
            "relation_type_qname_missing"
        );
        assert_eq!(
            ReadDegradeReason::EvidenceOverflowMissing.label(),
            "evidence_overflow_missing"
        );
    }
}
