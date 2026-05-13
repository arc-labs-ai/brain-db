//! `brain-cli shard delete <shard-id> --confirm` — DELETE
//! /v1/shards/{id}. Returns the structured 501 today.

use crate::http::delete;

pub fn run(server: &str, shard_id: &str, confirm: bool) -> anyhow::Result<String> {
    if !confirm {
        anyhow::bail!("`shard delete` is destructive; pass --confirm");
    }
    let path = format!("/v1/shards/{shard_id}");
    let resp = delete(server, &path)?;
    crate::commands::worker::common::surface_status(&resp, &path)
}
