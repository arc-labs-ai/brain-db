//! Entity family — 5 tables.
//!
//! See `spec/18_entities/` (record + resolution) and
//! `spec/26_knowledge_storage/00_purpose.md` (table catalog).
//!
//! - [`ENTITIES_TABLE`]                — primary `EntityId → EntityMetadata`.
//! - [`ENTITY_BY_CANONICAL_NAME_TABLE`] — exact-match resolution.
//! - [`ENTITY_ALIASES_TABLE`]          — alias resolution (multi-value via key).
//! - [`ENTITY_TRIGRAMS_TABLE`]         — fuzzy resolution via trigram index.
//! - [`ENTITY_MENTIONS_TABLE`]         — reverse index (which memories mention an entity).
//!
//! Phase 15.1 — types only. Phase 16 (entity layer) wires the resolver
//! and the typed CRUD around these tables.

use crate::impl_redb_rkyv_value;
use brain_core::{Entity, EntityAttributes, EntityId, EntityTypeId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const ENTITIES_TABLE: TableDefinition<'static, [u8; 16], EntityMetadata> =
    TableDefinition::new("entities");

/// `(entity_type_id, normalized_name)` → `EntityId.to_bytes()`.
pub const ENTITY_BY_CANONICAL_NAME_TABLE: TableDefinition<
    'static,
    (u32, &'static str),
    [u8; 16],
> = TableDefinition::new("entity_by_canonical_name");

/// `(entity_type_id, normalized_alias, EntityId.to_bytes())` → `()`.
/// The EntityId in the key lets one alias map to multiple entities
/// (ambiguity surfaces to the resolver).
pub const ENTITY_ALIASES_TABLE: TableDefinition<
    'static,
    (u32, &'static str, [u8; 16]),
    (),
> = TableDefinition::new("entity_aliases");

/// `(entity_type_id, trigram, EntityId.to_bytes())` → `()`.
///
/// Trigrams are fixed 3-byte windows (pg_trgm-style, byte-level) per
/// spec §18/02. 15.1 declared this with `&'static str` for the trigram
/// component; sub-task 16.4 corrected the key shape to `[u8; 3]`.
pub const ENTITY_TRIGRAMS_TABLE: TableDefinition<
    'static,
    (u32, [u8; 3], [u8; 16]),
    (),
> = TableDefinition::new("entity_trigrams");

/// `(EntityId.to_bytes(), MemoryId.to_be_bytes())` → [`MentionMetadata`].
pub const ENTITY_MENTIONS_TABLE: TableDefinition<
    'static,
    ([u8; 16], [u8; 16]),
    MentionMetadata,
> = TableDefinition::new("entity_mentions");

// ---------------------------------------------------------------------------
// Status flags (sub-task 16.2).
// ---------------------------------------------------------------------------

/// Bits in [`EntityMetadata::flags`].
///
/// Reserved high bits are *zero* on new rows; setters mask only the
/// documented bits via `flags |= MASK` / `flags &= !MASK` patterns.
pub mod flags {
    /// Bit 0: entity has been tombstoned. Secondary indexes
    /// (`entity_by_canonical_name`, `entity_aliases`) are torn down on
    /// tombstone so the resolver never sees the row again. The primary
    /// row stays for audit + 16.7 unmerge.
    pub const TOMBSTONED: u32 = 1 << 0;

    /// Bit 1: entity has been merged into another. Redundant with
    /// `merged_into_bytes.is_some()`; kept as a flag bit so
    /// flag-scan filters in 16.5+ don't have to deref the option.
    /// Set by 16.7; not used in 16.2.
    pub const MERGED: u32 = 1 << 1;

    /// Bits 2..=31 reserved.
    pub const RESERVED_MASK: u32 = !(TOMBSTONED | MERGED);
}

// ---------------------------------------------------------------------------
// Mention context discriminant.
// ---------------------------------------------------------------------------

/// `MentionMetadata::mention_context` byte values.
pub mod mention_context {
    /// Entity appears as the subject of a Statement.
    pub const SUBJECT_OF: u8 = 0;
    /// Entity appears as the object of a Statement.
    pub const OBJECT_OF: u8 = 1;
    /// Entity is mentioned in the memory text but not the subject /
    /// object of any extracted Statement.
    pub const IN_TEXT: u8 = 2;
}

// ---------------------------------------------------------------------------
// Value structs.
// ---------------------------------------------------------------------------

/// Primary entity record (spec §18 §"Entity record schema").
///
/// Sub-task 16.1 promoted `aliases` from an opaque `Vec<u8>` blob to a
/// typed `Vec<String>` and bumped `type_name` to `::v2`. `attributes`
/// remains an opaque blob until phase 19's schema DSL defines the
/// typed `Value` union.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EntityMetadata {
    pub entity_id_bytes: [u8; 16],
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub normalized_name: String,
    /// Spec §18/00 caps the alias list at 32 by default; not enforced
    /// at this layer (CRUD in 16.2 enforces).
    pub aliases: Vec<String>,
    /// rkyv-encoded `BTreeMap<String, Value>` (Value union resolves in phase 19).
    pub attributes_blob: Vec<u8>,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    /// `Some(_)` if this entity has been merged into another.
    pub merged_into_bytes: Option<[u8; 16]>,
    pub embedding_version: u32,
    pub flags: u32,
}

impl EntityMetadata {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_active(
        entity_id: EntityId,
        entity_type_id: EntityTypeId,
        canonical_name: String,
        normalized_name: String,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            entity_id_bytes: entity_id.to_bytes(),
            entity_type_id: entity_type_id.raw(),
            canonical_name,
            normalized_name,
            aliases: Vec::new(),
            attributes_blob: Vec::new(),
            mention_count: 0,
            created_at_unix_nanos,
            updated_at_unix_nanos: created_at_unix_nanos,
            merged_into_bytes: None,
            embedding_version: 0,
            flags: 0,
        }
    }

    #[must_use]
    pub fn entity_id(&self) -> EntityId {
        EntityId::from(self.entity_id_bytes)
    }

    #[must_use]
    pub fn entity_type(&self) -> EntityTypeId {
        EntityTypeId::from(self.entity_type_id)
    }

    #[must_use]
    pub fn merged_into(&self) -> Option<EntityId> {
        self.merged_into_bytes.map(EntityId::from)
    }

    /// Append an alias to this entity (no dedup or normalization;
    /// callers should pre-normalize). The on-rename caller in 16.2
    /// uses this to move an old canonical_name into the alias list.
    pub fn add_alias(&mut self, alias: String) {
        self.aliases.push(alias);
    }
}

impl_redb_rkyv_value!(EntityMetadata, "brain_metadata::EntityMetadata::v2");

// ---------------------------------------------------------------------------
// brain-core ↔ brain-metadata boundary conversions (sub-task 16.1).
// ---------------------------------------------------------------------------

impl From<&Entity> for EntityMetadata {
    fn from(e: &Entity) -> Self {
        Self {
            entity_id_bytes: e.id.to_bytes(),
            entity_type_id: e.entity_type.raw(),
            canonical_name: e.canonical_name.clone(),
            normalized_name: e.normalized_name.clone(),
            aliases: e.aliases.clone(),
            attributes_blob: e.attributes.as_bytes().to_vec(),
            mention_count: e.mention_count,
            created_at_unix_nanos: e.created_at_unix_nanos,
            updated_at_unix_nanos: e.updated_at_unix_nanos,
            merged_into_bytes: e.merged_into.map(EntityId::to_bytes),
            embedding_version: e.embedding_version,
            flags: e.flags,
        }
    }
}

impl From<&EntityMetadata> for Entity {
    fn from(m: &EntityMetadata) -> Self {
        Self {
            id: m.entity_id(),
            entity_type: m.entity_type(),
            canonical_name: m.canonical_name.clone(),
            normalized_name: m.normalized_name.clone(),
            aliases: m.aliases.clone(),
            attributes: EntityAttributes(m.attributes_blob.clone()),
            mention_count: m.mention_count,
            created_at_unix_nanos: m.created_at_unix_nanos,
            updated_at_unix_nanos: m.updated_at_unix_nanos,
            merged_into: m.merged_into(),
            embedding_version: m.embedding_version,
            flags: m.flags,
        }
    }
}

/// Per-mention metadata: how an entity appears in a given memory.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MentionMetadata {
    pub mentioned_at_unix_nanos: u64,
    pub mention_context: u8,
    pub confidence: f32,
    /// Byte offset of the mention within the memory text; `0` if not tracked.
    pub text_offset: u32,
    /// Byte length of the mention; `0` if not tracked.
    pub text_length: u32,
}

