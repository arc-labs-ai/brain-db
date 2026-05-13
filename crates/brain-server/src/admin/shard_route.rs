//! Admin HTTP handlers for `shard` (spec §14/06 §11; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/shards` → 200 + `{"shards":[{"index":N,"shard_id":N}]}`
//! - `POST /v1/shards` / `DELETE /v1/shards/{idx}` → 501 (cluster
//!   expansion / decommission is Phase-12 territory).
//!
//! Note: file name is `shard_route.rs`, not `shard.rs`, because
//! `crate::shard` already exists at the workspace level.

use std::fmt::Write as _;
use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;

use super::{write_not_implemented, write_response, AdminState};

const HDR_JSON: &str = "application/json; charset=utf-8";

pub async fn dispatch<W>(
    stream: &mut W,
    method: &str,
    path: &str,
    _query: &str,
    state: &Arc<AdminState>,
) -> Option<io::Result<()>>
where
    W: AsyncWrite + Unpin,
{
    match (method, path) {
        ("GET", "/v1/shards") => Some(handle_list(stream, state).await),
        ("POST", "/v1/shards") => Some(
            write_not_implemented(
                stream,
                "phase-12/shard-create",
                "cluster expansion via online shard creation",
            )
            .await,
        ),
        ("DELETE", p) if p.starts_with("/v1/shards/") => Some(
            write_not_implemented(
                stream,
                "phase-12/shard-delete",
                "cluster decommission via online shard delete",
            )
            .await,
        ),
        _ => None,
    }
}

async fn handle_list<W>(stream: &mut W, state: &Arc<AdminState>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut body = String::with_capacity(64);
    body.push_str("{\"shards\":[");
    for (i, shard) in state.shards.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(
            &mut body,
            "{{\"index\":{i},\"shard_id\":{id}}}",
            id = shard.shard_id(),
        )
        .expect("string write");
    }
    body.push_str("]}\n");
    write_response(stream, 200, "OK", HDR_JSON, &body).await
}
