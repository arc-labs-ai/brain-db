//! `/v1/spaces/{id}` prefix handler — dispatches on method.
//!
//! - `GET` → per-space stats (501; needs space_id secondary index).
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
            "phase-11/space-index",
            "per-space stats (needs space_id secondary index)",
        )),
        m if m == Method::DELETE => Ok(not_implemented(
            "phase-11/space-cascade-delete",
            "space cascade delete (memories + edges + contexts)",
        )),
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        )),
    }
}
