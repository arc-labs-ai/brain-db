//! Typed-graph enums that map to byte discriminants.
//!
//! Like `MemoryKind` in the substrate, these enums are encoded as
//! single bytes in redb composite keys and in WAL frame bodies. The
//! discriminants are stable and append-only.

use serde::{Deserialize, Serialize};

/// The statement kinds — the *shape* of a fact, not its topic.
///
/// A memory question is answered by the shape of the data, so we classify
/// every statement into one of a small, closed set of behavioral kinds.
/// The topic (the predicate, e.g. `works_at` / `donated_bone_marrow_to`) is
/// an open, unbounded space and is NOT part of this enum — it is captured
/// free-text and embedded. Storage and read-shape semantics derive from the
/// kind via [`KindBehavior`], never from the predicate.
///
/// Byte discriminants are stable and append-only. Bytes `0..=5` are the
/// built-in kinds; bytes `6..=255` are [`StatementKind::Custom`] ids
/// resolved against the per-shard kind registry (user-declared kinds carry
/// their own [`KindBehavior`]). This keeps the redb composite-key kind
/// column a single byte while leaving room for 250 user kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum StatementKind {
    /// Generic subject–predicate–object with no special cardinality or
    /// temporal semantics. The universal fallback: anything the classifier
    /// cannot place lands here, so ingestion never fails for lack of a kind.
    Fact,
    /// An entity ±wants a thing. Carries polarity; accumulates as a set.
    Preference,
    /// An entity did something at a time. Append-only; never supersedes.
    Event,
    /// An entity has a property with a value. Single-valued — a new value
    /// supersedes the prior current one ("where do they live *now*").
    Attribute,
    /// An entity is linked to another entity. Accumulates as a set.
    Relation,
    /// How the agent should behave for/about a subject. Single per key.
    Directive,
    /// A user-declared kind (byte `>= 6`), resolved against the kind
    /// registry for its [`KindBehavior`].
    Custom(u8),
}

impl StatementKind {
    /// First byte available for user-declared [`StatementKind::Custom`] kinds.
    pub const FIRST_CUSTOM_BYTE: u8 = 6;

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Fact => 0,
            Self::Preference => 1,
            Self::Event => 2,
            Self::Attribute => 3,
            Self::Relation => 4,
            Self::Directive => 5,
            Self::Custom(b) => b,
        }
    }

    /// Decode a kind byte. Every byte is valid: `0..=5` map to the built-in
    /// kinds, `6..=255` to a [`StatementKind::Custom`] id (the registry
    /// validates whether that id is actually declared).
    #[must_use]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::Fact,
            1 => Self::Preference,
            2 => Self::Event,
            3 => Self::Attribute,
            4 => Self::Relation,
            5 => Self::Directive,
            n => Self::Custom(n),
        }
    }

    /// True iff this is a built-in kind (byte `< FIRST_CUSTOM_BYTE`).
    #[must_use]
    pub const fn is_builtin(self) -> bool {
        !matches!(self, Self::Custom(_))
    }

    /// The behavioral semantics for a *built-in* kind. Returns `None` for
    /// [`StatementKind::Custom`] — those resolve via the kind registry in
    /// `brain-metadata`, which has access to the user's declaration.
    #[must_use]
    pub const fn builtin_behavior(self) -> Option<KindBehavior> {
        let b = match self {
            Self::Attribute => KindBehavior::new(KindCardinality::Single, TemporalModel::State, false),
            Self::Relation => KindBehavior::new(KindCardinality::Set, TemporalModel::State, false),
            Self::Preference => KindBehavior::new(KindCardinality::Set, TemporalModel::State, true),
            Self::Event => KindBehavior::new(KindCardinality::Set, TemporalModel::Event, false),
            Self::Directive => KindBehavior::new(KindCardinality::Single, TemporalModel::State, false),
            Self::Fact => KindBehavior::new(KindCardinality::Set, TemporalModel::Atemporal, false),
            Self::Custom(_) => return None,
        };
        Some(b)
    }
}

// ---------------------------------------------------------------------------
// KindBehavior — the per-kind storage / read-shape semantics.
// ---------------------------------------------------------------------------

/// How many current values a `(subject, predicate)` pair may hold.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum KindCardinality {
    /// At most one current value; a new assertion supersedes the prior one.
    /// Drives single-answer reads ("what is X's Y now").
    Single = 0,
    /// Values accumulate; reads return the whole current set.
    Set = 1,
}

