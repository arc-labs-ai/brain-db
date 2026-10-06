//! `POST /v1/config/reload` — re-read the config file and apply the
//! subset that is safe to change on a running server.
//!
//! Only the log level (`monitoring.logging.level`) can take effect live:
//! the tracing subscriber's level filter is an independent reload slot, so
//! swapping it never disturbs the formatter or an attached trace exporter.
//! The log *format* is not live-reloadable — its slot also carries the
//! OTel layer, and rebuilding it would drop tracing — so a changed format,
//! like everything else (shard count, arena size, listeners, HNSW params,
//! embedder, workers, extractor tuning, rerank, LLM), is captured at boot
//! and reported under `requires_restart` rather than silently ignored.
//!
//! The response never echoes config *values* — only the dotted key
//! paths that changed — so a secret rotated in the file (`llm.api_key`,
//! `admin.token`) surfaces as a path requiring restart without leaking.

use std::collections::BTreeSet;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use serde_json::Value;
use tracing::{info, warn};

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::config::Config;

/// Dotted key paths whose new value can be applied to a running server.
const RELOADABLE_KEYS: &[&str] = &["monitoring.logging.level"];

pub async fn reload(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let Some(path) = state.config_path.as_ref() else {
        return Ok(text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "config reload unavailable: server started without a config file path\n",
        ));
    };
    let Some(apply_log_level) = state.apply_log_level.as_ref() else {
        return Ok(text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "config reload unavailable: logging reload handle not wired\n",
        ));
    };

    let new_cfg = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "config reload: re-read failed");
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("config reload failed: {e}\n"),
            ));
        }
    };

    // Diff on the raw serialized trees so we can list every changed key.
    // We emit only key paths, never values, so secrets never leak.
    let old_json = match serde_json::to_value(state.config.as_ref()) {
        Ok(v) => v,
        Err(e) => return Ok(serialize_err(&e)),
    };
    let new_json = match serde_json::to_value(&new_cfg) {
        Ok(v) => v,
        Err(e) => return Ok(serialize_err(&e)),
    };
    let changed = changed_paths(&old_json, &new_json);

    let mut reloaded: Vec<String> = Vec::new();
    let mut requires_restart: Vec<String> = Vec::new();
    for path in &changed {
        if RELOADABLE_KEYS.contains(&path.as_str()) {
            reloaded.push(path.clone());
        } else {
            requires_restart.push(path.clone());
        }
    }

    // Apply the reloadable subset. Only the level is live-reloadable, via
    // the independent filter slot (leaves formatter + OTel untouched).
    if reloaded.iter().any(|p| p == "monitoring.logging.level") {
        apply_log_level(new_cfg.monitoring.logging.level.as_str());
        info!(
            reloaded = ?reloaded,
            "config reload: applied logging level live",
        );
    }

    let body = serde_json::json!({
        "status": if changed.is_empty() { "unchanged" } else { "reloaded" },
        "reloaded": reloaded,
        "requires_restart": requires_restart,
        "detail": "only monitoring.logging.level applies live; all other \
                   changed keys require a restart",
    });
    let out = match serde_json::to_string(&body) {
        Ok(s) => s + "\n",
        Err(e) => return Ok(serialize_err(&e)),
    };
    Ok(json_response(StatusCode::OK, out))
}

fn serialize_err(e: &serde_json::Error) -> Response<ResponseBody> {
    warn!(error = %e, "config reload: serialize failed");
    text_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "config serialize failed\n",
    )
}

/// Collect the dotted paths whose leaf value differs between `old` and
/// `new`. Objects recurse key-by-key; every other node (scalar, array)
/// compares whole. The config shape is fixed (`deny_unknown_fields`), so
/// in practice only leaves differ, but added/removed keys are reported at
/// their own path for robustness. Output is sorted for a stable response.
pub(super) fn changed_paths(old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diff_into(old, new, "", &mut out);
    out.sort();
    out
}

fn diff_into(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                let child = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => diff_into(x, y, &child, out),
                    // Key present on only one side — shape drift; report it.
                    _ => out.push(child),
                }
            }
        }
        _ => {
            if old != new {
                out.push(prefix.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changed_paths_reports_only_differing_leaves() {
        let old = json!({
            "monitoring": { "logging": { "level": "info", "format": "compact" } },
            "storage": { "shard_count": 4 },
        });
        let new = json!({
            "monitoring": { "logging": { "level": "debug", "format": "compact" } },
            "storage": { "shard_count": 4 },
        });
        assert_eq!(changed_paths(&old, &new), vec!["monitoring.logging.level"]);
    }

    #[test]
    fn changed_paths_empty_when_identical() {
        let v = json!({ "a": { "b": 1 }, "c": [1, 2, 3] });
        assert!(changed_paths(&v, &v).is_empty());
    }

    #[test]
    fn changed_paths_reports_multiple_sorted() {
        let old = json!({ "hnsw": { "ef_search": 64 }, "z": { "y": 1 } });
        let new = json!({ "hnsw": { "ef_search": 128 }, "z": { "y": 2 } });
        assert_eq!(changed_paths(&old, &new), vec!["hnsw.ef_search", "z.y"]);
    }

    #[test]
    fn reloadable_partition_matches_expected_keys() {
        // Guard against RELOADABLE_KEYS drifting away from the actual
        // config path the logging subsystem can hot-apply.
        assert!(RELOADABLE_KEYS.contains(&"monitoring.logging.level"));
        // `format` and `output` are NOT live-reloadable (the format slot
        // also carries the OTel layer; output redirection is fixed at
        // boot), so they must not be in the reloadable set.
        assert!(!RELOADABLE_KEYS.contains(&"monitoring.logging.format"));
        assert!(!RELOADABLE_KEYS.contains(&"monitoring.logging.output"));
    }
}
