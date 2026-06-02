//! Typed-graph enums that map to byte discriminants.
//!
//! Like `MemoryKind` in the substrate, these enums are encoded as
//! single bytes in redb composite keys and in WAL frame bodies. The
//! discriminants are stable and append-only.

use serde::{Deserialize, Serialize};

/// The three statement kinds.
///
/// Mutation rules differ by kind (Fact contradicts; Preference
/// supersedes; Event is append-only). All three share one storage
/// table with a `kind` discriminant column.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum StatementKind {
    Fact = 0,
    Preference = 1,
    Event = 2,
}

impl StatementKind {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Fact,
            1 => Self::Preference,
            2 => Self::Event,
            _ => return None,
        })
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
        ] {
            assert_eq!(StatementKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(StatementKind::from_u8(255), None);
    }

    #[test]
    fn cardinality_round_trip() {
        for c in [
            Cardinality::OneToOne,
            Cardinality::OneToMany,
            Cardinality::ManyToOne,
            Cardinality::ManyToMany,
        ] {
            assert_eq!(Cardinality::from_u8(c.as_u8()), Some(c));
        }
        assert_eq!(Cardinality::from_u8(4), None);
    }

    #[test]
    fn extractor_kind_round_trip() {
        for k in [
            ExtractorKind::Pattern,
            ExtractorKind::Classifier,
            ExtractorKind::Llm,
        ] {
            assert_eq!(ExtractorKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(ExtractorKind::from_u8(3), None);
    }
}
