//! `brain-cli snapshot {create,list,delete,restore}` —
//! spec §14/06 §5; sub-task 10.9.

pub mod create;
pub mod delete;
pub mod list;
pub mod restore;

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAction {
    Create { shard: usize },
    List,
    Delete { id: u64, shard: usize },
    Restore { id: u64 },
}

impl SnapshotAction {
    /// Parse the positional args following `snapshot`. The caller
    /// has already extracted the `--shard N` flag (if any).
    pub fn parse(args: &[String], shard: usize) -> Result<Self> {
        let action = args.first().ok_or_else(|| {
            anyhow!("snapshot requires a sub-action (create/list/delete/restore)")
        })?;
        match action.as_str() {
            "create" => Ok(Self::Create { shard }),
            "list" => Ok(Self::List),
            "delete" => {
                let id_str = args
                    .get(1)
                    .ok_or_else(|| anyhow!("`snapshot delete` requires <id>"))?;
                let id: u64 = id_str
                    .parse()
                    .map_err(|e| anyhow!("invalid snapshot id `{id_str}`: {e}"))?;
                Ok(Self::Delete { id, shard })
            }
            "restore" => {
                let id_str = args
                    .get(1)
                    .ok_or_else(|| anyhow!("`snapshot restore` requires <id>"))?;
                let id: u64 = id_str
                    .parse()
                    .map_err(|e| anyhow!("invalid snapshot id `{id_str}`: {e}"))?;
                Ok(Self::Restore { id })
            }
            other => Err(anyhow!("unknown snapshot sub-action `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], shard: usize) -> Result<SnapshotAction> {
        SnapshotAction::parse(
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            shard,
        )
    }

    #[test]
    fn parses_create() {
        assert_eq!(
            parse(&["create"], 0).unwrap(),
            SnapshotAction::Create { shard: 0 }
        );
        assert_eq!(
            parse(&["create"], 2).unwrap(),
            SnapshotAction::Create { shard: 2 }
        );
    }

    #[test]
    fn parses_list() {
        assert_eq!(parse(&["list"], 0).unwrap(), SnapshotAction::List);
    }

    #[test]
    fn parses_delete() {
        assert_eq!(
            parse(&["delete", "42"], 1).unwrap(),
            SnapshotAction::Delete { id: 42, shard: 1 }
        );
    }

    #[test]
    fn parses_restore() {
        assert_eq!(
            parse(&["restore", "7"], 0).unwrap(),
            SnapshotAction::Restore { id: 7 }
        );
    }

    #[test]
    fn delete_requires_id() {
        assert!(parse(&["delete"], 0).is_err());
    }

    #[test]
    fn unknown_action_errors() {
        assert!(parse(&["totally-fake"], 0).is_err());
    }
}
