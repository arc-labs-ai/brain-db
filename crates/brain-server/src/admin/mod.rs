//! Admin / observability HTTP server.
//!
//! Built on `brain-http` (hyper 1.x) as of Phase 11 M3. Replaces the
//! hand-rolled HTTP/1.1 parser + writeln-chain that lived here through
//! Phase 10.
//!
//! Spec §14/01. Binds a separate listener on `cfg.server.metrics_addr`
//! (default `127.0.0.1:9091`) and serves:
//!
//! - `GET /healthz` → `200 OK\nok`
//! - `GET /metrics` → Prometheus text-format exposition
//! - `/v1/snapshots`, `/v1/rebuild-ann`, `/v1/workers`, `/v1/config`,
//!   `/v1/audit`, `/v1/agents`, `/v1/shards`, `/v1/diagnostics/*`
//!   — see the per-family handler modules.
//! - unknown paths → `404 Not Found` (was `400 Bad Request` pre-M3
//!   — wire-behaviour delta documented in the M3 commit message).

#![cfg(target_os = "linux")]

mod handlers;
mod query;
mod router;
mod util;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_http::server::{BoundServer, HttpServer, ShutdownHandle};
use tracing::{info, warn};

use crate::config::Config;
use crate::connection::{ConnectionMetrics, ShutdownSignal};
use crate::shard::ShardHandle;

// ---------------------------------------------------------------------------
// AdminState
// ---------------------------------------------------------------------------

/// Static build information, populated from `env!` at compile time.
#[derive(Clone, Copy, Debug)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_commit: &'static str,
}

impl BuildInfo {
    pub fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            // No vergen wired up yet; surface the literal in v1 and
            // upgrade to `env!("VERGEN_GIT_SHA")` when the build script
            // is added.
            git_commit: "unknown",
        }
    }
}

/// Shared, read-only state the admin server consults per scrape.
/// All atomics live behind `Arc` so the connection layer and the
/// admin server can publish / read concurrently.
pub struct AdminState {
    pub started_at: Instant,
    pub started_at_unix_secs: u64,
    pub build_info: BuildInfo,
    pub shards: Arc<Vec<ShardHandle>>,
    pub connections: Arc<ConnectionMetrics>,
    /// Sub-task 10.11: read-only view of the loaded config, surfaced
    /// by `GET /v1/config`.
    pub config: Arc<Config>,
}

impl AdminState {
    pub fn new(
        shards: Arc<Vec<ShardHandle>>,
        connections: Arc<ConnectionMetrics>,
        config: Arc<Config>,
    ) -> Self {
        let started_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            started_at: Instant::now(),
            started_at_unix_secs,
            build_info: BuildInfo::from_env(),
            shards,
            connections,
            config,
        }
    }
}

// ---------------------------------------------------------------------------
// AdminServer
// ---------------------------------------------------------------------------

/// Pre-bind admin server descriptor. Call [`Self::bind`] to acquire
/// the TCP listener, then [`BoundAdminServer::serve`] to enter the
/// accept loop.
pub struct AdminServer {
    listen_addr: SocketAddr,
    state: Arc<AdminState>,
    shutdown: ShutdownSignal,
}

/// Admin server with its TCP listener already bound. Holds the
/// brain-http `BoundServer` plus the brain-server side shutdown
/// signal we bridge on `serve`.
pub struct BoundAdminServer {
    bound: BoundServer,
    local_addr: SocketAddr,
    shutdown: ShutdownSignal,
}

impl AdminServer {
    pub fn new(listen_addr: SocketAddr, state: Arc<AdminState>, shutdown: ShutdownSignal) -> Self {
        Self {
            listen_addr,
            state,
            shutdown,
        }
    }

    /// Bind the admin HTTP socket. `async` since brain-http's
    /// listener bind sits behind an `async fn` (matches the underlying
    /// tokio TCP listener API).
    pub async fn bind(self) -> io::Result<BoundAdminServer> {
        let router = router::build(self.state.clone());
        let bound = HttpServer::bind(self.listen_addr)
            .router(router)
            .listen()
            .await
            .map_err(|e| match e {
                brain_http::Error::Io(io_err) => io_err,
                other => io::Error::other(format!("brain-http bind: {other}")),
            })?;
        let local_addr = bound.local_addr()?;
        info!(addr = %local_addr, "admin server bound");
        Ok(BoundAdminServer {
            bound,
            local_addr,
            shutdown: self.shutdown,
        })
    }
}

impl BoundAdminServer {
    /// The address the listener is bound to. Cheap accessor for tests
    /// that bind on port 0.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the accept loop until the brain-server shutdown signal
    /// fires, then drain in-flight connections via brain-http's
    /// graceful shutdown (30 s cap). Returns the bound local address
    /// — same shape as the pre-M3 API.
    pub async fn serve(mut self) -> io::Result<SocketAddr> {
        let local_addr = self.local_addr;
        info!(addr = %local_addr, "admin server accepting");

        // Bridge: brain-server::ShutdownSignal → brain-http::ShutdownHandle.
        // When the project-wide shutdown fires, trigger brain-http's
        // accept loop to stop and drain.
        let (http_handle, run_fut) = self.bound.into_runner();
        let bridge_handle: ShutdownHandle = http_handle;
        let bridge_task = tokio::spawn(async move {
            self.shutdown.recv().await;
            bridge_handle.shutdown();
        });

        // Drive the accept loop. Map errors back to io::Error for the
        // existing return-type contract.
        let result = run_fut.await;
        // Ensure the bridge task is cleaned up even if shutdown
        // happened by another path (e.g. fatal I/O error in accept).
        bridge_task.abort();

        match result {
            Ok(()) => {
                info!(addr = %local_addr, "admin server shutdown complete");
                Ok(local_addr)
            }
            Err(brain_http::Error::Io(e)) => Err(e),
            Err(other) => {
                warn!(addr = %local_addr, error = %other, "admin server exited with error");
                Err(io::Error::other(format!("brain-http run: {other}")))
            }
        }
    }
}
