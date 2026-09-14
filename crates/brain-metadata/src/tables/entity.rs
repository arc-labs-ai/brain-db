//! Entity family — 5 tables.
//!
//! - [`ENTITIES_TABLE`]                — primary `EntityId → EntityMetadata`.
//! - [`ENTITY_BY_CANONICAL_NAME_TABLE`] — exact-match resolution.
//! - [`ENTITY_ALIASES_TABLE`]          — alias resolution (multi-value via key).
//! - [`ENTITY_TRIGRAMS_TABLE`]         — fuzzy resolution via trigram index.
//! - [`ENTITY_MENTIONS_TABLE`]         — reverse index (which memories mention an entity).

use crate::impl_redb_rkyv_value;
use crate::tables::scope::RowScope;
use brain_core::{Entity, EntityAttributes, EntityId, EntityTypeId, NamespaceId, SpaceId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------
//
// Every secondary index below carries a LEADING `(namespace_id,
// space_id_bytes)` scope prefix so a range scan for one `(namespace,
// space)` can physically never traverse another tenant's rows. The
// primary `ENTITIES_TABLE` stays keyed by the (globally-unique)
// `EntityId`; the scope lives on the row and is the discriminator that
// makes the same NAME resolve to DISTINCT entity ids per scope.

// Scope-prefixed secondary-index key shapes. Factored into aliases so the
// `(namespace, space)` prefix doesn't push the `TableDefinition` generics
// past clippy's type-complexity threshold — and so each key reads as a
// named shape rather than an anonymous tuple.

/// `(namespace_id, space_id_bytes, entity_type_id, normalized_alias, EntityId)`.
type AliasKey = (u32, [u8; 16], u32, &'static str, [u8; 16]);
/// `(namespace_id, space_id_bytes, entity_type_id, EntityId)`.
pub type ByTypeKey = (u32, [u8; 16], u32, [u8; 16]);
/// `(namespace_id, space_id_bytes, entity_type_id, trigram, EntityId)`.
type TrigramKey = (u32, [u8; 16], u32, [u8; 3], [u8; 16]);
/// `(namespace_id, space_id_bytes, EntityId, MemoryId)`.
type MentionKey = (u32, [u8; 16], [u8; 16], [u8; 16]);

pub const ENTITIES_TABLE: TableDefinition<'static, [u8; 16], EntityMetadata> =
    TableDefinition::new("entities");

/// `(namespace_id, space_id_bytes, entity_type_id, normalized_name)` →
/// `EntityId.to_bytes()`. The leading scope makes each tenant's exact-name
/// space private: the same canonical name under two scopes maps to two
/// distinct entity ids.
pub const ENTITY_BY_CANONICAL_NAME_TABLE: TableDefinition<
    'static,
    (u32, [u8; 16], u32, &'static str),
    [u8; 16],
> = TableDefinition::new("entity_by_canonical_name");

/// `(namespace_id, space_id_bytes, entity_type_id, normalized_alias,
/// EntityId.to_bytes())` → `()`. The trailing EntityId lets one alias map
/// to multiple entities (ambiguity surfaces to the resolver).
pub const ENTITY_ALIASES_TABLE: TableDefinition<'static, AliasKey, ()> =
    TableDefinition::new("entity_aliases");

/// `(namespace_id, space_id_bytes, entity_type_id, trigram,
/// EntityId.to_bytes())` → `()`.
///
/// Trigrams are fixed 3-byte windows (pg_trgm-style, byte-level),
/// keyed as `[u8; 3]`.
pub const ENTITY_TRIGRAMS_TABLE: TableDefinition<'static, TrigramKey, ()> =
    TableDefinition::new("entity_trigrams");

/// `(namespace_id, space_id_bytes, EntityId.to_bytes(),
/// MemoryId.to_be_bytes())` → [`MentionMetadata`].
pub const ENTITY_MENTIONS_TABLE: TableDefinition<'static, MentionKey, MentionMetadata> =
    TableDefinition::new("entity_mentions");

/// `(namespace_id, space_id_bytes, entity_type_id, EntityId.to_bytes())` →
/// `()`. The by-`(scope, type)` listing index: `ENTITY_LIST` range-scans
/// the contiguous `(namespace, space, type)` prefix in ascending EntityId
/// order (the same order the wire cursor resumes on) instead of scanning
/// the whole cross-tenant primary table. The trailing EntityId makes each
/// row unique.
///
/// Maintained on create ([`entity_put`]) and on a type change
/// ([`entity_update`]); scope is immutable so a row never moves tenant.
/// Deliberately NOT torn down on tombstone or merge — unlike the resolver
/// indexes — because the primary row is kept for audit/unmerge and the
/// listing supports `include_tombstoned` / `include_merged`, which need
/// those rows to remain discoverable.
///
/// [`entity_put`]: crate::entity::ops::entity_put
/// [`entity_update`]: crate::entity::ops::entity_update
pub const ENTITY_BY_TYPE_TABLE: TableDefinition<'static, ByTypeKey, ()> =
    TableDefinition::new("entity_by_type");

/// Bytes per persisted entity vector — 384 f32 components × 4 bytes
/// each. Pinned to the BGE-small dimensionality. If/when a deployment
/// migrates to a different model, the row's bytes are still valid for
/// the model that wrote them; the recovery path re-embeds any row
/// whose length doesn't match.
pub const ENTITY_VECTOR_BYTES: usize = 384 * 4;

/// `EntityId.to_bytes()` → bytemuck-cast `[f32; 384]` as a fixed-size
/// byte array. Written at entity-create alongside the HNSW insert so
/// restart can rebuild the entity HNSW from durable vectors without
/// re-embedding canonical names. A missing row (a pre-feature entity,
/// or a write that landed before the vector existed) falls back to
/// re-embed at startup.
pub const ENTITY_VECTORS_TABLE: TableDefinition<'static, [u8; 16], [u8; ENTITY_VECTOR_BYTES]> =
    TableDefinition::new("entity_vectors");

// ---------------------------------------------------------------------------
// Status flags.
// ---------------------------------------------------------------------------

/// Bits in [`EntityMetadata::flags`].
///
/// Reserved high bits are *zero* on new rows; setters mask only the
/// documented bits via `flags |= MASK` / `flags &= !MASK` patterns.
pub mod flags {
    /// Bit 0: entity has been tombstoned. Secondary indexes
    /// (`entity_by_canonical_name`, `entity_aliases`) are torn down on
    /// tombstone so the resolver never sees the row again. The primary
    /// row stays for audit + unmerge.
    pub const TOMBSTONED: u32 = 1 << 0;

    /// Bit 1: entity has been merged into another. Redundant with
    /// `merged_into_bytes.is_some()`; kept as a flag bit so flag-scan
    /// filters don't have to deref the option. Set by the merge path.
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

/// Primary entity record.
///
/// `aliases` is a typed `Vec<String>`. `attributes` remains an opaque
/// blob until the schema DSL defines the typed `Value` union.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
pub struct EntityMetadata {
    pub entity_id_bytes: [u8; 16],
    /// Owning namespace (tenant) — the outer half of the
    /// `(namespace, space)` scope key. `0` is the reserved `brain`
    /// system namespace. Required; stamped from the authenticated
    /// caller's scope at create time (fail-closed by construction).
    pub namespace_id: u32,
    /// Owning space (app) — the inner half of the scope key.
    pub space_id_bytes: [u8; 16],
    /// FIRST-MENTION provenance only: the session that first created this
    /// entity. Entity identity is session-AGNOSTIC — the same "Priya"
    /// appears across sessions 3 and 7 — so this is NOT the per-utterance
    /// session (statements/relations carry that) and is NEVER overwritten
    /// on a later mention. A GROUPING/FILTER column, never part of the
    /// `(namespace, space)` isolation prefix. `0` is the default session.
    /// Appended after the scope so old rkyv rows still decode (positional).
    pub session_id: u64,
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub normalized_name: String,
    /// Alias list is capped at 32 by default; the cap is enforced by
    /// the CRUD layer, not here.
    pub aliases: Vec<String>,
    /// rkyv-encoded `BTreeMap<String, Value>` (Value union resolves
    /// with the schema DSL).
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
        scope: RowScope,
        entity_type_id: EntityTypeId,
        canonical_name: String,
        normalized_name: String,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            entity_id_bytes: entity_id.to_bytes(),
            namespace_id: scope.namespace_id,
            space_id_bytes: scope.space_id_bytes,
            // Default first-mention session; `entity_put` stamps the real
            // one on create and `entity_update` preserves the existing one.
            session_id: brain_core::SessionId::DEFAULT.raw(),
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

    /// Build a row from a brain-core [`Entity`] plus the owning scope.
    /// Replaces the old `From<&Entity>` impl, which couldn't carry the
    /// scope (brain-core has no namespace/space slot).
    #[must_use]
    pub fn from_entity(e: &Entity, scope: RowScope) -> Self {
        Self {
            entity_id_bytes: e.id.to_bytes(),
            namespace_id: scope.namespace_id,
            space_id_bytes: scope.space_id_bytes,
            // Default first-mention session; `entity_put` stamps it on
            // create and `entity_update` re-stamps the existing row's
            // session so a later mention never overwrites it (the
            // brain-core `Entity` carries no session slot).
            session_id: brain_core::SessionId::DEFAULT.raw(),
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

    #[must_use]
    pub fn entity_id(&self) -> EntityId {
        EntityId::from(self.entity_id_bytes)
    }

    /// The owning namespace (tenant) of this entity.
    #[must_use]
    pub fn namespace(&self) -> NamespaceId {
        NamespaceId::from(self.namespace_id)
    }

    /// The owning space of this entity.
    #[must_use]
    pub fn space_id(&self) -> SpaceId {
        SpaceId::from(self.space_id_bytes)
    }

    /// The `(namespace, space)` scope this entity belongs to.
    #[must_use]
    pub fn scope(&self) -> RowScope {
        RowScope::from_bytes(self.namespace_id, self.space_id_bytes)
    }

    /// First-mention session provenance (session-agnostic identity).
    #[must_use]
    pub fn session(&self) -> brain_core::SessionId {
        brain_core::SessionId::from(self.session_id)
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
    /// callers should pre-normalize). The on-rename caller uses this to
    /// move an old canonical_name into the alias list.
    pub fn add_alias(&mut self, alias: String) {
        self.aliases.push(alias);
    }
}

impl_redb_rkyv_value!(EntityMetadata, "brain_metadata::EntityMetadata");

// ---------------------------------------------------------------------------
// brain-core ↔ brain-metadata boundary conversions.
// ---------------------------------------------------------------------------

// `EntityMetadata::from_entity(&Entity, RowScope)` replaces the old
// `From<&Entity>` impl — the scope can't be reconstructed from a
// brain-core `Entity` (it has no namespace/space slot), so it must be
// supplied explicitly. The reverse projection drops the scope (again,
// brain-core has nowhere to put it).

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

impl_redb_rkyv_value!(MentionMetadata, "brain_metadata::MentionMetadata");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::MemoryId;
    use redb::ReadableDatabase;

    /// Fixed test scope: system namespace + a stable test space.
    fn test_scope() -> RowScope {
        RowScope::from_bytes(NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    #[test]
    fn entities_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let e = EntityMetadata::new_active(
            id,
            test_scope(),
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
        // aliases is a typed `Vec<String>`, not a `Vec<u8>` blob.
        // Verify the typed field round-trips through rkyv + redb.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let mut e = EntityMetadata::new_active(
            id,
            test_scope(),
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
    fn entity_from_entity_carries_scope_and_reverse_drops_it() {
        // `from_entity(&Entity, scope)` stamps the scope; the reverse
        // projection preserves every brain-core field (scope is not a
        // brain-core field, so it round-trips through the scope-carrying
        // metadata, not the brain-core type).
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

        let scope = test_scope();
        let m = EntityMetadata::from_entity(&e, scope);
        assert_eq!(m.scope(), scope);
        let back: Entity = (&m).into();
        assert_eq!(back, e);
    }

    #[test]
    fn canonical_name_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EntityId::new();
        let s = test_scope();
        let key = (s.namespace_id, s.space_id_bytes, 1u32, "priya patel");

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
        let s = test_scope();
        let k_a = (
            s.namespace_id,
            s.space_id_bytes,
            entity_type,
            alias,
            id_a.to_bytes(),
        );
        let k_b = (
            s.namespace_id,
            s.space_id_bytes,
            entity_type,
            alias,
            id_b.to_bytes(),
        );

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
        let s = test_scope();
        // Trigram component is `[u8; 3]`, not `&str`.
        let key = (
            s.namespace_id,
            s.space_id_bytes,
            1u32,
            *b"pri",
            id.to_bytes(),
        );

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
        let s = test_scope();
        let m = MentionMetadata::new(1_700_000_000_000_000_000, mention_context::SUBJECT_OF, 0.95);
        let key = (
            s.namespace_id,
            s.space_id_bytes,
            id.to_bytes(),
            memory.to_be_bytes(),
        );

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
