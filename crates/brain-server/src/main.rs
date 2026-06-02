//! # brain-server
//!
//! Entry point for the Brain cognitive substrate.
//!
//! The Tokio connection layer accepts TCP/TLS and routes each accepted
//! stream through a per-connection task, then the frame dispatcher.

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
#[allow(dead_code)] // consumed by the connection layer.
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
use network::{auth, connection, dispatch, routing, subscribe};
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
        // Load the embedding model once at process startup. The same
        // CachingDispatcher Arc is cloned into each shard executor so
        // the ~130 MiB BGE weights live in memory exactly once
        // regardless of shard count. We fail-stop here when the model
        // is missing — the substrate has no honest fallback that still
        // produces meaningful recall results.
        let dispatcher = match linux_main::build_dispatcher(&cfg.embedder) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: failed to initialise embedder: {e}");
                eprintln!();
                eprintln!("brain-server requires a BERT-shaped embedding model on disk.");
                eprintln!("To install BGE-small-en-v1.5:");
                eprintln!("  ./scripts/bootstrap-model.sh");
                eprintln!("Or set BRAIN_EMBED_MODEL_DIR=/path/to/model");
                return ExitCode::FAILURE;
            }
        };
        linux_main::run(cfg, dispatcher)
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
// The listener accepts connections; the frame dispatcher routes to shards.
// ----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_main {
    use std::collections::HashMap;
    use std::process::ExitCode;
    use std::sync::Arc;

    use brain_protocol::connection::handshake::{AuthMethod, ServerCapabilities};

    use super::config::Config;
    use crate::connection::{
        ConnectionLimits, ConnectionListener, ShutdownSignal, ShutdownTrigger, Topology,
    };
    use crate::routing::RoutingTable;
    use crate::shard::{
        spawn_shard, AutoEdgeSpawnConfig, CausalEdgeSpawnConfig, ExtractorSpawnConfig,
        ExtractorTierSpawnConfig, RerankSpawnConfig, ShardHandle, ShardJoiner, ShardSpawnConfig,
        TemporalEdgeSpawnConfig,
    };

    /// Errors surfaced by [`build_dispatcher`]. Hand-rolled `Display`
    /// rather than `thiserror` because `main.rs` is a binary and the
    /// error never crosses a library boundary.
    #[derive(Debug)]
    pub enum EmbedderInitError {
        ResolvePath(String),
        MissingFile { expected: std::path::PathBuf },
        Load(String),
    }

    impl std::fmt::Display for EmbedderInitError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::ResolvePath(e) => write!(f, "resolve model path: {e}"),
                Self::MissingFile { expected } => {
                    write!(f, "missing required model file: {}", expected.display())
                }
                Self::Load(e) => write!(f, "load model: {e}"),
            }
        }
    }

    /// Resolve `cfg.embedder.model` to a directory, verify the three
    /// required model files are present, load the BERT weights once,
    /// and wrap the resulting `CpuDispatcher` in `CachingDispatcher`
    /// so process-wide repeated embeds skip the forward pass.
    ///
    /// Returns an `Arc<dyn Dispatcher>` that is cheap to clone into
    /// every shard's executor closure.
    pub fn build_dispatcher(
        cfg: &super::config::EmbedderConfig,
    ) -> Result<Arc<dyn brain_embed::Dispatcher>, EmbedderInitError> {
        let model_dir = cfg
            .resolve_model_dir()
            .map_err(|e| EmbedderInitError::ResolvePath(e.to_string()))?;

        // Friendly pre-check: the loader would fail with a candle /
        // tokenizers diagnostic that doesn't help operators figure out
        // which file is missing. Surface the path ourselves.
        for required in ["config.json", "tokenizer.json", "model.safetensors"] {
            let p = model_dir.join(required);
            if !p.exists() {
                return Err(EmbedderInitError::MissingFile { expected: p });
            }
        }

        let embed_cfg = brain_embed::EmbedderConfig::new(model_dir);
        let handle = brain_embed::ModelHandle::load(&embed_cfg)
            .map_err(|e| EmbedderInitError::Load(e.to_string()))?;
        let cpu = brain_embed::CpuDispatcher::new(handle);
        let cached = brain_embed::CachingDispatcher::new(cpu, cfg.cache_size);
        Ok(Arc::new(cached))
    }

    pub fn run(cfg: Config, dispatcher: Arc<dyn brain_embed::Dispatcher>) -> ExitCode {
        // Build the configured Summarizer (default
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

        // Spawn one Glommio shard per `cfg.storage.shard_count`,
        // then build a `Topology` (shards + `RoutingTable` + `ServerCapabilities`)
        // and feed it into the `ConnectionListener`.
        let (shards, joiners) = match spawn_shards(&cfg, &summarizer, &dispatcher) {
            Ok(pair) => pair,
            Err(rc) => return rc,
        };
        let shards = Arc::new(shards);

        // Publish the routing table via `ArcSwap` so a
        // future admin RPC can hot-reload it without restarting
        // connections.
        let routing = match RoutingTable::new(cfg.storage.shard_count as u16, HashMap::new()) {
            Ok(t) => Arc::new(arc_swap::ArcSwap::from_pointee(t)),
            Err(e) => {
                tracing::error!(error = %e, "RoutingTable construction failed");
                return ExitCode::FAILURE;
            }
        };

        // W2.5: scope-bound API keys. The store lives in its own redb
        // file under the configured data dir; strict enforcement is
        // opt-in via `BRAIN_REQUIRE_SCOPED_API_KEYS`. In permissive
        // mode the server still advertises `AuthMethod::Token` so
        // scoped clients can opt in client-side; the AUTH path treats
        // both methods uniformly when strict mode is off.
        let strict_scope = crate::auth::require_scoped_keys_from_env();
        let auth_store_path = cfg.storage.data_dir.join("api_keys.redb");
        let auth_store = match crate::auth::AuthStore::open(&auth_store_path, strict_scope) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    path = %auth_store_path.display(),
                    "failed to open API-key store",
                );
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(
            strict = strict_scope,
            path = %auth_store_path.display(),
            "API-key scope store opened",
        );

        let server_caps = Arc::new(ServerCapabilities::v1_default(
            format!("brain-server/{}", env!("CARGO_PKG_VERSION")),
            vec![AuthMethod::Token, AuthMethod::None],
        ));

        // Keep an extra `Arc<Vec<ShardHandle>>` clone outside the
        // runtime so `graceful_shutdown_shards` can
        // drop it (and thereby close every shard's request channel)
        // after the connection + admin servers have exited.
        let shards_for_drain = shards.clone();

        let request_metrics = Arc::new(crate::metrics::request::RequestMetrics::new());

        let topology = Topology {
            shards,
            routing,
            server_caps,
            request_metrics: request_metrics.clone(),
            auth_store,
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
                topology.auth_store.clone(),
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

            // Spawn the listener as a JoinHandle so we
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
            let admin_rc = drain_http_listener(
                "admin server",
                admin_handle,
                std::time::Duration::from_secs(2),
            )
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

        // Outside the Tokio runtime: close every shard's
        // request channel, then join each `ShardJoiner` with a per-
        // shard timeout.
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
        dispatcher: &Arc<dyn brain_embed::Dispatcher>,
    ) -> Result<(Vec<ShardHandle>, Vec<ShardJoiner>), ExitCode> {
        let mut handles = Vec::with_capacity(cfg.storage.shard_count);
        let mut joiners = Vec::with_capacity(cfg.storage.shard_count);
        for shard_id in 0..cfg.storage.shard_count {
            let mut spawn_cfg =
                ShardSpawnConfig::new(cfg.storage.data_dir.clone(), dispatcher.clone());
            spawn_cfg.summarizer = summarizer.clone();
            // Ferry the operator's `[llm]` provider keys / model
            // overrides so the LLM extractor tier can resolve a client
            // from config when no env var is set.
            spawn_cfg.llm = crate::shard::LlmSpawnConfig {
                openai_api_key: cfg.llm.openai_api_key.clone(),
                anthropic_api_key: cfg.llm.anthropic_api_key.clone(),
                openai_model: cfg.llm.openai_model.clone(),
                anthropic_model: cfg.llm.anthropic_model.clone(),
            };
            // Ferry the operator's `[workers.auto_edge]`
            // overrides into the per-shard spawn config so the
            // AutoEdgeWorker registers with the configured knobs
            // (or stays unwired when disabled).
            spawn_cfg.auto_edge = AutoEdgeSpawnConfig {
                enabled: cfg.workers.auto_edge.enabled,
                interval_ms: cfg.workers.auto_edge.interval_ms,
                batch_size: cfg.workers.auto_edge.batch_size,
                similarity_threshold: cfg.workers.auto_edge.similarity_threshold,
                top_k: cfg.workers.auto_edge.top_k,
                ef_search: cfg.workers.auto_edge.ef_search,
                channel_capacity: cfg.workers.auto_edge.channel_capacity,
            };
            // Ferry the operator's `[workers.extractor]`
            // overrides into the per-shard spawn config so the
            // ExtractorWorker registers with the configured knobs (or
            // stays unwired when disabled).
            spawn_cfg.extractor = ExtractorSpawnConfig {
                enabled: cfg.workers.extractor.enabled,
                interval_ms: cfg.workers.extractor.interval_ms,
                drain_per_cycle: cfg.workers.extractor.drain_per_cycle,
                llm_budget_per_cycle_micro_usd: cfg
                    .workers
                    .extractor
                    .llm_budget_per_cycle_micro_usd,
                channel_capacity: cfg.workers.extractor.channel_capacity,
                skip_already_extracted: cfg.workers.extractor.skip_already_extracted,
                batch_size: cfg.workers.extractor.batch_size,
            };
            // Ferry the operator's `[workers.temporal_edge]`
            // overrides into the per-shard spawn config.
            spawn_cfg.temporal_edge = TemporalEdgeSpawnConfig {
                enabled: cfg.workers.temporal_edge.enabled,
                interval_ms: cfg.workers.temporal_edge.interval_ms,
                batch_size: cfg.workers.temporal_edge.batch_size,
                window_seconds: cfg.workers.temporal_edge.window_seconds,
                weight_min: cfg.workers.temporal_edge.weight_min,
                channel_capacity: cfg.workers.temporal_edge.channel_capacity,
                cross_context: cfg.workers.temporal_edge.cross_context,
                topical_threshold: cfg.workers.temporal_edge.topical_threshold,
            };
            // Ferry the operator's `[workers.causal_edge]`
            // overrides. The whitelist strings are split into
            // (namespace, name) pairs here so the spawn config never
            // carries unparsed qnames. Malformed entries (missing or
            // extra `:`) are dropped with a warn — the worker resolver
            // also tolerates this on its side via the validator.
            let causal_whitelist: Vec<(String, String)> = cfg
                .workers
                .causal_edge
                .whitelist_qnames
                .iter()
                .filter_map(|s| {
                    let (ns, name) = s.split_once(':')?;
                    if ns.is_empty() || name.is_empty() {
                        tracing::warn!(
                            qname = %s,
                            "causal_edge whitelist entry is not in `namespace:name` form; skipping",
                        );
                        return None;
                    }
                    Some((ns.to_string(), name.to_string()))
                })
                .collect();
            spawn_cfg.causal_edge = CausalEdgeSpawnConfig {
                enabled: cfg.workers.causal_edge.enabled,
                interval_ms: cfg.workers.causal_edge.interval_ms,
                batch_size: cfg.workers.causal_edge.batch_size,
                min_confidence: cfg.workers.causal_edge.min_confidence,
                whitelist_qnames: causal_whitelist,
                max_effect_memories_per_statement: cfg
                    .workers
                    .causal_edge
                    .max_effect_memories_per_statement,
                max_cause_memories_per_statement: cfg
                    .workers
                    .causal_edge
                    .max_cause_memories_per_statement,
                max_related_statements_per_entity: cfg
                    .workers
                    .causal_edge
                    .max_related_statements_per_entity,
                channel_capacity: cfg.workers.causal_edge.channel_capacity,
            };
            // Cross-encoder + per-tier extractor capability gates.
            // Operator opt-outs ride the same config plumbing the
            // worker knobs above use; spawn_shard hard-fails when an
            // enabled capability can't be brought up.
            spawn_cfg.rerank = RerankSpawnConfig {
                enabled: cfg.rerank.enabled,
            };
            spawn_cfg.extractors = ExtractorTierSpawnConfig {
                pattern_enabled: cfg.extractors.pattern.enabled,
                classifier_enabled: cfg.extractors.classifier.enabled,
                llm_enabled: cfg.extractors.llm.enabled,
            };
            match spawn_shard(shard_id as u16, spawn_cfg) {
                Ok((h, j)) => {
                    handles.push(h);
                    joiners.push(j);
                }
                Err(e) => {
                    tracing::error!(shard_id, error = %e, "failed to spawn shard");
                    // Best-effort: drop the handles we have; ShardJoiners
                    // will warn on drop without `join()` (graceful shutdown cleans up).
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
            // Handle both SIGINT (ctrl-c) and SIGTERM.
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
// wiring. The shim below keeps the non-Linux build
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
