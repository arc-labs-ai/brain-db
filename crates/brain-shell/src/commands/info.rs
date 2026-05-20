//! `\info` (REPL meta) / `brain info` (one-shot) data collection.
//!
//! Pulls together the four pieces that drive the diagnostic card:
//! a snapshot of the handshake the SDK negotiated, the resolved
//! agent identity (plus its config entry, if any), the live SDK
//! connection state, and the shell's session preferences.
//!
//! The collection step is split out from the renderer (which lives
//! in `brain-explore`) so the shell can decide what state to read
//! without the renderer pulling in `brain-sdk-rust` or the shell's
//! config types.

use brain_core::AgentId;
use brain_explore::{
    AgentInfo, ConnectionInfo, InfoCard, ServerInfo, ServerWelcomeFields, SessionInfo,
};
use brain_sdk_rust::Client;

use crate::cli::agent::AgentIdSource;
use crate::cli::config::Config;
use crate::parser::format_txn_id;
use crate::session::Session;

/// Build the `InfoCard` for the current session.
///
/// The handshake snapshot is fetched via `Client::session()`, which
/// acquires a connection from the pool. On a healthy connected
/// client the call succeeds immediately (the pool already has a
/// warmed connection); if it fails (e.g. server gone, pool closed)
/// we surface that as "(not connected)" rather than an error — the
/// whole point of `\info` is to render *something* useful even
/// when the connection has misbehaved.
pub async fn collect(
    client: &Client,
    session: &Session,
    agent_id: AgentId,
    agent_source: &AgentIdSource,
) -> InfoCard {
    let welcome = match client.session().await {
        Ok(Some(s)) => Some(s),
        Ok(None) | Err(_) => None,
    };

    let agent_entry = lookup_agent_entry(agent_source);

    InfoCard {
        server: ServerInfo {
            address: session.server.to_string(),
            welcome: welcome.as_ref().map(|s| ServerWelcomeFields {
                server_id: s.welcome.server_id.clone(),
                wire_version: s.welcome.chosen_version,
                server_time_unix_nanos: s.auth_ok.server_time_unix_nanos,
                bound_shard: s.auth_ok.bound_shard_id,
                streaming: s.welcome.capabilities.streaming,
                compression_zstd: s.welcome.capabilities.compression_zstd,
                server_push: s.welcome.capabilities.server_push,
            }),
        },
        agent: AgentInfo {
            name: source_name(agent_source),
            agent_id: *agent_id.0.as_bytes(),
            source_label: source_label(agent_source),
            default: agent_entry.as_ref().map(|e| e.default).unwrap_or(false),
            note: agent_entry
                .as_ref()
                .map(|e| e.note.clone())
                .unwrap_or_default(),
            created_at: agent_entry
                .as_ref()
                .map(|e| e.created_at.clone())
                .filter(|s| !s.is_empty()),
        },
        connection: ConnectionInfo {
            authenticated: welcome.is_some(),
            // The SDK doesn't currently expose a connect timestamp;
            // leave None until that lands rather than fabricate one.
            connected_at_unix_nanos: None,
        },
        session: SessionInfo {
            output: session.output.short_name().to_string(),
            sticky_context: session.sticky_context,
            active_txn: session
                .active_txn
                .as_ref()
                .map(|bytes| format_txn_id(bytes)),
            timing: session.timing,
        },
    }
}

/// Pull the matching `AgentEntry` from the on-disk config — when the
/// resolved source has a name to look up. Returns `None` for raw-id
/// flows and ephemeral binds where there's nothing to find. Best-
/// effort: a missing or unreadable file silently yields `None` (the
/// renderer falls back to defaults).
fn lookup_agent_entry(source: &AgentIdSource) -> Option<crate::cli::config::AgentEntry> {
    let name = source_name(source)?;
    let (config, _note) = Config::load_or_default().ok()?;
    config.get_agent(&name).ok().cloned()
}

fn source_name(source: &AgentIdSource) -> Option<String> {
    match source {
        AgentIdSource::NamedFlag { name, .. }
        | AgentIdSource::NamedEnv { name, .. }
        | AgentIdSource::ActiveFromConfig { name, .. }
        | AgentIdSource::DefaultFromConfig { name, .. }
        | AgentIdSource::AutoMinted { name, .. } => Some(name.clone()),
        AgentIdSource::IdFlag | AgentIdSource::IdEnv | AgentIdSource::Ephemeral => None,
    }
}

/// Human-readable provenance for the agent. Mirrors the wording used
/// by the connect banner so the two surfaces agree.
fn source_label(source: &AgentIdSource) -> String {
    match source {
        AgentIdSource::NamedFlag { name, .. } => format!("--agent {name}"),
        AgentIdSource::IdFlag => "--agent-id".into(),
        AgentIdSource::NamedEnv { name, .. } => format!("BRAIN_AGENT={name}"),
        AgentIdSource::IdEnv => "BRAIN_AGENT_ID".into(),
        AgentIdSource::ActiveFromConfig { name, .. } => format!("config: active = {name}"),
        AgentIdSource::DefaultFromConfig { name, .. } => format!("config: default = {name}"),
        AgentIdSource::AutoMinted { name, .. } => format!("auto-minted as {name}"),
        AgentIdSource::Ephemeral => "ephemeral (no config file)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn source_name_returns_none_for_unnamed_variants() {
        assert!(source_name(&AgentIdSource::IdFlag).is_none());
        assert!(source_name(&AgentIdSource::IdEnv).is_none());
        assert!(source_name(&AgentIdSource::Ephemeral).is_none());
    }

    #[test]
    fn source_name_returns_some_for_named_variants() {
        let file = PathBuf::from("/x/y");
        assert_eq!(
            source_name(&AgentIdSource::NamedFlag {
                name: "work".into(),
                file: file.clone(),
            }),
            Some("work".to_string())
        );
        assert_eq!(
            source_name(&AgentIdSource::ActiveFromConfig {
                name: "active".into(),
                file: file.clone(),
            }),
            Some("active".to_string())
        );
        assert_eq!(
            source_name(&AgentIdSource::AutoMinted {
                name: "agent-deadbeef".into(),
                file,
            }),
            Some("agent-deadbeef".to_string())
        );
    }

    #[test]
    fn source_label_covers_every_variant() {
        let file = PathBuf::from("/x/y");
        // Every variant must produce a non-empty label, otherwise
        // \info would render a blank "source" row that's worse than
        // an explicit "ephemeral" / "--agent-id" string.
        for v in [
            AgentIdSource::IdFlag,
            AgentIdSource::IdEnv,
            AgentIdSource::Ephemeral,
            AgentIdSource::NamedFlag {
                name: "w".into(),
                file: file.clone(),
            },
            AgentIdSource::NamedEnv {
                name: "w".into(),
                file: file.clone(),
            },
            AgentIdSource::ActiveFromConfig {
                name: "w".into(),
                file: file.clone(),
            },
            AgentIdSource::DefaultFromConfig {
                name: "w".into(),
                file: file.clone(),
            },
            AgentIdSource::AutoMinted {
                name: "w".into(),
                file,
            },
        ] {
            assert!(!source_label(&v).is_empty(), "empty label for {v:?}");
        }
    }
}
