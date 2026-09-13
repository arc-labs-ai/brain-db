//! Accept loop + graceful drain.
//!
//! Loops on `listener.accept()`, races against the shutdown signal,
//! and tracks every spawned per-connection task in a
//! [`tokio::task::JoinSet`]. On shutdown we wait for the set to
//! drain with a 30 s cap; beyond that we abort stragglers.
//!
//! ## Why a `JoinSet` instead of `hyper_util::server::graceful::GracefulShutdown`?
//!
//! `GracefulShutdown::watch` requires the connection type to
//! implement `GracefulConnection`. `hyper-util` 0.1 implements that
//! trait for `http1::Connection<I, S>` but **not** for
//! `http1::UpgradeableConnection<I, S>` (the variant returned by
//! `.with_upgrades()`). Brain-http needs upgrades for WebSocket,
//! so every connection is upgradeable. We track tasks
//! ourselves and best-effort drain on shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tracing::{info, warn, Instrument};

use crate::body::ResponseBody;
use crate::router::Router;
use crate::server::connection::handle_request;
use crate::server::limits::ServerLimits;
use crate::server::shutdown::ShutdownSignal;
use crate::tcp::BindConfig;

/// Default deadline for in-flight connections to drain after the
/// shutdown signal fires. Stragglers beyond this are abandoned.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the accept loop until `shutdown` fires, then drain in-flight
/// connections.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if reading the listener's bound
/// address fails. Per-connection errors are logged via `tracing` and
/// do not propagate.
pub async fn run(
    listener: TcpListener,
    router: Arc<Router<hyper::body::Incoming>>,
    limits: Arc<ServerLimits>,
    bind_config: Arc<BindConfig>,
    shutdown: ShutdownSignal,
) -> crate::Result<()> {
    let mut tasks: JoinSet<()> = JoinSet::new();
    let local_addr: SocketAddr = listener.local_addr()?;
    info!(addr = %local_addr, "brain-http accepting");

    loop {
        // Reap finished tasks opportunistically so the JoinSet doesn't
        // grow without bound on long-running servers.
        while let Some(res) = tasks.try_join_next() {
            if let Err(e) = res {
                if !e.is_cancelled() {
                    warn!(error = %e, "connection task panicked");
                }
            }
        }

        tokio::select! {
            biased;
            () = shutdown.wait() => {
                info!(addr = %local_addr, "brain-http shutdown signalled");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(p) => p,
                    Err(e) => { warn!(error = %e, "accept failed"); continue; }
                };
                if let Err(e) = crate::tcp::apply_stream_opts(&stream, &bind_config) {
                    warn!(error = %e, peer = %peer, "apply_stream_opts failed");
                }

                let router_for_conn = router.clone();
                let request_timeout = limits.request_timeout;
                let service = service_fn(move |req| {
                    let router = router_for_conn.clone();
                    // Box-pin the per-request future so its `Send`
                    // bound is enforced at this boundary (rather than
                    // inferred through the trait-object dispatch).
                    let fut: std::pin::Pin<
                        Box<
                            dyn Future<
                                    Output = Result<
                                        http::Response<ResponseBody>,
                                        std::convert::Infallible,
                                    >,
                                > + Send,
                        >,
                    > = Box::pin(handle_request(router, request_timeout, req));
                    fut
                });

                // `.with_upgrades()` keeps the connection alive past
                // a `101 Switching Protocols` response so the
                // application can drive the upgraded protocol (e.g.
                // WebSocket via `crate::ws::accept`). Costs nothing
                // for plain HTTP responses.
                let conn = http1::Builder::new()
                    .max_buf_size(limits.max_header_bytes.max(8 * 1024))
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();

                // Wrap the connection task in a `http.connection`
                // span so per-request spans inherit the peer
                // attributes via the OTel-semconv parent-child
                // relationship.
                let conn_span = crate::observability::connection_span(peer);
                tasks.spawn(
                    async move {
                        if let Err(e) = conn.await {
                            warn!(peer = %peer, error = %e, "connection task ended with error");
                        }
                    }
                    .instrument(conn_span),
                );
            }
        }
    }

    // Drain: wait up to DEFAULT_DRAIN_TIMEOUT for in-flight
    // connections to finish naturally, then abort any stragglers.
    let drain = async { while tasks.join_next().await.is_some() {} };
    match tokio::time::timeout(DEFAULT_DRAIN_TIMEOUT, drain).await {
        Ok(()) => info!(addr = %local_addr, "brain-http drained cleanly"),
        Err(_) => {
            warn!(addr = %local_addr, "brain-http drain timed out; aborting stragglers");
            tasks.shutdown().await;
        }
    }
    Ok(())
}
