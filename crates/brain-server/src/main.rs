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
mod metrics;
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
use bootstrap::{logging, shutdown, tls};
#[cfg(target_os = "linux")]
use network::{connection, dispatch, routing, subscribe};
#[cfg(target_os = "linux")]
#[allow(unused_imports)] // re-export kept for symmetry; binary doesn't reach it directly
use shard::adapters as shard_adapters;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::Config;

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

    #[cfg(target_os = "linux")]
    logging::init_pre_config();
    #[cfg(not(target_os = "linux"))]
    init_tracing_pre_config_portable();

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    #[cfg(target_os = "linux")]
    let _tracer_provider = logging::reinit_from_config(&cfg.logging, &cfg.tracing);

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

        let request_metrics = Arc::new(crate::metrics::request::RequestMetrics::new());

        let topology = Topology {
            shards,
            routing,
            server_caps,
            request_metrics: request_metrics.clone(),
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

            // Two HTTP listeners (see `crates/brain-server/src/admin/mod.rs`
            // module docs):
            //   - public  → `/healthz` + `/metrics`  on `metrics_addr`
            //   - admin   → `/v1/*`                  on `admin_addr` (loopback default)
            // Both share the same ShutdownSignal so a single ctrl-c brings
            // them down together.
            let admin_state = Arc::new(crate::admin::AdminState::new(
                topology.shards.clone(),
                connection_metrics.clone(),
                Arc::new(cfg.clone()),
                request_metrics.clone(),
            ));

            let public = crate::admin::AdminServer::public(
                cfg.server.metrics_addr,
                admin_state.clone(),
                signal.clone(),
            );
            let public_handle = match public.bind().await {
                Ok(bound) => {
                    tracing::info!(addr = %bound.local_addr(), "metrics server listening");
                    tokio::spawn(async move { bound.serve().await })
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to bind metrics server");
                    return ExitCode::FAILURE;
                }
            };

            let admin = crate::admin::AdminServer::admin(
                cfg.server.admin_addr,
                admin_state,
                signal.clone(),
            );
            let admin_handle = match admin.bind().await {
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

            // Bounded wait for both HTTP listeners to observe the same
            // signal and exit. 2s is generous — the accept loop's
            // shutdown arm resolves immediately.
            let admin_rc =
                drain_http_listener("admin server", admin_handle, std::time::Duration::from_secs(2))
                    .await;
            let public_rc = drain_http_listener(
                "metrics server",
                public_handle,
                std::time::Duration::from_secs(2),
            )
            .await;

            [serve_rc, admin_rc, public_rc]
                .into_iter()
                .find(|rc| *rc != ExitCode::SUCCESS)
                .unwrap_or(ExitCode::SUCCESS)
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

    /// Bounded drain helper for one HTTP listener task. `label` is
    /// the human-facing name used in error logs (e.g. "admin server"
    /// vs "metrics server"). Returns the appropriate `ExitCode`.
    async fn drain_http_listener(
        label: &'static str,
        handle: tokio::task::JoinHandle<std::io::Result<std::net::SocketAddr>>,
        budget: std::time::Duration,
    ) -> ExitCode {
        match tokio::time::timeout(budget, handle).await {
            Ok(Ok(Ok(_))) => ExitCode::SUCCESS,
            Ok(Ok(Err(e))) => {
                tracing::error!(error = %e, "{label} failed");
                ExitCode::FAILURE
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "{label} task panicked");
                ExitCode::FAILURE
            }
            Err(_) => {
                tracing::error!("{label} drain timed out");
                ExitCode::FAILURE
            }
        }
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
// tracing init (non-Linux fallback)
//
// Linux uses crate::bootstrap::logging — it owns the JSON / EnvFilter
// wiring spec'd in §14/02. The shim below keeps the non-Linux build
// path (which never reaches linux_main) compilable.
// ----------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn init_tracing_pre_config_portable() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
