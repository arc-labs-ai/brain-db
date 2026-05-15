//! Knowledge-layer redb table definitions.
//!
//! Phase 15.1 — types only. Lays the 25 tables specified in
//! `spec/26_knowledge_storage/00_purpose.md` alongside the substrate
//! tables in `metadata.redb`. No knowledge-layer behavior is wired up
//! at this layer; later phases (16 entities, 17 statements, 18
//! relations, 19 schema DSL, 20–21 extractors) consume these tables.
//!
//! ## Module layout
//!
//! One file per logical "family":
//!
//! - [`entity`]          — 5 tables: primary + 4 resolution indexes.
//! - [`statement`]       — 8 tables: primary + 6 secondary indexes + evidence overflow.
//! - [`relation`]        — 4 tables: primary + 2 direction indexes + evidence index.
//! - [`predicate`]       — 1 table: interned predicate registry.
//! - [`entity_type`]     — 1 table: user-declared entity types.
//! - [`relation_type`]   — 1 table: user-declared relation types.
//! - [`extractor`]       — 1 table: active extractors.
//! - [`schema_version`]  — 1 table: schema upload history.
//! - [`audit`]           — 2 tables: extraction audit + resolution audit.
//! - [`merge`]           — 1 table: entity merge log.
//!
//! Total: 25 tables.

pub mod audit;
pub mod entity;
pub mod entity_type;
pub mod extractor;
pub mod merge;
pub mod predicate;
pub mod relation;
pub mod relation_type;
pub mod schema_version;
pub mod statement;

/// Boilerplate `redb::Value` impl for an rkyv-archived struct.
///
/// Each value type in the knowledge layer uses the same encoding
/// pattern (rkyv with `check_bytes`, deserialize-on-read, type_name
/// versioned with `::v1`). This macro emits that impl from the type
/// name and a stable `type_name` string.
///
/// Mirrors the per-file impl in substrate tables (`agent.rs`,
/// `memory.rs`); collapsed into a macro here because 11 knowledge-layer
/// value structs share the exact same body.
#[macro_export]
macro_rules! impl_redb_rkyv_value {
    ($ty:ty, $type_name:literal) => {
        impl ::redb::Value for $ty {
            type SelfType<'a> = $ty;
            type AsBytes<'a> = ::std::vec::Vec<u8>;

            fn fixed_width() -> Option<usize> {
                None
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
            where
                Self: 'a,
            {
                let mut buf = ::rkyv::AlignedVec::with_capacity(data.len());
                buf.extend_from_slice(data);
                ::rkyv::from_bytes::<$ty>(&buf).expect(concat!(
                    stringify!($ty),
                    " bytes failed rkyv validation; redb file is corrupt"
                ))
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'a,
                Self: 'b,
            {
                ::rkyv::to_bytes::<_, 256>(value)
                    .expect(concat!(stringify!($ty), " is rkyv-serializable"))
                    .into_vec()
            }

            fn type_name() -> ::redb::TypeName {
                ::redb::TypeName::new($type_name)
            }
        }
    };
}

#[cfg(all(test, not(miri)))]
pub(crate) fn fresh_db(dir: &tempfile::TempDir) -> redb::Database {
    redb::Database::create(dir.path().join("test.redb")).expect("create redb")
}
