//! # brain-server
//!
//! Entry point for the Brain cognitive substrate.
//!
//! See `spec/01_system_architecture/` for the layering and request lifecycle.
//! Phase 9 status: as of 9.9 the Tokio connection layer accepts TCP/TLS and
//! routes each accepted stream through a per-connection task; the frame
//! dispatcher lands in 9.10.

#![allow(clippy::missing_errors_doc)]

#[cfg(target_os = "linux")]
mod admin;
mod bootstrap;
mod config;
#[cfg(target_os = "linux")]
mod llm;
#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
#[allow(dead_code)] // consumed by the connection layer in sub-task 9.10.
mod shard;

// Crate-root aliases. The folder reorg moved each module into a
// thematic sub-module (`bootstrap::tls`, `network::connection`, …),
// but every file references its peers by the historical top-level
// name (`crate::tls`, `crate::connection`, …). Re-exporting at the
// crate root preserves those paths and matches the way integration
// tests load source files via `#[path]` + `mod xxx;`.
#[cfg(target_os = "linux")]
use bootstrap::{shutdown, tls};
#[cfg(target_os = "linux")]
use network::{connection, dispatch, routing, subscribe};
#[cfg(target_os = "linux")]
#[allow(unused_imports)] // re-export kept for symmetry; binary doesn't reach it directly
use shard::adapters as shard_adapters;

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

    #[cfg(target_os = "linux")]
    {
        linux_main::run(cfg)
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "non-Linux host: brain-server is a stub (Glommio + io_uring require Linux). \
             Config loaded; runtime not started."
        );
        tracing::info!("brain-server exiting cleanly");
        ExitCode::SUCCESS
    }
}

