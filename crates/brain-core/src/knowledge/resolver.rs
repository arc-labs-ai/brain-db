//! Entity-resolver types. **Types only** — algorithm lands in
//! sub-task 16.5 (`fn resolve(...) -> ResolutionOutcome`).
//!
//! See `spec/18_entities/01_resolution.md` for the full algorithm,
//! configuration semantics, and ambiguity-handling rules.

use serde::{Deserialize, Serialize};

use crate::knowledge::{AuditId, EntityId};

// ---------------------------------------------------------------------------
// ResolverTier.
// ---------------------------------------------------------------------------

/// Which tier of the resolver pipeline produced an outcome (spec
/// §18/01). `Created` is a side-effect, not a tier in the strict
/// sense — included for completeness so audit records carry a
/// single enum.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[repr(u8)]
pub enum ResolverTier {
    Exact = 0,
    Fuzzy = 1,
    Embedding = 2,
    Llm = 3,
    Created = 4,
}

impl ResolverTier {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Exact,
            1 => Self::Fuzzy,
            2 => Self::Embedding,
            3 => Self::Llm,
            4 => Self::Created,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// TypeConstraint.
// ---------------------------------------------------------------------------

/// How strictly the resolver honors the caller's `entity_type_hint`
/// (spec §18/01 §Configuration).
///
/// - `Strict` — candidates must match the hint; cross-type matches
///   are not considered.
/// - `Hint` — prefer the hinted type; fall back across types if no
///   in-type match.
/// - `None` — ignore the hint entirely.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
pub enum TypeConstraint {
    Strict,
    /// Default per spec.
    #[default]
    Hint,
    None,
}

// ---------------------------------------------------------------------------
// ResolutionOutcome.
// ---------------------------------------------------------------------------

/// The three possible outcomes of a resolution call (spec §18/01).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResolutionOutcome {
    /// Single high-confidence candidate found.
    Resolved {
        entity: EntityId,
        confidence: f32,
        tier: ResolverTier,
    },
    /// Multiple plausible candidates; resolution deferred for human
    /// or async-worker review. An audit record is written before
    /// returning this variant.
    Ambiguous {
        audit_id: AuditId,
        candidates: Vec<(EntityId, f32)>,
    },
    /// No match above threshold; a new entity was created.
    Created { entity: EntityId },
}

impl ResolutionOutcome {
    /// `true` for `Resolved` outcomes; `false` for `Ambiguous` and
    /// `Created`.
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved { .. })
    }

    /// `true` for `Created` outcomes only.
    #[must_use]
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created { .. })
    }
}

// ---------------------------------------------------------------------------
// ResolverConfig.
// ---------------------------------------------------------------------------

/// Resolver configuration. Defaults match spec §18/01 §Configuration.
/// Per-extractor overrides land in phase 20.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolverConfig {
    pub enable_exact: bool,
    pub enable_fuzzy: bool,
    pub fuzzy_threshold: f32,
    pub enable_embedding: bool,
    pub embedding_threshold: f32,
    pub embedding_top_k: usize,
    pub enable_llm: bool,
    pub llm_threshold: f32,
    pub create_confidence: f32,
    pub type_constraint: TypeConstraint,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        // Per spec §18/01 §Configuration.
        Self {
            enable_exact: true,
            enable_fuzzy: true,
            fuzzy_threshold: 0.85,
            enable_embedding: true,
            embedding_threshold: 0.78,
            embedding_top_k: 5,
            enable_llm: false,
            llm_threshold: 0.85,
            create_confidence: 0.6,
            type_constraint: TypeConstraint::Hint,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_tier_round_trip() {
        for t in [
            ResolverTier::Exact,
            ResolverTier::Fuzzy,
            ResolverTier::Embedding,
            ResolverTier::Llm,
            ResolverTier::Created,
        ] {
            assert_eq!(ResolverTier::from_u8(t.as_u8()), Some(t));
        }
        assert_eq!(ResolverTier::from_u8(5), None);
        assert_eq!(ResolverTier::from_u8(255), None);
    }

    #[test]
    fn type_constraint_default_is_hint() {
        assert_eq!(TypeConstraint::default(), TypeConstraint::Hint);
    }

    #[test]
    fn resolver_config_default_matches_spec() {
        let c = ResolverConfig::default();
        // Field-by-field check against spec §18/01.
        assert!(c.enable_exact);
        assert!(c.enable_fuzzy);
        assert!((c.fuzzy_threshold - 0.85).abs() < f32::EPSILON);
        assert!(c.enable_embedding);
        assert!((c.embedding_threshold - 0.78).abs() < f32::EPSILON);
        assert_eq!(c.embedding_top_k, 5);
        assert!(!c.enable_llm, "LLM defaults to off — cost control");
        assert!((c.llm_threshold - 0.85).abs() < f32::EPSILON);
        assert!((c.create_confidence - 0.6).abs() < f32::EPSILON);
        assert_eq!(c.type_constraint, TypeConstraint::Hint);
    }

    #[test]
    fn outcome_predicates() {
        let resolved = ResolutionOutcome::Resolved {
            entity: EntityId::new(),
            confidence: 1.0,
            tier: ResolverTier::Exact,
        };
        let created = ResolutionOutcome::Created {
            entity: EntityId::new(),
        };
        let ambiguous = ResolutionOutcome::Ambiguous {
            audit_id: AuditId::new(),
            candidates: vec![],
        };
        assert!(resolved.is_resolved());
        assert!(!resolved.is_created());
        assert!(created.is_created());
        assert!(!created.is_resolved());
        assert!(!ambiguous.is_resolved());
        assert!(!ambiguous.is_created());
    }
}
