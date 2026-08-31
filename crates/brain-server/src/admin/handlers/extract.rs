//! `POST /v1/extract/backfill` — re-enqueue existing memories for
//! the three-tier extractor pipeline.
//!
//! The selector arrives as query-string params (mirrors every other
//! admin POST in this server — see `rebuild::handle`):
//!
//! - `?memory=<128-bit MemoryId u128>` — single memory by id.
//! - `?since=<unix_nanos>` — every active memory with
//!   `created_at_unix_nanos >= since`.
//! - `?all` — every active memory in every shard.
//!
//! Exactly one of the three forms must be present; the handler returns
//! `400 Bad Request` otherwise.
//!
//! The handler fans the request out to every configured shard (the
//! memory-by-id form short-circuits on shards that don't own the id),
//! sums the per-shard `enqueued` + `skipped` counts, and replies
//!
//! ```json
//! {"enqueued": <u64>, "skipped": <u64>, "shards": <usize>}
//! ```
//!
//! The CLI prints this verbatim in JSON mode and renders a small KV
//! table otherwise.
//!
//! Fan-out is partial-tolerant. A backfill that some shards accept and
//! others reject is a *partial success*, not a silent one: the response
//! carries a top-level `"errors":[…]` array of the per-shard failures and
//! its status is `207 Multi-Status`, so an operator can see the skipped
//! shards rather than reading `200 OK` over a half-applied run. The route
//! only fails outright with `500` when *every* shard errored; a clean
//! all-shards fan-out stays `200 OK` with no `errors` key.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_protocol::BackfillSelector;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let selector = match parse_selector(&query_str) {
        Ok(s) => s,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    let mut enqueued: u64 = 0;
    let mut skipped: u64 = 0;
    let mut succeeded: usize = 0;
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.extract_backfill(selector.clone()).await {
            Ok(report) => {
                succeeded += 1;
                enqueued = enqueued.saturating_add(report.enqueued);
                skipped = skipped.saturating_add(report.skipped);
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "extract_backfill failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if succeeded == 0 {
        // No shard accepted the backfill. Per-shard detail already logged;
        // keep it off the wire.
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "extraction backfill failed\n",
        ));
    }

    // At least one shard accepted. A clean fan-out is `200 OK`; a partial
    // one (some shards rejected the backfill) is `207 Multi-Status` carrying
    // the per-shard failures, so the operator sees the skipped shards.
    let status = if shard_errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    let body = backfill_body_json(enqueued, skipped, state.shards.len(), &shard_errors);
    Ok(json_response(status, body))
}

/// Render the full backfill response body: the summed `enqueued` +
/// `skipped` counts, the shard count, and — only when the fan-out was
/// partial — a top-level `"errors"` array of the shards whose backfill was
/// rejected. A clean all-shards fan-out renders no `errors` key.
fn backfill_body_json(
    enqueued: u64,
    skipped: u64,
    shard_count: usize,
    shard_errors: &[String],
) -> String {
    let mut body =
        format!("{{\"enqueued\":{enqueued},\"skipped\":{skipped},\"shards\":{shard_count}");
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
                use std::fmt::Write as _;
                write!(&mut out, "\\u{:04x}", c as u32).expect("string write into String");
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Pull exactly one selector spec out of the query string. The three
/// forms are exclusive — passing two raises `400 Bad Request`.
fn parse_selector(query: &str) -> Result<BackfillSelector, String> {
    let mut memory: Option<&str> = None;
    let mut since: Option<&str> = None;
    let mut all = false;

    for kv in query.split('&').filter(|s| !s.is_empty()) {
        if let Some(rest) = kv.strip_prefix("memory=") {
            memory = Some(rest);
        } else if let Some(rest) = kv.strip_prefix("since=") {
            since = Some(rest);
        } else if kv == "all" || kv == "all=" || kv == "all=true" {
            all = true;
        }
    }

    let present = [memory.is_some(), since.is_some(), all]
        .into_iter()
        .filter(|b| *b)
        .count();
    if present == 0 {
        return Err("missing selector; pass ?memory=<id>, ?since=<unix_nanos>, or ?all".into());
    }
    if present > 1 {
        return Err("conflicting selectors; pass exactly one of memory, since, all".into());
    }

    if let Some(m) = memory {
        let id: u128 = m
            .parse::<u128>()
            .map_err(|e| format!("invalid memory id `{m}`: {e}"))?;
        return Ok(BackfillSelector::Memory(id));
    }
    if let Some(s) = since {
        let ts: u64 = s
            .parse::<u64>()
            .map_err(|e| format!("invalid since `{s}`: {e}"))?;
        return Ok(BackfillSelector::Since {
            since_unix_nanos: ts,
        });
    }
    Ok(BackfillSelector::All)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_selector_memory() {
        let s = parse_selector("memory=42").unwrap();
        assert!(matches!(s, BackfillSelector::Memory(42)));
    }

    #[test]
    fn parse_selector_since() {
        let s = parse_selector("since=1700000000000000000").unwrap();
        assert!(matches!(
            s,
            BackfillSelector::Since {
                since_unix_nanos: 1_700_000_000_000_000_000
            }
        ));
    }

    #[test]
    fn parse_selector_all_forms() {
        for form in ["all", "all=", "all=true"] {
            let s = parse_selector(form).unwrap();
            assert!(matches!(s, BackfillSelector::All));
        }
    }

    #[test]
    fn parse_selector_requires_exactly_one() {
        assert!(parse_selector("").is_err());
        assert!(parse_selector("memory=1&since=2").is_err());
        assert!(parse_selector("memory=1&all").is_err());
    }

    #[test]
    fn parse_selector_rejects_garbage_numbers() {
        assert!(parse_selector("memory=abc").is_err());
        assert!(parse_selector("since=xx").is_err());
    }

    #[test]
    fn backfill_body_surfaces_partial_shard_errors() {
        // The specific defect: a fan-out that some shards accept and one
        // rejects must NOT render as a clean success. The rejected shard's
        // message has to appear in a top-level "errors" array so the
        // handler can return 207 instead of a silent 200.
        let shard_errors = vec!["shard 1: worker mailbox closed".to_owned()];
        let body = backfill_body_json(9, 2, 2, &shard_errors);

        assert!(body.contains("\"enqueued\":9"), "{body}");
        assert!(body.contains("\"skipped\":2"), "{body}");
        assert!(body.contains("\"shards\":2"), "{body}");
        // The rejected shard is surfaced, not swallowed.
        assert!(
            body.contains("\"errors\":[\"shard 1: worker mailbox closed\"]"),
            "partial failure must surface the rejected shard: {body}"
        );
    }

    #[test]
    fn backfill_body_errors_are_json_escaped() {
        // A raw error Display carrying a quote or backslash must not break
        // out of the JSON string.
        let shard_errors = vec!["shard 1: bad \"key\"\\path".to_owned()];
        let body = backfill_body_json(0, 0, 2, &shard_errors);
        assert!(
            body.contains(r#"["shard 1: bad \"key\"\\path"]"#),
            "error text must be JSON-escaped: {body}"
        );
    }

    #[test]
    fn backfill_body_omits_errors_key_when_clean() {
        // A clean all-shards fan-out stays a plain success body — no errors
        // key, so 200 OK stays semantically accurate.
        let body = backfill_body_json(5, 1, 3, &[]);
        assert!(
            !body.contains("\"errors\""),
            "no errors key when clean: {body}"
        );
        assert!(body.contains("\"shards\":3"), "{body}");
    }
}
