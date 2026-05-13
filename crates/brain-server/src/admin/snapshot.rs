//! Admin HTTP handlers for the snapshot family (spec §14/06 §5;
//! sub-task 10.9).
//!
//! Routes:
//! - `POST /v1/snapshots[?shard=N]`     → take snapshot
//! - `GET  /v1/snapshots`               → list across all shards
//! - `DELETE /v1/snapshots/<id>[?shard=N]` → delete

use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;
use tracing::warn;

use super::write_response;
use super::AdminState;

const HDR_JSON: &str = "application/json; charset=utf-8";

/// Try to dispatch a `/v1/snapshots…` request. Returns `Some(())`
/// once handled; `None` if the path/method didn't match (caller
/// falls through to other routes).
///
/// `query` is the part of the URI after `?`, or empty.
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
    if method == "POST" && path == "/v1/snapshots" {
        return Some(handle_create(stream, query, state).await);
    }
    if method == "GET" && path == "/v1/snapshots" {
        return Some(handle_list(stream, state).await);
    }
    if method == "DELETE" {
        if let Some(id_str) = path.strip_prefix("/v1/snapshots/") {
            return Some(handle_delete(stream, id_str, query, state).await);
        }
    }
    None
}

async fn handle_create<W>(stream: &mut W, query: &str, state: &Arc<AdminState>) -> io::Result<()>
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
    match shard.take_snapshot().await {
        Ok(id) => {
            let body = format!("{{\"id\":{id},\"shard\":{shard_id}}}\n");
            write_response(stream, 201, "Created", HDR_JSON, &body).await
        }
        Err(e) => {
            warn!(error = %e, "snapshot create failed");
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

async fn handle_list<W>(stream: &mut W, state: &Arc<AdminState>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut all = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.list_snapshots().await {
            Ok(descs) => {
                for d in descs {
                    all.push((idx, d));
                }
            }
            Err(e) => errors.push(format!("shard {idx}: {e}")),
        }
    }
    if !errors.is_empty() {
        let msg = errors.join("; ");
        return write_response(
            stream,
            500,
            "Internal Server Error",
            "text/plain; charset=utf-8",
            &format!("{msg}\n"),
        )
        .await;
    }
    // Hand-rolled JSON to keep the admin server free of a json
    // dep. `[{...},{...}]`.
    let mut body = String::from("[");
    for (i, (shard_id, d)) in all.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            "{{\"shard\":{shard_id},\"id\":{id},\"taken_at_unix_nanos\":{ts},\"size_bytes\":{sz}}}",
            shard_id = shard_id,
            id = d.id,
            ts = d.taken_at_unix_nanos,
            sz = d.size_bytes,
        ));
    }
    body.push_str("]\n");
    write_response(stream, 200, "OK", HDR_JSON, &body).await
}

async fn handle_delete<W>(
    stream: &mut W,
    id_str: &str,
    query: &str,
    state: &Arc<AdminState>,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let id: u64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => {
            return write_response(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                "snapshot id must be a u64\n",
            )
            .await;
        }
    };
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
    match shard.delete_snapshot(id).await {
        Ok(()) => write_response(stream, 204, "No Content", HDR_JSON, "").await,
        Err(e) => {
            warn!(error = %e, "snapshot delete failed");
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

/// Parse `?shard=N` from a URI query string. Defaults to `0`.
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
        assert_eq!(parse_shard("other=1&shard=7").unwrap(), 7);
    }

    #[test]
    fn parse_shard_rejects_garbage() {
        assert!(parse_shard("shard=abc").is_err());
    }
}