impl KindCardinality {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Single,
            1 => Self::Set,
            _ => return None,
        })
    }

    /// True iff a new assertion supersedes the prior current value.
    #[must_use]
    pub const fn is_single(self) -> bool {
        matches!(self, Self::Single)
    }
}

/// How a kind relates to time.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum TemporalModel {
    /// A standing condition with bi-temporal validity; supersedable.
    State = 0,
    /// A point-in-time happening: `event_at` is required and the statement
    /// is append-only (never supersedes).
    Event = 1,
    /// No temporal dimension (generic facts).
    Atemporal = 2,
}

impl TemporalModel {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::State,
            1 => Self::Event,
            2 => Self::Atemporal,
            _ => return None,
        })
    }

    /// True iff statements of this kind are append-only (never supersede)
    /// and require an `event_at` timestamp.
    #[must_use]
    pub const fn is_event(self) -> bool {
        matches!(self, Self::Event)
    }
}

/// The behavioral contract of a statement kind. Built-in kinds get a const
/// default ([`StatementKind::builtin_behavior`]); user-declared kinds supply
/// their own via the schema and store it in the kind registry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct KindBehavior {
    /// Single (supersede) vs Set (accumulate).
    pub cardinality: KindCardinality,
    /// Relationship to time (State / Event / Atemporal).
    pub temporal: TemporalModel,
    /// Whether the kind carries a +/- polarity (Preferences do).
    pub polarity: bool,
}

impl KindBehavior {
    #[must_use]
    pub const fn new(cardinality: KindCardinality, temporal: TemporalModel, polarity: bool) -> Self {
        Self {
            cardinality,
            temporal,
            polarity,
        }
    }

    /// True iff a new `(subject, predicate)` assertion supersedes the prior
    /// current value (single-valued kinds).
    #[must_use]
    pub const fn supersedes(self) -> bool {
        self.cardinality.is_single() && !self.temporal.is_event()
    }
}

/// Relation cardinality.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum Cardinality {
    OneToOne = 0,
    OneToMany = 1,
    ManyToOne = 2,
    ManyToMany = 3,
}

impl Cardinality {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::OneToOne,
            1 => Self::OneToMany,
            2 => Self::ManyToOne,
            3 => Self::ManyToMany,
            _ => return None,
        })
    }
}

/// Extractor tier. Tiered fallback: pattern → classifier →
/// LLM. Each tier is cheaper than the next.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum ExtractorKind {
    Pattern = 0,
    Classifier = 1,
    Llm = 2,
}

impl ExtractorKind {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Pattern,
            1 => Self::Classifier,
            2 => Self::Llm,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_kind_round_trip() {
        for k in [
            StatementKind::Fact,
            StatementKind::Preference,
            StatementKind::Event,
            StatementKind::Attribute,
            StatementKind::Relation,
            StatementKind::Directive,
            StatementKind::Custom(6),
            StatementKind::Custom(255),
        ] {
            assert_eq!(StatementKind::from_u8(k.as_u8()), k);
        }
        // Built-in bytes decode to built-ins; >= 6 decodes to Custom.
        assert_eq!(StatementKind::from_u8(3), StatementKind::Attribute);
        assert_eq!(StatementKind::from_u8(255), StatementKind::Custom(255));
        assert!(StatementKind::Attribute.is_builtin());
        assert!(!StatementKind::Custom(7).is_builtin());
    }

    #[test]
    fn builtin_behavior_defaults() {
        // Attribute: single-valued state (supersedes).
        let a = StatementKind::Attribute.builtin_behavior().unwrap();
        assert_eq!(a.cardinality, KindCardinality::Single);
        assert_eq!(a.temporal, TemporalModel::State);
        assert!(a.supersedes());

        // Event: append-only, requires event_at (never supersedes).
        let e = StatementKind::Event.builtin_behavior().unwrap();
        assert_eq!(e.temporal, TemporalModel::Event);
        assert!(!e.supersedes());

        // Preference carries polarity and accumulates.
        let p = StatementKind::Preference.builtin_behavior().unwrap();
        assert!(p.polarity);
        assert_eq!(p.cardinality, KindCardinality::Set);
        assert!(!p.supersedes());

        // Directive: single per key.
        assert!(StatementKind::Directive.builtin_behavior().unwrap().supersedes());

        // Custom kinds resolve via the registry, not here.
        assert!(StatementKind::Custom(9).builtin_behavior().is_none());
    }

}
