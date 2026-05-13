//! `brain-cli agent {list,stats,delete}` — spec §14/06 §10;
//! sub-task 10.11. All deferred (agent_id index not yet present).

pub mod delete;
pub mod list;
pub mod stats;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    List { shard: Option<usize> },
    Stats { agent_id: String },
    Delete { agent_id: String, confirm: bool },
}

impl AgentAction {
    pub fn parse(args: &[String], _shard_default: usize, flags: &FamilyFlags) -> Result<Self> {
        let action = args
            .first()
            .ok_or_else(|| anyhow!("agent requires a sub-action (list/stats/delete)"))?;
        match action.as_str() {
            "list" => Ok(Self::List { shard: None }),
            "stats" => {
                let id = args
                    .get(1)
                    .cloned()
                    .ok_or_else(|| anyhow!("`agent stats` requires <agent-id>"))?;
                Ok(Self::Stats { agent_id: id })
            }
            "delete" => {
                let id = args
                    .get(1)
                    .cloned()
                    .ok_or_else(|| anyhow!("`agent delete` requires <agent-id>"))?;
                Ok(Self::Delete {
                    agent_id: id,
                    confirm: flags.confirm,
                })
            }
            other => Err(anyhow!("unknown agent sub-action `{other}`")),
        }
    }
}

pub fn run(server: &str, action: &AgentAction, _output: OutputFormat) -> Result<String> {
    match action {
        AgentAction::List { shard } => list::run(server, *shard),
        AgentAction::Stats { agent_id } => stats::run(server, agent_id),
        AgentAction::Delete { agent_id, confirm } => delete::run(server, agent_id, *confirm),
    }
}
