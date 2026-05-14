//! Admin HTTP handlers for `config` (spec §14/06 §7; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/config[?key=a.b.c]` → 200 + JSON (whole config or subtree).
//! - `POST /v1/config/reload` → 501 (no live-reload pathway yet).
//! - `POST /v1/config?key=…` → 501 (no editable in-memory store).
//!
//! Spec uses dotted paths like `workers.decay.interval`. We serialize
//! the config to a `serde_json::Value`, walk by segment, and return
//! whatever is at that subtree (object/scalar/array).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, not_implemented, text_response};
use crate::admin::AdminState;

/// `GET /v1/config[?key=...]` handler.
pub async fn get(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let key = parse_key(&query).map(|s| s.to_owned());
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

/// `POST /v1/config/reload` handler — deferred.
pub async fn reload(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/live-config-reload",
        "live config reload from disk",
    ))
}

/// `POST /v1/config?key=…` handler — deferred.
pub async fn set(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/runtime-config-set",
        "runtime mutation of config keys",
    ))
}

fn parse_key(query: &str) -> Option<&str> {
    if query.is_empty() {
        return None;
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("key=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}

fn walk<'a>(root: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut cursor = root;
    for segment in dotted.split('.') {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_extracts_dotted() {
        assert_eq!(parse_key(""), None);
        assert_eq!(
            parse_key("key=workers.decay.interval"),
            Some("workers.decay.interval")
        );
        assert_eq!(parse_key("other=1"), None);
        assert_eq!(parse_key("key="), None);
    }

    #[test]
    fn walk_steps_segments() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":{"b":{"c":42}},"x":1}"#).unwrap();
        assert_eq!(walk(&v, "a.b.c").unwrap(), &serde_json::json!(42));
        assert_eq!(walk(&v, "x").unwrap(), &serde_json::json!(1));
        assert!(walk(&v, "missing.path").is_none());
    }
}
