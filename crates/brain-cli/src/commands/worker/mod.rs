//! `brain-cli worker {list,stop,start,run-now}` — spec §14/06 §6;
//! sub-task 10.11. `list` is backed end-to-end; control actions
//! return the structured 501 surfaced by the admin server.

pub mod common;
pub mod list;
pub mod run_now;
pub mod start;
pub mod stop;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    List { shard: Option<usize> },
    Stop { name: String, shard: usize },
    Start { name: String, shard: usize },
    RunNow { name: String, shard: usize },
}

impl WorkerAction {
    /// `shard_default` is whatever `--shard N` parsed at the global
    /// level — for `worker list` this is treated as a filter only if
    /// the operator explicitly set it; we can't tell from outside, so
    /// `list` always sends to the server unfiltered (server handles
    /// `?shard=N` query). v2 can plumb a tri-state if needed.
    pub fn parse(args: &[String], shard_default: usize, flags: &FamilyFlags) -> Result<Self> {
        let action = args
            .first()
            .ok_or_else(|| anyhow!("worker requires a sub-action (list/stop/start/run-now)"))?;
        match action.as_str() {
            "list" => Ok(Self::List { shard: None }),
            "stop" | "start" | "run-now" => {
                let name = flags
                    .name
                    .clone()
                    .ok_or_else(|| anyhow!("`worker {action}` requires --name <worker>"))?;
                let result = match action.as_str() {
                    "stop" => Self::Stop {
                        name,
                        shard: shard_default,
                    },
                    "start" => Self::Start {
                        name,
                        shard: shard_default,
                    },
                    "run-now" => Self::RunNow {
                        name,
                        shard: shard_default,
                    },
                    _ => unreachable!(),
                };
                Ok(result)
            }
            other => Err(anyhow!("unknown worker sub-action `{other}`")),
        }
    }
}

/// Dispatch the action against the admin server.
pub fn run(server: &str, action: &WorkerAction, output: OutputFormat) -> Result<String> {
    match action {
        WorkerAction::List { shard } => list::run(server, *shard, output),
        WorkerAction::Stop { name, shard } => stop::run(server, name, *shard, output),
        WorkerAction::Start { name, shard } => start::run(server, name, *shard, output),
        WorkerAction::RunNow { name, shard } => run_now::run(server, name, *shard, output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], shard: usize, flags: FamilyFlags) -> Result<WorkerAction> {
        WorkerAction::parse(
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            shard,
            &flags,
        )
    }

    #[test]
    fn parses_list() {
        assert_eq!(
            parse(&["list"], 0, FamilyFlags::default()).unwrap(),
            WorkerAction::List { shard: None }
        );
    }

    #[test]
    fn stop_requires_name() {
        assert!(parse(&["stop"], 0, FamilyFlags::default()).is_err());
    }

    #[test]
    fn parses_stop() {
        let f = FamilyFlags {
            name: Some("decay".into()),
            ..Default::default()
        };
        assert_eq!(
            parse(&["stop"], 1, f).unwrap(),
            WorkerAction::Stop {
                name: "decay".into(),
                shard: 1,
            }
        );
    }

    #[test]
    fn unknown_action_errors() {
        assert!(parse(&["totally-fake"], 0, FamilyFlags::default()).is_err());
    }
}
