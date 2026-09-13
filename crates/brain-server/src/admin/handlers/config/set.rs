//! `POST /v1/config?key=<dotted>&value=<v>` — set one runtime-settable
//! config key on a live server.
//!
//! Exactly one key is safe to mutate live: `monitoring.logging.level`.
//! The tracing level filter is an independent reload slot, so swapping it
//! never disturbs the formatter or an attached trace exporter. The change
//! is applied to the running subscriber but NOT written back to the config
//! file — it is a live override that a subsequent `POST /v1/config/reload`
//! (or a restart) resets to the file's value.
//!
//! Every other key is fixed at boot (captured into shard state / listeners
//! / index params) and returns `501 Not Implemented` with a message
//! naming the key, rather than pretending to apply a change that would not
//! take effect.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::query;
use crate::admin::util::{json_response, not_implemented, text_response};
use crate::admin::AdminState;

/// The one dotted key path this endpoint can apply to a running server.
const LIVE_LEVEL_KEY: &str = "monitoring.logging.level";

pub async fn set(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let Some(key) = query::config_key(&query_str) else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing ?key=<dotted.path>\n",
        ));
    };

    if key != LIVE_LEVEL_KEY {
        // Boot-fixed key: honest 501 rather than a no-op success.
        return Ok(not_implemented(
            "restart-required",
            &format!("`{key}` is fixed at boot; change it in the config file and restart"),
        ));
    }

    let Some(value) = query::config_value(&query_str) else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing ?value=<level> (e.g. debug, brain=trace,info)\n",
        ));
    };

    let Some(apply_log_level) = state.apply_log_level.as_ref() else {
        return Ok(text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "config set unavailable: logging reload handle not wired\n",
        ));
    };

    let applied = apply_log_level(value);
    if !applied {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to apply log level (subscriber unavailable)\n",
        ));
    }

    let body = serde_json::json!({
        "status": "applied",
        "key": LIVE_LEVEL_KEY,
        "detail": "live override; not written to the config file — a reload \
                   or restart resets it to the file value",
    });
    let out = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string()) + "\n";
    Ok(json_response(StatusCode::OK, out))
}
