//! `POST /v1/config/reload` — deferred (no live-reload pathway yet).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

pub async fn reload(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/live-config-reload",
        "live config reload from disk",
    ))
}
