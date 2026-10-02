//! `GET /v1/schema/review[?shard=N]` — the predicate review queue.
//!
//! The names this deployment's corpus COINED that no schema declared.
//! Brain's predicate vocabulary is open: a coined predicate is interned on
//! demand (`SchemaOrigin::ImplicitFromWrite`) and its statement is
//! committed and queryable, exactly like a declared one. So this is not an
//! error log — it is the shortlist of names worth promoting into a real
//! `SCHEMA_UPLOAD`, ranked by how often the corpus had to invent them.
//!
//! Counts are per distinct qname, not per mention: the qname index
//! short-circuits repeat mentions before the mint branch, so a count of 1
//! means "this name was coined once", not "seen once".
//!
//! Fan-out follows `GET /v1/workers`: a listing where some shards answer
//! and others error is a partial success — `207 Multi-Status` with a
//! top-level `"errors"` array — never a `200 OK` over a silently short
//! list. `500` only when every attempted shard errored.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn review(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let shard_filter = match query::shard_optional(&query_str) {
        Ok(s) => s,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    let mut entries: Vec<(usize, String, u64)> = Vec::new();
    let mut shard_errors: Vec<String> = Vec::new();
    let mut attempted: usize = 0;
    for (idx, shard) in state.shards.iter().enumerate() {
        if let Some(want) = shard_filter {
            if idx != want {
                continue;
            }
        }
        attempted += 1;
        match shard.predicate_review().await {
            Ok(queue) => entries.extend(queue.into_iter().map(|(q, n)| (idx, q, n))),
            Err(e) => {
                warn!(shard = idx, error = %e, "predicate_review failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if attempted > 0 && shard_errors.len() == attempted {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "predicate review listing failed\n",
        ));
    }

    // Re-rank across the whole fan-out. Each shard returns its own queue
    // already sorted, but the operator wants one list: the same coined name
    // on three shards is one promotion decision, not three.
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));

    let status = if shard_errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    Ok(json_response(status, body_json(&entries, &shard_errors)))
}

/// Render the response: a `"candidates"` array and — only on a partial
/// fan-out — a top-level `"errors"` array, so a clean `200 OK` never
/// carries an empty `errors` key that reads like a suppressed failure.
fn body_json(entries: &[(usize, String, u64)], shard_errors: &[String]) -> String {
    let mut body = String::with_capacity(256 + entries.len() * 64);
    body.push_str("{\"candidates\":[");
    for (i, (shard, qname, count)) in entries.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(
            &mut body,
            "{{\"shard\":{shard},\"qname\":{q},\"count\":{count}}}",
            q = json_string(qname),
        )
        .expect("string write into String");
    }
    body.push(']');
    if !shard_errors.is_empty() {
        body.push_str(",\"errors\":[");
        for (i, e) in shard_errors.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&json_string(e));
        }
        body.push(']');
    }
    body.push_str("}\n");
    body
}

/// Minimal JSON string escaping (the two mandatory escapes plus control
/// characters). Kept local, matching every other admin handler.
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
    use super::body_json;

    #[test]
    fn clean_fanout_has_no_errors_key() {
        let body = body_json(&[(0, "brain:difficulty".into(), 3)], &[]);
        assert!(!body.contains("errors"), "{body}");
        assert!(body.contains("\"qname\":\"brain:difficulty\""), "{body}");
        assert!(body.contains("\"count\":3"), "{body}");
    }

    #[test]
    fn empty_queue_is_an_empty_array_not_null() {
        // A deployment whose corpus coined nothing is the healthy case and
        // must render as `[]` — `null` would make a client's length check
        // throw on the one result that means "all good".
        assert_eq!(body_json(&[], &[]), "{\"candidates\":[]}\n");
    }

    #[test]
    fn partial_fanout_reports_the_shards_that_dropped_out() {
        let body = body_json(&[(0, "brain:horizon".into(), 1)], &["shard 1: gone".into()]);
        assert!(body.contains("\"errors\":[\"shard 1: gone\"]"), "{body}");
    }

    #[test]
    fn a_qname_with_a_quote_cannot_break_out_of_the_json() {
        // Predicate names are validated on intern, but this renderer must
        // not depend on that: it is the last thing between stored bytes and
        // an operator's JSON parser.
        let body = body_json(&[(0, "brain:we\"ird".into(), 1)], &[]);
        assert!(body.contains(r#""qname":"brain:we\"ird""#), "{body}");
    }
}
