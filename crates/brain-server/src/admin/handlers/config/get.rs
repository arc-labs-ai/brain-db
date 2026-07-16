//! `GET /v1/config[?key=...]` — read the loaded config (or a subtree).
//!
//! Spec uses dotted paths like `workers.decay.interval`. We serialize
//! the config to a `serde_json::Value`, walk by segment, and return
//! whatever is at that subtree (object/scalar/array).
//!
//! Secrets (`llm.api_key`, `admin.token`, and anything under a key named
//! `api_key` / `token` / `secret` / `password`) are redacted before any
//! response is built, so an admin-token holder can confirm whether a
//! credential is *set* without ever reading its value — and no `?key=`
//! subtree access can reach around the redaction, because it runs on the
//! full tree first.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use serde_json::Value;
use tracing::warn;

use crate::admin::handlers::config::walk;
use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

/// Object keys whose values are secrets and must never be serialized to a
/// client. Matched case-insensitively, at any depth.
const SECRET_KEYS: &[&str] = &["api_key", "token", "secret", "password"];

/// Marker substituted for a set secret. A distinct sentinel (not `null`)
/// so operators can tell "configured" from "unset".
const REDACTED: &str = "***redacted***";

/// Recursively replace the value of any secret-named key. A `null` value is
/// left as `null` (unset stays visibly unset); any present value — string or
/// otherwise — becomes [`REDACTED`].
fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if SECRET_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                    if !v.is_null() {
                        *v = Value::String(REDACTED.to_string());
                    }
                } else {
                    redact_secrets(v);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_secrets(item);
            }
        }
        _ => {}
    }
}

pub async fn get(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let key = query::config_key(&query_str).map(|s| s.to_owned());
    let cfg_json = match serde_json::to_value(state.config.as_ref()) {
        Ok(mut v) => {
            // Redact on the full tree BEFORE any subtree walk, so
            // `?key=llm.api_key` cannot bypass it.
            redact_secrets(&mut v);
            v
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_set_secrets_at_any_depth() {
        let mut v = json!({
            "llm": { "api_key": "sk-live-abc", "model": "gpt-4o-mini" },
            "admin": { "token": "operator-secret" },
            "server": { "listen_addr": "0.0.0.0:8080" },
        });
        redact_secrets(&mut v);
        assert_eq!(v["llm"]["api_key"], REDACTED);
        assert_eq!(v["admin"]["token"], REDACTED);
        // Non-secret fields are untouched.
        assert_eq!(v["llm"]["model"], "gpt-4o-mini");
        assert_eq!(v["server"]["listen_addr"], "0.0.0.0:8080");
    }

    #[test]
    fn leaves_unset_secrets_as_null() {
        // An unset credential (serialized as null) stays visibly unset, so
        // operators can distinguish "not configured" from "redacted".
        let mut v = json!({ "llm": { "api_key": Value::Null } });
        redact_secrets(&mut v);
        assert!(v["llm"]["api_key"].is_null());
    }

    #[test]
    fn redaction_is_case_insensitive_and_covers_arrays() {
        let mut v = json!({
            "items": [{ "Token": "t1" }, { "PASSWORD": "p1" }],
        });
        redact_secrets(&mut v);
        assert_eq!(v["items"][0]["Token"], REDACTED);
        assert_eq!(v["items"][1]["PASSWORD"], REDACTED);
    }
}
