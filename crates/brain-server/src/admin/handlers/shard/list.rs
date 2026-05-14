//! `GET /v1/shards` — list configured shards.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::json_response;
use crate::admin::AdminState;

pub async fn list(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let mut body = String::with_capacity(64);
    body.push_str("{\"shards\":[");
    for (i, shard) in state.shards.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(
            &mut body,
            "{{\"index\":{i},\"shard_id\":{id}}}",
            id = shard.shard_id(),
        )
        .expect("string write");
    }
    body.push_str("]}\n");
    Ok(json_response(StatusCode::OK, body))
}