// ----------------------------------------------------------------------------
// Linux runtime: Tokio multi-thread + ConnectionListener.
// Shards land in 9.10's frame dispatcher; 9.9 just opens the listener.
// ----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_main {
    use std::collections::HashMap;
    use std::process::ExitCode;
    use std::sync::Arc;

    use brain_protocol::handshake::{AuthMethod, ServerCapabilities};

    use super::config::Config;
    use crate::connection::{
        ConnectionLimits, ConnectionListener, ShutdownSignal, ShutdownTrigger, Topology,
    };
    use crate::routing::RoutingTable;
    use crate::shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

    pub fn run(cfg: Config) -> ExitCode {
        // Sub-task 9.15: build the configured Summarizer (default
        // `DisabledSummarizer`). Construction happens once and the
        // resulting `Arc<dyn Summarizer>` is cloned into each shard's
        // `ShardSpawnConfig` so all shards share one bridge runtime.
        let summarizer = match crate::llm::factory::build_summarizer(&cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "summarizer construction failed");
                return ExitCode::FAILURE;
            }
        };

        // Sub-task 9.10: spawn one Glommio shard per `cfg.storage.shard_count`,
        // then build a `Topology` (shards + `RoutingTable` + `ServerCapabilities`)
        // and feed it into the `ConnectionListener`.
        let (shards, joiners) = match spawn_shards(&cfg, &summarizer) {
            Ok(pair) => pair,
            Err(rc) => return rc,
        };
        let shards = Arc::new(shards);

        // Sub-task 9.12: publish the routing table via `ArcSwap` so a
        // future admin RPC can hot-reload it without restarting
        // connections. Spec §10/05 §4 / §12/02 §2.
        let routing = match RoutingTable::new(cfg.storage.shard_count as u16, HashMap::new()) {
            Ok(t) => Arc::new(arc_swap::ArcSwap::from_pointee(t)),
            Err(e) => {
                tracing::error!(error = %e, "RoutingTable construction failed");
                return ExitCode::FAILURE;
            }
        };

        // v1 dev policy: accept `AuthMethod::None`. Real auth backends
        // (token / mTLS) land post-Phase-9.
        let server_caps = Arc::new(ServerCapabilities::v1_default(
            format!("brain-server/{}", env!("CARGO_PKG_VERSION")),
            vec![AuthMethod::None],
        ));

        // Keep an extra `Arc<Vec<ShardHandle>>` clone outside the
        // runtime so sub-task 9.14's `graceful_shutdown_shards` can
        // drop it (and thereby close every shard's request channel)
        // after the connection + admin servers have exited.
        let shards_for_drain = shards.clone();

        let topology = Topology {
            shards,
            routing,
            server_caps,
        };

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("brain-conn")
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "failed to build Tokio runtime");
                return ExitCode::FAILURE;
            }
        };

        let rc = runtime.block_on(async move {
            let (trigger, signal) = ShutdownSignal::channel();
            spawn_signal_listener(trigger);

            let tls = match build_tls(&cfg) {
                Ok(t) => t,
                Err(rc) => return rc,
            };

            let connection_metrics = Arc::new(crate::connection::ConnectionMetrics::default());

            // Sub-task 9.13: admin HTTP server (/healthz + /metrics).
            // Shares the shutdown signal so a single ctrl-c brings
            // both down.
            let admin_state = Arc::new(crate::admin::AdminState::new(
                topology.shards.clone(),
                connection_metrics.clone(),
                Arc::new(cfg.clone()),
            ));
            let admin = crate::admin::AdminServer::new(
                cfg.server.metrics_addr,
                admin_state,
                signal.clone(),
            );
            let admin_handle = match admin.bind() {
                Ok(bound) => {
                    tracing::info!(addr = %bound.local_addr(), "admin server listening");
                    tokio::spawn(async move { bound.serve().await })
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to bind admin server");
                    return ExitCode::FAILURE;
                }
            };

            let listener = ConnectionListener::new(
                cfg.server.listen_addr,
                tls,
                topology,
                connection_metrics.clone(),
                ConnectionLimits::default(),
                signal,
            );
            let bound = match listener.bind() {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "failed to bind connection listener");
                    return ExitCode::FAILURE;
                }
            };
            tracing::info!(addr = %bound.local_addr(), "brain-server listening");

            // Sub-task 9.14: spawn the listener as a JoinHandle so we
            // can `await` it deterministically, then drain the admin
            // server with a bounded budget. Both servers observe the
            // same `ShutdownSignal` clone, so a single SIGINT/SIGTERM
            // brings them both down.
            let listener_handle = tokio::spawn(async move { bound.serve().await });

            let serve_rc = match listener_handle.await {
                Ok(Ok(addr)) => {
                    tracing::info!(addr = %addr, "connection listener drained");
                    ExitCode::SUCCESS
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "connection listener failed");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    tracing::error!(error = %e, "connection listener task panicked");
                    ExitCode::FAILURE
                }
            };

            // Bounded wait for the admin server to observe the same
            // signal and exit. 2s is generous — the accept loop's
            // shutdown arm resolves immediately.
            let admin_rc =
                match tokio::time::timeout(std::time::Duration::from_secs(2), admin_handle).await {
                    Ok(Ok(Ok(_))) => ExitCode::SUCCESS,
                    Ok(Ok(Err(e))) => {
                        tracing::error!(error = %e, "admin server failed");
                        ExitCode::FAILURE
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "admin server task panicked");
                        ExitCode::FAILURE
                    }
                    Err(_) => {
                        tracing::error!("admin server drain timed out");
                        ExitCode::FAILURE
                    }
                };

            if serve_rc == ExitCode::SUCCESS {
                admin_rc
            } else {
                serve_rc
            }
        });

        // Phase B (outside the Tokio runtime): close every shard's
        // request channel, then join each `ShardJoiner` with a per-
        // shard timeout. Sub-task 9.14.
        let shard_rc = crate::shutdown::graceful_shutdown_shards(
            shards_for_drain,
            joiners,
            crate::shutdown::DEFAULT_SHARD_DRAIN_BUDGET,
        );

        if rc == ExitCode::SUCCESS {
            shard_rc
        } else {
            rc
        }
    }

    /// Spawn one shard per `cfg.storage.shard_count`. Returns the
    /// cloneable `Vec<ShardHandle>` for the listener + the `Vec<ShardJoiner>`
    /// to await on shutdown.
    fn spawn_shards(
        cfg: &Config,
        summarizer: &Arc<dyn brain_workers::Summarizer>,
    ) -> Result<(Vec<ShardHandle>, Vec<ShardJoiner>), ExitCode> {
        let mut handles = Vec::with_capacity(cfg.storage.shard_count);
        let mut joiners = Vec::with_capacity(cfg.storage.shard_count);
        for shard_id in 0..cfg.storage.shard_count {
            let mut spawn_cfg = ShardSpawnConfig::new(cfg.storage.data_dir.clone());
            spawn_cfg.summarizer = summarizer.clone();
            match spawn_shard(shard_id as u16, spawn_cfg) {
                Ok((h, j)) => {
                    handles.push(h);
                    joiners.push(j);
                }
                Err(e) => {
                    tracing::error!(shard_id, error = %e, "failed to spawn shard");
                    // Best-effort: drop the handles we have; ShardJoiners
                    // will warn on drop without `join()` (9.14 cleans up).
                    return Err(ExitCode::FAILURE);
                }
            }
        }
        Ok((handles, joiners))
    }

    fn spawn_signal_listener(trigger: ShutdownTrigger) {
        tokio::spawn(async move {
            // Sub-task 9.14: handle both SIGINT (ctrl-c) and SIGTERM.
            // SIGTERM is what process supervisors (systemd, k8s, docker
            // stop) send first; SIGKILL follows if we don't exit fast.
            // We install SIGTERM via tokio::signal::unix; if that
            // fails (rare — restricted containers / non-Linux
            // libc), fall back to SIGINT-only.
            use tokio::signal::unix::{signal, SignalKind};
            let sigterm_result = signal(SignalKind::terminate());
            match sigterm_result {
                Ok(mut sigterm) => {
                    tokio::select! {
                        r = tokio::signal::ctrl_c() => {
                            if let Err(e) = r {
                                tracing::error!(error = %e, "ctrl_c handler failed");
                            } else {
                                tracing::info!("SIGINT received; signalling shutdown");
                            }
                        }
                        _ = sigterm.recv() => {
                            tracing::info!("SIGTERM received; signalling shutdown");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "SIGTERM handler install failed; SIGINT-only",
                    );
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        tracing::error!(error = %e, "ctrl_c handler failed");
                    } else {
                        tracing::info!("SIGINT received; signalling shutdown");
                    }
                }
            }
            trigger.signal();
        });
    }

    fn build_tls(
        cfg: &Config,
    ) -> Result<Option<std::sync::Arc<tokio_rustls::rustls::ServerConfig>>, ExitCode> {
        if !cfg.server.tls.enabled {
            return Ok(None);
        }
        let cert = match cfg.server.tls.cert.as_ref() {
            Some(p) => p,
            None => {
                tracing::error!("server.tls.enabled = true but server.tls.cert is unset");
                return Err(ExitCode::FAILURE);
            }
        };
        let key = match cfg.server.tls.key.as_ref() {
            Some(p) => p,
            None => {
                tracing::error!("server.tls.enabled = true but server.tls.key is unset");
                return Err(ExitCode::FAILURE);
            }
        };
        match crate::tls::load_server_tls_config(cert, key) {
            Ok(cfg) => Ok(Some(cfg)),
            Err(e) => {
                tracing::error!(error = %e, "TLS config load failed");
                Err(ExitCode::FAILURE)
            }
        }
    }
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
