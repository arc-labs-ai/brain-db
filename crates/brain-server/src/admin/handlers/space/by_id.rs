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
        // Per-shard detail is already logged; keep it off the wire.
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "space cascade delete failed\n",
        );
    }

    let deleted = existed || memories_forgotten > 0;
    let body = format!(
        "{{\"space_id\":\"{space_id}\",\"deleted\":{deleted},\
         \"memories_forgotten\":{memories_forgotten},\"shards\":{n}}}\n",
        space_id = space_id.0,
        n = state.shards.len(),
    );
    json_response(StatusCode::OK, body)
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
}
