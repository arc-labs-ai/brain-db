//! `kinds` table — interned user-declared statement kinds.
//!
//! The six built-in kinds (`Fact/Preference/Event/Attribute/Relation/
//! Directive`, bytes `0..=5`) need no storage — their behavior is a const
//! in `brain_core::StatementKind::builtin_behavior`. This table holds only
//! **user-declared** kinds (bytes `>= 6`), each carrying its own
//! [`brain_core::KindBehavior`] plus a natural-language `hint` the
//! extractor's kind classifier uses.
//!
//! Two tables: [`KINDS_TABLE`] keyed by qname for declaration/merge, and
//! [`KINDS_BY_BYTE_TABLE`] keyed by the interned byte for the read-path
//! behavior lookup.

use crate::impl_redb_rkyv_value;
use redb::TableDefinition;

/// `kinds` table. Key is the canonical `"namespace:name"`; value is
/// [`KindDefinition`].
pub const KINDS_TABLE: TableDefinition<'static, &str, KindDefinition> =
    TableDefinition::new("kinds");

/// `kinds_by_byte` — reverse index `interned_byte → qname`, for the
/// read-path lookup of a `StatementKind::Custom(byte)`'s behavior.
pub const KINDS_BY_BYTE_TABLE: TableDefinition<'static, u8, &str> =
    TableDefinition::new("kinds_by_byte");

/// A registered user-declared statement kind. The `(namespace, name)`
/// pair is logically unique; uniqueness is enforced by the qname-keyed
/// [`KINDS_TABLE`]. `byte_id` is the interned `StatementKind::Custom`
/// discriminant (`>= 6`).
///
/// `cardinality` / `temporal` mirror `brain_core::KindCardinality::as_u8`
/// and `brain_core::TemporalModel::as_u8`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct KindDefinition {
    pub byte_id: u8,
    pub namespace: String,
    pub name: String,
    pub cardinality: u8,
    pub temporal: u8,
    pub polarity: bool,
    pub hint: String,
    pub schema_version: u32,
    pub created_at_unix_nanos: u64,
}

impl_redb_rkyv_value!(KindDefinition, "brain_metadata::KindDefinition");
