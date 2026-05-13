//! `brain-cli shard create --logical-id N` — POST /v1/shards.
//! Returns the structured 501 today (Phase-12 cluster expansion).

use std::time::Duration;

use crate::http::post;

pub fn run(server: &str, logical_id: u16) -> anyhow::Result<String> {
    let body = format!("{{\"logical_id\":{logical_id}}}");
    let resp = post(server, "/v1/shards", &body, Duration::from_secs(10))?;
    crate::commands::worker::common::surface_status(&resp, "/v1/shards")
}
