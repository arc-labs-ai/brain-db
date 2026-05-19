//! Admin / observability HTTP server.
//!
//! Built on `brain-http` (hyper 1.x) as of Phase 11 M3. Replaces the
//! hand-rolled HTTP/1.1 parser + writeln-chain that lived here through
//! Phase 10.
//!
//! ## Two listeners
//!
//! Production binds **two** [`AdminServer`] instances:
//!
//! - **Public** (constructed via [`AdminServer::public`], bound to
//!   `cfg.server.metrics_addr`, default `127.0.0.1:9091`):
//!   `GET /healthz` + `GET /metrics`. Safe to expose to load
//!   balancers and Prometheus scrapers.
//! - **Admin** (constructed via [`AdminServer::admin`], bound to
//!   `cfg.server.admin_addr`, default `127.0.0.1:9092` — loopback):
//!   every `/v1/*` route (snapshots, rebuild-ann, workers, config,
//!   audit, agents, shards, diagnostics). Operationally sensitive;
//!   v1 has no built-in authentication so the loopback default
//!   matters. Front with mTLS / a token-checking reverse proxy if
//!   you bind it to a public interface.
//!
//! Unknown paths → `404 Not Found` (was `400 Bad Request` pre-M3 —
//! wire-behaviour delta documented in the M3 commit message). Routes
//! that exist on the "other" listener also 404 — `/v1/workers` on
//! `metrics_addr` and `/metrics` on `admin_addr` both fail closed.
//!
//! Spec §14/01 (metrics) + §14/06 (admin). The unified
//! [`AdminServer::new`] constructor still exists for the test
//! harness; production must not use it.

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
use crate::metrics::format::{BuildInfo, Snapshot as MetricsSnapshot};
use crate::metrics::request::RequestMetrics;
use crate::shard::ShardHandle;

// ---------------------------------------------------------------------------
// AdminState
// ---------------------------------------------------------------------------

/// Build the canonical [`BuildInfo`] from `env!` at compile time.
pub fn build_info_from_env() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        // No vergen wired up yet; surface the literal in v1 and
        // upgrade to `env!("VERGEN_GIT_SHA")` when the build script
        // is added.
        git_commit: "unknown",
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
    /// 12.1b: per-op request counters / histograms / in-flight gauges.
    /// Same instance shared with `Topology::request_metrics`.
    pub request_metrics: Arc<RequestMetrics>,
}

impl AdminState {
    pub fn new(
        shards: Arc<Vec<ShardHandle>>,
        connections: Arc<ConnectionMetrics>,
        config: Arc<Config>,
        request_metrics: Arc<RequestMetrics>,
    ) -> Self {
        let started_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            started_at: Instant::now(),
            started_at_unix_secs,
            build_info: build_info_from_env(),
            shards,
            connections,
            config,
            request_metrics,
        }
    }

    /// Borrow-only view consumed by `crate::metrics::format::format`.
    /// Cheap; clones nothing.
    pub fn metrics_snapshot(&self) -> MetricsSnapshot<'_> {
        MetricsSnapshot {
            build_info: self.build_info,
            started_at: self.started_at,
            started_at_unix_secs: self.started_at_unix_secs,
            shards: self.shards.as_slice(),
            connections: self.connections.as_ref(),
            request_metrics: self.request_metrics.as_ref(),
            config: self.config.as_ref(),
        }
    }
}

// ---------------------------------------------------------------------------
// AdminServer
// ---------------------------------------------------------------------------

/// Which router this listener serves. See module docs.
#[derive(Clone, Copy, Debug)]
enum RouterKind {
    /// `/healthz` + `/metrics`.
    Public,
    /// `/v1/*`.
    Admin,
    /// Both, on one listener. Test-only.
    Unified,
}

impl RouterKind {
    fn log_name(self) -> &'static str {
        match self {
            RouterKind::Public => "metrics server",
            RouterKind::Admin => "admin server",
            RouterKind::Unified => "admin server",
        }
    }
}

/// Pre-bind admin server descriptor. Call [`Self::bind`] to acquire
/// the TCP listener, then [`BoundAdminServer::serve`] to enter the
/// accept loop.
pub struct AdminServer {
    listen_addr: SocketAddr,
    state: Arc<AdminState>,
    shutdown: ShutdownSignal,
    kind: RouterKind,
}

/// Admin server with its TCP listener already bound. Holds the
/// brain-http `BoundServer` plus the brain-server side shutdown
/// signal we bridge on `serve`.
pub struct BoundAdminServer {
    bound: BoundServer,
    local_addr: SocketAddr,
    shutdown: ShutdownSignal,
    log_name: &'static str,
}

impl AdminServer {
    /// Public listener: `/healthz` + `/metrics` only.
    pub fn public(
        listen_addr: SocketAddr,
        state: Arc<AdminState>,
        shutdown: ShutdownSignal,
    ) -> Self {
        Self {
            listen_addr,
            state,
            shutdown,
            kind: RouterKind::Public,
        }
    }

    /// Admin listener: `/v1/*` only.
    pub fn admin(listen_addr: SocketAddr, state: Arc<AdminState>, shutdown: ShutdownSignal) -> Self {
        Self {
            listen_addr,
            state,
            shutdown,
            kind: RouterKind::Admin,
        }
    }

    /// Test-only: serve every route on one listener. Production
    /// uses [`Self::public`] + [`Self::admin`] on separate ports.
    #[allow(dead_code)] // exercised by tests/admin.rs + tests/support_harness
    pub fn new(listen_addr: SocketAddr, state: Arc<AdminState>, shutdown: ShutdownSignal) -> Self {
        Self {
            listen_addr,
            state,
            shutdown,
            kind: RouterKind::Unified,
        }
    }

    /// Bind the admin HTTP socket. `async` since brain-http's
    /// listener bind sits behind an `async fn` (matches the underlying
    /// tokio TCP listener API).
    pub async fn bind(self) -> io::Result<BoundAdminServer> {
        let router = match self.kind {
            RouterKind::Public => router::build_public(self.state.clone()),
            RouterKind::Admin => router::build_admin(self.state.clone()),
            RouterKind::Unified => router::build_unified(self.state.clone()),
        };
        let log_name = self.kind.log_name();
        let bound = HttpServer::bind(self.listen_addr)
            .router(router)
            .listen()
            .await
            .map_err(|e| match e {
                brain_http::Error::Io(io_err) => io_err,
                other => io::Error::other(format!("brain-http bind: {other}")),
            })?;
        let local_addr = bound.local_addr()?;
        info!(addr = %local_addr, "{log_name} bound");
        Ok(BoundAdminServer {
            bound,
            local_addr,
            shutdown: self.shutdown,
            log_name,
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
        let log_name = self.log_name;
        info!(addr = %local_addr, "{log_name} accepting");

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
                info!(addr = %local_addr, "{log_name} shutdown complete");
                Ok(local_addr)
            }
            Err(brain_http::Error::Io(e)) => Err(e),
            Err(other) => {
                warn!(addr = %local_addr, error = %other, "{log_name} exited with error");
                Err(io::Error::other(format!("brain-http run: {other}")))
            }
        }
    }
}
