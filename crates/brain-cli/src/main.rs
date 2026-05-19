//! # brain-cli
//!
//! Admin CLI for the Brain substrate. See spec §14/06 for the full
//! command surface. health + stats hit the public metrics listener; every
//! other command lands on the admin listener.

#![allow(clippy::missing_errors_doc)]

use std::env;
use std::io::IsTerminal;
use std::process::ExitCode;

use brain_cli::cli::{parse, Command, OutputFormat};
use brain_cli::commands::diagnostics::{debug_snapshot, profile};
use brain_cli::commands::snapshot::SnapshotAction;
use brain_cli::commands::{
    agent, audit, config, extract, health, rebuild, shard, snapshot, stats, worker,
};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_help();
            return ExitCode::from(2);
        }
    };

    // Resolve `Auto` at the binary boundary so command modules (and their
    // tests) see a concrete format and the dispatch helper they use never
    // has to consult the TTY itself.
    let output = resolve_format(args.output);

    match args.command {
        Command::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("{NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Health => run_result(health::run(&args.metrics_addr, output)),
        Command::Stats => run_result(stats::run(&args.metrics_addr, output)),
        Command::RebuildAnn { shard } => run_result(rebuild::run(&args.server, shard, output)),
        Command::Snapshot(action) => {
            let result = match action {
                SnapshotAction::Create { shard } => {
                    snapshot::create::run(&args.server, shard, output)
                }
                SnapshotAction::List => snapshot::list::run(&args.server, output),
                SnapshotAction::Delete { id, shard } => {
                    snapshot::delete::run(&args.server, id, shard, output)
                }
                SnapshotAction::Restore { id } => snapshot::restore::run(&args.server, id, output),
            };
            run_result(result)
        }
        Command::Worker(action) => run_result(worker::run(&args.server, &action, output)),
        Command::Config(action) => run_result(config::run(&args.server, &action, output)),
        Command::Audit(action) => run_result(audit::run(&args.server, &action, output)),
        Command::Agent(action) => run_result(agent::run(&args.server, &action, output)),
        Command::Shard(action) => run_result(shard::run(&args.server, &action, output)),
        Command::Profile {
            shard,
            duration_secs,
            output_path,
        } => run_result(profile::run(
            &args.server,
            shard,
            duration_secs,
            output_path.as_deref(),
        )),
        Command::DebugSnapshot { shard, output_path } => run_result(debug_snapshot::run(
            &args.server,
            shard,
            output_path.as_deref(),
            output,
        )),
        Command::Extract(action) => run_result(extract::run(&args.server, &action, output)),
    }
}

/// Collapse `Auto` to a concrete format using stdout-TTY detection so
/// the command modules render deterministically (table on a terminal,
/// ndjson when piped — matches what kubectl/`gh` do).
fn resolve_format(requested: OutputFormat) -> OutputFormat {
    match requested {
        OutputFormat::Auto => {
            if std::io::stdout().is_terminal() {
                OutputFormat::Table
            } else {
                OutputFormat::Ndjson
            }
        }
        other => other,
    }
}

fn run_result(result: anyhow::Result<String>) -> ExitCode {
    match result {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "{NAME} {VERSION}
Admin CLI for the Brain substrate.

USAGE:
    brain-cli [OPTIONS] <COMMAND>

COMMANDS:
    health                                    Probe /healthz
    stats                                     Snapshot /metrics counters
    snapshot create|list|delete|restore       Snapshot family
    rebuild-ann [--shard N]                   Rebuild HNSW for a shard
    worker list|stop|start|run-now            Worker control (some deferred)
    config get|reload|set                     Read/write config (some deferred)
    audit query|export                        Audit log (deferred)
    agent list|stats|delete                   Agent operations (deferred)
    shard list|create|delete                  Shard operations (create/delete deferred)
    profile [--duration-secs N] [--value P]   CPU profile (deferred)
    debug-snapshot [--value PATH]             Runtime snapshot (partial schema)
    extract --backfill (--memory-id N | --since TS | --all)
                                              Re-enqueue memories through the
                                              three-tier extractor pipeline

OPTIONS:
    --server <host:port>      Admin endpoint — /v1/* routes
                              (default 127.0.0.1:9092)
    --metrics-addr <host:port>  Metrics endpoint — /healthz + /metrics,
                              used by `health` and `stats`
                              (default 127.0.0.1:9091)
    -o, --output <FORMAT>     Output format: auto | table | wide | json |
                              ndjson | yaml | jsonpath=<expr> (default auto)
    --color <MODE>            Color policy: auto | always | never
    --hyperlinks <MODE>       OSC 8 hyperlink policy: auto | always | never
    --token <value>           Admin token (parsed; auth wiring lands later)
    --shard <N>               Target a specific shard
    --name <worker>           Worker name (decay, consolidation, …)
    --key <dotted.path>       Config key
    --value <v>               Config value (for `config set`)
    --since|--until|--agent   Audit query filters
    --logical-id <N>          Shard create
    --confirm                 Required for destructive commands
    --duration-secs <N>       Profile capture duration (default 30)
    --value <PATH>            Output-file path (debug-snapshot, profile)
    --version, -V             Print version
    --help, -h                Print this help

See spec/14_observability_ops/06_admin_ops.md for the full surface.
"
    );
}
