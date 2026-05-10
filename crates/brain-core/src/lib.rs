//! # brain-core
//!
//! Foundational types for the Brain cognitive substrate, shared across the
//! workspace. Everything in here is a pure value type — no I/O, no async,
//! no runtime dependency.
//!
//! See `spec/02_data_model/` for the authoritative definitions.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod edge;
pub mod error;
pub mod ids;
pub mod memory;

pub use edge::{Edge, EdgeKind, EdgeOrigin};
pub use error::{Error, Result};
pub use ids::{
    AgentId, ContextId, MemoryId, RequestId, ShardId, SlotIndex, SlotVersion, TxnId, MAX_SLOT_INDEX,
};
pub use memory::{Memory, MemoryKind, Salience};
