//! `/v1/backfill` — drive the resumable, checkpointed `BackfillWorker`.
//!
//! This is distinct from `POST /v1/extract/backfill` (see
//! `handlers/extract.rs`), which is a one-shot synchronous re-enqueue
//! onto the extractor channel. The resumable worker walks
//! `memory_range × extractor_ids` under a durable per-(memory,
//! extractor) checkpoint table, reports progress, and is cancellable.
//! Admin is HTTP-only on the wire, so these routes are the sole
//! operator surface for the worker (the `ADMIN_BACKFILL` /
//! `ADMIN_BACKFILL_CANCEL` opcodes reject at dispatch).
//!
//! The worker is per-shard, so a single logical run spans every shard.
//! We mint one `BackfillId` and submit it to every shard's worker
//! (submit is idempotent by request id), so a later
//! `DELETE /v1/backfill/<id>` cancels the same run on every shard.
//!
//! Routes:
//! - `POST /v1/backfill?all&extractors=1,2[&dry_run]`      → submit
//! - `POST /v1/backfill?start=<u128>&end=<u128>&extractors=…` → submit (range)
//! - `GET  /v1/backfill`                                   → per-shard progress
//! - `DELETE /v1/backfill/<id>`                            → cancel run `<id>`
//!
//! Response body (submit):
//!
//! ```json
//! {"backfill_id":"<hex>","shards":<N>,"progress":[{"shard":0, …}, …]}
//! ```
//!
//! Fan-out is partial-tolerant. A submit that some shards accept and
//! others reject is a *partial success*, not a silent one: the response
//! carries a top-level `"errors":[…]` array of the per-shard failures and
//! its status is `207 Multi-Status`, so an operator can see the skipped
//! shards rather than reading `200 OK` over a half-applied run. The route
//! only fails outright with `500` when *every* shard errored; a clean
//! all-shards submit stays `200 OK` with no `errors` key.

use std::sync::Arc;

use brain_core::{
    BackfillId, BackfillProgress, BackfillRange, BackfillRequest, ExtractorId, MemoryId,
};
use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;
use uuid::Uuid;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

/// Per-request cap on extractor ids — one logical backfill run targets a
/// bounded handful of extractors so per-memory item counts and progress
/// reporting stay comprehensible. A larger sweep is a second request.
const MAX_EXTRACTORS_PER_BACKFILL: usize = 4;

