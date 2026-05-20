//! Agent identity resolution + persistence. One file per concern:
//!
//! - [`resolve`] — the precedence cascade from flags/env/config to an
//!   id at session start. Pure function; tests drive it directly.
//! - [`source`] — the `AgentIdSource` / `ResolvedAgentId` value
//!   shapes the resolver returns; consumed by the connect banner
//!   and `\agent` meta-command.

pub mod resolve;
pub mod source;

#[cfg(test)]
mod tests;

pub use resolve::{resolve, resolve_with, ResolveError, ResolveInputs, ENV_VAR_ID, ENV_VAR_NAME};
pub use source::{AgentIdSource, ResolvedAgentId};
