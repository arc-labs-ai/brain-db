//! Value shapes the resolver returns. Kept in their own module so the
//! connect banner / `\agent` meta-command can depend on the types
//! without pulling in the resolver's std::env / config-file machinery.

use std::path::PathBuf;

use brain_core::AgentId;

use super::super::config::MigrationNote;

/// Where the resolved agent id came from. Drives the connect banner
/// and the `\agent` meta-command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdSource {
    /// `--agent <name>` (resolved against the config file).
    NamedFlag { name: String, file: PathBuf },
    /// `--agent-id <uuid>` (raw id, no name).
    IdFlag,
    /// `BRAIN_AGENT=<name>` (resolved against the config file).
    NamedEnv { name: String, file: PathBuf },
    /// `BRAIN_AGENT_ID=<uuid>` (raw id, no name).
    IdEnv,
    /// Default — fresh UUIDv7 minted at connect, discarded at quit.
    Ephemeral,
}

#[derive(Debug)]
pub struct ResolvedAgentId {
    pub agent_id: AgentId,
    pub source: AgentIdSource,
    /// `Some` when the just-loaded config file triggered a
    /// legacy → named-default migration. The CLI surfaces it as a
    /// one-line stderr `note:`.
    pub migration: Option<MigrationNote>,
}
