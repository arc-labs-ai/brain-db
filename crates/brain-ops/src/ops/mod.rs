//! Op handlers: one module per spec §09 cognitive operation, plus
//! the in-shard write path (`writer`) that drains ENCODE/FORGET/LINK/
//! UNLINK. Infrastructure (`context`, `dispatch`, `idempotency`, etc.)
//! lives at the crate root.

pub mod encode;
pub mod forget;
pub mod knowledge_entity;
pub mod knowledge_statement;
pub mod link;
pub mod plan;
pub mod reason;
pub mod recall;
pub mod subscribe;
pub mod txn;
pub mod writer;
