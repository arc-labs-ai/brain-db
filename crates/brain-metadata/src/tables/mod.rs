//! redb table definitions and value types, one module per table.
//!
//! See `spec/07_metadata_graph/02_table_layout.md` §1 for the v1 table
//! catalog (13 spec'd domain tables; one internal `__schema_meta` from
//! [`crate::schema`]).

pub mod agent;
pub mod checkpoint;
pub mod context;
pub mod edge;
pub mod idempotency;
pub mod memory;
pub mod model_fingerprint;
pub mod next_lsn;
pub mod slot_version;
pub mod text;
