//! Admin HTTP handlers for `audit` (spec §14/06 §8; sub-task 10.11).
//!
//! Routes (both deferred — no audit-log primitive exists yet):
//! - `GET /v1/audit?...` → 501
//! - `GET /v1/audit/export` → 501

use std::io;
use std::sync::Arc;

use tokio::io::AsyncWrite;

use super::{write_not_implemented, AdminState};

pub async fn dispatch<W>(
    stream: &mut W,
    method: &str,
    path: &str,
    _query: &str,
    _state: &Arc<AdminState>,
) -> Option<io::Result<()>>
where
    W: AsyncWrite + Unpin,
{
    match (method, path) {
        ("GET", "/v1/audit") | ("GET", "/v1/audit/export") => Some(
            write_not_implemented(
                stream,
                "phase-11/audit-log",
                "audit-log query and export pathway",
            )
            .await,
        ),
        _ => None,
    }
}
