//! Admin HTTP handler for `rebuild-ann` (spec §14/06 §4;
//! sub-task 10.10).
//!
//! Route:
//! - `POST /v1/rebuild-ann[?shard=N]` → 201 +
//!   `{"entries":N,"elapsed_ms":N,"shard":N}`

use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;
use tracing::warn;

use super::{write_response, AdminState};

const HDR_JSON: &str = "application/json; charset=utf-8";

/// Try to dispatch a `/v1/rebuild-ann` request. Returns
/// `Some(...)` once handled.
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
    if method == "POST" && path == "/v1/rebuild-ann" {
        return Some(handle_rebuild(stream, query, state).await);
    }
    None
}

async fn handle_rebuild<W>(stream: &mut W, query: &str, state: &Arc<AdminState>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let shard_id = match parse_shard(query) {
        Ok(id) => id,
        Err(msg) => {
            return write_response(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                &format!("{msg}\n"),
            )
            .await;
        }
    };
    let shard = match state.shards.get(shard_id) {
        Some(s) => s,
        None => {
            return write_response(
                stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                "shard out of range\n",
            )
            .await;
        }
    };
    match shard.rebuild_hnsw().await {
        Ok(report) => {
            let body = format!(
                "{{\"entries\":{e},\"elapsed_ms\":{ms},\"shard\":{shard_id}}}\n",
                e = report.entries,
                ms = report.elapsed_ms
            );
            write_response(stream, 201, "Created", HDR_JSON, &body).await
        }
        Err(e) => {
            warn!(error = %e, "rebuild-ann failed");
            write_response(
                stream,
                500,
                "Internal Server Error",
                "text/plain; charset=utf-8",
                &format!("{e}\n"),
            )
            .await
        }
    }
}

/// Parse `?shard=N` from a query string. Defaults to `0`.
fn parse_shard(query: &str) -> Result<usize, String> {
    if query.is_empty() {
        return Ok(0);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map_err(|e| format!("invalid shard: {e}"));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_default() {
        assert_eq!(parse_shard("").unwrap(), 0);
    }

    #[test]
    fn parse_shard_explicit() {
        assert_eq!(parse_shard("shard=3").unwrap(), 3);
    }

    #[test]
    fn parse_shard_rejects_garbage() {
        assert!(parse_shard("shard=abc").is_err());
    }
}
