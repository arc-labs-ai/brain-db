//! `GET /v1/spaces` — deferred (needs space_id secondary index).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

pub async fn list(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/space-index",
        "space list (needs space_id secondary index)",
    ))
}
