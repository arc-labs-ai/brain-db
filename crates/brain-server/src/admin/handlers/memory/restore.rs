//! `POST /v1/memories/{id}/restore` — un-tombstone a soft-forgotten
//! memory (FORGET soft-cascade revert).
//!
//! The memory id is the 128-bit `MemoryId` (as a `u128`) in the path; the
//! owning namespace arrives as `?namespace=<name>` — the same
//! `(namespace, space)` pair the wire path resolves from the caller's API
//! key, named explicitly on the admin plane.
//!
//! Restore is admin-only: the reserved `AdminRestore` wire opcode stays
//! rejected (admin lives on this HTTP plane). The handler drives each
//! shard's `restore_memory`, which validates the id + namespace, refuses a
//! hard-forgotten or past-grace memory, submits a WAL-durable
//! un-tombstone, and enqueues the revert cascade for the dependent graph.
//!
//! A memory's whole footprint is homed on one shard, but the restore fans
//! out to every shard the way `POST /v1/extract/backfill` does: non-owning
//! shards find nothing and report `NotFound`. Fan-out is partial-tolerant:
//! a run that some shards accept and others reject carries a top-level
//! `"errors":[…]` array and a `207 Multi-Status` status so the operator
//! sees the skipped shards rather than a silent success.
//!
//! Status codes:
//! - `200 OK` — restored, or already-active (idempotent no-op).
//! - `207 Multi-Status` — restored/already-active on the owning shard but
//!   another shard's dispatch errored.
//! - `404 Not Found` — no such memory under this namespace.
//! - `409 Conflict` — hard-forgotten (irreversible) or past grace.
//! - `400 Bad Request` — bad id, or missing namespace.
//! - `500` — every shard errored (nothing restored, found, or rejected).

use std::sync::Arc;

use brain_http::body::ResponseBody;
use brain_ops::AdminRestoreOutcome;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    if req.method() != Method::POST {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        ));
    }

    // Path shape: /v1/memories/{id}/restore. `uri().path()` excludes the
    // query string.
    let path = req.uri().path().to_owned();
    let Some(rest) = path.strip_prefix("/v1/memories/") else {
        return Ok(text_response(StatusCode::BAD_REQUEST, "bad memory path\n"));
    };
    let Some(id_str) = rest.strip_suffix("/restore") else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "expected /v1/memories/{id}/restore\n",
        ));
    };
    if id_str.is_empty() || id_str.contains('/') {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing or malformed memory id in path\n",
        ));
    }
    let memory_id: brain_core::MemoryId = match id_str.parse::<u128>() {
        Ok(raw) => brain_core::MemoryId::from_raw(raw),
        Err(e) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid memory id `{id_str}`: {e}\n"),
            ))
        }
    };

    let namespace = match namespace_param(req.uri().query().unwrap_or("")) {
        Some(ns) if !ns.is_empty() => ns,
        _ => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                "missing namespace; pass ?namespace=<name>\n",
            ))
        }
    };

    let mut resolved: Option<AdminRestoreOutcome> = None;
    let mut shard_errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.restore_memory(memory_id, namespace.clone()).await {
            Ok(outcome) => resolved = Some(merge_outcome(resolved, outcome)),
            Err(e) => {
                warn!(shard = idx, error = %e, "memory restore dispatch failed");
                shard_errors.push(format!("shard {idx}: {e}"));
            }
        }
    }

    let Some(outcome) = resolved else {
        // Every shard errored — nothing observed the memory at all.
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory restore failed\n",
        ));
    };

    let base_status = match outcome {
        AdminRestoreOutcome::Restored | AdminRestoreOutcome::AlreadyActive => StatusCode::OK,
        AdminRestoreOutcome::NotFound => StatusCode::NOT_FOUND,
        AdminRestoreOutcome::HardForgotten | AdminRestoreOutcome::PastGrace => StatusCode::CONFLICT,
    };
    // A partial fan-out (owning shard succeeded, another errored) upgrades a
    // 200 to 207 so the skipped shards are visible. A 404/409 keeps its
    // semantic status — the errors still surface in the body.
    let status = if !shard_errors.is_empty() && base_status == StatusCode::OK {
        StatusCode::MULTI_STATUS
    } else {
        base_status
    };

    let body = restore_body_json(memory_id, outcome, state.shards.len(), &shard_errors);
    Ok(json_response(status, body))
}

/// Fold a per-shard outcome into the running result. A concrete outcome
/// (the owning shard found the memory) always wins over `NotFound` (a
/// non-owning shard). Two concrete outcomes can't legitimately co-occur
/// (one shard owns the id), but if they did, a rejection (HardForgotten /
/// PastGrace) is the most conservative answer and takes precedence.
fn merge_outcome(
    prev: Option<AdminRestoreOutcome>,
    next: AdminRestoreOutcome,
) -> AdminRestoreOutcome {
    match prev {
        None => next,
        Some(p) => {
            if rank(next) >= rank(p) {
                next
            } else {
                p
            }
        }
    }
}

