//! `GET /v1/config[?key=...]` — read the loaded config (or a subtree).
//!
//! Spec uses dotted paths like `workers.decay.interval`. We serialize
//! the config to a `serde_json::Value`, walk by segment, and return
//! whatever is at that subtree (object/scalar/array).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::config::walk;
use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn get(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let key = query::config_key(&query_str).map(|s| s.to_owned());
    let cfg_json = match serde_json::to_value(state.config.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "config serialize failed");
            return Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config serialize failed\n",
            ));
        }
    };
    let value = match key.as_deref() {
        None => cfg_json,
        Some(path) => match walk(&cfg_json, path) {
            Some(v) => v.clone(),
            None => {
                return Ok(text_response(
                    StatusCode::NOT_FOUND,
                    &format!("unknown config key `{path}`\n"),
                ));
            }
        },
    };
    let body = match serde_json::to_string(&value) {
        Ok(s) => s + "\n",
        Err(_) => return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "encode\n")),
    };
    Ok(json_response(StatusCode::OK, body))
}
