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
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.extract_backfill(selector.clone()).await {
            Ok(report) => {
                enqueued = enqueued.saturating_add(report.enqueued);
                skipped = skipped.saturating_add(report.skipped);
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "extract_backfill failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if !shard_errors.is_empty() && enqueued == 0 && skipped == 0 {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("backfill failed: {}\n", shard_errors.join("; ")),
        ));
    }

    let body = format!(
        "{{\"enqueued\":{enqueued},\"skipped\":{skipped},\"shards\":{n}}}\n",
        n = state.shards.len(),
    );
    Ok(json_response(StatusCode::OK, body))
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
}
