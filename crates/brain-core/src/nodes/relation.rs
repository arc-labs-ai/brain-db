//! `Relation` value types — typed edges between entities.
//!
//! Pure value types — no I/O, no async, no rkyv. The rkyv-archived
//! storage shape lives in `brain-metadata::tables::relation`;
//! the wire-archived shape lives in `brain-protocol::ops::relation`.

use serde::{Deserialize, Serialize};

use crate::ids::{EntityId, EntityTypeId, ExtractorId, RelationId, RelationTypeId};
use crate::nodes::kinds::Cardinality;
use crate::MemoryId;

// ---------------------------------------------------------------------------
// RelationType — registry value type.
// ---------------------------------------------------------------------------

/// A registered relation type.
///
/// `from_type` / `to_type` constrain the endpoints to specific
/// entity types (e.g., `reports_to` requires both to be `Person`).
/// `None` means any entity type is allowed.
///
/// `is_symmetric` triggers canonical from/to ordering at write time
/// and dual-index population so reads from either endpoint
/// find the relation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationType {
    pub id: RelationTypeId,
    pub namespace: String,
    pub name: String,
    pub from_type: Option<EntityTypeId>,
    pub to_type: Option<EntityTypeId>,
    pub cardinality: Cardinality,
    pub is_symmetric: bool,
    pub schema_version: u32,
    pub description: String,
}

impl RelationType {
    /// Canonical wire form: `"namespace:name"`.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!("{}:{}", self.namespace, self.name)
    }
}

// ---------------------------------------------------------------------------
// Relation — the value type.
// ---------------------------------------------------------------------------

/// A typed edge between two entities.
///
/// Pure value type. The brain-metadata storage layer holds the rkyv-
/// archived form (`RelationMetadata`); the wire layer holds
/// `RelationView`. Conversion between this and those layers is the
/// respective layer's responsibility.
///
/// `is_symmetric` is mirrored from the row's `RelationType` for fast
/// access on the read path (the storage row carries the bit so
/// callers don't need to dereference the type registry for every
/// projection). Writers set it from the `RelationType` lookup at
/// create time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub id: RelationId,
    pub relation_type: RelationTypeId,
    pub from_entity: EntityId,
    pub to_entity: EntityId,

    /// Opaque properties blob. The schema DSL will define the
    /// typed shape; v1 stores an empty `Vec<u8>` by default.
    pub properties_blob: Vec<u8>,

    pub confidence: f32,
    pub evidence: Vec<MemoryId>,
    pub extractor_id: ExtractorId,
    pub extracted_at_unix_nanos: u64,

    /// Open-ended if `None`.
    pub valid_from_unix_nanos: Option<u64>,
    /// Open-ended if `None`. Set on supersession.
    pub valid_to_unix_nanos: Option<u64>,

    pub version: u32,
    pub superseded_by: Option<RelationId>,
    pub supersedes: Option<RelationId>,
    /// Chain root — id of the first relation in this
    /// chain. Self-referential for un-superseded relations.
    pub chain_root: RelationId,

    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: Option<u64>,

    /// Mirrored from `RelationType.is_symmetric` for fast read-path
    /// dispatch. Writers set this from the registry lookup.
    pub is_symmetric: bool,
}

impl Relation {
    /// Build a fresh, never-superseded relation. Sets:
    /// - `chain_root = id`
    /// - `version = 1`
    /// - `superseded_by / supersedes / tombstone fields = None / false`
    /// - `valid_from / valid_to = None` (caller may override)
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_root(
        id: RelationId,
        relation_type: RelationTypeId,
        from_entity: EntityId,
        to_entity: EntityId,
        confidence: f32,
        evidence: Vec<MemoryId>,
        extractor_id: ExtractorId,
        extracted_at_unix_nanos: u64,
        is_symmetric: bool,
    ) -> Self {
        Self {
            id,
            relation_type,
            from_entity,
            to_entity,
            properties_blob: Vec::new(),
            confidence,
            evidence,
            extractor_id,
            extracted_at_unix_nanos,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            version: 1,
            superseded_by: None,
            supersedes: None,
            chain_root: id,
            tombstoned: false,
            tombstoned_at_unix_nanos: None,
            is_symmetric,
        }
    }

    /// `true` iff the relation is current at `now`: not superseded,
    /// not tombstoned, and (if validity-bounded) `now` falls within
    /// `[valid_from, valid_to)`. Mirrors `Statement::is_current`.
    #[must_use]
    pub fn is_current(&self, now_unix_nanos: u64) -> bool {
        if self.tombstoned || self.superseded_by.is_some() {
            return false;
        }
        if let Some(start) = self.valid_from_unix_nanos {
            if now_unix_nanos < start {
                return false;
            }
        }
        if let Some(end) = self.valid_to_unix_nanos {
            if now_unix_nanos >= end {
                return false;
            }
        }
        true
    }

