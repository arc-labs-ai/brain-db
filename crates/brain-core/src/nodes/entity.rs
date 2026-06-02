//! `Entity` value type — the brain-core public API for a typed-graph
//! entity.
//!
//! Mirrors the substrate's [`crate::Memory`] / `brain_metadata::tables::memory`
//! split: `Entity` is the high-level value type (no I/O, no rkyv); the
//! redb row lives in `brain-metadata::tables::nodes::entity::EntityMetadata`.
//! Conversion at the boundary is via `From` impls defined on the
//! brain-metadata side.

use serde::{Deserialize, Serialize};

use crate::ids::{EntityId, EntityTypeId};

// ---------------------------------------------------------------------------
// EntityAttributes — opaque-blob newtype.
// ---------------------------------------------------------------------------

/// Free-form per-entity key/value attribute bag.
///
/// Currently an opaque `Vec<u8>` (an rkyv-encoded
/// `BTreeMap<String, Value>` once the schema DSL lands). The
/// newtype isolates callers from the encoding so typed accessors can
/// be added later without changing the public field type.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EntityAttributes(pub Vec<u8>);

impl EntityAttributes {
    /// An empty attribute bag.
    #[must_use]
    pub const fn empty() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume the wrapper and return the inner blob.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Borrow the inner blob.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for EntityAttributes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<EntityAttributes> for Vec<u8> {
    fn from(a: EntityAttributes) -> Self {
        a.0
    }
}

// ---------------------------------------------------------------------------
// EntityType — registry entry for a user-declared (or built-in) entity type.
// ---------------------------------------------------------------------------

/// A registered entity type. Currently only the built-in `Person`
/// type exists (seeded by `MetadataDb::open`); the schema DSL
/// adds user-declared types via `SCHEMA_UPLOAD`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntityType {
    pub id: EntityTypeId,
    pub name: String,
    /// rkyv-encoded attribute schema. The schema DSL defines the shape.
    pub attribute_schema_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl EntityType {
    /// ID of the built-in `Person` type seeded by `MetadataDb::open`.
    /// User-declared types start at `EntityTypeId(2)` so this slot
    /// stays stable.
    pub const PERSON_ID: EntityTypeId = EntityTypeId(1);

    /// Canonical name of the built-in `Person` type.
    pub const PERSON_NAME: &'static str = "Person";

    /// Build the canonical `Person` type record for a given creation
    /// timestamp. Used by `MetadataDb::open`'s seed path.
    #[must_use]
    pub fn person(created_at_unix_nanos: u64) -> Self {
        Self {
            id: Self::PERSON_ID,
            name: Self::PERSON_NAME.to_owned(),
            attribute_schema_blob: Vec::new(),
            created_at_unix_nanos,
        }
    }
}

// ---------------------------------------------------------------------------
// Entity — the public value type.
// ---------------------------------------------------------------------------

/// A canonical reference to a noun in the typed graph.
///
/// Field semantics:
///
/// - `id` — immutable; survives renames.
/// - `entity_type` — the registry entry this entity instantiates;
///   mutation requires `RETYPE_ENTITY`.
/// - `canonical_name` — primary display name. Mutable; old values
///   move into `aliases` on rename.
/// - `normalized_name` — lowercased + whitespace-collapsed form for
///   exact-match lookup (`entity_by_canonical_name` index).
/// - `aliases` — alternative names that resolve to this entity. Length
///   caps at 32 by default; not enforced in the value type (the CRUD
///   layer enforces).
/// - `attributes` — opaque blob; typed accessors come with the schema DSL.
/// - `mention_count` — denormalized count of memories referencing
///   this entity; maintained by the `entity_put` paths.
/// - `merged_into` — `Some` if this entity has been merged into
///   another. Queries follow the redirect via the merge path.
/// - `embedding_version` — bumped on canonical_name change so the
///   embedding worker can detect stale vectors.
/// - `flags` — bitfield.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub entity_type: EntityTypeId,
    pub canonical_name: String,
    pub normalized_name: String,
    pub aliases: Vec<String>,
    pub attributes: EntityAttributes,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    pub merged_into: Option<EntityId>,
    pub embedding_version: u32,
    pub flags: u32,
}

