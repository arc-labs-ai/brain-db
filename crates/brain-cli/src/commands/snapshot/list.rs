//! `brain-cli snapshot list` — GET /v1/snapshots.

use serde::{Deserialize, Serialize};

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{dispatch_to_string, render::snapshot::SnapshotListRendered};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntry {
    pub shard: usize,
    pub id: u64,
    pub taken_at_unix_nanos: u64,
    pub size_bytes: u64,
}

pub fn run(server: &str, output: OutputFormat) -> anyhow::Result<String> {
    let resp = get(server, "/v1/snapshots")?;
    if resp.status != 200 {
        anyhow::bail!(
            "/v1/snapshots returned HTTP {}: {}",
            resp.status,
            resp.body.trim()
        );
    }
    let entries: Vec<ListEntry> = serde_json::from_str(&resp.body)
        .map_err(|e| anyhow::anyhow!("malformed list JSON: {e}; body = {}", resp.body))?;
    dispatch_to_string(&SnapshotListRendered(entries), output)
}