/// `POST /v1/backfill` — mint a run id, submit it to every shard's
/// resumable worker, and reply with the id + per-shard progress.
pub async fn submit(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let (range, extractor_ids, dry_run) = match parse_run_params(&query) {
        Ok(parts) => parts,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    // One id spans every shard; submit is idempotent by request id.
    let run_id = BackfillId::new();

    // A shard that ACCEPTED the submit contributes its element to the
    // response even if the follow-up progress read fails — the run is
    // live on that shard, so losing the snapshot must not lose the id.
    // Its progress degrades to `null` (a `None` element) rather than
    // being dropped or counted as a total failure. Only a shard whose
    // *submit* itself errored is a shard error.
    let mut submitted: Vec<(usize, Option<BackfillProgress>)> =
        Vec::with_capacity(state.shards.len());
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        let request = BackfillRequest {
            request_id: run_id,
            memory_range: range,
            extractor_ids: extractor_ids.clone(),
            priority: brain_core::WorkerPriority::backfill_default(),
            dry_run,
        };
        match shard.backfill_submit(request).await {
            Ok(_id) => match shard.backfill_progress().await {
                Ok(p) => submitted.push((idx, Some(p))),
                Err(e) => {
                    warn!(
                        shard = idx,
                        error = %e,
                        "backfill_progress after submit failed; reporting null progress",
                    );
                    submitted.push((idx, None));
                }
            },
            Err(e) => {
                warn!(shard = idx, error = %e, "backfill_submit failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if submitted.is_empty() {
        // No shard accepted the submit. Per-shard detail already logged;
        // keep it off the wire.
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "backfill submit failed on every shard\n",
        ));
    }

    // At least one shard accepted. A clean fan-out is `200 OK`; a partial
    // one (some shards rejected the submit) is `207 Multi-Status` carrying
    // the per-shard failures, so the operator sees the skipped shards.
    let status = if shard_errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    let body = submit_body_json(run_id, state.shards.len(), &submitted, &shard_errors);
    Ok(json_response(status, body))
}

/// Render the full submit response body: the run id, shard count, the
/// per-shard progress array, and — only when the fan-out was partial — a
/// top-level `"errors"` array of the shards whose submit was rejected.
/// A clean all-shards submit renders no `errors` key.
fn submit_body_json(
    run_id: BackfillId,
    shard_count: usize,
    submitted: &[(usize, Option<BackfillProgress>)],
    shard_errors: &[String],
) -> String {
    let mut body = format!(
        "{{\"backfill_id\":\"{id}\",\"shards\":{n},\"progress\":{progress}",
        id = hex_id(run_id),
        n = shard_count,
        progress = submit_progress_array_json(run_id, submitted),
    );
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

/// `GET /v1/backfill` — snapshot each shard's most-recent run progress.
pub async fn status(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let mut progress: Vec<(usize, BackfillProgress)> = Vec::with_capacity(state.shards.len());
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.backfill_progress().await {
            Ok(p) => progress.push((idx, p)),
            Err(e) => {
                warn!(shard = idx, error = %e, "backfill_progress failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if progress.is_empty() && !shard_errors.is_empty() {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "backfill progress failed on every shard\n",
        ));
    }

    let body = format!(
        "{{\"shards\":{n},\"progress\":{progress}}}\n",
        n = state.shards.len(),
        progress = progress_array_json(&progress),
    );
    Ok(json_response(StatusCode::OK, body))
}

/// `DELETE /v1/backfill/<id>` — flag the run `<id>` for cancellation on
/// every shard. Reply carries the per-shard cancelled flags + an
/// aggregate `any_cancelled`.
pub async fn cancel(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let path = req.uri().path();
    let id_str = path.trim_start_matches("/v1/backfill/");
    let id = match Uuid::parse_str(id_str) {
        Ok(u) => BackfillId::from_uuid(u),
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid backfill id `{id_str}`: {e}\n"),
            ))
        }
    };

    let mut cancelled: Vec<(usize, bool)> = Vec::with_capacity(state.shards.len());
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.backfill_cancel(id).await {
            Ok(flag) => cancelled.push((idx, flag)),
            Err(e) => {
                warn!(shard = idx, error = %e, "backfill_cancel failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if cancelled.is_empty() && !shard_errors.is_empty() {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "backfill cancel failed on every shard\n",
        ));
    }

    let any = cancelled.iter().any(|(_, flag)| *flag);
    let per_shard: Vec<String> = cancelled
        .iter()
        .map(|(idx, flag)| format!("{{\"shard\":{idx},\"cancelled\":{flag}}}"))
        .collect();
    let body = format!(
        "{{\"backfill_id\":\"{id}\",\"any_cancelled\":{any},\"cancelled\":[{list}]}}\n",
        id = hex_id(id),
        list = per_shard.join(","),
    );
    Ok(json_response(StatusCode::OK, body))
}

/// Simple (hyphen-less) hex of a run id. `DELETE` accepts either form
/// (`Uuid::parse_str` is lenient), but we always emit the simple form.
fn hex_id(id: BackfillId) -> String {
    id.0.simple().to_string()
}