impl Entity {
    /// Construct a fresh active entity. Sets `mention_count = 0`,
    /// `merged_into = None`, `embedding_version = 0`, empty aliases
    /// and attributes; `updated_at_unix_nanos == created_at_unix_nanos`.
    #[must_use]
    pub fn new_active(
        id: EntityId,
        entity_type: EntityTypeId,
        canonical_name: String,
        normalized_name: String,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            id,
            entity_type,
            canonical_name,
            normalized_name,
            aliases: Vec::new(),
            attributes: EntityAttributes::empty(),
            mention_count: 0,
            created_at_unix_nanos,
            updated_at_unix_nanos: created_at_unix_nanos,
            merged_into: None,
            embedding_version: 0,
            flags: 0,
        }
    }

    /// True iff this entity has been merged into another (queries
    /// follow the redirect via `merged_into`).
    #[must_use]
    pub fn is_merged(&self) -> bool {
        self.merged_into.is_some()
    }

    /// True iff `alias` matches any entry in [`Self::aliases`]
    /// exactly (no normalization; callers should pre-normalize).
    /// O(n) caps aliases at 32 per entity so a linear
    /// scan is faster than a HashSet for typical sizes.
    #[must_use]
    pub fn has_alias(&self, alias: &str) -> bool {
        self.aliases.iter().any(|a| a == alias)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_empty_round_trip() {
        let a = EntityAttributes::empty();
        assert!(a.is_empty());
        assert_eq!(a.as_bytes(), &[] as &[u8]);
        let bytes = a.into_bytes();
        assert!(bytes.is_empty());
    }

    #[test]
    fn attributes_from_vec_preserves_bytes() {
        let raw = vec![1u8, 2, 3, 4];
        let a = EntityAttributes::from(raw.clone());
        assert_eq!(a.as_bytes(), raw.as_slice());
        let back: Vec<u8> = a.into();
        assert_eq!(back, raw);
    }

    #[test]
    fn entity_type_person_id_is_stable() {
        assert_eq!(EntityType::PERSON_ID, EntityTypeId(1));
        assert_eq!(EntityType::PERSON_NAME, "Person");
    }

    #[test]
    fn entity_type_person_constructor_populates_fields() {
        let t = EntityType::person(1_700_000_000_000_000_000);
        assert_eq!(t.id, EntityType::PERSON_ID);
        assert_eq!(t.name, "Person");
        assert!(t.attribute_schema_blob.is_empty());
        assert_eq!(t.created_at_unix_nanos, 1_700_000_000_000_000_000);
    }

    #[test]
    fn entity_new_active_sets_defaults() {
        let id = EntityId::new();
        let e = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            "priya patel".into(),
            1_700_000_000_000_000_000,
        );
        assert_eq!(e.id, id);
        assert_eq!(e.entity_type, EntityType::PERSON_ID);
        assert_eq!(e.canonical_name, "Priya Patel");
        assert_eq!(e.normalized_name, "priya patel");
        assert!(e.aliases.is_empty());
        assert!(e.attributes.is_empty());
        assert_eq!(e.mention_count, 0);
        assert_eq!(e.created_at_unix_nanos, e.updated_at_unix_nanos);
        assert!(!e.is_merged());
        assert_eq!(e.embedding_version, 0);
        assert_eq!(e.flags, 0);
    }

    #[test]
    fn entity_has_alias_scans_aliases() {
        let mut e = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "Priya Patel".into(),
            "priya patel".into(),
            0,
        );
        assert!(!e.has_alias("priya"));
        e.aliases.push("priya".to_owned());
        e.aliases.push("p. patel".to_owned());
        assert!(e.has_alias("priya"));
        assert!(e.has_alias("p. patel"));
        assert!(!e.has_alias("PRIYA")); // exact match; callers normalize
    }

    #[test]
    fn entity_is_merged_reports_redirect() {
        let mut e = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            "old".into(),
            "old".into(),
            0,
        );
        assert!(!e.is_merged());
        e.merged_into = Some(EntityId::new());
        assert!(e.is_merged());
    }
}
