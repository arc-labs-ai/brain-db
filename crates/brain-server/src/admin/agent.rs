//! Admin HTTP handlers for `agent` (spec §14/06 §10; sub-task 10.11).
//!
//! All routes deferred — the agent_id secondary index doesn't exist
//! yet (would need a redb scan keyed by agent + cascade delete +
//! audit entry). Each route returns a structured 501 so the CLI can
//! surface a uniform deferral message.

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
    if path == "/v1/agents" {
        return Some(match method {
            "GET" => {
                write_not_implemented(
                    stream,
                    "phase-11/agent-index",
                    "agent list (needs agent_id secondary index)",
                )
                .await
            }
            _ => return None,
        });
    }
    if let Some(_id) = path.strip_prefix("/v1/agents/") {
        return Some(match method {
            "GET" => {
                write_not_implemented(
                    stream,
                    "phase-11/agent-index",
                    "per-agent stats (needs agent_id secondary index)",
                )
                .await
            }
            "DELETE" => {
                write_not_implemented(
                    stream,
                    "phase-11/agent-cascade-delete",
                    "agent cascade delete (memories + edges + contexts)",
                )
                .await
            }
            _ => return None,
        });
    }
    None
}
