//! `DELETE /v1/shards/{idx}` — deferred (cluster decommission not yet supported).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

pub async fn delete(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-12/shard-delete",
        "cluster decommission via online shard delete",
    ))
}
