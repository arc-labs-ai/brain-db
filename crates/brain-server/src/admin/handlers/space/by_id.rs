//! `/v1/spaces/{id}` prefix handler — dispatches on method.
//!
//! - `GET` → per-space stats (501; needs space_id secondary index).
//! - `DELETE` → cascade-delete: hard-tombstones every memory under
//!   `(namespace, space)` (tearing down the backing entity/statement/
//!   relation graph rows via the FORGET cascade) and drops the space
//!   registry row. This reuses the exact `SPACE_DELETE` wire path
//!   (`brain_ops::handlers::space::handle_space_delete`) by driving it
//!   through each shard's `dispatch_op`, so the admin plane and the wire
//!   share one cascade implementation.
//!
//! The space is addressed by its 16-byte `SpaceId` (a UUID) in the path
//! and its owning namespace in the `?namespace=<name>` query param — the
//! same `(namespace, space)` pair the wire path resolves from the caller's
//! API key. A space's whole footprint is homed on one shard
//! (`hash_space_to_shard`), but the delete fans out to every shard the way
//! `POST /v1/extract/backfill` does: non-owning shards find nothing and
//! report zero, which keeps the handler correct under routing overrides
//! without the admin plane needing the routing table.
//!
//! Fan-out is partial-tolerant. A delete that some shards apply and others
//! reject is a *partial success*, not a silent one: the response carries a
//! top-level `"errors":[…]` array of the per-shard failures and its status
//! is `207 Multi-Status`, so an operator can see the skipped shards rather
//! than reading `200 OK` over a half-applied cascade. The route stays
//! `200 OK` only when every shard's dispatch succeeded, and still fails
//! outright with `500` on a total miss (errors, and nothing found or
//! forgotten on any shard).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_metadata::api_keys::bits as perm_bits;
use brain_ops::{DispatchOutcome, RequestCaller};
use brain_protocol::envelope::request::{RequestBody, SpaceDeleteRequest};
use brain_protocol::envelope::response::ResponseBody as WireResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;
use uuid::Uuid;

use crate::admin::util::{json_response, not_implemented, text_response};
use crate::admin::AdminState;

pub async fn by_id(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    match req.method() {
        m if m == Method::GET => Ok(not_implemented(
            "phase-11/space-index",
            "per-space stats (needs space_id secondary index)",
        )),
        m if m == Method::DELETE => Ok(delete(req, state).await),
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        )),
    }
}

/// `DELETE /v1/spaces/{space_uuid}?namespace=<name>` — cascade-delete a
/// space and every memory / graph row under it.
async fn delete(req: Request<Incoming>, state: Arc<AdminState>) -> Response<ResponseBody> {
    // The {id} path segment is the space's 16-byte SpaceId, formatted as a
    // UUID. `uri().path()` excludes the query string, so the trim yields
    // exactly the id.
    let id_str = req
        .uri()
        .path()
        .trim_start_matches("/v1/spaces/")
        .to_owned();
    if id_str.is_empty() {
        return text_response(StatusCode::BAD_REQUEST, "missing space id in path\n");
    }
    let space_id = match Uuid::parse_str(&id_str) {
        Ok(u) => brain_core::SpaceId(u),
        Err(e) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid space id `{id_str}`: {e}\n"),
            )
        }
    };

    // The owning namespace is the outer half of the (namespace, space)
    // scope key; the wire path takes it from the caller's key, so the admin
    // plane must name it explicitly.
    let namespace = match namespace_param(req.uri().query().unwrap_or("")) {
        Some(ns) if !ns.is_empty() => ns,
        _ => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "missing namespace; pass ?namespace=<name>\n",
            )
        }
    };

    // Drive the same SPACE_DELETE the wire path drives, as an operator
    // acting with FULL permissions on the target (namespace, space).
    let caller = RequestCaller::from_scope(
        space_id,
        [0u8; 16],
        [0u8; 16],
        namespace.clone(),
        perm_bits::FULL,
    );
    let request = SpaceDeleteRequest {
        request_id: *Uuid::now_v7().as_bytes(),
        act_as: None,
    };

    let mut memories_forgotten: u64 = 0;
    let mut existed = false;
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        let body = RequestBody::SpaceDelete(request.clone());
        match shard
            .dispatch_op(body, caller.clone(), tracing::Span::none())
            .await
        {
            Ok(DispatchOutcome::Single(WireResponseBody::SpaceDelete(resp))) => {
                memories_forgotten = memories_forgotten.saturating_add(resp.memories_forgotten);
                existed = existed || resp.existed;
            }
            Ok(other) => {
                warn!(
                    shard = idx,
                    "space delete: unexpected dispatch outcome {other:?}"
                );
                shard_errors.push(format!("shard {idx}: unexpected response"));
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "space delete dispatch failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    if !shard_errors.is_empty() && !existed && memories_forgotten == 0 {
        // Total miss: some shard errored and no shard found or forgot
        // anything, so the cascade accomplished nothing. Per-shard detail is
        // already logged; keep it off the wire.
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "space cascade delete failed\n",
        );
    }

    // The cascade applied somewhere (existed or forgot rows on at least one
    // shard). A clean fan-out is `200 OK`; a partial one (some shards errored)
    // is `207 Multi-Status` carrying the per-shard failures, so the operator
    // sees the skipped shards rather than reading a silent success.
    let deleted = existed || memories_forgotten > 0;
    let status = if shard_errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    let body = delete_body_json(
        space_id,
        deleted,
        memories_forgotten,
        state.shards.len(),
        &shard_errors,
    );
    json_response(status, body)
}

