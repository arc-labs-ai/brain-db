//! `brain-cli agent stats <agent-id>` — GET /v1/agents/{id}.
//! Returns the structured 501 today.

use crate::http::get;

pub fn run(server: &str, agent_id: &str) -> anyhow::Result<String> {
    let path = format!("/v1/agents/{agent_id}");
    let resp = get(server, &path)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