    /// `true` iff this relation is the chain root (never superseded).
    #[must_use]
    pub fn is_chain_root(&self) -> bool {
        self.supersedes.is_none() && self.chain_root == self.id
    }
}

// ---------------------------------------------------------------------------
// canonical_pair — symmetric canonicalisation.
// ---------------------------------------------------------------------------

/// Order a pair of EntityIds for symmetric-relation storage.
/// Returns `(min, max)` byte-wise.
///
/// Asymmetric relations pass `from, to` verbatim through this
/// function (or don't call it at all); only symmetric relations
/// require canonical ordering.
#[must_use]
pub fn canonical_pair(a: EntityId, b: EntityId) -> (EntityId, EntityId) {
    if a.to_bytes() <= b.to_bytes() {
        (a, b)
    } else {
        (b, a)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{EntityId, ExtractorId, RelationId, RelationTypeId};

    fn rid(byte: u8) -> RelationId {
        let mut b = [0u8; 16];
        b[15] = byte;
        RelationId::from_bytes(b)
    }

    fn eid(byte: u8) -> EntityId {
        let mut b = [0u8; 16];
        b[15] = byte;
        EntityId::from_bytes(b)
    }

    fn sample(symmetric: bool) -> Relation {
        Relation::new_root(
            rid(1),
            RelationTypeId::from(7),
            eid(2),
            eid(3),
            0.9,
            vec![],
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            symmetric,
        )
    }

    // ----- Constructor + defaults -----

    #[test]
    fn new_root_defaults() {
        let r = sample(false);
        assert_eq!(r.version, 1);
        assert_eq!(r.chain_root, r.id);
        assert!(r.supersedes.is_none());
        assert!(r.superseded_by.is_none());
        assert!(!r.tombstoned);
        assert!(r.valid_from_unix_nanos.is_none());
        assert!(r.valid_to_unix_nanos.is_none());
        assert!(r.is_chain_root());
    }

    // ----- is_current -----

    #[test]
    fn is_current_true_when_active() {
        let r = sample(false);
        assert!(r.is_current(1_700_000_000_000_000_001));
    }

    #[test]
    fn is_current_false_after_tombstone() {
        let mut r = sample(false);
        r.tombstoned = true;
        r.tombstoned_at_unix_nanos = Some(1_700_000_000_000_000_001);
        assert!(!r.is_current(1_700_000_000_000_000_002));
    }

    #[test]
    fn is_current_false_after_supersede() {
        let mut r = sample(false);
        r.superseded_by = Some(rid(99));
        assert!(!r.is_current(1_700_000_000_000_000_002));
    }

    #[test]
    fn is_current_respects_valid_window() {
        let mut r = sample(false);
        r.valid_from_unix_nanos = Some(100);
        r.valid_to_unix_nanos = Some(200);
        assert!(!r.is_current(50)); // before window
        assert!(r.is_current(150)); // inside
        assert!(!r.is_current(200)); // at end (exclusive)
        assert!(!r.is_current(250)); // after
    }

    #[test]
    fn is_chain_root_after_supersede_returns_false() {
        let mut r = sample(false);
        r.supersedes = Some(rid(50));
        r.chain_root = rid(50);
        r.version = 2;
        assert!(!r.is_chain_root());
    }

    // ----- canonical_pair -----

    #[test]
    fn canonical_pair_sorts_ascending() {
        let a = eid(1);
        let b = eid(2);
        assert_eq!(canonical_pair(a, b), (a, b));
        assert_eq!(canonical_pair(b, a), (a, b));
    }

    #[test]
    fn canonical_pair_handles_equal() {
        let a = eid(7);
        assert_eq!(canonical_pair(a, a), (a, a));
    }

    // ----- RelationType -----

    #[test]
    fn relation_type_canonical_form() {
        let rt = RelationType {
            id: RelationTypeId::from(1),
            namespace: "brain".into(),
            name: "related_to".into(),
            from_type: None,
            to_type: None,
            cardinality: Cardinality::ManyToMany,
            is_symmetric: false,
            schema_version: 1,
            description: "Generic relation".into(),
        };
        assert_eq!(rt.canonical(), "brain:related_to");
    }

    #[test]
    fn relation_type_symmetric_flag_round_trips() {
        let mut rt = RelationType {
            id: RelationTypeId::from(2),
            namespace: "test".into(),
            name: "discussed_with".into(),
            from_type: None,
            to_type: None,
            cardinality: Cardinality::ManyToMany,
            is_symmetric: true,
            schema_version: 1,
            description: String::new(),
        };
        assert!(rt.is_symmetric);
        rt.is_symmetric = false;
        assert!(!rt.is_symmetric);
    }

    #[test]
    fn relation_is_symmetric_mirrored_field() {
        let asym = sample(false);
        let sym = sample(true);
        assert!(!asym.is_symmetric);
        assert!(sym.is_symmetric);
    }
}
