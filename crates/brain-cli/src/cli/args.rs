//! Hand-rolled argv parsing. Tiny surface (1 main command + global flags);
//! skipping `clap` keeps the CLI's dep footprint minimal. May switch when
//! nested subcommands grow.

use std::str::FromStr;

use anyhow::{anyhow, Result};
use brain_explore::term::policy::{ColorMode, HyperlinkMode};

/// Default admin endpoint — serves `/v1/*` routes. Loopback by
/// default; matches `server.admin_addr` in `config/dev.toml`.
pub const DEFAULT_SERVER: &str = "127.0.0.1:9092";

/// Default metrics endpoint — serves `/healthz` + `/metrics`.
/// Matches `server.metrics_addr` in `config/dev.toml`. Used by the
/// `health` and `stats` subcommands.
pub const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9091";

/// Public alias so callers say `brain_cli::cli::OutputFormat` rather
/// than reaching across the workspace into `brain_explore`. Same enum;
/// no conversion needed.
pub use brain_explore::OutputFormat;

#[derive(Debug, Clone)]
pub struct Args {
    /// Admin endpoint (`/v1/*` routes). Default `127.0.0.1:9092`.
    pub server: String,
    /// Metrics endpoint (`/healthz` + `/metrics`). Default
    /// `127.0.0.1:9091`. Used by the `health` and `stats`
    /// subcommands.
    pub metrics_addr: String,
    pub output: OutputFormat,
    pub color: ColorMode,
    pub hyperlinks: HyperlinkMode,
    pub token: Option<String>,
    pub command: Command,
}

/// Optional sub-flags consumed by the command families. Each field is
/// populated only if the operator passes the corresponding `--name`,
/// `--key`, `--value`, … flag. Stored as a flat bag so the argv loop
/// stays simple; family parsers pull what they need.
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
    /// `profile --duration-secs N`. Defaults to 30.
    pub duration_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Health,
    Stats,
    /// Snapshot family. The sub-action + args are validated by
    /// [`crate::commands::snapshot::SnapshotAction::parse`].
    Snapshot(crate::commands::snapshot::SnapshotAction),
    /// `rebuild-ann [--shard N]`.
    RebuildAnn {
        shard: usize,
    },
    /// Five command families. Sub-actions live in
    /// `crate::commands::<family>`.
    Worker(crate::commands::worker::WorkerAction),
    Config(crate::commands::config::ConfigAction),
    Audit(crate::commands::audit::AuditAction),
    Agent(crate::commands::agent::AgentAction),
    Shard(crate::commands::shard::ShardAction),
    /// `profile --shard N [--duration-secs D] [--value PATH]`.
    Profile {
        shard: usize,
        duration_secs: u32,
        output_path: Option<String>,
    },
    /// `debug-snapshot --shard N [--value PATH]`.
    DebugSnapshot {
        shard: usize,
        output_path: Option<String>,
    },
    /// `extract --backfill --memory-id <id> | --since <ts> | --all`.
    /// Re-enqueues existing memories for the three-tier extractor
    /// pipeline (POST `/v1/extract/backfill`).
    Extract(crate::commands::extract::ExtractAction),
}

fn parse_color_mode(s: &str) -> Result<ColorMode> {
    match s {
        "auto" => Ok(ColorMode::Auto),
        "always" => Ok(ColorMode::Always),
        "never" => Ok(ColorMode::Never),
        other => Err(anyhow!(
            "unknown --color `{other}`; use auto | always | never"
        )),
    }
}

fn parse_hyperlink_mode(s: &str) -> Result<HyperlinkMode> {
    match s {
        "auto" => Ok(HyperlinkMode::Auto),
        "always" => Ok(HyperlinkMode::Always),
        "never" => Ok(HyperlinkMode::Never),
        other => Err(anyhow!(
            "unknown --hyperlinks `{other}`; use auto | always | never"
        )),
    }
}

