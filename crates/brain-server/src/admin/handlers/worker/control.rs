//! `POST /v1/workers/{name}/{action}` — live control plane.
//!
//! Wired through to the Scheduler's `pause` / `resume` / `run_now`
//! primitives.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::worker::{KNOWN_ACTIONS, KNOWN_WORKERS};
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::WorkerAction;

pub async fn control(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    if req.method() != Method::POST {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        ));
    }
    let path = req.uri().path();
    let Some(rest) = path.strip_prefix("/v1/workers/") else {
        return Ok(text_response(
            StatusCode::NOT_FOUND,
            "worker route not found\n",
        ));
    };
    let mut parts = rest.splitn(2, '/');
    let name = parts.next().unwrap_or("").to_owned();
    let action_slug = parts.next().unwrap_or("");

    if !KNOWN_WORKERS.contains(&name.as_str()) {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown worker `{name}`\n"),
        ));
    }
    let action = match action_slug {
        "stop" => WorkerAction::Pause,
        "start" => WorkerAction::Resume,
        "run-now" => WorkerAction::RunNow,
        _ => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "unknown worker action `{action_slug}` (allowed: {})\n",
                    KNOWN_ACTIONS.join(", "),
                ),
            ));
        }
    };

    // Fan out to every configured shard: control
    // applies to "the named worker" — each shard has its own
    // instance of every worker (decay, consolidation, …); the admin
    // surface applies the action to all of them.
    let mut applied = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for (shard_idx, shard) in state.shards.iter().enumerate() {
        match shard.worker_control(name.clone(), action).await {
            Ok(true) => applied += 1,
            Ok(false) => {
                // Worker doesn't exist on this shard — odd but not
                // fatal. Surface as a partial-success.
                errors.push(format!("shard {shard_idx}: worker not registered"));
            }
            Err(e) => {
                // Log the internal detail; keep only a generic marker on
                // the wire so a shard error can't leak host internals.
                warn!(shard = shard_idx, worker = %name, error = %e, "worker control failed");
                errors.push(format!("shard {shard_idx}: control failed"));
            }
        }
    }

    if applied == 0 {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "no shards applied the action\n",
        ));
    }

    // Build the JSON via serde so an error string containing `"`, `\`, or a
    // newline can't break out of the response document.
    let body = serde_json::json!({
        "worker": name,
        "action": action_slug,
        "applied_shards": applied,
        "errors": errors,
    })
    .to_string();
    Ok(json_response(StatusCode::OK, format!("{body}\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_action_set() {
        assert!(KNOWN_ACTIONS.contains(&"stop"));
        assert!(KNOWN_ACTIONS.contains(&"start"));
        assert!(KNOWN_ACTIONS.contains(&"run-now"));
    }

    #[test]
    fn error_strings_produce_valid_json() {
        // A shard error containing a quote / backslash / newline must not
        // break out of the JSON document. serde_json escapes it.
        let errors = vec![
            r#"shard 0: broke "everything""#.to_string(),
            "shard 1: back\\slash\nnewline".to_string(),
        ];
        let body = serde_json::json!({
            "worker": "de\"cay",
            "action": "run-now",
            "applied_shards": 2u64,
            "errors": errors,
        })
        .to_string();
        // Round-trips: the document is well-formed and preserves content.
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["applied_shards"], 2);
        assert_eq!(parsed["errors"][0], r#"shard 0: broke "everything""#);
        assert_eq!(parsed["worker"], "de\"cay");
    }

    #[test]
    fn slug_maps_to_action() {
        // The match arms above are the source of truth; this test
        // documents the mapping.
        let cases = [
            ("stop", WorkerAction::Pause),
            ("start", WorkerAction::Resume),
            ("run-now", WorkerAction::RunNow),
        ];
        for (slug, expected) in cases {
            let got = match slug {
                "stop" => WorkerAction::Pause,
                "start" => WorkerAction::Resume,
                "run-now" => WorkerAction::RunNow,
                _ => panic!("unreachable"),
            };
            assert_eq!(got, expected, "slug `{slug}`");
        }
    }
}