/// Precedence when folding: rejections win over successes, successes win
/// over `NotFound`.
fn rank(o: AdminRestoreOutcome) -> u8 {
    match o {
        AdminRestoreOutcome::NotFound => 0,
        AdminRestoreOutcome::AlreadyActive => 1,
        AdminRestoreOutcome::Restored => 2,
        AdminRestoreOutcome::PastGrace => 3,
        AdminRestoreOutcome::HardForgotten => 4,
    }
}

fn outcome_str(o: AdminRestoreOutcome) -> &'static str {
    match o {
        AdminRestoreOutcome::Restored => "restored",
        AdminRestoreOutcome::AlreadyActive => "already_active",
        AdminRestoreOutcome::NotFound => "not_found",
        AdminRestoreOutcome::HardForgotten => "hard_forgotten",
        AdminRestoreOutcome::PastGrace => "past_grace",
    }
}

/// Render the response body: the memory id, the resolved outcome, the
/// shard count, and — only when the fan-out was partial — a top-level
/// `"errors"` array of the shards whose dispatch was rejected.
fn restore_body_json(
    memory_id: brain_core::MemoryId,
    outcome: AdminRestoreOutcome,
    shard_count: usize,
    shard_errors: &[String],
) -> String {
    let restored = matches!(
        outcome,
        AdminRestoreOutcome::Restored | AdminRestoreOutcome::AlreadyActive
    );
    let mut body = format!(
        "{{\"memory_id\":{},\"outcome\":\"{}\",\"restored\":{},\"shards\":{}}}",
        memory_id.raw(),
        outcome_str(outcome),
        restored,
        shard_count,
    );
    if !shard_errors.is_empty() {
        body.pop();
        body.push_str(",\"errors\":");
        body.push_str(&errors_array_json(shard_errors));
        body.push('}');
    }
    body.push('\n');
    body
}

/// Render `["shard i: …", …]`, JSON-escaping each message so an error's
/// `Display` can never break out of the string.
fn errors_array_json(errors: &[String]) -> String {
    let items: Vec<String> = errors.iter().map(|e| json_string(e)).collect();
    format!("[{}]", items.join(","))
}

/// Minimal JSON string escaping (the two mandatory escapes plus control
/// characters).
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

/// Pull the `namespace=<name>` value out of a query string.
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

    fn sample_id() -> brain_core::MemoryId {
        brain_core::MemoryId::from_raw(42)
    }

    #[test]
    fn namespace_param_extracts_value() {
        assert_eq!(namespace_param("namespace=acme"), Some("acme".to_owned()));
        assert_eq!(namespace_param("x=1&namespace=n"), Some("n".to_owned()));
        assert_eq!(namespace_param(""), None);
    }

    #[test]
    fn merge_prefers_concrete_over_not_found() {
        // A non-owning shard reports NotFound; the owning shard reports the
        // real outcome. The real outcome must win regardless of fold order.
        let a = merge_outcome(
            Some(AdminRestoreOutcome::NotFound),
            AdminRestoreOutcome::Restored,
        );
        assert_eq!(a, AdminRestoreOutcome::Restored);
        let b = merge_outcome(
            Some(AdminRestoreOutcome::Restored),
            AdminRestoreOutcome::NotFound,
        );
        assert_eq!(b, AdminRestoreOutcome::Restored);
    }

    #[test]
    fn merge_prefers_rejection_over_success() {
        let a = merge_outcome(
            Some(AdminRestoreOutcome::Restored),
            AdminRestoreOutcome::HardForgotten,
        );
        assert_eq!(a, AdminRestoreOutcome::HardForgotten);
    }

    #[test]
    fn body_reports_restored() {
        let body = restore_body_json(sample_id(), AdminRestoreOutcome::Restored, 2, &[]);
        assert!(body.contains("\"memory_id\":42"), "{body}");
        assert!(body.contains("\"outcome\":\"restored\""), "{body}");
        assert!(body.contains("\"restored\":true"), "{body}");
        assert!(body.contains("\"shards\":2"), "{body}");
        assert!(!body.contains("\"errors\""), "{body}");
    }

    #[test]
    fn body_reports_hard_forgotten_not_restored() {
        let body = restore_body_json(sample_id(), AdminRestoreOutcome::HardForgotten, 1, &[]);
        assert!(body.contains("\"outcome\":\"hard_forgotten\""), "{body}");
        assert!(body.contains("\"restored\":false"), "{body}");
    }

    #[test]
    fn body_surfaces_partial_shard_errors() {
        let shard_errors = vec!["shard 1: dispatch failed".to_owned()];
        let body = restore_body_json(sample_id(), AdminRestoreOutcome::Restored, 2, &shard_errors);
        assert!(
            body.contains("\"errors\":[\"shard 1: dispatch failed\"]"),
            "{body}"
        );
        assert!(body.trim_end().ends_with("]}"), "{body}");
    }

    #[test]
    fn body_errors_are_json_escaped() {
        let shard_errors = vec!["shard 1: bad \"key\"\\path".to_owned()];
        let body = restore_body_json(sample_id(), AdminRestoreOutcome::Restored, 2, &shard_errors);
        assert!(body.contains(r#"["shard 1: bad \"key\"\\path"]"#), "{body}");
    }
}
