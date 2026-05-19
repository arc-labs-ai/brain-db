//! Resolve the agent id for this shell invocation.
//!
//! Precedence, highest first:
//!
//! 1. `--agent <name>` flag → look up `[agents.<name>]` in
//!    `~/.config/brain/config.toml`. Missing name is an error.
//! 2. `--agent-id <uuid>` flag → raw id; no file touched.
//! 3. `BRAIN_AGENT=<name>` env → same lookup as (1).
//! 4. `BRAIN_AGENT_ID=<uuid>` env → raw id.
//! 5. Fresh ephemeral UUIDv7, minted at connect, discarded at quit.
//!
//! Conflict (both `--agent` and `--agent-id`, or both env vars set)
//! is an error so the user doesn't get silent "name wins"
//! semantics. Migration of the previous single-field
//! `config.toml` shape is handled by [`super::config`].

use std::path::{Path, PathBuf};

use brain_core::AgentId;
use uuid::Uuid;

use super::config::{Config, ConfigError, MigrationNote};

/// Env-var read in the named-agent slot (`BRAIN_AGENT=work`).
pub const ENV_VAR_NAME: &str = "BRAIN_AGENT";
/// Env-var read in the raw-id slot (`BRAIN_AGENT_ID=<uuid>`).
pub const ENV_VAR_ID: &str = "BRAIN_AGENT_ID";

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
    let path = super::config::default_path();
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

    // 5. Ephemeral. Load the config opportunistically only to surface
    // a pending migration note — but do NOT auto-bind to the
    // migrated `default` agent.
    let migration = match config_path {
        Some(path) => match Config::load_or_default_at(path) {
            Ok((_, note)) => note,
            Err(_) => None, // ephemeral path should never crash the shell
        },
        None => None,
    };
    Ok(ResolvedAgentId {
        agent_id: AgentId::default(),
        source: AgentIdSource::Ephemeral,
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
    let uuid = Uuid::parse_str(raw).map_err(|e| {
        ResolveError::Config(ConfigError::AgentBadId(e))
    })?;
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::config::path_in;
    use tempfile::TempDir;

    fn seed_config(t: &TempDir, names: &[&str]) -> PathBuf {
        let path = path_in(t.path());
        let mut c = Config::load_or_default_at(&path).unwrap().0;
        for n in names {
            c.create_agent(n, "").unwrap();
        }
        c.save().unwrap();
        path
    }

    // ----- precedence + happy paths --------------------------------

    #[test]
    fn flag_name_resolves_to_stored_agent() {
        let t = TempDir::new().unwrap();
        let path = seed_config(&t, &["work"]);
        let r = resolve_with(
            ResolveInputs {
                agent_flag: Some("work"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap();
        match r.source {
            AgentIdSource::NamedFlag { name, file } => {
                assert_eq!(name, "work");
                assert_eq!(file, path);
            }
            other => panic!("expected NamedFlag, got {other:?}"),
        }
    }

    #[test]
    fn flag_id_bypasses_named_lookup() {
        let t = TempDir::new().unwrap();
        let path = path_in(t.path()); // file may not exist; OK
        let uuid = Uuid::now_v7();
        let r = resolve_with(
            ResolveInputs {
                agent_id_flag: Some(&uuid.to_string()),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap();
        assert_eq!(r.agent_id.0, uuid);
        assert_eq!(r.source, AgentIdSource::IdFlag);
    }

    #[test]
    fn env_name_resolves_to_stored_agent() {
        let t = TempDir::new().unwrap();
        let path = seed_config(&t, &["work"]);
        let r = resolve_with(
            ResolveInputs {
                agent_env: Some("work"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap();
        assert!(matches!(r.source, AgentIdSource::NamedEnv { .. }));
    }

    #[test]
    fn env_id_resolves_directly() {
        let t = TempDir::new().unwrap();
        let uuid = Uuid::now_v7();
        let r = resolve_with(
            ResolveInputs {
                agent_id_env: Some(&uuid.to_string()),
                ..Default::default()
            },
            Some(&path_in(t.path())),
        )
        .unwrap();
        assert_eq!(r.agent_id.0, uuid);
        assert_eq!(r.source, AgentIdSource::IdEnv);
    }

    #[test]
    fn bare_resolution_returns_ephemeral() {
        let t = TempDir::new().unwrap();
        // Even with a config that HAS agents, bare invocation goes
        // ephemeral — that's the locked design decision.
        let path = seed_config(&t, &["work", "demo"]);
        let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
        assert_eq!(r.source, AgentIdSource::Ephemeral);
        assert_ne!(r.agent_id.0, Uuid::nil());
    }

    #[test]
    fn flag_name_overrides_env_name() {
        let t = TempDir::new().unwrap();
        let path = seed_config(&t, &["work", "demo"]);
        let r = resolve_with(
            ResolveInputs {
                agent_flag: Some("demo"),
                agent_env: Some("work"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap();
        match r.source {
            AgentIdSource::NamedFlag { name, .. } => assert_eq!(name, "demo"),
            other => panic!("expected NamedFlag, got {other:?}"),
        }
    }

    // ----- error paths ---------------------------------------------

    #[test]
    fn flag_name_missing_errors_with_hint() {
        let t = TempDir::new().unwrap();
        let path = seed_config(&t, &["work"]);
        let err = resolve_with(
            ResolveInputs {
                agent_flag: Some("wokr"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap_err();
        match err {
            ResolveError::UnknownNamed { name, suggestion } => {
                assert_eq!(name, "wokr");
                assert_eq!(suggestion.as_deref(), Some("work"));
            }
            other => panic!("expected UnknownNamed, got {other:?}"),
        }
    }

    #[test]
    fn env_name_missing_errors() {
        let t = TempDir::new().unwrap();
        let path = seed_config(&t, &["work"]);
        let err = resolve_with(
            ResolveInputs {
                agent_env: Some("nope"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::UnknownNamed { .. }), "got {err:?}");
    }

    #[test]
    fn flag_id_invalid_uuid_errors() {
        let err = resolve_with(
            ResolveInputs {
                agent_id_flag: Some("definitely-not-a-uuid"),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::BadFlagId(_)), "got {err:?}");
    }

    #[test]
    fn env_id_invalid_uuid_errors() {
        let err = resolve_with(
            ResolveInputs {
                agent_id_env: Some("garbage"),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::BadEnvId(_)), "got {err:?}");
    }

    #[test]
    fn flag_name_and_flag_id_both_set_errors() {
        let err = resolve_with(
            ResolveInputs {
                agent_flag: Some("work"),
                agent_id_flag: Some(&Uuid::now_v7().to_string()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::FlagsConflict), "got {err:?}");
    }

    #[test]
    fn env_name_and_env_id_both_set_errors() {
        let err = resolve_with(
            ResolveInputs {
                agent_env: Some("work"),
                agent_id_env: Some(&Uuid::now_v7().to_string()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::EnvConflict), "got {err:?}");
    }

    // ----- migration ----------------------------------------------

    #[test]
    fn legacy_singleton_migrates_and_bare_still_ephemeral() {
        let t = TempDir::new().unwrap();
        let path = path_in(t.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = "019e3b00-0000-7000-8000-000000000001";
        std::fs::write(&path, format!("agent_id = \"{legacy}\"\n")).unwrap();

        // Bare resolution returns ephemeral AND surfaces migration.
        let r = resolve_with(ResolveInputs::default(), Some(&path)).unwrap();
        assert_eq!(r.source, AgentIdSource::Ephemeral);
        let note = r.migration.as_ref().expect("migration note");
        assert_eq!(note.migrated_name, "default");

        // But the migrated `default` agent is now reachable via name.
        let r2 = resolve_with(
            ResolveInputs {
                agent_flag: Some("default"),
                ..Default::default()
            },
            Some(&path),
        )
        .unwrap();
        assert_eq!(r2.agent_id.0.to_string(), legacy);
    }
}