impl MentionMetadata {
    #[must_use]
    pub fn new(mentioned_at_unix_nanos: u64, context: u8, confidence: f32) -> Self {
        Self {
            mentioned_at_unix_nanos,
            mention_context: context,
            confidence,
            text_offset: 0,
            text_length: 0,
        }
    }
}

impl_redb_rkyv_value!(MentionMetadata, "brain_metadata::MentionMetadata::v1");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use brain_core::MemoryId;
    use redb::ReadableDatabase;

    #[test]
    fn entities_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let e = EntityMetadata::new_active(
            id,
            EntityTypeId::from(1),
            "Priya Patel".into(),
            "priya patel".into(),
            1_700_000_000_000_000_000,
        );
        let key = e.entity_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITIES_TABLE).unwrap();
            t.insert(&key, &e).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITIES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, e);
        assert_eq!(got.entity_id(), id);
    }

    #[test]
    fn aliases_round_trip() {
        // Sub-task 16.1: aliases moved from `Vec<u8>` blob to
        // `Vec<String>`. Verify the typed field round-trips through
        // rkyv + redb.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let mut e = EntityMetadata::new_active(
            id,
            EntityTypeId::from(1),
            "Priya Patel".into(),
            "priya patel".into(),
            1_700_000_000_000_000_000,
        );
        e.add_alias("priya".into());
        e.add_alias("p. patel".into());
        e.add_alias("priya p.".into());
        let key = e.entity_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITIES_TABLE).unwrap();
            t.insert(&key, &e).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITIES_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.aliases.len(), 3);
        assert_eq!(got.aliases[0], "priya");
        assert_eq!(got.aliases[1], "p. patel");
        assert_eq!(got.aliases[2], "priya p.");
    }

    #[test]
    fn entity_round_trip_through_brain_core() {
        // Sub-task 16.1: `From<&Entity> for EntityMetadata` and the
        // reverse must preserve every field. Build a fully-populated
        // `brain_core::Entity`, convert to `EntityMetadata`, convert
        // back, assert equality.
        use brain_core::{Entity, EntityAttributes};
        let id = EntityId::new();
        let merged_into = EntityId::new();
        let mut e = Entity::new_active(
            id,
            EntityTypeId::from(1),
            "Priya Patel".into(),
            "priya patel".into(),
            1_700_000_000_000_000_000,
        );
        e.aliases.push("priya".into());
        e.aliases.push("p. patel".into());
        e.attributes = EntityAttributes::from(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        e.mention_count = 7;
        e.updated_at_unix_nanos = 1_700_000_000_000_000_500;
        e.merged_into = Some(merged_into);
        e.embedding_version = 3;
        e.flags = 0b0001;

        let m: EntityMetadata = (&e).into();
        let back: Entity = (&m).into();
        assert_eq!(back, e);
    }

    #[test]
    fn canonical_name_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let key = (1u32, "priya patel");

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE).unwrap();
            t.insert(&key, &id.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_BY_CANONICAL_NAME_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(EntityId::from(got), id);
    }

    #[test]
    fn aliases_index_inserts_and_iterates() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id_a = EntityId::new();
        let id_b = EntityId::new();
        let alias = "p patel";
        let entity_type = 1u32;
        let k_a = (entity_type, alias, id_a.to_bytes());
        let k_b = (entity_type, alias, id_b.to_bytes());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_ALIASES_TABLE).unwrap();
            t.insert(&k_a, &()).unwrap();
            t.insert(&k_b, &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_ALIASES_TABLE).unwrap();
        assert!(t.get(&k_a).unwrap().is_some());
        assert!(t.get(&k_b).unwrap().is_some());
    }

    #[test]
    fn trigrams_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        // 16.4: trigram component is `[u8; 3]` (spec §18/02), not `&str`.
        let key = (1u32, *b"pri", id.to_bytes());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_TRIGRAMS_TABLE).unwrap();
            t.insert(&key, &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_TRIGRAMS_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }

    #[test]
    fn mentions_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let memory = MemoryId::pack(1, 100, 1);
        let m = MentionMetadata::new(
            1_700_000_000_000_000_000,
            mention_context::SUBJECT_OF,
            0.95,
        );
        let key = (id.to_bytes(), memory.to_be_bytes());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_MENTIONS_TABLE).unwrap();
            t.insert(&key, &m).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_MENTIONS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, m);
    }
}
