//! Wire-opcode handlers: one module cognitive operation.
//! Engine code (`writer`, `apply`, `index`) and infrastructure
//! (`context`, `dispatch`, `idempotency`, etc.) live at the crate root.

pub mod admin;
pub mod capabilities;
pub mod cursor;
pub mod encode;
pub mod encode_vector_direct;
pub mod entity;
pub mod events;
pub mod extractor_admin;
pub mod forget;
pub mod graph_fetch;
pub mod link;
pub mod memory_inspect;
pub mod memory_list;
pub mod plan;
pub mod procedural;
pub mod query;
pub mod reason;
pub mod recall;
pub mod registry_cascade;
pub mod relation;
pub mod restore;
pub mod schema;
pub mod schema_drop;
pub mod schema_replace;
pub mod session;
pub mod space;
pub mod statement;
pub mod subscribe;
pub mod txn;
