//! Admin HTTP handlers for `agent` (spec §14/06 §10; sub-task 10.11).
//!
//! All routes deferred — agent_id secondary index doesn't exist yet.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::{not_implemented, text_response};
use crate::admin::AdminState;

/// `GET /v1/agents` handler — deferred.
pub async fn list(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/agent-index",
        "agent list (needs agent_id secondary index)",
    ))
}

/// `/v1/agents/{id}` prefix handler — internally dispatches on method.
pub async fn by_id(
    req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    match req.method() {
        m if m == Method::GET => Ok(not_implemented(
            "phase-11/agent-index",
            "per-agent stats (needs agent_id secondary index)",
        )),
        m if m == Method::DELETE => Ok(not_implemented(
            "phase-11/agent-cascade-delete",
            "agent cascade delete (memories + edges + contexts)",
        )),
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        )),
    }
}
