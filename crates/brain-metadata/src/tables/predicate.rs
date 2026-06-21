//! `predicates` table — interned predicate registry.
//!
//! The row carries `kind_constraint`, `object_type_constraint_byte`,
//! `schema_version`, and `description`, plus a `predicates_by_qname`
//! lookup index. The schema DSL populates user predicates at
//! `SCHEMA_UPLOAD` time; built-ins are seeded by the substrate.

use crate::impl_redb_rkyv_value;
use brain_core::PredicateId;
use brain_core::{Predicate, StatementKind};
use redb::TableDefinition;

/// `predicates` table. Key is `PredicateId.raw()` (u32); value is
/// [`PredicateDefinition`].
pub const PREDICATES_TABLE: TableDefinition<'static, u32, PredicateDefinition> =
    TableDefinition::new("predicates");

/// `predicates_by_qname` — secondary index for `(namespace, name) →
/// PredicateId`. Key is the canonical `"namespace:name"` string;
/// value is the predicate id.
pub const PREDICATES_BY_QNAME_TABLE: TableDefinition<'static, &str, u32> =
    TableDefinition::new("predicates_by_qname");

/// `predicate_review_queue` — durable record of predicate qnames the
/// extractor proposed but that are NOT declared in the active schema, so
/// closed-vocab extraction dropped them from the live graph. Key is the
/// canonical `"namespace:name"`; value is the number of times it was
/// seen. Operators scan this to decide which coined predicates to promote
/// into a schema via `SCHEMA_UPLOAD`. Nothing reads it on the hot path.
pub const PREDICATE_REVIEW_QUEUE_TABLE: TableDefinition<'static, &str, u64> =
    TableDefinition::new("predicate_review_queue");

/// `predicate_embeddings` — per-predicate semantic vector, keyed by
/// `PredicateId.raw()` (u32). Value is the embedding as little-endian
/// `f32` bytes (BGE-small → 384 dims → 1536 bytes). Written when a
/// predicate is first interned at extraction time; read by the grounded
/// answer engine to match a query's relation against a subject's
/// predicates by cosine (the "two-way match", alongside the exact qname
/// index). Open-vocab predicates are never gated, so this is how a free
/// predicate stays *findable* by a paraphrased question.
pub const PREDICATE_EMBEDDINGS_TABLE: TableDefinition<'static, u32, &[u8]> =
    TableDefinition::new("predicate_embeddings");

/// Origin of a registered predicate. Tracks whether the row was
/// authored by an explicit `SCHEMA_UPLOAD` (strict mode) or interned
/// on demand from an open-vocabulary write (schemaless mode).
///
/// Encoded as a single byte (`origin_byte` below) plus a payload word:
/// for `SchemaDeclared(v)` the word holds the schema version; for
/// `ImplicitFromWrite { first_seen_lsn }` it holds the LSN. Implicit
/// rows still carry `schema_version = 0` in the legacy field so query
/// code that filters by version naturally skips them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaOrigin {
    /// Predicate was registered through `SCHEMA_UPLOAD` for the
    /// referenced schema version.
    SchemaDeclared { version: u32 },
    /// Predicate was lazily interned because a STATEMENT_CREATE
    /// referenced it without a schema being declared. The `lsn` is
    /// captured at intern time so operator tooling can correlate
    /// vocabulary growth with the write stream.
    ImplicitFromWrite { first_seen_lsn: u64 },
}

impl SchemaOrigin {
    /// Tag byte: 0 = SchemaDeclared, 1 = ImplicitFromWrite. Anything
    /// else collapses to SchemaDeclared{0} on read for forwards-
    /// compatibility — corrupt rows degrade gracefully rather than
    /// vanish from query results.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::SchemaDeclared { .. } => 0,
            Self::ImplicitFromWrite { .. } => 1,
        }
    }

    #[must_use]
    pub fn payload(self) -> u64 {
        match self {
            Self::SchemaDeclared { version } => u64::from(version),
            Self::ImplicitFromWrite { first_seen_lsn } => first_seen_lsn,
        }
    }

    #[must_use]
    pub fn decode(tag: u8, payload: u64) -> Self {
        match tag {
            1 => Self::ImplicitFromWrite {
                first_seen_lsn: payload,
            },
            // Tag 0 (and anything unknown — see doc above) is treated
            // as SchemaDeclared.
            _ => Self::SchemaDeclared {
                #[allow(clippy::cast_possible_truncation)]
                version: payload as u32,
            },
        }
    }

    /// Convenience: was this row authored by a SCHEMA_UPLOAD?
    #[must_use]
    pub fn is_schema_declared(self) -> bool {
        matches!(self, Self::SchemaDeclared { .. })
    }
}

/// A registered predicate. The `(namespace, name)` pair is logically
/// unique within a deployment; uniqueness is enforced by
/// [`PREDICATES_BY_QNAME_TABLE`] writes inside `predicate_intern`.
///
/// `kind_constraint`: `0` means "any kind allowed", else `1=Fact /
/// 2=Preference / 3=Event` (matching [`StatementKind::as_u8`] offset by
/// 1). `object_type_constraint_byte`: `0` means "any object type", else
/// `1=Entity / 2=Value / 3=Memory / 4=Statement` (matches
/// `StatementObject::discriminant()` offset by 1).
///
/// `origin_tag` + `origin_payload` encode the [`SchemaOrigin`].
/// Implicit-from-write rows are how Brain supports open-vocabulary
/// STATEMENT_CREATE without a schema declaration.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct PredicateDefinition {
    pub predicate_id: u32,
    pub namespace: String,
    pub name: String,
    pub kind_constraint: u8,
    pub object_type_constraint_byte: u8,
    pub schema_version: u32,
    pub description: String,
    pub created_at_unix_nanos: u64,
    pub origin_tag: u8,
    pub origin_payload: u64,
    /// Auto-supersession flag. When true, statement_create tombstones
    /// any prior active statement with the same `(subject, predicate)`
    /// before inserting the new row.
    pub is_stateful: bool,
}

