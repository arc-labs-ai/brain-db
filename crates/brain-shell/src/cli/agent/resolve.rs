//! Resolve the agent id for this shell invocation.
//!
//! Precedence, highest first:
//!
//! 1. `--agent <name>` flag → look up `[agents.<name>]` in
//!    `~/.config/brain/config.toml`. Missing name is an error.
//! 2. `--agent-id <uuid>` flag → raw id; no file touched.
//! 3. `BRAIN_AGENT=<name>` env → same lookup as (1).
//! 4. `BRAIN_AGENT_ID=<uuid>` env → raw id.
//! 5. `[agents.<x>] active = true` in the config file (sticky
//!    selection set by `\agent use`).
//! 6. `[agents.<x>] default = true` in the config file (factory
//!    default — only reachable if the load-time auto-promote did
//!    not also synthesise an `active` flag).
//! 7. Auto-mint a fresh UUIDv7, persist it as both default and
//!    active, return it. First-run path.
//! 8. No config file path available (no HOME / XDG dir): mint a
//!    fresh UUIDv7 in-memory; nothing is written. This is the only
//!    surviving `Ephemeral` case.
//!
//! Conflict (both `--agent` and `--agent-id`, or both env vars set)
//! is an error so the user doesn't get silent "name wins"
//! semantics. Migration of the previous single-field
//! `config.toml` shape is handled by [`super::super::config`].

use std::path::Path;

use brain_core::AgentId;
use uuid::Uuid;

use super::super::config::{Config, ConfigError, MigrationNote};
use super::auto_mint;
use super::source::{AgentIdSource, ResolvedAgentId};

/// Env-var read in the named-agent slot (`BRAIN_AGENT=work`).
pub const ENV_VAR_NAME: &str = "BRAIN_AGENT";
/// Env-var read in the raw-id slot (`BRAIN_AGENT_ID=<uuid>`).
pub const ENV_VAR_ID: &str = "BRAIN_AGENT_ID";

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("invalid --agent-id: {0}")]
    BadFlagId(#[source] uuid::Error),
    #[error("invalid {ENV_VAR_ID}: {0}")]
    BadEnvId(#[source] uuid::Error),
    #[error("unknown agent '{name}'{hint}. Try `brain agent list` or `brain agent create {name}`",
            hint = match suggestion { Some(s) => format!(" — did you mean '{s}'?"), None => String::new() })]
    UnknownNamed {
        name: String,
        suggestion: Option<String>,
    },
    #[error("--agent and --agent-id are both set; pass only one")]
    FlagsConflict,
    #[error("{ENV_VAR_NAME} and {ENV_VAR_ID} are both set; unset one")]
    EnvConflict,
    #[error("config file unusable: {0}")]
    Config(#[from] ConfigError),
}

/// Inputs to the resolver. Keeping them as a struct (rather than 4
/// positional args) makes the call site at `cli::args::dispatch_argv`
/// readable and the test factories tidy.
#[derive(Debug, Default, Clone)]
pub struct ResolveInputs<'a> {
    pub agent_flag: Option<&'a str>,
    pub agent_id_flag: Option<&'a str>,
    pub agent_env: Option<&'a str>,
    pub agent_id_env: Option<&'a str>,
}

/// Top-level entry: gather process env + config dir and resolve.
pub fn resolve(
    agent_flag: Option<&str>,
    agent_id_flag: Option<&str>,
) -> Result<ResolvedAgentId, ResolveError> {
    let agent_env = std::env::var(ENV_VAR_NAME).ok();
    let agent_id_env = std::env::var(ENV_VAR_ID).ok();
    let path = super::super::config::default_path();
    resolve_with(
        ResolveInputs {
            agent_flag,
            agent_id_flag,
            agent_env: agent_env.as_deref().filter(|s| !s.is_empty()),
            agent_id_env: agent_id_env.as_deref().filter(|s| !s.is_empty()),
        },
        path.as_deref(),
    )
}

