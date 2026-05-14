//! `/v1/agents/{id}` prefix handler — dispatches on method.
//!
//! - `GET` → per-agent stats (501; needs agent_id secondary index).
//! - `DELETE` → cascade-delete (501).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::{not_implemented, text_response};
use crate::admin::AdminState;

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
