//! One file per admin-domain response shape. Each module exposes a
//! `*Rendered` newtype that wraps the response and impls
//! [`brain_explore::Render`]. The newtype wrap is the orphan rule:
//! the response shapes live in `crate::commands::*` (which can't impl
//! a trait from brain-explore for a foreign type without the wrap).

pub mod agent_record;
pub mod audit_row;
pub mod config_dump;
pub mod extract_status;
pub mod shard_health;
pub mod shard_stats;
pub mod snapshot;
pub mod worker_status;
