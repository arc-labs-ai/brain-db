//! `brain-cli shard {list,create,delete}` — spec §14/06 §11;
//! sub-task 10.11. Only `list` is backed; create/delete are
//! Phase-12 cluster operations.

pub mod create;
pub mod delete;
pub mod list;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardAction {
    List,
    Create { logical_id: u16 },
    Delete { shard_id: String, confirm: bool },
}

impl ShardAction {
    pub fn parse(args: &[String], flags: &FamilyFlags) -> Result<Self> {
        let action = args
            .first()
            .ok_or_else(|| anyhow!("shard requires a sub-action (list/create/delete)"))?;
        match action.as_str() {
            "list" => Ok(Self::List),
            "create" => {
                let logical_id = flags
                    .logical_id
                    .ok_or_else(|| anyhow!("`shard create` requires --logical-id <N>"))?;
                Ok(Self::Create { logical_id })
            }
            "delete" => {
                let id = args
                    .get(1)
                    .cloned()
                    .ok_or_else(|| anyhow!("`shard delete` requires <shard-id>"))?;
                Ok(Self::Delete {
                    shard_id: id,
                    confirm: flags.confirm,
                })
            }
            other => Err(anyhow!("unknown shard sub-action `{other}`")),
        }
    }
}

pub fn run(server: &str, action: &ShardAction, output: OutputFormat) -> Result<String> {
    match action {
        ShardAction::List => list::run(server, output),
        ShardAction::Create { logical_id } => create::run(server, *logical_id),
        ShardAction::Delete { shard_id, confirm } => delete::run(server, shard_id, *confirm),
    }
}
