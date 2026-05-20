//! Auto-mint-on-first-run. When bare `brain` is started with no
//! flag, no env, and no agents in the config file, the resolver
//! mints a fresh UUIDv7 and persists it as the default + active
//! agent. The user gets a working substrate identity without any
//! `brain agent create` ceremony.

use brain_core::AgentId;
use uuid::Uuid;

use crate::cli::config::{AgentPromotion, Config, ConfigError};

/// Mint a UUIDv7 and a deterministic name (`agent-<first 8 hex>`),
/// persist as default + active. Returns the chosen name + the
/// minted id so the resolver can return them in a `ResolvedAgentId`.
///
/// Side effect: writes the config file. Persistence is the whole
/// point — without it, the agent vanishes the next session, which
/// is what the pre-redesign behavior did and what made the on-disk
/// file useless to first-run users.
pub fn create_and_persist(config: &mut Config) -> Result<(String, AgentId), ConfigError> {
    let uuid = Uuid::now_v7();
    let name = derive_name(&uuid);
    // `create_agent` mints a new UUID internally, so we can't use it
    // directly when we need the resolver to also know the id. Use
    // `import_agent` with the externally-minted uuid string.
    let entry = config.import_agent(
        &name,
        &uuid.to_string(),
        "",
        AgentPromotion::DefaultAndActive,
    )?;
    let _ = entry; // entry borrow released
    config.save()?;
    Ok((name, AgentId(uuid)))
}

/// Build the canonical auto-mint name: `agent-<first 8 hex chars
/// of the UUID>`. Deterministic from the uuid — same machine
/// regenerates the same name shape if the file is deleted (it
/// won't, but the property is nice for debugging).
fn derive_name(uuid: &Uuid) -> String {
    let hex = uuid.simple().to_string();
    format!("agent-{}", &hex[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::config::path_in;
    use tempfile::TempDir;

    #[test]
    fn derive_name_uses_first_8_hex() {
        let uuid = Uuid::parse_str("01927a8b-4c2f-7000-8000-deadbeeffeed").unwrap();
        assert_eq!(derive_name(&uuid), "agent-01927a8b");
    }

    #[test]
    fn create_and_persist_writes_file_with_both_flags() {
        let tmp = TempDir::new().unwrap();
        let path = path_in(tmp.path());
        let mut config = Config::load_or_default_at(&path).unwrap().0;
        assert!(config.agents().is_empty());

        let (name, agent_id) = create_and_persist(&mut config).unwrap();
        assert!(name.starts_with("agent-"));
        assert_eq!(name.len(), 14); // "agent-" + 8 hex

        // The created agent must be both default and active.
        let entry = config.agents().get(&name).expect("agent persisted");
        assert!(entry.default);
        assert!(entry.active);
        assert_eq!(entry.id, agent_id.0.to_string());

        // And the file should be on disk (not just in memory).
        let reloaded = Config::load_or_default_at(&path).unwrap().0;
        assert_eq!(reloaded.agents().len(), 1);
        let reloaded_entry = reloaded.agents().get(&name).expect("persisted on disk");
        assert!(reloaded_entry.default);
        assert!(reloaded_entry.active);
    }
}
