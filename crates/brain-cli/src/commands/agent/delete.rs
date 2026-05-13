//! `brain-cli agent delete <agent-id> --confirm` — DELETE
//! /v1/agents/{id}. Returns the structured 501 today.

use crate::http::delete;

pub fn run(server: &str, agent_id: &str, confirm: bool) -> anyhow::Result<String> {
    if !confirm {
        anyhow::bail!("`agent delete` is destructive; pass --confirm");
    }
    let path = format!("/v1/agents/{agent_id}");
    let resp = delete(server, &path)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