/// Render the full delete response body: the space id, the `deleted` flag,
/// the summed `memories_forgotten` count, the shard count, and — only when
/// the fan-out was partial — a top-level `"errors"` array of the shards
/// whose dispatch was rejected. A clean all-shards fan-out renders no
/// `errors` key.
fn delete_body_json(
    space_id: brain_core::SpaceId,
    deleted: bool,
    memories_forgotten: u64,
    shard_count: usize,
    shard_errors: &[String],
) -> String {
    let mut body = format!(
        "{{\"space_id\":\"{space_id}\",\"deleted\":{deleted},\
         \"memories_forgotten\":{memories_forgotten},\"shards\":{shard_count}}}",
        space_id = space_id.0,
    );
    if !shard_errors.is_empty() {
        // Splice the errors array in just before the closing brace.
        body.pop();
        body.push_str(",\"errors\":");
        body.push_str(&errors_array_json(shard_errors));
        body.push('}');
    }
    body.push('\n');
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

/// Pull the `namespace=<name>` value out of a query string. Returns the
/// first match, percent-decoding untouched (namespaces are ASCII names).
fn namespace_param(query: &str) -> Option<String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .find_map(|kv| kv.strip_prefix("namespace="))
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_param_extracts_value() {
        assert_eq!(namespace_param("namespace=test"), Some("test".to_owned()));
        assert_eq!(
            namespace_param("foo=1&namespace=acme&bar=2"),
            Some("acme".to_owned())
        );
    }

    #[test]
    fn namespace_param_absent() {
        assert_eq!(namespace_param(""), None);
        assert_eq!(namespace_param("foo=1"), None);
    }

    fn sample_space_id() -> brain_core::SpaceId {
        brain_core::SpaceId(Uuid::from_bytes([7u8; 16]))
    }

    #[test]
    fn delete_body_surfaces_partial_shard_errors() {
        // The specific defect: the owning shard applied the cascade
        // (deleted=true) while another shard's dispatch errored. That must
        // NOT render as a clean success — the rejected shard's message has to
        // appear in a top-level "errors" array so the handler can return 207
        // instead of a silent 200 OK "deleted":true.
        let shard_errors = vec!["shard 1: dispatch failed".to_owned()];
        let body = delete_body_json(sample_space_id(), true, 4, 2, &shard_errors);

        assert!(body.contains("\"deleted\":true"), "{body}");
        assert!(body.contains("\"memories_forgotten\":4"), "{body}");
        assert!(body.contains("\"shards\":2"), "{body}");
        // The rejected shard is surfaced, not swallowed.
        assert!(
            body.contains("\"errors\":[\"shard 1: dispatch failed\"]"),
            "partial failure must surface the rejected shard: {body}"
        );
        // The errors array sits inside the object, before the close brace.
        assert!(body.trim_end().ends_with("]}"), "{body}");
    }

    #[test]
    fn delete_body_errors_are_json_escaped() {
        // A raw error Display carrying a quote or backslash must not break
        // out of the JSON string.
        let shard_errors = vec!["shard 1: bad \"key\"\\path".to_owned()];
        let body = delete_body_json(sample_space_id(), true, 0, 2, &shard_errors);
        assert!(
            body.contains(r#"["shard 1: bad \"key\"\\path"]"#),
            "error text must be JSON-escaped: {body}"
        );
    }

    #[test]
    fn delete_body_omits_errors_key_when_clean() {
        // A clean all-shards fan-out stays a plain success body — no errors
        // key, so 200 OK stays semantically accurate.
        let body = delete_body_json(sample_space_id(), true, 9, 3, &[]);
        assert!(
            !body.contains("\"errors\""),
            "no errors key when clean: {body}"
        );
        assert!(body.contains("\"deleted\":true"), "{body}");
        assert!(body.contains("\"memories_forgotten\":9"), "{body}");
        assert!(body.contains("\"shards\":3"), "{body}");
    }
}