impl PredicateDefinition {
    #[must_use]
    pub fn id(&self) -> PredicateId {
        PredicateId::from(self.predicate_id)
    }

    /// Build a redb row from the brain-core value type. The origin
    /// defaults to `SchemaDeclared` at the predicate's `schema_version`
    /// — the legacy entry point used by `predicate_intern` (schema-
    /// driven). Open-vocabulary writes go through
    /// [`Self::from_predicate_with_origin`].
    #[must_use]
    pub fn from_predicate(p: &Predicate, created_at_unix_nanos: u64) -> Self {
        let origin = SchemaOrigin::SchemaDeclared {
            version: p.schema_version,
        };
        Self::from_predicate_with_origin(p, created_at_unix_nanos, origin)
    }

    #[must_use]
    pub fn from_predicate_with_origin(
        p: &Predicate,
        created_at_unix_nanos: u64,
        origin: SchemaOrigin,
    ) -> Self {
        Self {
            predicate_id: p.id.raw(),
            namespace: p.namespace.clone(),
            name: p.name.clone(),
            kind_constraint: encode_kind_constraint(p.kind_constraint),
            object_type_constraint_byte: p.object_type_constraint_byte,
            schema_version: p.schema_version,
            description: p.description.clone(),
            created_at_unix_nanos,
            origin_tag: origin.tag(),
            origin_payload: origin.payload(),
            is_stateful: p.is_stateful,
        }
    }

    #[must_use]
    pub fn origin(&self) -> SchemaOrigin {
        SchemaOrigin::decode(self.origin_tag, self.origin_payload)
    }

    /// Project to the brain-core value type. `created_at_unix_nanos`
    /// is intentionally dropped — it lives only in the persisted row.
    #[must_use]
    pub fn to_predicate(&self) -> Predicate {
        Predicate {
            id: self.id(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            kind_constraint: decode_kind_constraint(self.kind_constraint),
            object_type_constraint_byte: self.object_type_constraint_byte,
            schema_version: self.schema_version,
            description: self.description.clone(),
            is_stateful: self.is_stateful,
        }
    }
}

/// `0 → None` / `1 → Fact / 2 → Preference / 3 → Event`. Unknown
/// bytes collapse to `None` (forwards-compatible).
#[must_use]
pub fn decode_kind_constraint(b: u8) -> Option<StatementKind> {
    match b {
        1 => Some(StatementKind::Fact),
        2 => Some(StatementKind::Preference),
        3 => Some(StatementKind::Event),
        4 => Some(StatementKind::Attribute),
        5 => Some(StatementKind::Relation),
        6 => Some(StatementKind::Directive),
        // `0` (any) and any unknown byte collapse to None. User-declared
        // Custom kinds are not used as a coarse predicate constraint.
        _ => None,
    }
}

/// Inverse of [`decode_kind_constraint`].
#[must_use]
pub fn encode_kind_constraint(k: Option<StatementKind>) -> u8 {
    match k {
        Some(StatementKind::Fact) => 1,
        Some(StatementKind::Preference) => 2,
        Some(StatementKind::Event) => 3,
        Some(StatementKind::Attribute) => 4,
        Some(StatementKind::Relation) => 5,
        Some(StatementKind::Directive) => 6,
        // No coarse constraint for Custom kinds (treated as "any").
        Some(StatementKind::Custom(_)) | None => 0,
    }
}

impl_redb_rkyv_value!(PredicateDefinition, "brain_metadata::PredicateDefinition");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let pred = Predicate {
            id: PredicateId::from(7),
            namespace: "acme".into(),
            name: "reports_to".into(),
            kind_constraint: Some(StatementKind::Fact),
            object_type_constraint_byte: 1,
            schema_version: 3,
            description: "Reports-to relation".into(),
            is_stateful: false,
        };
        let row = PredicateDefinition::from_predicate(&pred, 1_700_000_000_000_000_000);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(PREDICATES_TABLE).unwrap();
            t.insert(&row.predicate_id, &row).unwrap();
            let mut q = wtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
            q.insert("acme:reports_to", &row.predicate_id).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let got = t.get(&row.predicate_id).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.to_predicate(), pred);

        let q = rtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
        let by_qname = q.get("acme:reports_to").unwrap().unwrap().value();
        assert_eq!(by_qname, row.predicate_id);
    }

    #[test]
    fn kind_constraint_round_trip() {
        assert_eq!(decode_kind_constraint(0), None);
        assert_eq!(decode_kind_constraint(1), Some(StatementKind::Fact));
        assert_eq!(decode_kind_constraint(2), Some(StatementKind::Preference));
        assert_eq!(decode_kind_constraint(3), Some(StatementKind::Event));
        assert_eq!(decode_kind_constraint(99), None);

        assert_eq!(encode_kind_constraint(None), 0);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Fact)), 1);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Preference)), 2);
        assert_eq!(encode_kind_constraint(Some(StatementKind::Event)), 3);
    }
}
