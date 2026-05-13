//! Admin HTTP handlers for `worker` (spec §14/06 §6; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/workers[?shard=N]` → 200 +
//!   `{"workers":[{shard,name,cycles,processed,errors,last_run_unix}]}`
//! - `POST /v1/workers/{name}/{stop|start|run-now}` → 501
//!   (worker control plane is deferred; spec §14/06 §6 calls for
//!   pause/resume/trigger but the Scheduler has no such hooks today.)

use std::fmt::Write as _;
use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;
use tracing::warn;

use super::{write_not_implemented, write_response, AdminState};

const HDR_JSON: &str = "application/json; charset=utf-8";
const HDR_TEXT: &str = "text/plain; charset=utf-8";
const KNOWN_WORKERS: &[&str] = &[
    "decay",
    "access_boost",
    "consolidation",
    "hnsw_maintenance",
    "idempotency_cleanup",
    "slot_reclamation",
    "wal_retention",
    "edge_scrub",
    "counter_reconcile",
    "statistics",
    "embedder_cache_evict",
    "snapshot",
];
const KNOWN_ACTIONS: &[&str] = &["stop", "start", "run-now"];

pub async fn dispatch<W>(
    stream: &mut W,
    method: &str,
    path: &str,
    query: &str,
    state: &Arc<AdminState>,
) -> Option<io::Result<()>>
where
    W: AsyncWrite + Unpin,
{
    if method == "GET" && path == "/v1/workers" {
        return Some(handle_list(stream, query, state).await);
    }
    if method == "POST" {
        if let Some(rest) = path.strip_prefix("/v1/workers/") {
            // Expect `<name>/<action>` with no further segments.
            let mut parts = rest.splitn(2, '/');
            let name = parts.next().unwrap_or("");
            let action = parts.next().unwrap_or("");
            return Some(handle_control(stream, name, action).await);
        }
    }
    None
}

async fn handle_list<W>(stream: &mut W, query: &str, state: &Arc<AdminState>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let shard_filter = match parse_shard(query) {
        Ok(s) => s,
        Err(msg) => return write_response(stream, 400, "Bad Request", HDR_TEXT, &msg).await,
    };
    let mut body = String::with_capacity(512);
    body.push_str("{\"workers\":[");
    let mut first = true;
    for (idx, shard) in state.shards.iter().enumerate() {
        if let Some(want) = shard_filter {
            if idx != want {
                continue;
            }
        }
        match shard.scheduler_snapshot().await {
            Ok(mut snaps) => {
                snaps.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in snaps {
                    if !first {
                        body.push(',');
                    }
                    first = false;
                    write!(
                        &mut body,
                        "{{\"shard\":{idx},\"name\":\"{name}\",\"cycles\":{c},\"processed\":{p},\"errors\":{e},\"last_run_unix\":{lr}}}",
                        c = snap.cycles_total,
                        p = snap.processed_total,
                        e = snap.errors_total,
                        lr = snap.last_run_unix_secs,
                    )
                    .expect("string write");
                }
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "scheduler_snapshot failed");
            }
        }
    }
    body.push_str("]}\n");
    write_response(stream, 200, "OK", HDR_JSON, &body).await
}

async fn handle_control<W>(stream: &mut W, name: &str, action: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if !KNOWN_WORKERS.contains(&name) {
        return write_response(
            stream,
            400,
            "Bad Request",
            HDR_TEXT,
            &format!("unknown worker `{name}`\n"),
        )
        .await;
    }
    if !KNOWN_ACTIONS.contains(&action) {
        return write_response(
            stream,
            400,
            "Bad Request",
            HDR_TEXT,
            &format!("unknown worker action `{action}`\n"),
        )
        .await;
    }
    write_not_implemented(
        stream,
        "phase-11/scheduler-control",
        "live worker pause/resume/trigger",
    )
    .await
}

fn parse_shard(query: &str) -> Result<Option<usize>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map(Some)
                .map_err(|e| format!("invalid shard: {e}\n"));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_optional() {
        assert_eq!(parse_shard("").unwrap(), None);
        assert_eq!(parse_shard("shard=2").unwrap(), Some(2));
        assert!(parse_shard("shard=abc").is_err());
    }

    #[test]
    fn known_action_set() {
        assert!(KNOWN_ACTIONS.contains(&"stop"));
        assert!(KNOWN_ACTIONS.contains(&"start"));
        assert!(KNOWN_ACTIONS.contains(&"run-now"));
    }
}