/// Pure resolver — tests drive this directly.
pub fn resolve_with(
    inputs: ResolveInputs<'_>,
    config_path: Option<&Path>,
) -> Result<ResolvedAgentId, ResolveError> {
    // Conflict checks first — surface clearly even if the user
    // also has a config dir set up.
    if inputs.agent_flag.is_some() && inputs.agent_id_flag.is_some() {
        return Err(ResolveError::FlagsConflict);
    }
    if inputs.agent_env.is_some() && inputs.agent_id_env.is_some() {
        return Err(ResolveError::EnvConflict);
    }

    // 1. --agent <name>
    if let Some(name) = inputs.agent_flag {
        let (config, migration) = load_config(config_path)?;
        let entry = config
            .get_agent(name)
            .map_err(|e| named_lookup_error(e, name))?;
        let agent_id = parse_id_or_fail(&entry.id)?;
        return Ok(ResolvedAgentId {
            agent_id,
            source: AgentIdSource::NamedFlag {
                name: name.to_owned(),
                file: config.path.clone(),
            },
            migration,
        });
    }

    // 2. --agent-id <uuid>
    if let Some(raw) = inputs.agent_id_flag {
        let uuid = Uuid::parse_str(raw).map_err(ResolveError::BadFlagId)?;
        return Ok(ResolvedAgentId {
            agent_id: AgentId(uuid),
            source: AgentIdSource::IdFlag,
            migration: None,
        });
    }

    // 3. BRAIN_AGENT=<name>
    if let Some(name) = inputs.agent_env {
        let (config, migration) = load_config(config_path)?;
        let entry = config
            .get_agent(name)
            .map_err(|e| named_lookup_error(e, name))?;
        let agent_id = parse_id_or_fail(&entry.id)?;
        return Ok(ResolvedAgentId {
            agent_id,
            source: AgentIdSource::NamedEnv {
                name: name.to_owned(),
                file: config.path.clone(),
            },
            migration,
        });
    }

    // 4. BRAIN_AGENT_ID=<uuid>
    if let Some(raw) = inputs.agent_id_env {
        let uuid = Uuid::parse_str(raw).map_err(ResolveError::BadEnvId)?;
        return Ok(ResolvedAgentId {
            agent_id: AgentId(uuid),
            source: AgentIdSource::IdEnv,
            migration: None,
        });
    }

    // 5 / 6 / 7 / 8 — all require knowing where the config lives.
    // Without a config path the only honest answer is the in-memory
    // ephemeral path: we can't persist, so we don't pretend to.
    let config_path = match config_path {
        Some(p) => p,
        None => {
            return Ok(ResolvedAgentId {
                agent_id: AgentId::new(),
                source: AgentIdSource::Ephemeral,
                migration: None,
            });
        }
    };

    let (mut config, migration) = load_config(Some(config_path))?;

    // 5. config.active
    if let Some((name, entry)) = config.active_agent() {
        let agent_id = parse_id_or_fail(&entry.id)?;
        let name = name.to_owned();
        let file = config.path.clone();
        return Ok(ResolvedAgentId {
            agent_id,
            source: AgentIdSource::ActiveFromConfig { name, file },
            migration,
        });
    }

    // 6. config.default (only reachable if `active` wasn't set;
    // load-time auto-promote usually fills both, so this fires only
    // when the user hand-edited the file to clear `active` on every
    // entry).
    if let Some((name, entry)) = config.default_agent() {
        let agent_id = parse_id_or_fail(&entry.id)?;
        let name = name.to_owned();
        let file = config.path.clone();
        return Ok(ResolvedAgentId {
            agent_id,
            source: AgentIdSource::DefaultFromConfig { name, file },
            migration,
        });
    }

    // 7. Auto-mint + persist. The config is freshly-loaded and has
    // no agents at all (or the previous defaults got wiped); minting
    // here is the first-run UX.
    //
    // The resolver is pure — it returns AgentIdSource::AutoMinted
    // carrying the new name + file path; the caller decides how to
    // surface the "first run" event to the user. brain-shell's
    // REPL banner formats it in the welcome card; one-shot CLI
    // flows that don't print a banner suppress it entirely.
    let (name, agent_id) = auto_mint::create_and_persist(&mut config)?;
    let file = config.path.clone();
    Ok(ResolvedAgentId {
        agent_id,
        source: AgentIdSource::AutoMinted { name, file },
        migration,
    })
}

fn load_config(
    config_path: Option<&Path>,
) -> Result<(Config, Option<MigrationNote>), ResolveError> {
    let path = config_path.ok_or(ResolveError::Config(ConfigError::NoConfigDir))?;
    Config::load_or_default_at(path).map_err(ResolveError::Config)
}

fn parse_id_or_fail(raw: &str) -> Result<AgentId, ResolveError> {
    let uuid =
        Uuid::parse_str(raw).map_err(|e| ResolveError::Config(ConfigError::AgentBadId(e)))?;
    Ok(AgentId(uuid))
}

fn named_lookup_error(e: ConfigError, name: &str) -> ResolveError {
    match e {
        ConfigError::AgentUnknown { suggestion, .. } => ResolveError::UnknownNamed {
            name: name.to_owned(),
            suggestion,
        },
        other => ResolveError::Config(other),
    }
}