/// Parse a `Vec<String>` (typically `env::args().skip(1).collect()`).
pub fn parse(argv: Vec<String>) -> Result<Args> {
    let mut server = DEFAULT_SERVER.to_string();
    let mut metrics_addr = DEFAULT_METRICS_ADDR.to_string();
    let mut output = OutputFormat::default();
    let mut color = ColorMode::Auto;
    let mut hyperlinks = HyperlinkMode::Auto;
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
                    metrics_addr,
                    output,
                    color,
                    hyperlinks,
                    token,
                    command: Command::Help,
                })
            }
            "--version" | "-V" => {
                return Ok(Args {
                    server,
                    metrics_addr,
                    output,
                    color,
                    hyperlinks,
                    token,
                    command: Command::Version,
                })
            }
            "--server" => {
                i += 1;
                server = take_value("--server", &argv, i)?.to_string();
            }
            "--metrics-addr" => {
                i += 1;
                metrics_addr = take_value("--metrics-addr", &argv, i)?.to_string();
            }
            "--output" | "-o" => {
                i += 1;
                let v = take_value("--output", &argv, i)?;
                output = OutputFormat::from_str(v)
                    .map_err(|e| anyhow!("unknown --output `{v}`: {e}"))?;
            }
            "--color" => {
                i += 1;
                color = parse_color_mode(take_value("--color", &argv, i)?)?;
            }
            "--hyperlinks" => {
                i += 1;
                hyperlinks = parse_hyperlink_mode(take_value("--hyperlinks", &argv, i)?)?;
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
            // ----- family flags --------------------------------
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
        Some("extract") => {
            use crate::commands::extract::ExtractAction;
            Command::Extract(ExtractAction::parse(&positional[1..], &family)?)
        }
        Some(other) => return Err(anyhow!("unknown subcommand `{other}`")),
    };

    Ok(Args {
        server,
        metrics_addr,
        output,
        color,
        hyperlinks,
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
        assert_eq!(a.metrics_addr, DEFAULT_METRICS_ADDR);
        // Default output is Auto — TTY-aware at dispatch time.
        assert_eq!(a.output, OutputFormat::Auto);
        assert!(a.token.is_none());
        assert_eq!(a.command, Command::Health);
    }

    #[test]
    fn server_override() {
        let a = parse_str(&["--server", "foo:7", "health"]).unwrap();
        assert_eq!(a.server, "foo:7");
        // --server alone leaves the metrics default untouched.
        assert_eq!(a.metrics_addr, DEFAULT_METRICS_ADDR);
    }

    #[test]
    fn metrics_addr_override() {
        let a = parse_str(&["--metrics-addr", "bar:8", "health"]).unwrap();
        assert_eq!(a.metrics_addr, "bar:8");
        assert_eq!(a.server, DEFAULT_SERVER);
    }

    #[test]
    fn json_output() {
        let a = parse_str(&["--output", "json", "stats"]).unwrap();
        assert_eq!(a.output, OutputFormat::Json);
        assert_eq!(a.command, Command::Stats);
    }

    #[test]
    fn short_o_flag_works() {
        let a = parse_str(&["-o", "yaml", "stats"]).unwrap();
        assert_eq!(a.output, OutputFormat::Yaml);
    }

    #[test]
    fn ndjson_format() {
        let a = parse_str(&["--output", "ndjson", "health"]).unwrap();
        assert_eq!(a.output, OutputFormat::Ndjson);
    }

    #[test]
    fn color_modes_parse() {
        let a = parse_str(&["--color", "never", "health"]).unwrap();
        assert_eq!(a.color, ColorMode::Never);
        let a = parse_str(&["--color", "always", "health"]).unwrap();
        assert_eq!(a.color, ColorMode::Always);
    }

    #[test]
    fn hyperlinks_modes_parse() {
        let a = parse_str(&["--hyperlinks", "never", "health"]).unwrap();
        assert_eq!(a.hyperlinks, HyperlinkMode::Never);
    }

    #[test]
    fn unknown_subcommand_errors() {
        let err = parse_str(&["totally-fake"]).err().unwrap();
        assert!(err.to_string().contains("unknown subcommand"));
    }

    #[test]
    fn unknown_output_errors() {
        let err = parse_str(&["--output", "csv", "stats"]).err().unwrap();
        assert!(err.to_string().contains("unknown --output"));
    }

    #[test]
    fn unknown_color_errors() {
        let err = parse_str(&["--color", "rainbow", "stats"]).err().unwrap();
        assert!(err.to_string().contains("unknown --color"));
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
