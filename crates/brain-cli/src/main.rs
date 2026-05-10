//! # brain-cli
//!
//! Admin CLI for the Brain substrate. See
//! `spec/14_observability_ops/06_admin_ops.md` for the command surface.
//!
//! Currently a placeholder; sub-commands are stubbed out and print a
//! "not yet implemented" notice.

#![allow(clippy::missing_errors_doc)]

use std::env;
use std::process::ExitCode;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }

    let cmd = args[0].as_str();
    match cmd {
        "stats" | "health" | "info" | "snapshot" | "rebuild-ann" | "worker" | "config"
        | "audit" | "agent" | "shard" | "profile" | "debug-snapshot" => {
            eprintln!("brain-cli {cmd}: not yet implemented");
            eprintln!("see spec/14_observability_ops/06_admin_ops.md for the spec'd surface.");
            ExitCode::from(2)
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "{NAME} {VERSION}
Admin CLI for the Brain substrate.

USAGE:
    brain-cli <COMMAND> [ARGS]

COMMANDS:
    stats           Show substrate statistics
    health          Health check across shards
    info            Build and runtime info
    snapshot        Create / list / restore snapshots
    rebuild-ann     Trigger an HNSW rebuild
    worker          Manage background workers
    config          Get / set / reload configuration
    audit           Query the audit log
    agent           List or modify agents
    shard           List or modify shards
    profile         Capture a CPU profile
    debug-snapshot  Capture a runtime debug snapshot

OPTIONS:
    --version, -V   Print version
    --help, -h      Print this help

See spec/14_observability_ops/06_admin_ops.md for full surface.
"
    );
}
