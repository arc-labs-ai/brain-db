//! # brain-server
//!
//! Entry point for the Brain cognitive substrate.
//!
//! See `spec/01_system_architecture/` for the layering and request lifecycle.
//! Phase 9 status: config loads; runtime / shards land in subsequent sub-tasks.

#![allow(clippy::missing_errors_doc)]

mod config;
#[allow(dead_code)] // consumed by the frame dispatcher in sub-task 9.10.
mod routing;
#[cfg(target_os = "linux")]
#[allow(dead_code)] // consumed by the connection layer in sub-task 9.9.
mod shard;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{Config, LoggingConfig};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_PATH: &str = "config/dev.toml";

fn main() -> ExitCode {
    let args = match parse_args(env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!();
            print_help();
            return ExitCode::FAILURE;
        }
    };

    if args.show_version {
        println!("{NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }
    if args.show_help {
        print_help();
        return ExitCode::SUCCESS;
    }

    init_tracing_pre_config();

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    reinit_tracing_from_config(&cfg.logging);

    tracing::info!(
        version = %VERSION,
        listen = %cfg.server.listen_addr,
        metrics = %cfg.server.metrics_addr,
        admin = %cfg.server.admin_addr,
        shards = cfg.storage.shard_count,
        data_dir = %cfg.storage.data_dir.display(),
        "brain-server starting"
    );
    tracing::warn!("Phase 9 stub: config loaded but runtime not yet wired (sub-tasks 9.2+)");
    tracing::info!("brain-server exiting cleanly");

    ExitCode::SUCCESS
}

// ----------------------------------------------------------------------------
// argv parsing (no clap dep)
// ----------------------------------------------------------------------------

struct Args {
    config: PathBuf,
    show_version: bool,
    show_help: bool,
}

fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Result<Args, String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut show_version = false;
    let mut show_help = false;
    let mut iter = iter.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let v = iter
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_owned())?;
                config = PathBuf::from(v);
            }
            "--version" | "-V" => show_version = true,
            "--help" | "-h" => show_help = true,
            other => {
                if let Some(rest) = other.strip_prefix("--config=") {
                    config = PathBuf::from(rest);
                } else {
                    return Err(format!("unrecognized argument: {other}"));
                }
            }
        }
    }
    Ok(Args {
        config,
        show_version,
        show_help,
    })
}

// ----------------------------------------------------------------------------
// tracing init
// ----------------------------------------------------------------------------

fn init_tracing_pre_config() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

/// Best-effort re-init using the config's logging level / format. Because
/// `tracing` only allows one global subscriber per process, this only takes
/// effect if `init_tracing_pre_config` failed to install one (rare). In the
/// common case the pre-config subscriber stays active and we just log the
/// intended values for operator visibility.
fn reinit_tracing_from_config(logging: &LoggingConfig) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(logging.level.as_str()));
    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

fn print_help() {
    println!(
        "{NAME} {VERSION}
The Brain cognitive substrate server.

USAGE:
    brain-server [OPTIONS]

OPTIONS:
    --config <PATH>     Path to configuration file (default: {DEFAULT_CONFIG_PATH})
    --version, -V       Print version
    --help, -h          Print this help

ENVIRONMENT:
    RUST_LOG            Tracing filter (default: info)
    BRAIN__SECTION__FIELD=value
                        Override any TOML field. Double underscore separates
                        nesting. Examples:
                          BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:8080
                          BRAIN__STORAGE__SHARD_COUNT=8
                          BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB
"
    );
}
