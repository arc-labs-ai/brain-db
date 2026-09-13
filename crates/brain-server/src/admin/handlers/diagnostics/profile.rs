//! `POST /v1/diagnostics/profile` — deferred (the Glommio profiler is
//! not yet wired; operators today can run `perf record` against
//! the server PID).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

pub async fn profile(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/glommio-profiler",
        "in-process CPU profiler for the shard's Glommio executor",
    ))
}
