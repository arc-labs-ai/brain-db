//! Hand-rolled argv parsing. Tiny surface (1 main command + 4
//! global flags); skipping `clap` keeps the CLI's dep footprint
//! minimal. 10.9+ may switch when nested subcommands grow.

use anyhow::{anyhow, Result};

pub const DEFAULT_SERVER: &str = "127.0.0.1:9091";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Table,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "json" => Ok(Self::Json),
            "table" => Ok(Self::Table),
            other => Err(anyhow!("unknown --output `{other}`; use json | table")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Args {
    pub server: String,
    pub output: OutputFormat,
    pub token: Option<String>,
    pub command: Command,
}

/// Optional sub-flags consumed by the 10.11 command families. Each
/// field is populated only if the operator passes the corresponding
/// `--name`, `--key`, `--value`, … flag. Stored as a flat bag so the
/// argv loop stays simple; family parsers pull what they need.
#[derive(Debug, Clone, Default)]
pub struct FamilyFlags {
    pub name: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub agent: Option<String>,
    pub logical_id: Option<u16>,
    pub confirm: bool,
    /// Sub-task 10.12 — `profile --duration-secs N`. Defaults to 30.
    pub duration_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Health,
    Stats,
    /// Sub-task 10.9 — snapshot family. The sub-action + args are
    /// validated by [`crate::commands::snapshot::SnapshotAction::parse`].
    Snapshot(crate::commands::snapshot::SnapshotAction),
    /// Sub-task 10.10 — `rebuild-ann [--shard N]`.
    RebuildAnn {
        shard: usize,
    },
    /// Sub-task 10.11 — five new command families. Sub-actions live
    /// in `crate::commands::<family>`.
    Worker(crate::commands::worker::WorkerAction),
    Config(crate::commands::config::ConfigAction),
    Audit(crate::commands::audit::AuditAction),
    Agent(crate::commands::agent::AgentAction),
    Shard(crate::commands::shard::ShardAction),
    /// Sub-task 10.12 — `profile --shard N [--duration-secs D] [--value PATH]`.
    Profile {
        shard: usize,
        duration_secs: u32,
        output_path: Option<String>,
    },
    /// Sub-task 10.12 — `debug-snapshot --shard N [--value PATH]`.
    DebugSnapshot {
        shard: usize,
        output_path: Option<String>,
    },
}

/// Parse a `Vec<String>` (typically `env::args().skip(1).collect()`).
pub fn parse(argv: Vec<String>) -> Result<Args> {
    let mut server = DEFAULT_SERVER.to_string();
    let mut output = OutputFormat::Table;
    let mut token: Option<String> = None;
    let mut shard: usize = 0;
    let mut positional: Vec<String> = Vec::new();
    let mut family = FamilyFlags::default();

    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--help" | "-h" => {
                return Ok(Args {
                    server,
                    output,
                    token,
                    command: Command::Help,
                })
            }
            "--version" | "-V" => {
                return Ok(Args {
                    server,
                    output,
                    token,
                    command: Command::Version,
                })
            }
            "--server" => {
                i += 1;
                server = take_value("--server", &argv, i)?.to_string();
            }
            "--output" => {
                i += 1;
                output = OutputFormat::parse(take_value("--output", &argv, i)?)?;
            }
            "--token" => {
                i += 1;
                token = Some(take_value("--token", &argv, i)?.to_string());
            }
            "--shard" => {
                i += 1;
                let v = take_value("--shard", &argv, i)?;
                shard = v
                    .parse::<usize>()
                    .map_err(|e| anyhow!("invalid --shard `{v}`: {e}"))?;
            }
            // ----- 10.11 family flags --------------------------------
            "--name" => {
                i += 1;
                family.name = Some(take_value("--name", &argv, i)?.to_string());
            }
            "--key" => {
                i += 1;
                family.key = Some(take_value("--key", &argv, i)?.to_string());
            }
            "--value" => {
                i += 1;
                family.value = Some(take_value("--value", &argv, i)?.to_string());
            }
            "--since" => {
                i += 1;
                family.since = Some(take_value("--since", &argv, i)?.to_string());
            }
            "--until" => {
                i += 1;
                family.until = Some(take_value("--until", &argv, i)?.to_string());
            }
            "--agent" => {
                i += 1;
                family.agent = Some(take_value("--agent", &argv, i)?.to_string());
            }
            "--logical-id" => {
                i += 1;
                let v = take_value("--logical-id", &argv, i)?;
                family.logical_id = Some(
                    v.parse::<u16>()
                        .map_err(|e| anyhow!("invalid --logical-id `{v}`: {e}"))?,
                );
            }
            "--confirm" => {
                family.confirm = true;
            }
            "--duration-secs" => {
                i += 1;
                let v = take_value("--duration-secs", &argv, i)?;
                family.duration_secs = Some(
                    v.parse::<u32>()
                        .map_err(|e| anyhow!("invalid --duration-secs `{v}`: {e}"))?,
                );
            }
            other if other.starts_with("--") => {
                return Err(anyhow!("unknown flag `{other}`"));
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let command = match positional.first().map(String::as_str) {
        None => Command::Help,
        Some("health") => Command::Health,
        Some("stats") => Command::Stats,
        Some("snapshot") => {
            use crate::commands::snapshot::SnapshotAction;
            let rest = positional[1..].to_vec();
            let action = SnapshotAction::parse(&rest, shard)?;
            Command::Snapshot(action)
        }
        Some("rebuild-ann") => Command::RebuildAnn { shard },
        Some("worker") => {
            use crate::commands::worker::WorkerAction;
            Command::Worker(WorkerAction::parse(&positional[1..], shard, &family)?)
        }
        Some("config") => {
            use crate::commands::config::ConfigAction;
            Command::Config(ConfigAction::parse(&positional[1..], &family)?)
        }
        Some("audit") => {
            use crate::commands::audit::AuditAction;
            Command::Audit(AuditAction::parse(&positional[1..], &family)?)
        }
        Some("agent") => {
            use crate::commands::agent::AgentAction;
            Command::Agent(AgentAction::parse(&positional[1..], shard, &family)?)
        }
        Some("shard") => {
            use crate::commands::shard::ShardAction;
            Command::Shard(ShardAction::parse(&positional[1..], &family)?)
        }
        Some("profile") => Command::Profile {
            shard,
            duration_secs: family.duration_secs.unwrap_or(30),
            output_path: family.value.clone(),
        },
        Some("debug-snapshot") => Command::DebugSnapshot {
            shard,
            output_path: family.value.clone(),
        },
        Some(other) => return Err(anyhow!("unknown subcommand `{other}`")),
    };

    Ok(Args {
        server,
        output,
        token,
        command,
    })
}

fn take_value<'a>(flag: &str, argv: &'a [String], i: usize) -> Result<&'a str> {
    argv.get(i)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("{flag} expects a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Args> {
        parse(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn defaults() {
        let a = parse_str(&["health"]).unwrap();
        assert_eq!(a.server, DEFAULT_SERVER);
        assert_eq!(a.output, OutputFormat::Table);
        assert!(a.token.is_none());
        assert_eq!(a.command, Command::Health);
    }

    #[test]
    fn server_override() {
        let a = parse_str(&["--server", "foo:7", "health"]).unwrap();
        assert_eq!(a.server, "foo:7");
    }

    #[test]
    fn json_output() {
        let a = parse_str(&["--output", "json", "stats"]).unwrap();
        assert_eq!(a.output, OutputFormat::Json);
        assert_eq!(a.command, Command::Stats);
    }

    #[test]
    fn unknown_subcommand_errors() {
        let err = parse_str(&["totally-fake"]).err().unwrap();
        assert!(err.to_string().contains("unknown subcommand"));
    }

    #[test]
    fn unknown_output_errors() {
        let err = parse_str(&["--output", "yaml", "stats"]).err().unwrap();
        assert!(err.to_string().contains("unknown --output"));
    }

    #[test]
    fn no_args_is_help() {
        let a = parse_str(&[]).unwrap();
        assert_eq!(a.command, Command::Help);
    }

    #[test]
    fn help_flag_short_circuits() {
        let a = parse_str(&["--server", "x:1", "--help"]).unwrap();
        assert_eq!(a.command, Command::Help);
    }

    #[test]
    fn version_flag() {
        let a = parse_str(&["-V"]).unwrap();
        assert_eq!(a.command, Command::Version);
    }
}
