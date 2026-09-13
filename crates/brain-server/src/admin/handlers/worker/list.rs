//! `GET /v1/workers[?shard=N]` — list scheduler worker snapshots.
//!
//! Fan-out is partial-tolerant. A listing where some shards answer and
//! others error is a *partial success*, not a silent one: the response
//! carries a top-level `"errors":[…]` array of the per-shard failures and
//! its status is `207 Multi-Status`, so an operator sees the shards that
//! dropped out rather than reading `200 OK` over a silently-short worker
//! list. The route only fails outright with `500` when *every* attempted
//! shard errored; a clean fan-out stays `200 OK` with no `errors` key.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_workers::MetricsSnapshot;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn list(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let shard_filter = match query::shard_optional(&query_str) {
        Ok(s) => s,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    let mut workers: Vec<String> = Vec::new();
    let mut shard_errors: Vec<String> = Vec::new();
    let mut attempted: usize = 0;
    for (idx, shard) in state.shards.iter().enumerate() {
        if let Some(want) = shard_filter {
            if idx != want {
                continue;
            }
        }
        attempted += 1;
        match shard.scheduler_snapshot().await {
            Ok(mut snaps) => {
                snaps.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in snaps {
                    workers.push(worker_entry_json(idx, name, &snap));
                }
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "scheduler_snapshot failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    // Every attempted shard errored: fail outright rather than answer with an
    // empty worker list that reads like "no workers running". The per-shard
    // detail is already logged; keep it off the wire.
    if attempted > 0 && shard_errors.len() == attempted {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "worker listing failed\n",
        ));
    }

    // At least one shard answered. A clean fan-out is `200 OK`; a partial one
    // (some shards errored) is `207 Multi-Status` carrying the per-shard
    // failures, so the operator sees the shards that dropped out.
    let status = if shard_errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    let body = workers_body_json(&workers, &shard_errors);
    Ok(json_response(status, body))
}

/// Render one worker snapshot as a JSON object. `name` is a controlled
/// `&'static str` worker identifier, but it is escaped anyway so no snapshot
/// field can ever break out of the JSON string.
fn worker_entry_json(shard: usize, name: &str, snap: &MetricsSnapshot) -> String {
    let mut out = String::with_capacity(256);
    write!(
        &mut out,
        "{{\"shard\":{shard},\"name\":{name},\"cycles\":{c},\"processed\":{p},\"errors\":{e},\"panics\":{pa},\"pending_work\":{pw},\"last_cycle_duration_ms\":{d},\"last_run_unix\":{lr},\"paused\":{paused}}}",
        name = json_string(name),
        c = snap.cycles_total,
        p = snap.processed_total,
        e = snap.errors_total,
        pa = snap.panics_total,
        pw = snap.pending_work_estimate,
        d = snap.last_cycle_duration_ms,
        lr = snap.last_run_unix_secs,
        paused = snap.paused,
    )
    .expect("string write into String");
    out
}

/// Render the full workers response body: the `"workers"` array and — only
/// when the fan-out was partial — a top-level `"errors"` array of the shards
/// whose snapshot was rejected. A clean all-shards fan-out renders no
/// `errors` key, so `200 OK` stays semantically accurate.
fn workers_body_json(workers: &[String], shard_errors: &[String]) -> String {
    let mut body = String::with_capacity(512);
    body.push_str("{\"workers\":[");
    body.push_str(&workers.join(","));
    body.push(']');
    if !shard_errors.is_empty() {
        body.push_str(",\"errors\":");
        body.push_str(&errors_array_json(shard_errors));
    }
    body.push_str("}\n");
    body
}

/// Render `["shard i: …", …]`, JSON-escaping each message so an error's
/// `Display` can never break out of the string and corrupt the document.
fn errors_array_json(errors: &[String]) -> String {
    let items: Vec<String> = errors.iter().map(|e| json_string(e)).collect();
    format!("[{}]", items.join(","))
}

/// Minimal JSON string escaping (the two mandatory escapes plus control
/// characters). Kept local so per-shard error text is always emitted safely.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(&mut out, "\\u{:04x}", c as u32).expect("string write into String");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            cycles_total: 7,
            processed_total: 3,
            errors_total: 1,
            panics_total: 0,
            last_cycle_duration_ms: 12,
            last_run_unix_secs: 1_700_000_000,
            pending_work_estimate: 2,
            paused: false,
        }
    }

    #[test]
    fn workers_body_surfaces_partial_shard_errors() {
        // The specific defect: a fan-out that some shards answer and one
        // rejects must NOT render as a clean success. The rejected shard's
        // message has to appear in a top-level "errors" array so the handler
        // can return 207 instead of a silently-partial 200.
        let workers = vec![worker_entry_json(0, "decay", &sample_snapshot())];
        let shard_errors = vec!["shard 1: shard disconnected".to_owned()];
        let body = workers_body_json(&workers, &shard_errors);

        assert!(body.contains("\"workers\":["), "{body}");
        assert!(body.contains("\"shard\":0"), "{body}");
        assert!(body.contains("\"name\":\"decay\""), "{body}");
        // The rejected shard is surfaced in a top-level errors array, not
        // swallowed.
        assert!(
            body.contains(",\"errors\":[\"shard 1: shard disconnected\"]}"),
            "partial failure must surface the rejected shard: {body}"
        );
    }

    #[test]
    fn workers_body_errors_are_json_escaped() {
        // A raw error Display carrying a quote or backslash must not break out
        // of the JSON string.
        let shard_errors = vec!["shard 1: bad \"key\"\\path".to_owned()];
        let body = workers_body_json(&[], &shard_errors);
        assert!(
            body.contains(r#"["shard 1: bad \"key\"\\path"]"#),
            "error text must be JSON-escaped: {body}"
        );
    }

    #[test]
    fn workers_body_omits_errors_key_when_clean() {
        // A clean all-shards fan-out stays a plain success body — no errors
        // key, so 200 OK stays semantically accurate.
        let workers = vec![worker_entry_json(0, "decay", &sample_snapshot())];
        let body = workers_body_json(&workers, &[]);
        // The per-worker object carries an `"errors"` count field, so the
        // guard is the top-level errors *array* specifically.
        assert!(
            !body.contains(",\"errors\":["),
            "no top-level errors array when clean: {body}"
        );
        assert!(body.contains("\"workers\":["), "{body}");
    }
}
