//! Build the admin HTTP router from an [`AdminState`].
//!
//! Three public builders:
//!
//! - [`build_public`] — `/healthz` + `/metrics`. Safe to expose to
//!   load balancers and Prometheus scrapers. Bound to `metrics_addr`.
//! - [`build_admin`] — every `/v1/*` route. Operationally sensitive
//!   (audit log, snapshots, worker control, config). Bound to
//!   `admin_addr` — default loopback.
//! - [`build_unified`] — both of the above on one router. Test-only
//!   path so the existing harness can bring up a single listener.
//!
//! `with_state` and `with_state_prefix` hide the `Arc::clone`
//! boilerplate that state injection requires when the router has no
//! typed extractors.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_http::router::Router;
use http::{Method, Request, Response};
use hyper::body::Incoming;

use crate::admin::handlers::{
    agent, api_keys, audit, config, diagnostics, extract, healthz, metrics, readyz, rebuild, shard,
    snapshot, worker,
};
use crate::admin::AdminState;

/// `/healthz` + `/readyz` + `/metrics`. Operator-safe to expose.
pub fn build_public(state: Arc<AdminState>) -> Router<Incoming> {
    let r = Router::new();
    let r = r.get("/healthz", healthz::handle);
    let r = with_state(r, Method::GET, "/readyz", state.clone(), readyz::handle);
    with_state(r, Method::GET, "/metrics", state, metrics::handle)
}

/// Every `/v1/*` admin route. Bind to loopback or front with mTLS.
pub fn build_admin(state: Arc<AdminState>) -> Router<Incoming> {
    let r = Router::new();
    attach_v1_routes(r, state)
}

/// Test-only: register everything on one router. Production uses two
/// separate routers via [`build_public`] + [`build_admin`].
pub fn build_unified(state: Arc<AdminState>) -> Router<Incoming> {
    let r = Router::new();

    // ──────── /healthz — string OK, no state ───────────────────────────
    let r = r.get("/healthz", healthz::handle);

    // ──────── /readyz — shard-liveness readiness probe ─────────────────
    let r = with_state(r, Method::GET, "/readyz", state.clone(), readyz::handle);

    // ──────── /metrics — Prometheus text exposition ────────────────────
    let r = with_state(r, Method::GET, "/metrics", state.clone(), metrics::handle);

    attach_v1_routes(r, state)
}

/// Register every `/v1/*` route on `r`. Shared between
/// [`build_admin`] and [`build_unified`].
fn attach_v1_routes(r: Router<Incoming>, state: Arc<AdminState>) -> Router<Incoming> {
    // ──────── Snapshot family (POST / GET / DELETE) ────────────────────
    // One handler dispatches on (method, path) internally.
    let r = with_state_prefix(
        r,
        Method::POST,
        "/v1/snapshots",
        state.clone(),
        snapshot::handle,
    );
    let r = with_state_prefix(
        r,
        Method::GET,
        "/v1/snapshots",
        state.clone(),
        snapshot::handle,
    );
    let r = with_state_prefix(
        r,
        Method::DELETE,
        "/v1/snapshots/",
        state.clone(),
        snapshot::handle,
    );

    // ──────── /v1/rebuild-ann ──────────────────────────────────────────
    let r = with_state(
        r,
        Method::POST,
        "/v1/rebuild-ann",
        state.clone(),
        rebuild::handle,
    );

    // ──────── /v1/extract/backfill ─────────────────────────────────────
    let r = with_state(
        r,
        Method::POST,
        "/v1/extract/backfill",
        state.clone(),
        extract::handle,
    );

    // ──────── /v1/workers ──────────────────────────────────────────────
    let r = with_state(r, Method::GET, "/v1/workers", state.clone(), worker::list);
    let r = with_state_prefix(
        r,
        Method::POST,
        "/v1/workers/",
        state.clone(),
        worker::control,
    );

    // ──────── /v1/config ───────────────────────────────────────────────
    let r = with_state(r, Method::GET, "/v1/config", state.clone(), config::get);
    let r = with_state(
        r,
        Method::POST,
        "/v1/config/reload",
        state.clone(),
        config::reload,
    );
    let r = with_state(r, Method::POST, "/v1/config", state.clone(), config::set);

    // ──────── /v1/audit ────────────────────────────────────────────────
    let r = with_state(r, Method::GET, "/v1/audit", state.clone(), audit::query);
    let r = with_state(
        r,
        Method::GET,
        "/v1/audit/export",
        state.clone(),
        audit::export,
    );

    // ──────── /v1/api-keys (W2.5) ──────────────────────────────────────
    let r = with_state(
        r,
        Method::POST,
        "/v1/api-keys",
        state.clone(),
        api_keys::handle,
    );
    let r = with_state(
        r,
        Method::GET,
        "/v1/api-keys",
        state.clone(),
        api_keys::handle,
    );
    let r = with_state_prefix(
        r,
        Method::DELETE,
        "/v1/api-keys/",
        state.clone(),
        api_keys::handle,
    );

    // ──────── /v1/agents ───────────────────────────────────────────────
    let r = with_state(r, Method::GET, "/v1/agents", state.clone(), agent::list);
    // /v1/agents/{id} prefix handler dispatches GET vs DELETE internally.
    // brain-http's match_route(MethodMismatch) handles wrong method;
    // we register both methods on the same prefix so they hit `by_id`.
    let r = with_state_prefix(r, Method::GET, "/v1/agents/", state.clone(), agent::by_id);
    let r = with_state_prefix(
        r,
        Method::DELETE,
        "/v1/agents/",
        state.clone(),
        agent::by_id,
    );

    // ──────── /v1/shards ───────────────────────────────────────────────
    let r = with_state(r, Method::GET, "/v1/shards", state.clone(), shard::list);
    let r = with_state(r, Method::POST, "/v1/shards", state.clone(), shard::create);
    let r = with_state_prefix(
        r,
        Method::DELETE,
        "/v1/shards/",
        state.clone(),
        shard::delete,
    );

    // ──────── /v1/diagnostics ──────────────────────────────────────────
    let r = with_state(
        r,
        Method::POST,
        "/v1/diagnostics/profile",
        state.clone(),
        diagnostics::profile,
    );
    with_state(
        r,
        Method::GET,
        "/v1/diagnostics/debug-snapshot",
        state,
        diagnostics::debug_snapshot,
    )
}

/// Register an exact-match route bound to an `Arc<AdminState>`.
fn with_state<F, Fut>(
    r: Router<Incoming>,
    method: Method,
    path: &'static str,
    state: Arc<AdminState>,
    handler: F,
) -> Router<Incoming>
where
    F: Fn(Request<Incoming>, Arc<AdminState>) -> Fut + Send + Sync + Copy + 'static,
    Fut: std::future::Future<Output = brain_http::Result<Response<ResponseBody>>> + Send + 'static,
{
    r.route(method, path, move |req| {
        let s = state.clone();
        handler(req, s)
    })
}

/// Register a prefix-match route bound to an `Arc<AdminState>`.
fn with_state_prefix<F, Fut>(
    r: Router<Incoming>,
    method: Method,
    prefix: &'static str,
    state: Arc<AdminState>,
    handler: F,
) -> Router<Incoming>
where
    F: Fn(Request<Incoming>, Arc<AdminState>) -> Fut + Send + Sync + Copy + 'static,
    Fut: std::future::Future<Output = brain_http::Result<Response<ResponseBody>>> + Send + 'static,
{
    r.route_prefix(method, prefix, move |req| {
        let s = state.clone();
        handler(req, s)
    })
}
