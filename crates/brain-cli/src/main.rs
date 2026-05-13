//! # brain-cli
//!
//! Admin CLI for the Brain substrate. See
//! `spec/14_observability_ops/06_admin_ops.md` for the full
//! command surface. 10.8 implements `health` and `stats`; the
//! other commands land in 10.9–10.12.

#![allow(clippy::missing_errors_doc)]

use std::env;
use std::process::ExitCode;

use brain_cli::cli::{parse, Command};
use brain_cli::commands::snapshot::SnapshotAction;
use brain_cli::commands::{health, rebuild, snapshot, stats};

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

    match args.command {
        Command::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("{NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Health => match health::run(&args.server, args.output) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        Command::Stats => match stats::run(&args.server, args.output) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        Command::RebuildAnn { shard } => match rebuild::run(&args.server, shard, args.output) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        Command::Snapshot(action) => {
            let result = match action {
                SnapshotAction::Create { shard } => {
                    snapshot::create::run(&args.server, shard, args.output)
                }
                SnapshotAction::List => snapshot::list::run(&args.server, args.output),
                SnapshotAction::Delete { id, shard } => {
                    snapshot::delete::run(&args.server, id, shard, args.output)
                }
                SnapshotAction::Restore { id } => {
                    snapshot::restore::run(&args.server, id, args.output)
                }
            };
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
    }
}

fn print_help() {
    println!(
        "{NAME} {VERSION}
Admin CLI for the Brain substrate.

USAGE:
    brain-cli [OPTIONS] <COMMAND>

COMMANDS:
    health          Probe the admin /healthz endpoint
    stats           Snapshot /metrics counters

OPTIONS:
    --server <host:port>   Admin endpoint (default 127.0.0.1:9091)
    --output <json|table>  Output format (default table)
    --token <value>        Admin token (parsed for forward compat; unused in 10.8)
    --version, -V          Print version
    --help, -h             Print this help

10.9–10.12 add: snapshot, rebuild-ann, worker, config, audit,
agent, shard, profile, debug-snapshot.

See spec/14_observability_ops/06_admin_ops.md for the full surface.
"
    );
}
