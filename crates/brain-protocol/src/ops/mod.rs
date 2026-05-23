//! Per-domain wire ops: one file per capability, each holding the
//! request, response, and per-op view types together. The grouping is
//! by business noun (memory, entity, statement, relation, query,
//! procedural, txn, subscribe, admin, extractor), not by wire
//! direction. Schema-DSL ops live under `crate::schema::ops`;
//! connection-lifecycle ops live under `crate::connection`.

pub mod admin;
pub mod entity;
pub mod extractor;
pub mod memory;
pub mod procedural;
pub mod query;
pub mod relation;
pub mod statement;
pub mod subscribe;
pub mod txn;
