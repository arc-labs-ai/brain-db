//! # brain-server
//!
//! Entry point for the Brain cognitive substrate.
//!
//! See `spec/01_system_architecture/` for the layering and request lifecycle.
//! Currently a placeholder — wires up tracing and prints version info.

#![allow(clippy::missing_errors_doc)]

use std::env;
use std::process::ExitCode;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    init_tracing();

    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    tracing::info!(version = %VERSION, "brain-server starting");
    tracing::warn!("not yet implemented — see ROADMAP.md");
    tracing::info!("brain-server exiting cleanly");

    ExitCode::SUCCESS
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(filter).with_target(true).init();
}

fn print_help() {
    println!(
        "{NAME} {VERSION}
The Brain cognitive substrate server.

USAGE:
    brain-server [OPTIONS]

OPTIONS:
    --config <PATH>     Path to configuration file (default: config/dev.toml)
    --version, -V       Print version
    --help, -h          Print this help

ENVIRONMENT:
    RUST_LOG            Tracing filter (default: info)
"
    );
}