/// Render `[{"shard":i, …progress…}, …]`.
fn progress_array_json(progress: &[(usize, BackfillProgress)]) -> String {
    let items: Vec<String> = progress
        .iter()
        .map(|(idx, p)| {
            let mut obj = format!("{{\"shard\":{idx},");
            obj.push_str(&progress_fields_json(p));
            obj.push('}');
            obj
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Render the submit response's per-shard progress array from
/// `(shard, Option<progress>)` elements. A `Some` element renders like
/// [`progress_array_json`]; a `None` element (submit succeeded but the
/// follow-up progress read failed) still contributes its shard, with the
/// run id preserved and every progress metric rendered `null` — the run
/// is live on that shard, only the snapshot is unavailable.
fn submit_progress_array_json(
    run_id: BackfillId,
    submitted: &[(usize, Option<BackfillProgress>)],
) -> String {
    let items: Vec<String> = submitted
        .iter()
        .map(|(idx, p)| match p {
            Some(p) => {
                let mut obj = format!("{{\"shard\":{idx},");
                obj.push_str(&progress_fields_json(p));
                obj.push('}');
                obj
            }
            None => format!(
                "{{\"shard\":{idx},\"request_id\":\"{id}\",\"running\":null,\
                 \"completed\":null,\"failed\":null,\"skipped_already_completed\":null,\
                 \"last_processed_memory_id\":null,\"eta_secs\":null}}",
                id = hex_id(run_id),
            ),
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Render the interior `"field":value,…` (no braces) of a
/// [`BackfillProgress`]. `Option` fields become JSON `null` when absent.
fn progress_fields_json(p: &BackfillProgress) -> String {
    let request_id = match p.request_id {
        Some(id) => format!("\"{}\"", hex_id(id)),
        None => "null".to_owned(),
    };
    let last_mem = match p.last_processed_memory_id {
        Some(m) => m.raw().to_string(),
        None => "null".to_owned(),
    };
    let eta_secs = match p.eta {
        Some(d) => d.as_secs().to_string(),
        None => "null".to_owned(),
    };
    format!(
        "\"request_id\":{request_id},\"running\":{running},\"completed\":{completed},\
         \"failed\":{failed},\"skipped_already_completed\":{skipped},\
         \"last_processed_memory_id\":{last_mem},\"eta_secs\":{eta_secs}",
        running = p.running,
        completed = p.completed,
        failed = p.failed,
        skipped = p.skipped_already_completed,
    )
}

/// Parse the run params out of the query string: the memory range, the
/// extractor id list, and the dry-run flag.
///
/// - Range: `all` (or `all=true`) OR `start=<u128>&end=<u128>`. Exactly
///   one form must be present; `start`/`end` require each other.
/// - `extractors=<u32>[,<u32>…]` — required, `1..=4` ids.
/// - `dry_run` / `dry_run=true` — optional, defaults `false`.
fn parse_run_params(query: &str) -> Result<(BackfillRange, Vec<ExtractorId>, bool), String> {
    let mut all = false;
    let mut start: Option<&str> = None;
    let mut end: Option<&str> = None;
    let mut extractors: Option<&str> = None;
    let mut dry_run = false;

    for kv in query.split('&').filter(|s| !s.is_empty()) {
        if kv == "all" || kv == "all=" || kv == "all=true" {
            all = true;
        } else if let Some(rest) = kv.strip_prefix("start=") {
            start = Some(rest);
        } else if let Some(rest) = kv.strip_prefix("end=") {
            end = Some(rest);
        } else if let Some(rest) = kv.strip_prefix("extractors=") {
            extractors = Some(rest);
        } else if kv == "dry_run" || kv == "dry_run=" || kv == "dry_run=true" {
            dry_run = true;
        }
    }

    let range = match (all, start, end) {
        (true, None, None) => BackfillRange::All,
        (false, Some(s), Some(e)) => {
            let start_id: u128 = s
                .parse::<u128>()
                .map_err(|err| format!("invalid start `{s}`: {err}"))?;
            let end_id: u128 = e
                .parse::<u128>()
                .map_err(|err| format!("invalid end `{e}`: {err}"))?;
            if start_id > end_id {
                return Err(format!(
                    "invalid range: start ({start_id}) > end ({end_id})"
                ));
            }
            BackfillRange::ById {
                start: MemoryId::from_raw(start_id),
                end: MemoryId::from_raw(end_id),
            }
        }
        (true, _, _) => return Err("conflicting range: pass either ?all or ?start&end".into()),
        (false, _, _) => {
            return Err("missing range; pass ?all or ?start=<id>&end=<id> (both required)".into())
        }
    };

    let extractor_ids = match extractors {
        Some(list) => parse_extractor_ids(list)?,
        None => return Err("missing extractors; pass ?extractors=<id>[,<id>…]".into()),
    };

    Ok((range, extractor_ids, dry_run))
}

/// Parse a comma-separated `u32` id list into `1..=4` [`ExtractorId`]s.
fn parse_extractor_ids(list: &str) -> Result<Vec<ExtractorId>, String> {
    let mut ids = Vec::new();
    for tok in list.split(',').filter(|s| !s.is_empty()) {
        let raw: u32 = tok
            .parse::<u32>()
            .map_err(|e| format!("invalid extractor id `{tok}`: {e}"))?;
        ids.push(ExtractorId(raw));
    }
    if ids.is_empty() || ids.len() > MAX_EXTRACTORS_PER_BACKFILL {
        return Err(format!(
            "invalid extractors: expected 1..={MAX_EXTRACTORS_PER_BACKFILL} ids, got {}",
            ids.len()
        ));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_scope() {
        let (range, ids, dry) = parse_run_params("all&extractors=1,2").unwrap();
        assert!(matches!(range, BackfillRange::All));
        assert_eq!(ids, vec![ExtractorId(1), ExtractorId(2)]);
        assert!(!dry);
    }

    #[test]
    fn parse_range_scope_with_dry_run() {
        let (range, ids, dry) = parse_run_params("start=10&end=20&extractors=7&dry_run").unwrap();
        match range {
            BackfillRange::ById { start, end } => {
                assert_eq!(start.raw(), 10);
                assert_eq!(end.raw(), 20);
            }
            BackfillRange::All => panic!("expected ById range"),
        }
        assert_eq!(ids, vec![ExtractorId(7)]);
        assert!(dry);
    }

    #[test]
    fn range_requires_both_ends() {
        assert!(parse_run_params("start=1&extractors=1").is_err());
        assert!(parse_run_params("end=1&extractors=1").is_err());
    }

    #[test]
    fn inverted_range_rejected() {
        assert!(parse_run_params("start=100&end=1&extractors=1").is_err());
    }

    #[test]
    fn all_plus_range_conflicts() {
        assert!(parse_run_params("all&start=1&end=2&extractors=1").is_err());
    }

    #[test]
    fn missing_range_rejected() {
        assert!(parse_run_params("extractors=1").is_err());
    }

    #[test]
    fn extractors_required_and_bounded() {
        assert!(parse_run_params("all").is_err());
        assert!(parse_run_params("all&extractors=").is_err());
        assert!(parse_run_params("all&extractors=1,2,3,4,5").is_err());
        assert!(parse_run_params("all&extractors=abc").is_err());
    }

    #[test]
    fn progress_json_null_and_populated() {
        let idle = BackfillProgress::default();
        let s = progress_fields_json(&idle);
        assert!(s.contains("\"request_id\":null"));
        assert!(s.contains("\"running\":false"));
        assert!(s.contains("\"last_processed_memory_id\":null"));
        assert!(s.contains("\"eta_secs\":null"));

        let run = BackfillProgress {
            request_id: Some(BackfillId::from_bytes([1u8; 16])),
            completed: 42,
            failed: 1,
            skipped_already_completed: 7,
            last_processed_memory_id: Some(MemoryId::from_raw(99)),
            running: true,
            eta: Some(std::time::Duration::from_secs(5)),
        };
        let s = progress_fields_json(&run);
        assert!(s.contains("\"running\":true"));
        assert!(s.contains("\"completed\":42"));
        assert!(s.contains("\"failed\":1"));
        assert!(s.contains("\"skipped_already_completed\":7"));
        assert!(s.contains("\"last_processed_memory_id\":99"));
        assert!(s.contains("\"eta_secs\":5"));
    }

    #[test]
    fn submit_array_keeps_id_on_null_progress_shard() {
        // A shard whose submit succeeded but whose progress read failed
        // must still appear in the array (progress null), and every
        // successful shard renders its metrics as before.
        let run_id = BackfillId::from_bytes([3u8; 16]);
        let ok = BackfillProgress {
            request_id: Some(run_id),
            completed: 5,
            failed: 0,
            skipped_already_completed: 1,
            last_processed_memory_id: Some(MemoryId::from_raw(12)),
            running: true,
            eta: None,
        };
        let submitted = vec![(0usize, Some(ok)), (1usize, None)];
        let s = submit_progress_array_json(run_id, &submitted);

        // Both shards present.
        assert!(s.contains("\"shard\":0"), "{s}");
        assert!(s.contains("\"shard\":1"), "{s}");
        // Successful shard carries real metrics.
        assert!(s.contains("\"completed\":5"), "{s}");
        assert!(s.contains("\"last_processed_memory_id\":12"), "{s}");
        // Degraded shard keeps the run id and nulls the metrics — it is
        // NOT dropped and does NOT surface as a failure.
        let expected_id = hex_id(run_id);
        assert_eq!(
            s.matches(&expected_id).count(),
            2,
            "both shards echo the run id: {s}"
        );
        assert!(s.contains("\"completed\":null"), "{s}");
        assert!(s.contains("\"running\":null"), "{s}");
    }

    #[test]
    fn submit_array_all_shards_ok() {
        let run_id = BackfillId::from_bytes([4u8; 16]);
        let p = BackfillProgress {
            request_id: Some(run_id),
            completed: 2,
            failed: 0,
            skipped_already_completed: 0,
            last_processed_memory_id: Some(MemoryId::from_raw(1)),
            running: true,
            eta: Some(std::time::Duration::from_secs(3)),
        };
        let submitted = vec![(0usize, Some(p))];
        let s = submit_progress_array_json(run_id, &submitted);
        assert!(s.contains("\"shard\":0"), "{s}");
        // A fully-populated progress renders no `null` — the degraded
        // path is the only source of nulls here.
        assert!(!s.contains("null"), "no degraded fields when all ok: {s}");
    }

    #[test]
    fn submit_body_surfaces_partial_shard_errors() {
        // The specific defect: a fan-out that some shards accept and one
        // rejects must NOT render as a clean success. The rejected shard's
        // message has to appear in a top-level "errors" array so the
        // handler can return 207 instead of a silent 200.
        let run_id = BackfillId::from_bytes([5u8; 16]);
        let ok = BackfillProgress {
            request_id: Some(run_id),
            completed: 3,
            failed: 0,
            skipped_already_completed: 0,
            last_processed_memory_id: Some(MemoryId::from_raw(7)),
            running: true,
            eta: None,
        };
        let submitted = vec![(0usize, Some(ok))];
        let shard_errors = vec!["shard 1: worker mailbox closed".to_owned()];
        let body = submit_body_json(run_id, 2, &submitted, &shard_errors);

        assert!(body.contains("\"backfill_id\":"), "{body}");
        assert!(body.contains("\"shards\":2"), "{body}");
        // The accepted shard still reports real progress.
        assert!(body.contains("\"shard\":0"), "{body}");
        assert!(body.contains("\"completed\":3"), "{body}");
        // The rejected shard is surfaced, not swallowed.
        assert!(
            body.contains("\"errors\":[\"shard 1: worker mailbox closed\"]"),
            "partial failure must surface the rejected shard: {body}"
        );
    }

    #[test]
    fn submit_body_errors_are_json_escaped() {
        // A raw error Display carrying a quote or backslash must not break
        // out of the JSON string.
        let run_id = BackfillId::from_bytes([6u8; 16]);
        let submitted: Vec<(usize, Option<BackfillProgress>)> = vec![(0usize, None)];
        let shard_errors = vec!["shard 1: bad \"key\"\\path".to_owned()];
        let body = submit_body_json(run_id, 2, &submitted, &shard_errors);
        assert!(
            body.contains(r#"["shard 1: bad \"key\"\\path"]"#),
            "error text must be JSON-escaped: {body}"
        );
    }

    #[test]
    fn submit_body_omits_errors_key_when_clean() {
        // A clean all-shards fan-out stays a plain success body — no
        // errors key, so 200 OK stays semantically accurate.
        let run_id = BackfillId::from_bytes([7u8; 16]);
        let p = BackfillProgress {
            request_id: Some(run_id),
            ..BackfillProgress::default()
        };
        let submitted = vec![(0usize, Some(p))];
        let body = submit_body_json(run_id, 1, &submitted, &[]);
        assert!(
            !body.contains("\"errors\""),
            "no errors key when clean: {body}"
        );
    }

    #[test]
    fn hex_id_round_trips_via_parse() {
        let id = BackfillId::from_bytes([9u8; 16]);
        let hex = hex_id(id);
        let parsed = BackfillId::from_uuid(Uuid::parse_str(&hex).unwrap());
        assert_eq!(parsed, id);
    }
}
