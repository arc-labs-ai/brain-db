//! `GET /readyz` handler.
//!
//! Readiness probe — distinct from `/healthz` (liveness). `/healthz`
//! answers "is the admin accept loop running"; `/readyz` answers "is
//! this node actually able to serve data-plane traffic right now".
//!
//! A node is ready iff it has at least one shard and **every** shard's
//! executor loop is still draining requests (`ShardHandle::is_alive`).
//! Shards are spawned (and their WAL recovered) before the admin
//! listener binds, so a fresh process is ready as soon as `/readyz` is
//! reachable; the probe earns its keep afterwards, flipping to `503`
//! the moment a shard thread dies (panic, or a leaked drain on a hung
//! shutdown) so a load balancer / orchestrator drains the node instead
//! of routing requests that can never be served.
//!
//! Body is JSON either way so a probe can log the detail:
//! `{"ready":true,"shards":4,"alive":4}`.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::json_response;
use crate::admin::AdminState;

pub async fn handle(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let total = state.shards.len();
    let alive = state.shards.iter().filter(|s| s.is_alive()).count();
    let ready = total > 0 && alive == total;

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = format!("{{\"ready\":{ready},\"shards\":{total},\"alive\":{alive}}}\n");
    Ok(json_response(status, body))
}
