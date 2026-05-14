//! `POST /v1/config?key=…` — deferred (no editable in-memory store).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

pub async fn set(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/runtime-config-set",
        "runtime mutation of config keys",
    ))
}
