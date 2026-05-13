//! `brain-cli agent list [--shard N]` — GET /v1/agents.
//! Returns the structured 501 today.

use crate::http::get;

pub fn run(server: &str, shard: Option<usize>) -> anyhow::Result<String> {
    let path = match shard {
        Some(n) => format!("/v1/agents?shard={n}"),
        None => "/v1/agents".to_string(),
    };
    let resp = get(server, &path)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
